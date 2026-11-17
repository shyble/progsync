//! Batch land: several workspaces' changes under one check (T-33).
#![cfg(unix)]

use progsync_core::repo::LandResult;
use progsync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::{Path, PathBuf};

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn open(root: &Path, agent: &str) -> Repo {
    let r = Repo::discover_as(root, id(agent)).unwrap();
    r.snapshot().unwrap();
    r
}

fn commit(root: &Path, agent: &str, file: &str, content: &str, intent: &str, task: Option<&str>) {
    let r = open(root, agent);
    fs::write(root.join(file), content).unwrap();
    r.snapshot().unwrap();
    r.commit(&MetaUpdate { intent: Some(intent.into()), task: task.map(String::from), ..Default::default() }).unwrap();
}

fn specs(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

/// A repository with main and workspaces w1, w2.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("p");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let r = Repo::init(&p, id("lead")).unwrap();
    r.commit(&MetaUpdate { intent: Some("start".into()), ..Default::default() }).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    let w1 = r.workspace_add("w1", None, None).unwrap().info.path.unwrap();
    let w2 = r.workspace_add("w2", None, None).unwrap().info.path.unwrap();
    (tmp, p, w1, w2)
}

#[test]
fn several_workspaces_land_under_one_check_and_keep_their_changes() {
    let (tmp, p, w1, w2) = setup();
    let lead = open(&p, "lead");
    lead.task_add("Batch", None, "", None, vec![]).unwrap();
    drop(lead);
    commit(&w1, "agent-1", "b.txt", "b\n", "add b", Some("T-1"));
    commit(&w1, "agent-1", "c.txt", "c\n", "add c", None);
    fs::write(w1.join("draft.txt"), "not committed\n").unwrap();
    commit(&w2, "agent-2", "a.txt", "one\ntwo\nthree\nfour\n", "extend a", None);
    commit(&p, "lead", "d.txt", "d\n", "add d", None);
    open(&w1, "agent-1").claim(&["b.txt".into()], 30 * 60_000, false).unwrap();

    let runs = tmp.path().join("runs");
    let check = format!("echo run >> {} && test -f b.txt -a -f d.txt && grep -q four a.txt", runs.display());
    let lead = open(&p, "lead");
    let b = lead.land_batch(&specs(&["w1@", "w2"]), Some(&check), &mut |_| {}).unwrap();
    let LandResult::Landed { main, changes, check: Some(run), .. } = b.result else { panic!("not landed") };
    assert!(run.passed);
    assert_eq!(fs::read_to_string(&runs).unwrap().lines().count(), 1, "one check");
    let intents: Vec<&str> = changes.iter().map(|(_, r)| r.meta.intent.as_str()).collect();
    assert_eq!(intents, ["add d", "add b", "add c", "extend a"]);
    assert_eq!(b.sources, ["you", "w1@", "w1@", "w2@"]);
    assert_eq!(changes[1].1.author.agent.as_deref(), Some("agent-1"), "authors are kept");
    let top = lead.store.get_revision(&main).unwrap();
    assert_eq!(top.meta.checks.len(), 1, "the check is recorded on the top change");
    assert!(lead.active_claims().unwrap().is_empty(), "w1's claims are released");
    assert_eq!(lead.task("T-1").unwrap().task.landed, vec![changes[1].1.change_id, changes[2].1.change_id], "add c inherited the task");
    assert!(lead.status().unwrap().changes.is_empty(), "the lead is on the new main");

    // Each owner's next sync drops what landed; uncommitted work stays.
    let r1 = open(&w1, "agent-1");
    r1.sync().unwrap();
    let (_, w) = r1.working().unwrap();
    assert_eq!(w.parents[0], main);
    assert_eq!(fs::read_to_string(w1.join("draft.txt")).unwrap(), "not committed\n");
    assert!(r1.land(None).is_err(), "nothing left to land");
}

#[test]
fn a_conflict_or_a_failed_check_lands_nothing_and_restores_the_files() {
    let (_tmp, p, w1, w2) = setup();
    commit(&w1, "agent-1", "a.txt", "one\nTWO\nthree\n", "shout two", None);
    commit(&w2, "agent-2", "a.txt", "one\n2\nthree\n", "number two", None);
    let lead = open(&p, "lead");
    let main = lead.main_tip().unwrap();
    let err = lead.land_batch(&specs(&["w1@", "w2@"]), None, &mut |_| {}).err().expect("refused").to_string();
    assert!(err.contains("w2@'s change") && err.contains("a.txt"), "{err}");
    assert_eq!(lead.main_tip().unwrap(), main);
    assert_eq!(fs::read_to_string(p.join("a.txt")).unwrap(), "one\ntwo\nthree\n");

    let failed = lead.land_batch(&specs(&["w1@"]), Some("false"), &mut |_| {}).unwrap();
    assert!(matches!(failed.result, LandResult::CheckFailed { .. }));
    assert_eq!(lead.main_tip().unwrap(), main);
    assert_eq!(fs::read_to_string(p.join("a.txt")).unwrap(), "one\ntwo\nthree\n", "files put back");
    assert!(lead.status().unwrap().changes.is_empty());

    let ok = lead.land_batch(&specs(&["w1@"]), Some("grep -q TWO a.txt"), &mut |_| {}).unwrap();
    assert!(matches!(ok.result, LandResult::Landed { .. }), "landing someone else's work alone");
    assert!(lead.land_batch(&specs(&["nobody@"]), None, &mut |_| {}).is_err());
}

#[test]
fn another_clones_workspace_lands_through_the_hub() {
    let (tmp, p, _w1, _w2) = setup();
    let b = tmp.path().join("b");
    Repo::clone_from(p.to_str().unwrap(), &b, id("b")).unwrap();
    let rb = open(&b, "b");
    let w9 = rb.workspace_add("w9", None, None).unwrap().info.path.unwrap();
    drop(rb);
    commit(&w9, "agent-9", "e.txt", "e\n", "add e", None);
    open(&w9, "agent-9").push("origin").unwrap();

    let lead = open(&p, "lead");
    let landed = lead.land_batch(&specs(&["w9@"]), None, &mut |_| {}).unwrap();
    let LandResult::Landed { main, changes, .. } = landed.result else { panic!("not landed") };
    assert_eq!(changes.len(), 1);
    assert_eq!(fs::read_to_string(p.join("e.txt")).unwrap(), "e\n");

    // Back in the clone, pull and sync: the change is on main, nothing is left to land.
    let r9 = open(&w9, "agent-9");
    r9.pull("origin").unwrap();
    r9.sync().unwrap();
    assert_eq!(r9.working().unwrap().1.parents[0], main);
    assert!(r9.land(None).is_err());
}
