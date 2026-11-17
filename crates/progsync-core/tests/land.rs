use progsync_core::repo::LandResult;
use progsync_core::{ConflictKind, Identity, MetaUpdate, Repo};
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

/// Open a workspace as `agent`, snapshot it (like every CLI command does).
fn open(path: &Path, agent: &str) -> Repo {
    let r = Repo::discover_as(path, id(agent)).unwrap();
    r.snapshot().unwrap();
    r
}

fn commit(path: &Path, agent: &str, m: &str) {
    open(path, agent).commit(&msg(m)).unwrap();
}

fn landed(r: LandResult) -> usize {
    match r {
        LandResult::Landed { changes, .. } => changes.len(),
        LandResult::Conflicts(s) => panic!("unexpected conflicts: {:?}", s.conflicts),
        LandResult::CheckFailed { check, .. } => panic!("unexpected check failure: {}", check.output_tail),
    }
}

/// main exists with one landed change; two agent workspaces on top of it.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("app");
    write(&main, "src/auth.rs", "fn login() {\n    check();\n}\n");
    write(&main, "src/api.rs", "fn route() {}\n");
    write(&main, "README.md", "# app\n");
    let repo = Repo::init(&main, id("lead")).unwrap();
    repo.commit(&msg("skeleton")).unwrap();
    assert_eq!(landed(repo.land(None).unwrap()), 1);
    let a = repo.workspace_add("a", None, None).unwrap().info.path.unwrap();
    let b = repo.workspace_add("b", None, None).unwrap().info.path.unwrap();
    (tmp, main.canonicalize().unwrap(), a, b)
}

#[test]
fn first_land_creates_main_and_nothing_left_errors() {
    let (_tmp, main, _, _) = setup();
    let repo = open(&main, "lead");
    assert!(repo.main_tip().unwrap().is_some());
    assert!(repo.land(None).is_err(), "nothing to land");
    // New workspaces start from main.
    let (_, w) = repo.working_of("a").unwrap();
    assert_eq!(w.parents[0], repo.main_tip().unwrap().unwrap());
}

#[test]
fn agents_land_and_sync_disjoint_work() {
    let (_tmp, _main, a, b) = setup();
    write(&a, "src/auth.rs", "fn login() {\n    check();\n    rate_limit();\n}\n");
    commit(&a, "a", "rate limit");
    assert_eq!(landed(open(&a, "a").land(None).unwrap()), 1);

    write(&b, "src/api.rs", "fn route() { v2() }\n");
    commit(&b, "b", "api v2");
    let rb = open(&b, "b");
    let (_, ahead, behind) = rb.ahead_behind().unwrap().unwrap();
    assert_eq!((ahead, behind), (1, 1));
    // land syncs first: b gets a's change, main gets b's.
    assert_eq!(landed(rb.land(None).unwrap()), 1);
    assert!(read(&b, "src/auth.rs").contains("rate_limit"));
    let main_tip = rb.main_tip().unwrap().unwrap();
    let history: Vec<String> = rb.log(main_tip, 10).unwrap().into_iter().map(|(_, r)| r.meta.intent).collect();
    assert_eq!(history, vec!["api v2", "rate limit", "skeleton"], "main is linear");
    drop(rb);

    // a syncs and receives b's work.
    let ra = open(&a, "a");
    let s = ra.sync().unwrap();
    assert!(s.conflicts.is_empty());
    assert_eq!(read(&a, "src/api.rs"), "fn route() { v2() }\n");
    let (_, ahead, behind) = ra.ahead_behind().unwrap().unwrap();
    assert_eq!((ahead, behind), (0, 0));
}

#[test]
fn conflict_blocks_commit_until_resolved_then_lands() {
    let (_tmp, _main, a, b) = setup();
    write(&a, "src/auth.rs", "fn login() {\n    check_a();\n}\n");
    commit(&a, "a", "a's login");
    landed(open(&a, "a").land(None).unwrap());

    // b edits the same line, uncommitted, and syncs.
    write(&b, "src/auth.rs", "fn login() {\n    check_b();\n}\n");
    let rb = open(&b, "b");
    let s = rb.sync().unwrap();
    assert_eq!(s.conflicts.len(), 1);
    assert_eq!(s.conflicts[0].kind, ConflictKind::Content);
    let text = read(&b, "src/auth.rs");
    assert!(text.contains("<<<<<<< main") && text.contains("check_a") && text.contains("check_b"), "{text}");
    assert!(rb.commit(&msg("b's login")).is_err(), "commit refuses unresolved conflicts");
    drop(rb);

    // Resolve by editing the file: the next snapshot clears the conflict.
    write(&b, "src/auth.rs", "fn login() {\n    check_a();\n    check_b();\n}\n");
    let rb = open(&b, "b");
    assert!(rb.working().unwrap().1.conflicts.is_empty());
    rb.commit(&msg("b's login")).unwrap();
    assert_eq!(landed(rb.land(None).unwrap()), 1);
}

#[test]
fn land_reports_conflicts_without_landing() {
    let (_tmp, main, a, b) = setup();
    write(&a, "README.md", "# A\n");
    commit(&a, "a", "readme a");
    landed(open(&a, "a").land(None).unwrap());
    write(&b, "README.md", "# B\n");
    commit(&b, "b", "readme b");
    let committed_id = {
        let rb = open(&b, "b");
        rb.store.get_revision(&rb.resolve("@-").unwrap()).unwrap().change_id
    };
    let before = open(&main, "lead").main_tip().unwrap();
    match open(&b, "b").land(None).unwrap() {
        LandResult::Conflicts(s) => {
            assert_eq!(s.folded.len(), 1, "conflicting committed change folded into @");
            assert_eq!(s.conflicts[0].path, "README.md");
        }
        _ => panic!("expected conflicts"),
    }
    assert_eq!(open(&main, "lead").main_tip().unwrap(), before, "main unchanged");
    // The folded change keeps its intent and identity in the working change.
    let rb = open(&b, "b");
    let w = rb.working().unwrap().1;
    assert_eq!(w.meta.intent, "readme b");
    assert_eq!(w.change_id, committed_id);
}

#[test]
fn checks_gate_landing_and_are_recorded() {
    let (_tmp, main, a, _) = setup();
    write(&a, "ok.txt", "fine\n");
    commit(&a, "a", "add ok");
    let ra = open(&a, "a");
    let before = ra.main_tip().unwrap();
    match ra.land(Some("test -f ok.txt && grep -q nope ok.txt")).unwrap() {
        LandResult::CheckFailed { check, .. } => assert!(!check.passed),
        _ => panic!("expected check failure"),
    }
    assert_eq!(ra.main_tip().unwrap(), before);

    match ra.land(Some("test -f ok.txt")).unwrap() {
        LandResult::Landed { changes, check, .. } => {
            assert!(check.unwrap().passed);
            let tip = &changes.last().unwrap().1;
            assert_eq!(tip.meta.checks.len(), 1);
            assert_eq!(tip.meta.checks[0].tree, tip.tree, "evidence names the exact tree tested");
        }
        _ => panic!("expected landed"),
    }
    drop(ra);
    let repo = open(&main, "lead");
    let tip = repo.store.get_revision(&repo.main_tip().unwrap().unwrap()).unwrap();
    assert_eq!(tip.meta.checks[0].command, "test -f ok.txt");
}

#[test]
fn check_that_edits_tracked_files_aborts() {
    let (_tmp, _main, a, _) = setup();
    write(&a, "x.txt", "1\n");
    commit(&a, "a", "x");
    let err = open(&a, "a").land(Some("echo 2 > x.txt")).err().expect("must abort");
    assert!(err.to_string().contains("modified tracked files"));
}

#[test]
fn landing_on_top_of_main_rewrites_nothing() {
    let (_tmp, _main, a, _) = setup();
    write(&a, "x.txt", "1\n");
    commit(&a, "a", "x");
    let ra = open(&a, "a");
    let tip_before = ra.resolve("@-").unwrap();
    match ra.land(None).unwrap() {
        LandResult::Landed { synced, changes, .. } => {
            assert!(!synced);
            assert_eq!(changes[0].0, tip_before, "the change itself lands, unrewritten");
        }
        _ => panic!("expected landed"),
    }
}

#[test]
fn land_cannot_be_undone() {
    let (_tmp, _main, a, _) = setup();
    write(&a, "x.txt", "1\n");
    commit(&a, "a", "x");
    let ra = open(&a, "a");
    landed(ra.land(None).unwrap());
    assert!(ra.undo().is_err());
}

#[test]
fn merge_brings_in_another_workspace() {
    let (_tmp, _main, a, b) = setup();
    write(&a, "feature.rs", "// a's feature\n");
    open(&a, "a");
    let rb = open(&b, "b");
    let m = rb.merge("a@").unwrap();
    assert!(m.conflicts.is_empty());
    assert_eq!(read(&b, "feature.rs"), "// a's feature\n");
    assert_eq!(rb.working().unwrap().1.parents.len(), 2);
    assert!(rb.merge("a@").is_err(), "already merged");
}

#[test]
fn resolve_command_and_force() {
    let (_tmp, _main, a, b) = setup();
    write(&a, "README.md", "# A\n");
    commit(&a, "a", "a");
    landed(open(&a, "a").land(None).unwrap());
    write(&b, "README.md", "# B\n");
    let rb = open(&b, "b");
    rb.sync().unwrap();
    assert!(rb.resolve_conflict("README.md", false).is_err(), "markers still present");
    assert!(rb.resolve_conflict("README.md", true).unwrap().is_empty());
    rb.commit(&msg("keep markers deliberately")).unwrap();
}

#[test]
fn parallel_lands_all_reach_main() {
    let (_tmp, main, _, _) = setup();
    let repo = open(&main, "lead");
    let paths: Vec<PathBuf> =
        (0..4).map(|i| repo.workspace_add(&format!("p{i}"), None, None).unwrap().info.path.unwrap()).collect();
    drop(repo);
    std::thread::scope(|s| {
        for (i, p) in paths.iter().enumerate() {
            s.spawn(move || {
                write(p, &format!("p{i}.txt"), "x\n");
                commit(p, &format!("p{i}"), &format!("p{i} work"));
                loop {
                    match open(p, &format!("p{i}")).land(None) {
                        Ok(r) => {
                            landed(r);
                            break;
                        }
                        Err(e) if e.to_string().contains("kept moving") => continue,
                        Err(e) => panic!("{e:#}"),
                    }
                }
            });
        }
    });
    let repo = open(&main, "lead");
    repo.sync().unwrap();
    for i in 0..4 {
        assert!(main.join(format!("p{i}.txt")).exists(), "p{i} missing from main");
    }
}
