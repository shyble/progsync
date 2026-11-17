use psync_core::repo::LandResult;
use psync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;

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

fn land(r: &Repo) {
    match r.land(None).unwrap() {
        LandResult::Landed { .. } => {}
        LandResult::Conflicts(s) => panic!("conflicts: {:?}", s.conflicts),
        LandResult::CheckFailed { check, .. } => panic!("check failed: {}", check.output_tail),
    }
}

/// main: "base" then "feature" (edits a.txt line 2, adds new.txt) then "later" (edits a.txt line 4).
fn setup() -> (tempfile::TempDir, Repo, String) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "a.txt", "1\n2\n3\n4\n");
    let r = Repo::init(root, id("lead")).unwrap();
    r.commit(&msg("base")).unwrap();
    write(root, "a.txt", "1\n2 feature\n3\n4\n");
    write(root, "new.txt", "new\n");
    r.snapshot().unwrap();
    let ((_, feature), _) = r.commit(&msg("feature\n\nlong description")).unwrap();
    write(root, "a.txt", "1\n2 feature\n3\n4 later\n");
    r.snapshot().unwrap();
    r.commit(&msg("later")).unwrap();
    land(&r);
    (tmp, r, feature.change_id.short())
}

#[test]
fn revert_a_landed_change_keeps_later_work() {
    let (tmp, r, feature) = setup();
    let root = tmp.path();
    let out = r.revert(&feature).unwrap();
    assert!(out.conflicts.is_empty());
    assert_eq!(out.files, 2);
    assert_eq!(read(root, "a.txt"), "1\n2\n3\n4 later\n");
    assert!(!root.join("new.txt").exists());
    let (_, w) = r.working().unwrap();
    assert_eq!(w.meta.intent, "Revert \"feature\"");
    assert_eq!(w.meta.extra["revert_of"], feature);

    r.commit(&MetaUpdate::default()).unwrap();
    land(&r);
    let main = r.store.get_revision(&r.main_tip().unwrap().unwrap()).unwrap();
    assert_eq!(main.meta.intent, "Revert \"feature\"");

    // Reverting it again finds nothing left to undo.
    assert!(r.revert(&feature).is_err());
}

#[test]
fn conflicting_later_edits_become_conflicts() {
    let (tmp, r, feature) = setup();
    let root = tmp.path();
    write(root, "a.txt", "1\n2 feature, then more\n3\n4 later\n");
    r.snapshot().unwrap();
    r.commit(&msg("more")).unwrap();
    let out = r.revert(&feature).unwrap();
    assert_eq!(out.conflicts.len(), 1);
    assert_eq!(out.conflicts[0].path, "a.txt");
    assert!(read(root, "a.txt").contains("<<<<<<< current"));
    assert!(!root.join("new.txt").exists(), "the clean part is still reverted");
    assert!(r.commit(&MetaUpdate::default()).is_err(), "commit refuses while conflicted");
}

#[test]
fn revert_refuses_dirty_working_change_and_foreign_or_self_changes() {
    let (tmp, r, feature) = setup();
    let root = tmp.path();
    assert!(r.revert("@").is_err(), "the working change itself");
    write(root, "wip.txt", "wip\n");
    r.snapshot().unwrap();
    assert!(r.revert(&feature).is_err(), "working change has its own edits");
    fs::remove_file(root.join("wip.txt")).unwrap();
    r.snapshot().unwrap();

    // A change in another workspace's history only is not ours to revert.
    let ws = r.workspace_add("b", None, None).unwrap().info.path.unwrap();
    let rb = Repo::discover_as(&ws, id("b")).unwrap();
    write(&ws, "b.txt", "b\n");
    rb.snapshot().unwrap();
    let ((_, b_change), _) = rb.commit(&msg("b's work")).unwrap();
    drop(rb);
    assert!(r.revert(&b_change.change_id.short()).is_err());
    // Undo of the revert operation works like for any other.
    r.revert(&feature).unwrap();
    r.undo().unwrap();
    assert!(root.join("new.txt").exists());
}
