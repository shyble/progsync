//! Revs, other repositories' workspaces, and switch (T-29).

use psync_core::diff;
use psync_core::gc::check;
use psync_core::repo::LandResult;
use psync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::{Path, PathBuf};

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn write(root: &Path, rel: &str, content: &str) {
    fs::write(root.join(rel), content).unwrap();
}

fn open(root: &Path, agent: &str) -> Repo {
    let r = Repo::discover_as(root, id(agent)).unwrap();
    r.snapshot().unwrap();
    r
}

fn land(r: &Repo) {
    match r.land(None).unwrap() {
        LandResult::Landed { .. } => {}
        LandResult::Conflicts(s) => panic!("conflicts: {:?}", s.conflicts),
        LandResult::CheckFailed { check, .. } => panic!("check failed: {}", check.output_tail),
    }
}

fn on_main(r: &Repo, path: &str) -> Option<String> {
    let tree = r.store.get_revision(&r.main_tip().unwrap().unwrap()).unwrap().tree;
    let (_, h) = diff::lookup(&r.store, tree, path).unwrap()?;
    Some(String::from_utf8(r.store.get_blob(&h).unwrap()).unwrap())
}

/// A hub with one landed change, and clones "b" and "c" of it.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let hub = tmp.path().join("hub");
    fs::create_dir(&hub).unwrap();
    write(&hub, "x.txt", "x\n");
    let r = Repo::init(&hub, id("hub")).unwrap();
    r.commit(&msg("start")).unwrap();
    land(&r);
    drop(r);
    let (b, c) = (tmp.path().join("b"), tmp.path().join("c"));
    Repo::clone_from(hub.to_str().unwrap(), &b, id("b")).unwrap();
    Repo::clone_from(hub.to_str().unwrap(), &c, id("c")).unwrap();
    (tmp, hub, b, c)
}

#[test]
fn switching_onto_another_clones_unfinished_work_loses_nothing() {
    let (_tmp, hub, b, c) = setup();
    // In b, claude-1 commits one change and keeps working on another.
    let ws = open(&b, "b").workspace_add("claude-1", None, None).unwrap().info.path.unwrap();
    write(&ws, "y.txt", "y1\n");
    let rb = open(&ws, "claude-1");
    rb.commit(&msg("add y")).unwrap();
    write(&ws, "z.txt", "z draft\n");
    rb.snapshot().unwrap();
    rb.push("origin").unwrap();

    // c sees it after a pull, in context, and can name it.
    let rc = open(&c, "c");
    let f = rc.pull("origin").unwrap().fetch;
    assert!(f.workspaces.unwrap() >= 2, "b's default and claude-1, and the hub's");
    let brief = rc.context(3).unwrap();
    let peer = brief.peers.iter().find(|p| p.info.name.starts_with("claude-1@")).expect("listed");
    assert_eq!(peer.files, ["y.txt", "z.txt"]);
    assert!(peer.place.as_ref().unwrap().ends_with(&format!(":{}", b.canonicalize().unwrap().display())));
    let theirs = rb.working().unwrap();
    assert_eq!(rc.resolve("claude-1@").unwrap(), theirs.0);
    assert_eq!(rc.resolve(&peer.info.name).unwrap(), theirs.0);
    assert!(rc.resolve("nobody@").is_err());

    // c switches onto it: the files, a change of its own, the old one parked.
    write(&c, "mine.txt", "c's unfinished\n");
    rc.snapshot().unwrap();
    let before = rc.working().unwrap().0;
    let s = rc.switch("claude-1@").unwrap();
    assert_eq!(fs::read_to_string(c.join("z.txt")).unwrap(), "z draft\n");
    assert!(!c.join("mine.txt").exists());
    assert_ne!(s.revision.change_id, theirs.1.change_id, "a copy, not the same change");
    assert_eq!(s.revision.parents, theirs.1.parents, "on top of their commit");
    assert_eq!(rc.parked().unwrap().unwrap().0, before);

    // c finishes the draft and lands it (with b's commit below it).
    write(&c, "z.txt", "z by c\n");
    rc.snapshot().unwrap();
    rc.commit(&msg("finish z")).unwrap();
    land(&rc);
    rc.push("origin").unwrap();

    // b goes on with its own version and lands after pulling: both survive.
    write(&ws, "w.txt", "w\n");
    rb.snapshot().unwrap();
    rb.pull("origin").unwrap();
    rb.sync().unwrap();
    // z.txt: both sides added it, differently: a conflict for b to settle.
    let w = rb.working().unwrap().1;
    assert!(w.conflicts.iter().any(|c| c.path == "z.txt"), "{:?}", w.conflicts);
    write(&ws, "z.txt", "z final\n");
    rb.snapshot().unwrap();
    rb.commit(&msg("w, and z settled")).unwrap();
    land(&rb);
    assert_eq!(on_main(&rb, "y.txt").as_deref(), Some("y1\n"));
    assert_eq!(on_main(&rb, "w.txt").as_deref(), Some("w\n"), "b's later work was not dropped");
    assert_eq!(on_main(&rb, "z.txt").as_deref(), Some("z final\n"));

    // switch - goes back to c's own unfinished work.
    rc.switch("-").unwrap();
    assert_eq!(rc.working().unwrap().0, before);
    assert_eq!(fs::read_to_string(c.join("mine.txt")).unwrap(), "c's unfinished\n");
    assert!(check(&rc).unwrap().problems.is_empty());
    drop(rc);
    assert!(check(&open(&hub, "hub")).unwrap().problems.is_empty());
}

#[test]
fn revs_are_created_directly_on_a_remote_and_never_move() {
    let (_tmp, hub, b, c) = setup();
    let rb = open(&b, "b");
    write(&b, "t.txt", "to try\n");
    rb.snapshot().unwrap();
    let hub_main = open(&hub, "hub").main_tip().unwrap();
    let (h, notes) = rb.rev_add_remote("try-t", "@", "origin").unwrap();
    assert!(notes.is_empty());
    let rh = open(&hub, "hub");
    assert_eq!(rh.resolve("try-t").unwrap(), h);
    assert_eq!(rh.main_tip().unwrap(), hub_main, "main is not touched");
    drop(rh);

    // c fetches it and can test it; the name cannot be reused here.
    let rc = open(&c, "c");
    assert_eq!(rc.fetch("origin").unwrap().revs, ["try-t"]);
    rc.switch("try-t").unwrap();
    assert_eq!(fs::read_to_string(c.join("t.txt")).unwrap(), "to try\n");
    let err = rc.rev_add("try-t", "main").err().unwrap().to_string();
    assert!(err.contains("never moves"), "{err}");
    assert!(rc.rev_add("kkk", "main").is_err(), "reads as a change id");

    // Two repositories that name different revisions keep their own.
    rc.rev_add("v1", "main").unwrap();
    rb.rev_add("v1", "@").unwrap();
    rb.rev_add_remote("v1", "@", "origin").unwrap();
    rc.switch("-").unwrap();
    let notes = rc.pull("origin").unwrap().fetch.notes;
    assert!(notes.iter().any(|n| n.starts_with("rev v1 is")), "{notes:?}");
    assert_eq!(rc.resolve("v1").unwrap(), rc.main_tip().unwrap().unwrap());
    assert_eq!(rc.rev_remove("v1").unwrap(), rc.main_tip().unwrap().unwrap());
    rc.fetch("origin").unwrap();
    assert_eq!(rc.resolve("v1").unwrap(), rb.resolve("v1").unwrap(), "fetched again: the hub's, which came from b");
}
