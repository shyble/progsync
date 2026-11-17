use psync_core::{EntryKind, Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;

fn id() -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some("t".into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn changed(repo: &Repo) -> Vec<String> {
    repo.status().unwrap().changes.into_iter().map(|c| c.path).collect()
}

#[test]
fn restores_deleted_and_modified_files_exactly_and_keeps_other_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".psyncignore", b"weights/\n");
    write(root, "src/lib.rs", b"fn a() {}\n");
    let repo = Repo::init(root, id()).unwrap();
    repo.commit(&msg("start")).unwrap();

    // A committed run folder: no trailing newline, binary, executable, and
    // large enough to be stored in chunks.
    let big: Vec<u8> = (0..9_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    write(root, "runs/a/manifest.json", b"{\"seed\": 1}");
    write(root, "runs/a/rows.bin", &[0, 159, 146, 150, 0, 255]);
    write(root, "runs/a/big.bin", &big);
    write(root, "runs/a/run.sh", b"#!/bin/sh\n");
    psync_core::platform::set_executable(&root.join("runs/a/run.sh"), true).unwrap();
    write(root, "src/lib.rs", b"fn a() { 1 }\n");
    repo.snapshot().unwrap();
    let ((_, saved), _) = repo.commit(&msg("run a")).unwrap();

    // New uncommitted work, then a cleanup deletes the run and a later edit
    // breaks lib.rs; an ignored file sits beside them.
    write(root, "src/new.rs", b"fn new() {}\n");
    write(root, "src/lib.rs", b"broken\n");
    write(root, "weights/w.bin", b"ignored");
    fs::remove_dir_all(root.join("runs")).unwrap();
    write(root, "runs/a/stray.txt", b"not in the run\n");
    repo.snapshot().unwrap();

    let spec = &saved.change_id.short()[..8];
    let r = repo.restore(spec, &["runs/".into(), "src/lib.rs".into()]).unwrap();
    let mut paths: Vec<&str> = r.files.iter().map(|f| f.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["runs/a/big.bin", "runs/a/manifest.json", "runs/a/rows.bin", "runs/a/run.sh", "runs/a/stray.txt", "src/lib.rs"]
    );
    assert!(r.files.iter().find(|f| f.path == "runs/a/stray.txt").unwrap().leaf.is_none(), "removed");
    assert_eq!(r.files.iter().find(|f| f.path == "runs/a/big.bin").unwrap().size, Some(big.len() as u64));

    assert_eq!(fs::read(root.join("runs/a/manifest.json")).unwrap(), b"{\"seed\": 1}");
    assert_eq!(fs::read(root.join("runs/a/rows.bin")).unwrap(), [0, 159, 146, 150, 0, 255]);
    assert!(fs::read(root.join("runs/a/big.bin")).unwrap() == big);
    assert!(psync_core::platform::is_executable(&fs::metadata(root.join("runs/a/run.sh")).unwrap()));
    assert!(!root.join("runs/a/stray.txt").exists());
    assert_eq!(fs::read(root.join("src/lib.rs")).unwrap(), b"fn a() { 1 }\n");
    assert_eq!(fs::read(root.join("src/new.rs")).unwrap(), b"fn new() {}\n", "other work untouched");
    assert_eq!(fs::read(root.join("weights/w.bin")).unwrap(), b"ignored", "ignored files untouched");
    assert_eq!(changed(&repo), vec!["src/new.rs"], "working change = saved + new work");

    // Restoring again changes nothing; undo brings back the state before.
    assert!(repo.restore(spec, &["runs/".into()]).unwrap().files.is_empty());
    repo.undo().unwrap();
    assert!(!root.join("runs/a/manifest.json").exists());
    assert_eq!(fs::read(root.join("src/lib.rs")).unwrap(), b"broken\n");
    assert_eq!(fs::read(root.join("runs/a/stray.txt")).unwrap(), b"not in the run\n");
}

#[test]
fn restore_and_file_at_reject_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", b"a\n");
    let repo = Repo::init(root, id()).unwrap();
    repo.commit(&msg("start")).unwrap();
    assert!(repo.restore("@-", &[]).is_err());
    assert!(repo.restore("@-", &["nope/".into()]).is_err());
    assert!(repo.restore("nosuchrev", &["src/a.rs".into()]).is_err());

    let (_, kind, h) = repo.file_at("@-", "src/a.rs").unwrap();
    assert_eq!(kind, EntryKind::File);
    assert_eq!(repo.store.get_blob(&h).unwrap(), b"a\n");
    assert!(repo.file_at("@-", "src").is_err(), "a directory");
    assert!(repo.file_at("@-", "src/b.rs").is_err(), "missing");
}

#[test]
fn identify_reports_last_commit_and_dirty_without_recording() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".psyncignore", b"weights/\n");
    write(root, "src/a.rs", b"a\n");
    write(root, "docs/x.md", b"x\n");
    let repo = Repo::init(root, id()).unwrap();
    let ((done_hash, done), _) = repo.commit(&msg("start")).unwrap();
    // `id` needs no lock: it works while a command holds the workspace.
    let held = repo;
    let r = Repo::open_unlocked(root).unwrap();
    let head = r.op_head().unwrap();

    let i = r.identify(&[]).unwrap();
    assert_eq!((i.revision, i.commit.change_id), (done_hash, done.change_id));
    assert!(i.changed.is_empty());
    assert_eq!(i.on_main, None, "no main yet");

    write(root, "weights/w.bin", b"ignored, never counts");
    write(root, "docs/x.md", b"edited\n");
    let i = r.identify(&[]).unwrap();
    assert_eq!(i.changed, vec!["docs/x.md"]);
    assert!(r.identify(&["src/".into()]).unwrap().changed.is_empty(), "only paths asked about count");
    assert_eq!(r.op_head().unwrap(), head, "nothing recorded");
    drop(held);
}
