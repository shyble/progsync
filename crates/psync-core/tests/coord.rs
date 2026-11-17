use psync_core::coord::{parse_ttl, TaskDone, TaskEdit};
use psync_core::repo::LandResult;
use psync_core::{Identity, MetaUpdate, Repo, TaskStatus};
use std::fs;
use std::path::{Path, PathBuf};

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn open(path: &Path, agent: &str) -> Repo {
    let r = Repo::discover_as(path, id(agent)).unwrap();
    r.snapshot().unwrap();
    r
}

fn landed(r: LandResult) {
    match r {
        LandResult::Landed { .. } => {}
        LandResult::Conflicts(s) => panic!("conflicts: {:?}", s.conflicts),
        LandResult::CheckFailed { check, .. } => panic!("check failed: {}", check.output_tail),
    }
}

/// main with one landed change; workspaces "a" and "b".
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("app");
    write(&main, "src/auth/login.rs", "fn login() {}\n");
    write(&main, "src/api.rs", "fn route() {}\n");
    let repo = Repo::init(&main, id("lead")).unwrap();
    repo.commit(&msg("skeleton")).unwrap();
    landed(repo.land(None).unwrap());
    let a = repo.workspace_add("a", None, None).unwrap().info.path.unwrap();
    let b = repo.workspace_add("b", None, None).unwrap().info.path.unwrap();
    (tmp, main.canonicalize().unwrap(), a, b)
}

#[test]
fn task_ids_order_and_dependencies() {
    let (_tmp, main, _, _) = setup();
    let r = open(&main, "lead");
    for i in 1..=10 {
        assert_eq!(r.task_add(&format!("task {i}"), None, "", None, vec![]).unwrap().id, format!("T-{i}"));
    }
    let ids: Vec<String> = r.tasks().unwrap().into_iter().map(|t| t.task.id).collect();
    assert_eq!(ids[1], "T-2");
    assert_eq!(ids[9], "T-10", "natural order");
    assert!(r.task_add("bad dep", None, "", None, vec!["T-99".into()]).is_err());
    assert!(r.task_add("dup", Some("T-1"), "", None, vec![]).is_err());
    let t = r.task_add("needs T-1", Some("auth-2"), "", None, vec!["T-1".into()]).unwrap();
    assert_eq!(r.task(&t.id).unwrap().blocked_by, vec!["T-1"]);
    assert_eq!(r.task_counts().unwrap().blocked, 1);
}

#[test]
fn start_respects_blockers_and_ownership() {
    let (_tmp, main, a, b) = setup();
    let r = open(&main, "lead");
    r.task_add("base", None, "", None, vec![]).unwrap();
    r.task_add("after base", None, "", None, vec!["T-1".into()]).unwrap();
    drop(r);

    let ra = open(&a, "a");
    assert!(ra.task_start("T-2", false).is_err(), "blocked");
    let t = ra.task_start("T-1", false).unwrap();
    assert_eq!(t.status, TaskStatus::InProgress);
    assert_eq!(ra.working().unwrap().1.meta.task.as_deref(), Some("T-1"));
    drop(ra);

    let rb = open(&b, "b");
    assert!(rb.task_start("T-1", false).is_err(), "owned by a");
    assert!(rb.task_drop("T-1", false).is_err(), "not b's to drop");
    let t = rb.task_start("T-1", true).unwrap();
    assert_eq!(t.owner.unwrap().workspace, "b");
    rb.task_drop("T-1", false).unwrap();
    assert!(rb.task("T-1").unwrap().is_ready());
}

#[test]
fn land_records_progress_and_done_runs_acceptance_on_main() {
    let (_tmp, main, a, _) = setup();
    let r = open(&main, "lead");
    r.task_add("rate limit", None, "", Some("grep -q rate_limit src/auth/login.rs".into()), vec![]).unwrap();
    r.task_add("follow-up", None, "", None, vec!["T-1".into()]).unwrap();
    drop(r);

    let ra = open(&a, "a");
    ra.task_start("T-1", false).unwrap();
    // Acceptance cannot run while the workspace differs from main.
    write(&a, "src/auth/login.rs", "fn login() { rate_limit() }\n");
    ra.snapshot().unwrap();
    assert!(ra.task_done("T-1").is_err());
    ra.commit(&msg("add rate limit")).unwrap();
    landed(ra.land(None).unwrap());

    let t = ra.task("T-1").unwrap();
    assert_eq!(t.task.landed.len(), 1);
    assert_eq!(t.task.status, TaskStatus::InProgress);
    assert!(!ra.task("T-2").unwrap().is_ready());

    match ra.task_done("T-1").unwrap() {
        TaskDone::Done(t) => {
            let c = t.completion.unwrap();
            let main_tree = ra.store.get_revision(&ra.main_tip().unwrap().unwrap()).unwrap().tree;
            assert_eq!(c.tree, main_tree, "evidence is for main");
        }
        TaskDone::CheckFailed(c) => panic!("{}", c.output_tail),
    }
    assert!(ra.task("T-2").unwrap().is_ready(), "dependent unblocked");
    assert!(ra.task_done("T-1").is_err(), "already done");
}

#[test]
fn failing_acceptance_keeps_task_open() {
    let (_tmp, main, _, _) = setup();
    let r = open(&main, "lead");
    r.task_add("impossible", None, "", Some("false".into()), vec![]).unwrap();
    match r.task_done("T-1").unwrap() {
        TaskDone::CheckFailed(c) => assert!(!c.passed),
        TaskDone::Done(_) => panic!("must not complete"),
    }
    assert_ne!(r.task("T-1").unwrap().task.status, TaskStatus::Done);
}

#[test]
fn undo_skips_shared_task_and_claim_operations() {
    let (_tmp, _main, a, _) = setup();
    write(&a, "x.txt", "x\n");
    let ra = open(&a, "a");
    ra.task_add("t", None, "", None, vec![]).unwrap();
    ra.claim(&["src/".into()], parse_ttl("10m").unwrap(), false).unwrap();
    let u = ra.undo().unwrap();
    assert!(u.undone_description.starts_with("snapshot"), "{}", u.undone_description);
    assert!(!a.join("x.txt").exists());
    assert_eq!(ra.tasks().unwrap().len(), 1);
    assert_eq!(ra.active_claims().unwrap().len(), 1);
}

#[test]
fn claims_overlap_expire_warn_and_release_on_land() {
    let (_tmp, _main, a, b) = setup();
    let ra = open(&a, "a");
    ra.claim(&["src/auth/".into()], parse_ttl("30m").unwrap(), false).unwrap();
    drop(ra);

    let rb = open(&b, "b");
    assert!(rb.claim(&["src/auth/login.rs".into()], 60_000, false).is_err());
    assert!(rb.claim(&["src/auth/new_file.rs".into()], 60_000, false).is_err(), "future files too");
    rb.claim(&["src/api.rs".into()], 60_000, false).unwrap();
    // Editing under a's claim is reported, not blocked.
    write(&b, "src/auth/login.rs", "fn login() { b() }\n");
    rb.snapshot().unwrap();
    let w = rb.claimed_by_others(&rb.my_changed_paths().unwrap()).unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].1.workspace, "a");
    drop(rb);

    // a lands: its claims are released.
    write(&a, "src/auth/login.rs", "fn login() { a() }\n");
    let ra = open(&a, "a");
    ra.commit(&msg("a's login")).unwrap();
    landed(ra.land(None).unwrap());
    assert!(ra.active_claims().unwrap().iter().all(|c| c.workspace != "a"));
    drop(ra);

    let rb = open(&b, "b");
    rb.claim(&["src/auth/".into()], 60_000, false).unwrap();
    assert_eq!(rb.release(&[]).unwrap(), 2);
    assert!(rb.active_claims().unwrap().is_empty());
}

#[test]
fn expired_claims_do_not_block() {
    let (_tmp, _main, a, b) = setup();
    open(&a, "a").claim(&["src/".into()], 1, false).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    open(&b, "b").claim(&["src/".into()], 60_000, false).unwrap();
}

#[test]
fn workspace_remove_releases_claims() {
    let (_tmp, main, a, _) = setup();
    open(&a, "a").claim(&["src/".into()], 60_000, false).unwrap();
    let r = open(&main, "lead");
    r.workspace_remove("a").unwrap();
    assert!(r.active_claims().unwrap().is_empty());
}

#[test]
fn partial_commit_takes_only_selected_paths() {
    let (_tmp, _main, a, _) = setup();
    write(&a, "src/api.rs", "fn route() { v2() }\n");
    write(&a, "src/auth/login.rs", "fn login() { x() }\n");
    write(&a, "notes.md", "wip\n");
    let ra = open(&a, "a");
    assert!(ra.commit_paths(&msg("nothing"), &["missing.rs".into()]).is_err());
    let ((_, done), (_, next)) = ra.commit_paths(&msg("api v2"), &["src/api.rs".into(), "*.md".into()]).unwrap();
    let committed: Vec<String> = ra.changes_of(&done).unwrap().into_iter().map(|c| c.path).collect();
    assert_eq!(committed, vec!["notes.md", "src/api.rs"]);
    let rest: Vec<String> = ra.changes_of(&next).unwrap().into_iter().map(|c| c.path).collect();
    assert_eq!(rest, vec!["src/auth/login.rs"]);
    // Files on disk are untouched.
    assert_eq!(fs::read_to_string(a.join("src/auth/login.rs")).unwrap(), "fn login() { x() }\n");
}

#[test]
fn ttl_parsing() {
    assert_eq!(parse_ttl("90s").unwrap(), 90_000);
    assert_eq!(parse_ttl("30m").unwrap(), 1_800_000);
    assert_eq!(parse_ttl("2h").unwrap(), 7_200_000);
    assert_eq!(parse_ttl("1d").unwrap(), 86_400_000);
    assert!(parse_ttl("soon").is_err() && parse_ttl("5y").is_err());
}

#[test]
fn edit_and_cancel_tasks() {
    let (_tmp, main, a, _) = setup();
    let r = open(&main, "lead");
    r.task_add("one", None, "", None, vec![]).unwrap();
    r.task_add("two", None, "", None, vec!["T-1".into()]).unwrap();
    // Editing T-1 to wait for T-2 would be a cycle.
    assert!(r.task_edit("T-1", TaskEdit { after: Some(vec!["T-2".into()]), ..Default::default() }).is_err());
    let t = r
        .task_edit("T-1", TaskEdit { title: Some("one, renamed".into()), check: Some("true".into()), ..Default::default() })
        .unwrap();
    assert_eq!((t.title.as_str(), t.check.as_deref()), ("one, renamed", Some("true")));
    let t = r.task_edit("T-1", TaskEdit { check: Some("".into()), ..Default::default() }).unwrap();
    assert!(t.check.is_none());
    drop(r);

    let ra = open(&a, "a");
    ra.task_start("T-1", false).unwrap();
    ra.claim(&["src/".into()], 60_000, false).unwrap();
    ra.task_cancel("T-1").unwrap();
    assert_eq!(ra.task("T-1").unwrap().display_status(), "cancelled");
    assert!(ra.task("T-2").unwrap().is_ready(), "cancelled dependency no longer blocks");
    assert!(ra.active_claims().unwrap().is_empty(), "claims of the task released");
    assert!(ra.task_start("T-1", false).is_err());
    assert_eq!(ra.task_counts().unwrap().cancelled, 1);
}

#[test]
fn symbol_claims_narrow_overlaps_and_warnings() {
    let (_tmp, _main, a, b) = setup();
    let ra = open(&a, "a");
    ra.claim(&["src/auth/login.rs::login".into()], 60_000, false).unwrap();
    assert!(ra.claim(&["src/auth/login.rs::nope".into()], 60_000, false).is_err(), "unknown symbol");
    assert!(ra.claim(&["notes.txt::x".into()], 60_000, false).is_err(), "unsupported language");
    drop(ra);

    // b adds a function next to login and claims only that one.
    write(&b, "src/auth/login.rs", "fn login() {}\n\nfn logout() {}\n");
    let rb = open(&b, "b");
    assert!(rb.claim(&["src/auth/login.rs::login".into()], 60_000, false).is_err(), "same symbol");
    assert!(rb.claim(&["src/auth/".into()], 60_000, false).is_err(), "whole directory covers a's symbol");
    rb.claim(&["src/auth/login.rs::logout".into()], 60_000, false).unwrap();
    // Editing a different declaration in the same file is not a warning...
    assert!(rb.claimed_by_others(&rb.my_changed_paths().unwrap()).unwrap().is_empty());
    // ...editing the claimed one is.
    write(&b, "src/auth/login.rs", "fn login() { b() }\n\nfn logout() {}\n");
    rb.snapshot().unwrap();
    let w = rb.claimed_by_others(&rb.my_changed_paths().unwrap()).unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].1.pattern, "src/auth/login.rs::login");
    assert_eq!(rb.release(&["src/auth/login.rs::logout".into()]).unwrap(), 1);
}
