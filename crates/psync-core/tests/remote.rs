use psync_core::coord::TaskEdit;
use psync_core::remote::PullMain;
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

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
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

fn edit_commit_land(root: &Path, agent: &str, rel: &str, content: &str, m: &str) {
    write(root, rel, content);
    let r = open(root, agent);
    r.commit(&msg(m)).unwrap();
    land(&r);
}

/// Repository "a" with one landed change and a task; "b" cloned from it.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    write(&a, "x.txt", "1\n2\n3\n");
    let r = Repo::init(&a, id("a")).unwrap();
    r.commit(&msg("start")).unwrap();
    land(&r);
    r.task_add("shared task", None, "", None, vec![]).unwrap();
    drop(r);
    let b = tmp.path().join("b");
    Repo::clone_from(a.to_str().unwrap(), &b, id("b")).unwrap();
    (tmp, a.canonicalize().unwrap(), b.canonicalize().unwrap())
}

#[test]
fn clone_then_push_and_pull_fast_forward() {
    let (_tmp, a, b) = setup();
    assert_eq!(read(&b, "x.txt"), "1\n2\n3\n");
    let rb = open(&b, "b");
    assert_eq!(rb.main_tip().unwrap(), open(&a, "a").main_tip().unwrap());
    assert_eq!(rb.tasks().unwrap().len(), 1, "tasks come along");
    assert_eq!(rb.remotes().unwrap()[0].name, "origin");
    drop(rb);

    edit_commit_land(&b, "b", "y.txt", "from b\n", "b adds y");
    let rb = open(&b, "b");
    let p = rb.push("origin").unwrap();
    assert!(p.objects > 0);
    let b_main = rb.main_tip().unwrap();
    assert_eq!(open(&a, "a").main_tip().unwrap(), b_main);
    assert_eq!(rb.resolve("origin/main").unwrap(), b_main.unwrap());
    drop(rb);

    // a's workspace picks the change up with sync, like any landed change.
    let ra = open(&a, "a");
    ra.sync().unwrap();
    assert_eq!(read(&a, "y.txt"), "from b\n");
    drop(ra);

    edit_commit_land(&a, "a", "z.txt", "from a\n", "a adds z");
    let rb = open(&b, "b");
    assert!(matches!(rb.pull("origin").unwrap().main, PullMain::FastForward { .. }));
    rb.sync().unwrap();
    assert_eq!(read(&b, "z.txt"), "from a\n");
    assert!(matches!(rb.pull("origin").unwrap().main, PullMain::UpToDate));
}

#[test]
fn diverged_main_pull_moves_local_changes_back_then_push_works() {
    let (_tmp, a, b) = setup();
    edit_commit_land(&a, "a", "x.txt", "1 a\n2\n3\n", "a edits line 1");
    edit_commit_land(&b, "b", "x.txt", "1\n2\n3 b\n", "b edits line 3");

    let rb = open(&b, "b");
    assert!(rb.push("origin").is_err(), "remote is ahead");
    let out = rb.pull("origin").unwrap();
    let PullMain::Diverged { moved_back, sync } = out.main else { panic!("expected divergence") };
    assert_eq!(moved_back.len(), 1);
    assert!(sync.conflicts.is_empty());
    assert_eq!(read(&b, "x.txt"), "1 a\n2\n3 b\n");
    land(&rb);
    rb.push("origin").unwrap();
    drop(rb);

    let ra = open(&a, "a");
    ra.sync().unwrap();
    assert_eq!(read(&a, "x.txt"), "1 a\n2\n3 b\n");
    let intents: Vec<String> = ra.log(ra.main_tip().unwrap().unwrap(), 10).unwrap().into_iter().map(|(_, r)| r.meta.intent).collect();
    assert_eq!(intents.iter().filter(|i| *i == "b edits line 3").count(), 1, "{intents:?}");
}

#[test]
fn second_workspace_does_not_duplicate_changes_landed_again_after_pull() {
    let (_tmp, a, b) = setup();
    let rb = open(&b, "b");
    let w2 = rb.workspace_add("w2", None, None).unwrap().info.path.unwrap();
    drop(rb);
    edit_commit_land(&a, "a", "x.txt", "1 a\n2\n3\n", "a edits");
    edit_commit_land(&b, "b", "y.txt", "b\n", "b adds y");
    // w2 builds on b's local main (which includes "b adds y").
    let r2 = open(&w2, "w2");
    r2.sync().unwrap();
    drop(r2);
    edit_commit_land(&w2, "w2", "w2.txt", "w2\n", "w2 work");

    // b's main now has two local changes; pull diverges.
    let rb = open(&b, "b");
    rb.sync().unwrap();
    assert!(matches!(rb.pull("origin").unwrap().main, PullMain::Diverged { .. }));
    land(&rb);
    drop(rb);

    let r2 = open(&w2, "w2");
    r2.sync().unwrap();
    let main = r2.main_tip().unwrap().unwrap();
    let intents: Vec<String> = r2.log(main, 20).unwrap().into_iter().map(|(_, r)| r.meta.intent).collect();
    for i in ["b adds y", "w2 work", "a edits"] {
        assert_eq!(intents.iter().filter(|x| *x == i).count(), 1, "{i}: {intents:?}");
    }
    assert_eq!(r2.ahead_behind().unwrap().unwrap().1, 0, "w2 has nothing left to land");
}

#[test]
fn tasks_changed_on_both_sides_converge() {
    let (_tmp, a, b) = setup();
    let ra = open(&a, "a");
    ra.task_edit("T-1", TaskEdit { title: Some("renamed in a".into()), ..Default::default() }).unwrap();
    drop(ra);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let rb = open(&b, "b");
    rb.task_cancel("T-1").unwrap();
    rb.task_add("only in b", None, "", None, vec![]).unwrap();
    let f = rb.fetch("origin").unwrap();
    assert_eq!(f.notes.len(), 1, "{:?}", f.notes);
    let t = rb.task("T-1").unwrap().task;
    assert_eq!(t.status, TaskStatus::Cancelled, "b's later change wins");
    assert_eq!(t.merged_from.len(), 1);
    rb.push("origin").unwrap();
    let b_tasks = rb.view().unwrap().tasks;
    drop(rb);
    let ra = open(&a, "a");
    assert_eq!(ra.view().unwrap().tasks, b_tasks, "both repositories hold the same task objects");
    assert_eq!(ra.task("T-2").unwrap().task.title, "only in b");
}

#[test]
fn remote_add_validates() {
    let (tmp, a, b) = setup();
    let rb = open(&b, "b");
    assert!(rb.remote_add("origin", a.to_str().unwrap()).is_err(), "exists");
    assert!(rb.remote_add("self", b.to_str().unwrap()).is_err());
    assert!(rb.remote_add("none", tmp.path().to_str().unwrap()).is_err());
    assert!(rb.remote_add("bad/name", a.to_str().unwrap()).is_err());
    rb.remote_add("a2", &format!("{}/.psync", a.display())).unwrap();
    rb.fetch("a2").unwrap();
    assert!(rb.resolve("a2/main").is_ok());
    rb.remote_remove("a2").unwrap();
    assert!(rb.resolve("a2/main").is_err());
    assert!(rb.pull("nope").is_err());
}

/// Claims made in one clone reach another through the shared remote, with
/// pushes and pulls, and so do releases.
#[test]
fn claims_travel_through_a_shared_remote() {
    let (tmp, a, b) = setup();
    let c = tmp.path().join("c");
    Repo::clone_from(a.to_str().unwrap(), &c, id("c")).unwrap();
    let ttl = 3_600_000;

    let rb = open(&b, "b");
    rb.claim(&["x.txt".into()], ttl, false).unwrap();
    rb.push("origin").unwrap();
    let hub = open(&a, "a").active_claims().unwrap();
    assert_eq!(hub.len(), 1);
    let home = hub[0].home.clone().expect("marked as b's");
    assert_eq!(home.id, rb.repo_id().unwrap());
    assert!(home.place.ends_with(&format!(":{}", b.display())), "{}", home.place);

    // c sees it after a pull, as another workspace's claim.
    let rc = open(&c, "c");
    let f = rc.pull("origin").unwrap().fetch;
    assert_eq!(f.claims, Some(1));
    let seen = rc.active_claims().unwrap();
    assert_eq!((seen[0].pattern.as_str(), seen[0].by.short()), ("x.txt", "b".to_string()));
    let err = rc.claim(&["x.txt".into()], ttl, false).err().unwrap().to_string();
    assert!(err.contains(&format!("in workspace default at {}", home.place)), "{err}");
    write(&c, "x.txt", "c's edit\n");
    rc.snapshot().unwrap();
    let warned = rc.claimed_by_others(&rc.my_changed_paths().unwrap()).unwrap();
    assert_eq!(warned.len(), 1);
    // c's own work releases only c's claims.
    rc.claim(&["y.txt".into()], ttl, false).unwrap();
    rc.commit(&msg("c edits x")).unwrap();
    land(&rc);
    assert_eq!(rc.active_claims().unwrap().len(), 1, "b's claim stays");
    assert_eq!(rc.release(&[]).unwrap(), 0, "c cannot release b's claim");
    assert!(rc.pull("origin").unwrap().fetch.claims.is_none(), "nothing new");
    drop(rc);

    // b gets its own claim back from the hub, and ignores it.
    rb.pull("origin").unwrap();
    assert!(rb.active_claims().unwrap().iter().all(|c| c.home.is_none()));
    // A release travels the same way.
    assert_eq!(rb.release(&[]).unwrap(), 1);
    rb.push("origin").unwrap();
    assert!(open(&a, "a").active_claims().unwrap().is_empty());
    let rc = open(&c, "c");
    assert_eq!(rc.pull("origin").unwrap().fetch.claims, Some(0));
    assert!(rc.active_claims().unwrap().is_empty());
    rc.claim(&["x.txt".into()], ttl, false).unwrap();
}
