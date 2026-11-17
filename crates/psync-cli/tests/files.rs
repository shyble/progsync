//! `cat` and `restore` from the command line.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_psync");

fn psync(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("PSYNC_USER_NAME", "Ada")
        .env("PSYNC_AGENT", "test")
        .arg("-C")
        .arg(dir.join("repo"))
        .args(args)
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = psync(dir, args);
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

#[test]
fn cat_prints_exact_bytes_and_restore_reports_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("runs")).unwrap();
    let big: Vec<u8> = (0..6_000_000u32).map(|i| (i.wrapping_mul(2246822519) >> 11) as u8).collect();
    std::fs::write(repo.join("runs/m.json"), b"{\"a\": 1}").unwrap();
    std::fs::write(repo.join("runs/big.bin"), &big).unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["commit", "-m", "run"]);

    assert_eq!(ok(dir.path(), &["cat", "@-:runs/m.json"]), b"{\"a\": 1}");
    assert_eq!(ok(dir.path(), &["cat", "@-", "runs/m.json"]), b"{\"a\": 1}");
    assert!(ok(dir.path(), &["cat", "@-:runs/big.bin"]) == big, "large (chunked) file byte for byte");
    let j: psync_core::json::Value = psync_core::json::from_slice(&ok(dir.path(), &["--json", "cat", "@-:runs/m.json"])).unwrap();
    assert_eq!((j["content"].as_str(), j["size"].as_u64(), j["binary"].as_bool()), (Some("{\"a\": 1}"), Some(8), Some(false)));
    assert!(!psync(dir.path(), &["cat", "@-:runs"]).status.success(), "a directory");
    assert!(!psync(dir.path(), &["cat", "@-"]).status.success(), "no path");

    std::fs::remove_dir_all(repo.join("runs")).unwrap();
    let j: psync_core::json::Value = psync_core::json::from_slice(&ok(dir.path(), &["--json", "restore", "@-", "--", "runs/"])).unwrap();
    let files = j["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f["hash"].as_str().is_some() && f["size"].as_u64().is_some()));
    assert_eq!(std::fs::read(repo.join("runs/m.json")).unwrap(), b"{\"a\": 1}");
    assert!(std::fs::read(repo.join("runs/big.bin")).unwrap() == big);
}

#[test]
fn id_prints_revision_and_dirty() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), b"a\n").unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["commit", "-m", "start"]);
    let j: psync_core::json::Value = psync_core::json::from_slice(&ok(dir.path(), &["--json", "id"])).unwrap();
    let rev = j["revision"].as_str().unwrap().to_string();
    assert_eq!(rev.len(), 64);
    assert_eq!(j["dirty"].as_bool(), Some(false));
    assert_eq!(String::from_utf8(ok(dir.path(), &["id"])).unwrap(), format!("{rev}\n"));
    std::fs::write(repo.join("a.txt"), b"b\n").unwrap();
    assert_eq!(String::from_utf8(ok(dir.path(), &["id"])).unwrap(), format!("{rev}-dirty\n"));
}

#[cfg(unix)]
#[test]
fn archive_is_exactly_the_revision_and_reproducible() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let long = format!("deep/{}/file.txt", "d".repeat(120));
    let big: Vec<u8> = (0..5_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 7) as u8).collect();
    for (p, c) in [("src/a.rs", &b"fn a() {}\n"[..]), ("run.sh", b"#!/bin/sh\necho hi\n"), (long.as_str(), b"long\n"), ("big.bin", &big)] {
        let f = repo.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
    }
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src/a.rs", repo.join("link")).unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["commit", "-m", "release"]);
    // Uncommitted and untracked files stay out.
    std::fs::write(repo.join("src/a.rs"), b"uncommitted\n").unwrap();
    std::fs::write(repo.join("notes.txt"), b"untracked\n").unwrap();

    let tar1 = dir.path().join("a.tar");
    let j: psync_core::json::Value = psync_core::json::from_slice(&ok(
        dir.path(),
        &["--json", "archive", "@-", "--prefix", "app-1/", "-o", tar1.to_str().unwrap()],
    ))
    .unwrap();
    let rev = j["revision"].as_str().unwrap().to_string();
    assert_eq!(j["files"].as_u64(), Some(4));

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let st = Command::new("tar").arg("-xf").arg(&tar1).arg("-C").arg(&out).status().unwrap();
    assert!(st.success());
    let app = out.join("app-1");
    assert_eq!(std::fs::read(app.join("src/a.rs")).unwrap(), b"fn a() {}\n", "the committed version");
    assert!(!app.join("notes.txt").exists());
    assert_eq!(std::fs::read(app.join(&long)).unwrap(), b"long\n", "pax path over 100 bytes");
    assert!(std::fs::read(app.join("big.bin")).unwrap() == big);
    assert_ne!(std::fs::metadata(app.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(std::fs::read_link(app.join("link")).unwrap(), Path::new("src/a.rs"));

    // The revision is in the tar (pax global comment, as git archive), and
    // the same revision gives the same bytes.
    let bytes = std::fs::read(&tar1).unwrap();
    assert!(bytes.windows(rev.len() + 8).any(|w| w == format!("comment={rev}").as_bytes()));
    let tar2 = dir.path().join("b.tar");
    ok(dir.path(), &["archive", "@-", "--prefix", "app-1/", "-o", tar2.to_str().unwrap()]);
    assert!(std::fs::read(&tar2).unwrap() == bytes, "reproducible");

    // Paths limit the archive; a path matching nothing is an error.
    let tar3 = dir.path().join("c.tar");
    ok(dir.path(), &["archive", "@-", "src/", "-o", tar3.to_str().unwrap()]);
    let list = Command::new("tar").arg("-tf").arg(&tar3).output().unwrap().stdout;
    let names: Vec<String> = String::from_utf8(list).unwrap().lines().map(String::from).collect();
    assert_eq!(names, vec!["src/", "src/a.rs"]);
    assert!(!psync(dir.path(), &["archive", "@-", "nope/", "-o", tar3.to_str().unwrap()]).status.success());
    assert!(!psync(dir.path(), &["--json", "archive", "@-"]).status.success(), "--json needs -o");
}
