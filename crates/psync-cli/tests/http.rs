//! The HTTP API of `psync daemon run --http`, driven like an external tool.

use psync_core::json;
use psync_core::json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    psync_core::json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

/// (status, JSON body)
fn request(url: &str, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let addr = url.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n", body.len());
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\nX-Psync-Agent: http-client\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, psync_core::json::from_str(body).unwrap())
}

#[test]
fn http_api_serves_tools_with_a_token() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, repo) = (tmp.path().join("cfg"), tmp.path().join("app"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    assert_eq!(psync(&cfg, &repo, &["init"])["ok"], json!(true));

    let mut daemon = Command::new(BIN)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("PSYNC_USER_NAME", "Ada")
        .env("PSYNC_USER_EMAIL", "ada@example.com")
        .env_remove("PSYNC_AGENT")
        .arg("-C")
        .arg(&repo)
        .args(["daemon", "run", "--no-snapshot", "--http"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let info = repo.join(".psync/workspaces/default/http.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(info.exists() && psync(&cfg, &repo, &["daemon", "status"])["running"] == json!(true)) {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&info).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let v: Value = psync_core::json::from_slice(&std::fs::read(&info).unwrap()).unwrap();
    let (url, token) = (v["url"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string());
    assert!(url.starts_with("http://127.0.0.1:"));
    let t = Some(token.as_str());

    assert_eq!(request(&url, "GET", "/v1/health", None, None).0, 200);
    assert_eq!(request(&url, "GET", "/v1/tools", None, None).0, 401);
    assert_eq!(request(&url, "GET", "/v1/tools", Some("wrong"), None).0, 401);
    let (s, tools) = request(&url, "GET", "/v1/tools", t, None);
    assert_eq!(s, 200);
    assert!(tools["tools"].as_array().unwrap().iter().any(|x| x["name"] == "context"));

    let (s, st) = request(&url, "GET", "/v1/tools/status", t, None);
    assert_eq!(s, 200, "{st}");
    assert_eq!(st["files"][0]["path"], "a.txt");
    let (s, added) = request(&url, "POST", "/v1/tools/task_add", t, Some(json!({ "title": "via http" })));
    assert_eq!(s, 200, "{added}");
    assert_eq!(added["task"]["created_by"]["agent"], "http-client");
    let (s, list) = request(&url, "GET", "/v1/tools/task_list?all=true", t, None);
    assert_eq!((s, list["tasks"][0]["title"].clone()), (200, json!("via http")));
    let (s, log) = request(&url, "GET", "/v1/tools/log?limit=1", t, None);
    assert_eq!((s, log["revisions"].as_array().unwrap().len()), (200, 1));

    assert_eq!(request(&url, "GET", "/v1/tools/commit", t, None).0, 405, "changes need POST");
    assert_eq!(request(&url, "POST", "/v1/tools/nope", t, Some(json!({}))).0, 404);
    let (s, e) = request(&url, "POST", "/v1/tools/commit", t, Some(json!({})));
    assert_eq!((s, e["ok"].clone()), (422, json!(false)), "{e}");
    let (s, e) = request(&url, "POST", "/v1/tools/status", t, Some(json!([1])));
    assert_eq!((s, e["ok"].clone()), (400, json!(false)));

    assert_eq!(psync(&cfg, &repo, &["daemon", "stop"])["stopped"], json!(true));
    let _ = daemon.wait();
    assert!(!info.exists(), "token file removed on exit");
}
