//! Drives `psync mcp` over stdio like an agent client would.

use psync_core::json;
use psync_core::json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_psync");

fn psync(cfg: &Path, dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(BIN)
        .env("XDG_CONFIG_HOME", cfg)
        .env("PSYNC_USER_NAME", "Ada")
        .env("PSYNC_USER_EMAIL", "ada@example.com")
        .env_remove("PSYNC_AGENT")
        .arg("--json")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    psync_core::json::from_slice(&out.stdout).unwrap()
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    fn start(cfg: &Path, dir: &Path) -> Client {
        let mut child = Command::new(BIN)
            .env("XDG_CONFIG_HOME", cfg)
            .env("PSYNC_USER_NAME", "Ada")
            .env("PSYNC_USER_EMAIL", "ada@example.com")
            .env_remove("PSYNC_AGENT")
            .arg("-C")
            .arg(dir)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client { child, stdin, stdout, next: 0 }
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = psync_core::json::from_str(&line).unwrap();
        assert_eq!(reply["id"], json!(self.next));
        reply
    }

    /// Call a tool; returns (structured result, isError).
    fn call(&mut self, name: &str, args: Value) -> (Value, bool) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }))["result"].clone();
        let text: Value = psync_core::json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, r["structuredContent"]);
        (text, r["isError"].as_bool().unwrap())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn agent_workflow_over_mcp() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, repo) = (tmp.path().join("cfg"), tmp.path().join("app"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "fn a() {}\n").unwrap();
    assert_eq!(psync(&cfg, &repo, &["init"])["ok"], json!(true));

    let mut c = Client::start(&cfg, &repo);
    let init = c.request(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test-agent", "version": "1" } }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "psync");
    c.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in ["context", "commit", "land", "task_start", "claim"] {
        assert!(names.contains(&n), "{n} missing");
    }

    let (t, err) = c.call("task_add", json!({ "title": "say hello", "check": "grep -q hello lib.rs" }));
    assert!(!err, "{t}");
    let id = t["task"]["id"].as_str().unwrap().to_string();
    assert!(!c.call("task_start", json!({ "id": id })).1);
    assert!(!c.call("claim", json!({ "patterns": ["lib.rs"] })).1);

    std::fs::write(repo.join("lib.rs"), "fn a() { hello() }\n").unwrap();
    let (ctx, _) = c.call("context", json!({}));
    assert_eq!(ctx["working"]["files"], json!(["lib.rs"]), "{ctx}");

    // Errors come back as tool errors, not protocol errors.
    let (e, err) = c.call("commit", json!({}));
    assert!(err && e["error"].as_str().unwrap().contains("message"), "{e}");
    let (e, err) = c.call("show", json!({ "rev": "nosuchrev" }));
    assert!(err && e["ok"] == json!(false), "{e}");

    let (commit, err) = c.call("commit", json!({ "message": "say hello", "meta": { "model": "test" } }));
    assert!(!err, "{commit}");
    let (land, err) = c.call("land", json!({ "no_check": true }));
    assert!(!err, "{land}");
    let (done, err) = c.call("task_done", json!({ "id": id }));
    assert!(!err, "{done}");

    // Work is attributed to the client when no agent is configured.
    let (log, _) = c.call("log", json!({ "limit": 5 }));
    let landed = log["revisions"].as_array().unwrap().iter().find(|r| r["intent"] == "say hello").unwrap();
    assert_eq!(landed["committer"]["agent"], "test-agent");

    let unknown = c.request("nope/method", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
}
