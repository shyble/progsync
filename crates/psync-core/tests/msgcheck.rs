//! The message check, `commit.check` (T-31).
#![cfg(unix)]

use psync_core::config;
use psync_core::repo::LandResult;
use psync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;
use std::process::Command;

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn open(root: &Path, agent: &str) -> Repo {
    let r = Repo::discover_as(root, id(agent)).unwrap();
    r.snapshot().unwrap();
    r
}

fn edit(r: &Repo, root: &Path, file: &str) {
    fs::write(root.join(file), file).unwrap();
    r.snapshot().unwrap();
}

/// A check that refuses any record mentioning "acme" (in the intent,
/// metadata or an identity), and keeps the last record it saw.
fn set_check(r: &Repo, root: &Path) {
    let seen = root.parent().unwrap().join("seen.json");
    let cmd = format!(
        "tee {} | grep -qi acme && {{ echo \"no brand names ($PSYNC_STAGE)\"; exit 1; }}; exit 0",
        seen.display()
    );
    config::set(&config::repo_path(r.dot()), "commit.check", Some(&cmd)).unwrap();
}

fn seen(root: &Path) -> psync_core::json::Value {
    psync_core::json::from_slice(&fs::read(root.parent().unwrap().join("seen.json")).unwrap()).unwrap()
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("p");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("a.txt"), "a\n").unwrap();
    Repo::init(&p, id("dev-1")).unwrap();
    (tmp, p)
}

#[test]
fn the_whole_record_is_checked_at_commit_and_describe() {
    let (_tmp, p) = setup();
    let r = open(&p, "dev-1");
    set_check(&r, &p);
    let err = r.commit(&msg("Add the Acme widget")).unwrap_err().to_string();
    assert!(err.contains("no brand names (commit)"), "{err}");
    let meta = MetaUpdate { intent: Some("Add a widget".into()), extra: vec![("model".into(), "acme-7".into())], ..Default::default() };
    assert!(r.commit(&meta).is_err(), "metadata is checked");
    assert!(r.describe(&meta).unwrap_err().to_string().contains("(describe)"));
    drop(r);
    let bad = open(&p, "acme-bot");
    assert!(bad.commit(&msg("Add a widget")).is_err(), "the identity is checked");
    drop(bad);

    let r = open(&p, "dev-1");
    let ((h, done), _) = r.commit(&msg("Add a widget")).unwrap();
    assert_eq!(done.meta.intent, "Add a widget");
    let s = seen(&p);
    assert_eq!(s["stage"].as_str(), Some("commit"));
    assert_eq!(s["intent"].as_str(), Some("Add a widget"));
    assert_eq!(s["author"]["agent"].as_str(), Some("dev-1"));
    assert_eq!(s["committer"]["email"].as_str(), Some("ada@example.com"));
    assert!(s["revision"].is_null());
    assert_eq!(r.store.get_revision(&h).unwrap().meta.intent, "Add a widget");

    // psync's own metadata updates (task start sets the task) are not checked.
    config::set(&config::repo_path(r.dot()), "commit.check", Some("exit 1")).unwrap();
    r.task_add("Something", None, "", None, vec![]).unwrap();
    r.task_start("T-1", false).unwrap();
}

#[test]
fn land_checks_every_change_and_describe_r_fixes_one() {
    let (_tmp, p) = setup();
    let r = open(&p, "dev-1");
    // Recorded before the check existed.
    edit(&r, &p, "b.txt");
    let ((bad, bad_rev), _) = r.commit(&msg("Acme support")).unwrap();
    edit(&r, &p, "c.txt");
    let ((_, good_rev), _) = r.commit(&msg("More work")).unwrap();
    set_check(&r, &p);
    let err = r.land(Some("true")).err().expect("refused").to_string();
    assert!(err.contains("no brand names (land)") && err.contains("psync describe -r"), "{err}");
    assert!(r.main_tip().unwrap().is_none(), "nothing landed");
    let s = seen(&p);
    assert_eq!(s["revision"].as_str(), Some(bad.to_hex().as_str()));
    assert_eq!(s["landed_by"]["agent"].as_str(), Some("dev-1"));

    assert!(r.describe_rev(&bad_rev.change_id.to_string(), &msg("Acme again")).is_err(), "describe -r is checked too");
    let (_, fixed) = r.describe_rev(&bad_rev.change_id.to_string(), &msg("Vendor support")).unwrap();
    assert_eq!(fixed.change_id, bad_rev.change_id);
    let landed = match r.land(Some("true")).unwrap() {
        LandResult::Landed { changes, .. } => changes,
        _ => panic!("not landed"),
    };
    let intents: Vec<&str> = landed.iter().map(|(_, r)| r.meta.intent.as_str()).collect();
    assert_eq!(intents, ["Vendor support", "More work"]);
    assert_eq!(landed[1].1.change_id, good_rev.change_id, "later changes keep their ids");
    assert_eq!(r.status().unwrap().changes.len(), 0, "files untouched");
    assert!(r.describe_rev("main", &msg("x")).is_err(), "landed changes cannot be described");
}

#[test]
fn git_export_checks_what_it_writes() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let (tmp, p) = setup();
    let r = open(&p, "dev-1");
    r.commit(&msg("Acme support")).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    set_check(&r, &p);
    let g = tmp.path().join("g");
    fs::create_dir(&g).unwrap();
    assert!(Command::new("git").arg("-C").arg(&g).args(["init", "-q", "-b", "main"]).status().unwrap().success());
    let err = r.git_export(&g, "main", true).err().expect("refused").to_string();
    assert!(err.contains("(export)") && err.contains("--no-commit-check"), "{err}");
    let head = Command::new("git").arg("-C").arg(&g).args(["rev-parse", "-q", "--verify", "main"]).output().unwrap();
    assert!(!head.status.success(), "nothing was written");
    assert_eq!(r.git_export(&g, "main", false).unwrap().commits, 1);
}
