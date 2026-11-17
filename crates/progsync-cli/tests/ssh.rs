//! Remotes over ssh, with a fake `ssh` that runs the remote command locally.
#![cfg(unix)]

use progsync_core::json;
use progsync_core::json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_psync");

struct Env {
    _tmp: tempfile::TempDir,
    cfg: PathBuf,
    ssh: PathBuf,
    log: PathBuf,
    root: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (cfg, ssh, log) = (root.join("cfg"), root.join("fake-ssh"), root.join("ssh.log"));
    // Logs its arguments, then runs the last one (the remote command) here.
    std::fs::write(&ssh, format!("#!/bin/sh\necho \"$@\" >> '{}'\nfor a; do last=\"$a\"; done\nexec sh -c \"$last\"\n", log.display())).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    Env { _tmp: tmp, cfg, ssh, log, root }
}

fn psync(e: &Env, dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(BIN)
        .env("XDG_CONFIG_HOME", &e.cfg)
        .env("PSYNC_USER_NAME", "Ada")
        .env("PSYNC_USER_EMAIL", "ada@example.com")
        .env("PSYNC_AGENT", "ssh-test")
        .env("PSYNC_SSH", &e.ssh)
        .env("PSYNC_REMOTE_PSYNC", BIN)
        .arg("--json")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    progsync_core::json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)))
}

fn ok(v: Value) -> Value {
    assert_eq!(v["ok"], json!(true), "{v}");
    v
}

fn edit_land(e: &Env, dir: &Path, file: &str, text: &str, msg: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    ok(psync(e, dir, &["commit", "-m", msg]));
    ok(psync(e, dir, &["land", "--no-check"]));
}

#[test]
fn clone_push_pull_over_ssh() {
    let e = env();
    let a = e.root.join("server/app");
    std::fs::create_dir_all(a.join("src")).unwrap();
    std::fs::write(a.join("src/lib.rs"), "fn a() {}\n").unwrap();
    ok(psync(&e, &a, &["init"]));
    edit_land(&e, &a, "README.md", "hi\n", "start");
    ok(psync(&e, &a, &["task", "add", "shared task"]));

    let url = format!("ssh://ada@example.com:2222{}", a.display());
    let c = ok(psync(&e, &e.root, &["clone", &url, "laptop"]));
    assert_eq!(c["tasks"], 1);
    let b = e.root.join("laptop");
    assert_eq!(std::fs::read_to_string(b.join("src/lib.rs")).unwrap(), "fn a() {}\n");
    let log = std::fs::read_to_string(&e.log).unwrap();
    assert!(log.contains("-p 2222 ada@example.com"), "{log}");
    assert!(log.contains(&format!("remote-serve '{}'", a.display())), "{log}");

    // Push from the clone; the server's main moves.
    edit_land(&e, &b, "b.txt", "from laptop\n", "laptop work");
    let p = ok(psync(&e, &b, &["push"]));
    assert!(p["objects"].as_u64().unwrap() >= 3, "{p}");
    let a_main = ok(psync(&e, &a, &["show", "main"]))["revision"]["intent"].clone();
    assert_eq!(a_main, "laptop work");

    // Work on the server; pull fast-forwards; push when behind is refused.
    ok(psync(&e, &a, &["sync"]));
    edit_land(&e, &a, "a.txt", "on server\n", "server work");
    edit_land(&e, &b, "c.txt", "more\n", "more laptop work");
    let refused = psync(&e, &b, &["push"]);
    assert_eq!(refused["ok"], json!(false));
    assert!(refused["error"].as_str().unwrap().contains("pull"), "{refused}");
    let pulled = ok(psync(&e, &b, &["pull"]));
    assert_eq!(pulled["main_update"], "diverged", "{pulled}");
    ok(psync(&e, &b, &["land", "--no-check"]));
    ok(psync(&e, &b, &["push"]));
    ok(psync(&e, &a, &["sync"]));
    assert_eq!(std::fs::read_to_string(a.join("c.txt")).unwrap(), "more\n");
    assert_eq!(std::fs::read_to_string(b.join("a.txt")).unwrap(), "on server\n");

    // Up to date: nothing is sent.
    assert_eq!(ok(psync(&e, &b, &["push"]))["objects"], 0);
    // scp-like addresses, and a clear error for a path that is not a repository.
    ok(psync(&e, &b, &["remote", "add", "scp", &format!("example.com:{}", a.display())]));
    ok(psync(&e, &b, &["fetch", "scp"]));
    let bad = psync(&e, &b, &["remote", "add", "bad", "example.com:/no/such/repo"]);
    assert_eq!(bad["ok"], json!(false));
    assert!(bad["error"].as_str().unwrap().contains("not a psync repository") || bad["error"].as_str().unwrap().contains("no such"), "{bad}");
}

#[test]
fn claims_travel_over_ssh() {
    let e = env();
    let a = e.root.join("server/app");
    std::fs::create_dir_all(&a).unwrap();
    ok(psync(&e, &a, &["init"]));
    edit_land(&e, &a, "x.txt", "x\n", "start");
    let url = format!("ssh://example.com{}", a.display());
    ok(psync(&e, &e.root, &["clone", &url, "one"]));
    ok(psync(&e, &e.root, &["clone", &url, "two"]));
    let (one, two) = (e.root.join("one"), e.root.join("two"));

    ok(psync(&e, &one, &["claim", "x.txt"]));
    ok(psync(&e, &one, &["push"]));
    let f = ok(psync(&e, &two, &["pull"]));
    assert_eq!(f["claims_from_other_repositories"], 1, "{f}");
    let claims = ok(psync(&e, &two, &["claim"]))["claims"].clone();
    assert!(claims[0]["repository"].as_str().unwrap().ends_with(&format!(":{}", one.display())), "{claims}");
    let refused = psync(&e, &two, &["claim", "x.txt"]);
    assert_eq!(refused["ok"], json!(false), "{refused}");

    // Landing over someone else's claim works, with a warning.
    std::fs::write(two.join("x.txt"), "two\n").unwrap();
    ok(psync(&e, &two, &["commit", "-m", "edit x"]));
    let landed = ok(psync(&e, &two, &["land", "--no-check"]));
    assert_eq!(landed["claimed_by_others"][0]["path"], "x.txt", "{landed}");

    ok(psync(&e, &one, &["release"]));
    ok(psync(&e, &one, &["push"]));
    assert_eq!(ok(psync(&e, &two, &["pull"]))["claims_from_other_repositories"], 0);
    assert_eq!(ok(psync(&e, &two, &["claim"]))["claims"], json!([]));
}

#[test]
fn revs_and_workspaces_travel_over_ssh() {
    let e = env();
    let a = e.root.join("server/app");
    std::fs::create_dir_all(&a).unwrap();
    ok(psync(&e, &a, &["init"]));
    edit_land(&e, &a, "x.txt", "x\n", "start");
    let url = format!("ssh://example.com{}", a.display());
    ok(psync(&e, &e.root, &["clone", &url, "one"]));
    ok(psync(&e, &e.root, &["clone", &url, "two"]));
    let (one, two) = (e.root.join("one"), e.root.join("two"));

    std::fs::write(one.join("t.txt"), "try me\n").unwrap();
    let added = ok(psync(&e, &one, &["rev", "add", "try-t", "--remote", "origin"]));
    assert_eq!(added["remote"], "origin", "{added}");
    let f = ok(psync(&e, &two, &["pull"]));
    assert_eq!(f["revs"], json!(["try-t"]), "{f}");
    assert!(f["workspaces_of_other_repositories"].as_u64().unwrap() >= 2, "{f}");
    let ctx = ok(psync(&e, &two, &["context"]));
    let peers = ctx["workspaces"].as_array().unwrap();
    assert!(peers.iter().any(|p| p["repository"].as_str().is_some_and(|r| r.ends_with(&format!(":{}", one.display())))), "{ctx}");

    ok(psync(&e, &two, &["switch", "try-t"]));
    assert_eq!(std::fs::read_to_string(two.join("t.txt")).unwrap(), "try me\n");
    assert!(ok(psync(&e, &two, &["status"]))["parked"].is_object());
    ok(psync(&e, &two, &["switch", "-"]));
    assert!(!two.join("t.txt").exists());
    assert_eq!(ok(psync(&e, &two, &["rev", "list"]))["revs"][0]["name"], "try-t");
}
