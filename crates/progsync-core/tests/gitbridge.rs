//! Uses Unix permission bits and symlinks to check modes round-trip.
#![cfg(unix)]

use progsync_core::repo::LandResult;
use progsync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn id() -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some("t".into()) }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Grace", "-c", "user.email=grace@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn commit_all(dir: &Path, m: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", m]);
}

/// A Git repository with an exec file, a symlink, a deletion, a subdirectory and a merge.
fn git_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    write(dir, "README.md", "hello\n");
    write(dir, "run.sh", "#!/bin/sh\n");
    fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("README.md", dir.join("link")).unwrap();
    commit_all(dir, "first");
    write(dir, "src/a.rs", "fn a() {}\n");
    fs::remove_file(dir.join("link")).unwrap();
    commit_all(dir, "second\n\nwith a body");
    git(dir, &["checkout", "-q", "-b", "side"]);
    write(dir, "side.txt", "side\n");
    commit_all(dir, "side work");
    git(dir, &["checkout", "-q", "main"]);
    write(dir, "src/b.rs", "fn b() {}\n");
    commit_all(dir, "third");
    git(dir, &["merge", "-q", "--no-edit", "side"]);
}

fn psync_repo(dir: &Path) -> Repo {
    fs::create_dir_all(dir).unwrap();
    Repo::init(dir, id()).unwrap()
}

#[test]
fn import_history_then_continue() {
    let tmp = tempfile::tempdir().unwrap();
    let (g, p) = (tmp.path().join("g"), tmp.path().join("p"));
    git_repo(&g);
    let r = psync_repo(&p);
    let out = r.git_import(&g, "main").unwrap();
    assert_eq!(out.commits, 4, "first-parent history: first, second, third, merge");
    assert!(out.synced);
    r.snapshot().unwrap();
    let main = r.main_tip().unwrap().unwrap();
    let log = r.log(main, 10).unwrap();
    let intents: Vec<&str> = log.iter().map(|(_, rv)| rv.meta.intent.as_str()).collect();
    assert_eq!(intents, ["Merge branch 'side'", "third", "second\n\nwith a body", "first"]);
    assert_eq!(log[0].1.author.email, "grace@example.com");
    assert_eq!(log[0].1.meta.extra["git_commit"], git(&g, &["rev-parse", "main"]));
    // Files match Git's tree, including modes; the deleted symlink is gone.
    assert_eq!(fs::read_to_string(p.join("side.txt")).unwrap(), "side\n");
    assert_eq!(fs::read_to_string(p.join("src/b.rs")).unwrap(), "fn b() {}\n");
    assert_ne!(fs::metadata(p.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert!(!p.join("link").exists());

    // Nothing new: no-op. New commits: only those are imported.
    assert_eq!(r.git_import(&g, "main").unwrap().commits, 0);
    write(&g, "src/a.rs", "fn a() { 1 }\n");
    commit_all(&g, "fourth");
    assert_eq!(r.git_import(&g, "main").unwrap().commits, 1);

    // Importing the same history elsewhere gives the same change ids.
    let p2 = tmp.path().join("p2");
    let r2 = psync_repo(&p2);
    r2.git_import(&g, "main").unwrap();
    let ids = |r: &Repo| r.log(r.main_tip().unwrap().unwrap(), 10).unwrap().into_iter().map(|(_, x)| x.change_id).collect::<Vec<_>>();
    assert_eq!(ids(&r), ids(&r2));

    // Once main has psync-only work, import refuses to continue.
    write(&p, "psync.txt", "x\n");
    r.snapshot().unwrap();
    r.commit(&MetaUpdate { intent: Some("psync work".into()), ..Default::default() }).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    assert!(r.git_import(&g, "main").is_err());
}

#[test]
fn export_main_and_continue_fast_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let (p, g) = (tmp.path().join("p"), tmp.path().join("g"));
    write(&p, "a.txt", "a\n");
    write(&p, "bin/tool", "#!/bin/sh\n");
    fs::set_permissions(p.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    let r = psync_repo(&p);
    r.commit(&MetaUpdate { intent: Some("start".into()), ..Default::default() }).unwrap();
    write(&p, "b.txt", "b\n");
    fs::remove_file(p.join("a.txt")).unwrap();
    r.snapshot().unwrap();
    r.commit(&MetaUpdate { intent: Some("swap a for b".into()), ..Default::default() }).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));

    fs::create_dir_all(&g).unwrap();
    git(&g, &["init", "-q", "-b", "main"]);
    let out = r.git_export(&g, "main", true).unwrap();
    assert_eq!(out.commits, 2);
    assert_eq!(git(&g, &["log", "--format=%s|%an", "main"]), "swap a for b|Ada\nstart|Ada");
    assert_eq!(git(&g, &["ls-tree", "-r", "--format=%(objectmode) %(path)", "main"]), "100644 b.txt\n100755 bin/tool");
    assert_eq!(git(&g, &["show", "main:b.txt"]), "b");
    assert_eq!(r.git_export(&g, "main", true).unwrap().commits, 0, "up to date");

    write(&p, "c.txt", "c\n");
    r.snapshot().unwrap();
    r.commit(&MetaUpdate { intent: Some("add c".into()), ..Default::default() }).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    let before = git(&g, &["rev-parse", "main"]);
    assert_eq!(r.git_export(&g, "main", true).unwrap().commits, 1);
    assert_eq!(git(&g, &["rev-parse", "main~1"]), before, "fast-forward");

    // A branch with commits psync did not make is not overwritten.
    git(&g, &["checkout", "-q", "-b", "other", "main"]);
    write(&g, "x.txt", "x\n");
    commit_all(&g, "foreign");
    let other = tmp.path().join("p-other");
    let r2 = psync_repo(&other);
    r2.commit(&MetaUpdate { intent: Some("unrelated".into()), ..Default::default() }).unwrap();
    assert!(matches!(r2.land(None).unwrap(), LandResult::Landed { .. }));
    assert!(r2.git_export(&g, "other", true).is_err());
}

#[test]
fn round_trip_exports_only_new_psync_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (g, p) = (tmp.path().join("g"), tmp.path().join("p"));
    git_repo(&g);
    let r = psync_repo(&p);
    r.git_import(&g, "main").unwrap();
    r.snapshot().unwrap();
    write(&p, "new.txt", "from psync\n");
    r.snapshot().unwrap();
    r.commit(&MetaUpdate { intent: Some("psync change".into()), ..Default::default() }).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    let head = git(&g, &["rev-parse", "main"]);
    assert_eq!(r.git_export(&g, "main", true).unwrap().commits, 1);
    assert_eq!(git(&g, &["rev-parse", "main~1"]), head);
    assert_eq!(git(&g, &["show", "main:new.txt"]), "from psync");
}
