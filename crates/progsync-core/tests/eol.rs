//! Line-ending conversion (T-27).

use progsync_core::config;
use progsync_core::diff;
use progsync_core::{Identity, MetaUpdate, Repo};
use std::fs;

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn stored(repo: &Repo, path: &str) -> Vec<u8> {
    let tree = repo.working().unwrap().1.tree;
    let (_, h) = diff::lookup(&repo.store, tree, path).unwrap().unwrap();
    repo.store.get_blob(&h).unwrap()
}

fn set_eol(repo: &Repo, value: &str) {
    config::set(&config::repo_path(repo.dot()), "text.eol", Some(value)).unwrap();
}

#[test]
fn off_by_default_files_are_stored_as_they_are() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "a\r\nb\r\n").unwrap();
    let repo = Repo::init(dir.path(), id("a")).unwrap();
    assert_eq!(stored(&repo, "a.txt"), b"a\r\nb\r\n");
    assert!(config::set(&config::repo_path(repo.dot()), "text.eol", Some("dos")).is_err());
}

#[test]
fn crlf_on_disk_lf_in_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("win");
    fs::create_dir(&root).unwrap();
    // The setting must be in place before the first snapshot: set it on a
    // repository created empty.
    let repo = Repo::init(&root, id("a")).unwrap();
    set_eol(&repo, "crlf");
    fs::write(root.join("a.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
    fs::write(root.join("data.bin"), b"\0\x01\r\n").unwrap();
    fs::write(root.join(".psyncattributes"), "*.sh eol=lf\n").unwrap();
    fs::write(root.join("run.sh"), "echo hi\n").unwrap();
    repo.snapshot().unwrap();
    assert_eq!(stored(&repo, "a.txt"), b"one\ntwo\nthree\n");
    assert_eq!(stored(&repo, "data.bin"), b"\0\x01\r\n", "binary files are left alone");
    repo.commit(&msg("start")).unwrap();
    repo.land(None).unwrap();
    assert!(repo.status().unwrap().changes.is_empty());

    // An edit of one line is a one-line change, whatever the endings.
    fs::write(root.join("a.txt"), "one\r\n2\r\nthree\r\n").unwrap();
    repo.snapshot().unwrap();
    let st = repo.status().unwrap();
    let d = diff::unified(&repo.store, &st.changes[0]).unwrap();
    assert!(d.contains("-two\n+2\n") && !d.contains('\r'), "{d}");
    repo.commit(&msg("edit")).unwrap();
    repo.land(None).unwrap();

    // Checkouts write the setting's endings: a new workspace, and undo.
    let ws = repo.workspace_add("other", None, None).unwrap().info.path.unwrap();
    assert_eq!(fs::read(ws.join("a.txt")).unwrap(), b"one\r\n2\r\nthree\r\n");
    assert_eq!(fs::read(ws.join("run.sh")).unwrap(), b"echo hi\n", "attributes win over the setting");
    assert_eq!(fs::read(ws.join("data.bin")).unwrap(), b"\0\x01\r\n");
    let other = Repo::discover_as(&ws, id("b")).unwrap();
    assert!(other.status().unwrap().changes.is_empty());
    drop(other);

    // A clone without the setting gets LF files.
    let lf = dir.path().join("mac");
    let clone = Repo::clone_from(root.to_str().unwrap(), &lf, id("c")).unwrap();
    assert_eq!(fs::read(lf.join("a.txt")).unwrap(), b"one\n2\nthree\n");
    assert!(clone.status().unwrap().changes.is_empty());
}

#[test]
fn attributes_in_the_tree_apply_without_the_setting() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    fs::create_dir(&root).unwrap();
    fs::write(root.join(".gitattributes"), "*.bat eol=crlf\n*.txt text\n").unwrap();
    fs::write(root.join("run.bat"), "@echo off\r\necho hi\r\n").unwrap();
    fs::write(root.join("notes.txt"), "a\r\n").unwrap();
    fs::write(root.join("keep.md"), "kept\r\n").unwrap();
    let repo = Repo::init(&root, id("a")).unwrap();
    assert_eq!(stored(&repo, "run.bat"), b"@echo off\necho hi\n");
    assert_eq!(stored(&repo, "notes.txt"), b"a\n");
    assert_eq!(stored(&repo, "keep.md"), b"kept\r\n", "no rule: unchanged");
    repo.commit(&msg("start")).unwrap();
    repo.land(None).unwrap();

    let dst = dir.path().join("dst");
    Repo::clone_from(root.to_str().unwrap(), &dst, id("b")).unwrap();
    assert_eq!(fs::read(dst.join("run.bat")).unwrap(), b"@echo off\r\necho hi\r\n");
    assert_eq!(fs::read(dst.join("notes.txt")).unwrap(), if cfg!(windows) { &b"a\r\n"[..] } else { b"a\n" });
    assert_eq!(fs::read(dst.join("keep.md")).unwrap(), b"kept\r\n");
}

#[test]
fn checkout_is_stable_for_files_with_mixed_endings() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path(), id("a")).unwrap();
    set_eol(&repo, "crlf");
    fs::write(dir.path().join("m.txt"), "a\r\nb\nc\r\n").unwrap();
    repo.snapshot().unwrap();
    assert_eq!(stored(&repo, "m.txt"), b"a\nb\nc\n");
    repo.commit(&msg("mixed")).unwrap();
    // Undo the commit and redo the checkout from the store: CRLF throughout,
    // and still no change.
    fs::remove_file(dir.path().join("m.txt")).unwrap();
    repo.snapshot().unwrap();
    repo.undo().unwrap();
    assert_eq!(fs::read(dir.path().join("m.txt")).unwrap(), b"a\r\nb\r\nc\r\n");
    assert!(repo.status().unwrap().changes.is_empty());
}
