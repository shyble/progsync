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
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

/// A repo with one committed change, in `<tmp>/main`.
fn setup() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    write(&main, "src/lib.rs", "pub fn a() {}\n");
    write(&main, "README.md", "hello\n");
    let repo = Repo::init(&main, id("lead")).unwrap();
    repo.commit(&msg("baseline")).unwrap();
    (tmp, main.canonicalize().unwrap())
}

fn add_ws(main: &Path, name: &str) -> PathBuf {
    let repo = Repo::discover_as(main, id("lead")).unwrap();
    repo.workspace_add(name, None, None).unwrap().info.path.unwrap()
}

#[test]
fn add_creates_populated_isolated_workspace() {
    let (_tmp, main) = setup();
    write(&main, "uncommitted.txt", "only in main\n");
    let path = add_ws(&main, "agent-1");

    assert_eq!(path.file_name().unwrap(), "main-agent-1");
    assert_eq!(read(&path, "src/lib.rs"), "pub fn a() {}\n");
    // Built on @- (the baseline), so main's uncommitted work is not there.
    assert!(!path.join("uncommitted.txt").exists());

    // Edits in the workspace do not leak into main, and vice versa.
    write(&path, "src/lib.rs", "pub fn a() { /* agent-1 */ }\n");
    let ws = Repo::discover_as(&path, id("agent-1")).unwrap();
    assert_eq!(ws.workspace(), "agent-1");
    assert_eq!(ws.working().unwrap().1.author.agent.as_deref(), Some("lead"));
    assert!(ws.snapshot().unwrap().changed);
    // The empty change created by `lead` is now credited to agent-1.
    assert_eq!(ws.working().unwrap().1.author.agent.as_deref(), Some("agent-1"));
    drop(ws);

    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    repo.snapshot().unwrap();
    let paths: Vec<String> = repo.status().unwrap().changes.into_iter().map(|c| c.path).collect();
    assert_eq!(paths, vec!["uncommitted.txt"]);
    assert_eq!(read(&main, "src/lib.rs"), "pub fn a() {}\n");
}

#[test]
fn undo_is_scoped_to_the_workspace() {
    let (_tmp, main) = setup();
    let path = add_ws(&main, "agent-1");

    // agent-1 makes a change...
    write(&path, "a1.txt", "agent-1 work\n");
    let ws = Repo::discover_as(&path, id("agent-1")).unwrap();
    ws.snapshot().unwrap();
    drop(ws);

    // ...then main makes a later change.
    write(&main, "lead.txt", "lead work\n");
    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    repo.snapshot().unwrap();
    drop(repo);

    // agent-1 undoes: its own snapshot is undone, main's later work stays.
    let ws = Repo::discover_as(&path, id("agent-1")).unwrap();
    let u = ws.undo().unwrap();
    assert!(u.undone_description.starts_with("snapshot"));
    assert!(!path.join("a1.txt").exists());
    assert_eq!(read(&main, "lead.txt"), "lead work\n");
    let repo_view = ws.view().unwrap();
    let (main_rev, _) = ws.working_of("default").unwrap();
    assert_eq!(repo_view.workspaces["default"], main_rev);

    // Undoing again reaches the workspace's creation: nothing more to undo.
    assert!(ws.undo().is_err());
}

#[test]
fn context_reports_peers_and_overlaps() {
    let (_tmp, main) = setup();
    let p1 = add_ws(&main, "agent-1");
    let p2 = add_ws(&main, "agent-2");

    write(&p1, "src/lib.rs", "pub fn a() { 1 }\n");
    write(&p1, "src/one.rs", "// one\n");
    let ws1 = Repo::discover_as(&p1, id("agent-1")).unwrap();
    ws1.snapshot().unwrap();
    ws1.describe(&msg("agent-1 refactor")).unwrap();
    drop(ws1);

    write(&p2, "src/lib.rs", "pub fn a() { 2 }\n");
    let ws2 = Repo::discover_as(&p2, id("agent-2")).unwrap();
    ws2.snapshot().unwrap();
    let b = ws2.context(5).unwrap();

    assert_eq!(b.workspace, "agent-2");
    assert_eq!(b.files, vec!["src/lib.rs"]);
    let peer = b.peers.iter().find(|p| p.info.name == "agent-1").unwrap();
    assert_eq!(peer.files, vec!["src/lib.rs", "src/one.rs"]);
    assert_eq!(peer.overlap, vec!["src/lib.rs"]);
    assert_eq!(peer.info.revision.meta.intent, "agent-1 refactor");
    let main_peer = b.peers.iter().find(|p| p.info.name == "default").unwrap();
    assert!(main_peer.overlap.is_empty());
}

#[test]
fn committed_work_counts_toward_overlap() {
    let (_tmp, main) = setup();
    let p1 = add_ws(&main, "agent-1");
    write(&p1, "README.md", "agent-1 docs\n");
    let ws1 = Repo::discover_as(&p1, id("agent-1")).unwrap();
    ws1.snapshot().unwrap();
    ws1.commit(&msg("docs")).unwrap();
    drop(ws1);

    write(&main, "README.md", "lead docs\n");
    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    repo.snapshot().unwrap();
    let b = repo.context(5).unwrap();
    assert_eq!(b.peers[0].overlap, vec!["README.md"]);
}

#[test]
fn resolve_workspace_at_syntax() {
    let (_tmp, main) = setup();
    let p1 = add_ws(&main, "agent-1");
    let ws1 = Repo::discover_as(&p1, id("agent-1")).unwrap();
    let (h, _) = ws1.working().unwrap();
    drop(ws1);
    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    assert_eq!(repo.resolve("agent-1@").unwrap(), h);
    assert_eq!(repo.resolve("agent-1@-").unwrap(), repo.resolve("@-").unwrap());
}

#[test]
fn remove_deletes_directory_but_keeps_history() {
    let (_tmp, main) = setup();
    let p1 = add_ws(&main, "agent-1");
    write(&p1, "late.txt", "written just before removal\n");

    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    let last = repo.workspace_remove("agent-1").unwrap();
    assert!(!p1.exists());
    assert!(repo.working_of("agent-1").is_err());
    // The final edit was snapshotted before removal.
    let rev = repo.store.get_revision(&last).unwrap();
    let changes = repo.changes_of(&rev).unwrap();
    assert_eq!(changes[0].path, "late.txt");

    assert!(repo.workspace_remove("default").is_err());
}

#[test]
fn rejects_bad_names_and_duplicates() {
    let (_tmp, main) = setup();
    add_ws(&main, "agent-1");
    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    assert!(repo.workspace_add("agent-1", None, None).is_err());
    assert!(repo.workspace_add("../evil", None, None).is_err());
    assert!(repo.workspace_add("trailing-", None, None).is_err());
}

#[test]
fn nested_workspace_is_not_tracked_by_its_parent() {
    let (_tmp, main) = setup();
    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    let inner = main.join("agents/a1");
    repo.workspace_add("a1", Some(&inner), None).unwrap();
    repo.snapshot().unwrap();
    let paths: Vec<String> = repo.status().unwrap().changes.into_iter().map(|c| c.path).collect();
    assert!(paths.is_empty(), "nested workspace leaked into parent: {paths:?}");
}

#[test]
fn parallel_agents_do_not_lose_operations() {
    let (_tmp, main) = setup();
    let paths: Vec<PathBuf> = (0..4).map(|i| add_ws(&main, &format!("agent-{i}"))).collect();
    std::thread::scope(|s| {
        for (i, p) in paths.iter().enumerate() {
            s.spawn(move || {
                for j in 0..5 {
                    write(p, &format!("f{i}-{j}.txt"), "x\n");
                    let ws = Repo::discover_as(p, id(&format!("agent-{i}"))).unwrap();
                    ws.snapshot().unwrap();
                    ws.commit(&msg(&format!("agent {i} step {j}"))).unwrap();
                }
            });
        }
    });
    let repo = Repo::discover_as(&main, id("lead")).unwrap();
    for i in 0..4 {
        let (h, _) = repo.working_of(&format!("agent-{i}")).unwrap();
        // 5 commits + the workspace's base chain (baseline, init).
        let history = repo.log(h, 100).unwrap();
        let committed: Vec<_> = history.iter().filter(|(_, r)| r.meta.intent.starts_with(&format!("agent {i}"))).collect();
        assert_eq!(committed.len(), 5, "agent-{i} lost commits");
        for (_, r) in committed {
            assert_eq!(repo.changes_of(r).unwrap().len(), 1, "each commit carries its file");
        }
    }
}
