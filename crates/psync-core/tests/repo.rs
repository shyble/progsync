use psync_core::diff::FileStatus;
use psync_core::{Identity, MetaUpdate, Repo};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn changed_paths(repo: &Repo) -> Vec<(String, FileStatus)> {
    repo.status().unwrap().changes.into_iter().map(|c| (c.path.clone(), c.status())).collect()
}

#[test]
fn init_tracks_existing_files_and_respects_ignores() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/main.rs", "fn main() {}\n");
    write(root, "README.md", "hi\n");
    write(root, ".gitignore", "target/\n*.log\n");
    write(root, "target/debug/out", "binary");
    write(root, "debug.log", "noise");
    write(root, ".git/HEAD", "ref");

    let repo = Repo::init(root, id("test-agent")).unwrap();
    let mut paths: Vec<String> = changed_paths(&repo).into_iter().map(|(p, _)| p).collect();
    paths.sort();
    assert_eq!(paths, vec![".gitignore", "README.md", "src/main.rs"]);
}

#[test]
fn snapshot_detects_add_modify_delete() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a.txt", "one\n");
    write(root, "b.txt", "two\n");
    let repo = Repo::init(root, id("agent")).unwrap();
    repo.commit(&msg("initial")).unwrap();

    write(root, "a.txt", "one changed\n");
    fs::remove_file(root.join("b.txt")).unwrap();
    write(root, "dir/c.txt", "three\n");
    assert!(repo.snapshot().unwrap().changed);

    assert_eq!(
        changed_paths(&repo),
        vec![
            ("a.txt".to_string(), FileStatus::Modified),
            ("b.txt".to_string(), FileStatus::Deleted),
            ("dir/c.txt".to_string(), FileStatus::Added),
        ]
    );
    // Snapshotting again without edits is a no-op.
    assert!(!repo.snapshot().unwrap().changed);
}

#[test]
fn commit_creates_history_with_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = Repo::init(root, id("claude-1")).unwrap();

    write(root, "x.txt", "x\n");
    repo.snapshot().unwrap();
    let update = MetaUpdate {
        intent: Some("add x".into()),
        task: Some("T-1".into()),
        extra: vec![("model".into(), "opus".into())],
    };
    let ((done_hash, done), (_, next)) = repo.commit(&update).unwrap();
    assert_eq!(done.meta.intent, "add x");
    assert_eq!(done.author.agent.as_deref(), Some("claude-1"));
    assert_eq!(done.author.email, "ada@example.com");
    assert_eq!(done.meta.task.as_deref(), Some("T-1"));
    assert_eq!(done.meta.extra["model"], "opus");
    assert_eq!(next.parents, vec![done_hash]);
    assert_eq!(next.meta.task.as_deref(), Some("T-1"), "task carries over");
    assert!(next.meta.intent.is_empty());

    // Resolve by change id prefix, by @-, and by revision hash prefix.
    let by_id = repo.resolve(&done.change_id.short()[..6]).unwrap();
    assert_eq!(by_id, done_hash);
    assert_eq!(repo.resolve("@-").unwrap(), done_hash);
    assert_eq!(repo.resolve(&done_hash.short()[..10]).unwrap(), done_hash);

    let (working, _) = repo.working().unwrap();
    // init change (now committed as "add x") + new working change.
    assert_eq!(repo.log(working, 10).unwrap().len(), 2);
}

#[test]
fn commit_requires_intent() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path(), id("a")).unwrap();
    assert!(repo.commit(&MetaUpdate::default()).is_err());
}

#[test]
fn undo_restores_files_and_repeats_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "f.txt", "v1\n");
    let repo = Repo::init(root, id("a")).unwrap();
    repo.commit(&msg("v1")).unwrap();

    write(root, "f.txt", "v2\n");
    repo.snapshot().unwrap();
    write(root, "g.txt", "new\n");
    repo.snapshot().unwrap();

    // Undo the second snapshot: g.txt disappears, f.txt stays v2.
    repo.undo().unwrap();
    assert!(!root.join("g.txt").exists());
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "v2\n");

    // Undo again: the first snapshot is undone too.
    repo.undo().unwrap();
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "v1\n");

    // Nothing was lost: restore the state before the undos.
    let ops = repo.op_log(10).unwrap();
    let before_undos = ops[2].0;
    repo.op_restore(before_undos).unwrap();
    assert_eq!(fs::read_to_string(root.join("g.txt")).unwrap(), "new\n");
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "v2\n");
}

#[cfg(unix)]
#[test]
fn undo_handles_file_directory_transitions_and_exec_bit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "thing", "file\n");
    write(root, "run.sh", "#!/bin/sh\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let repo = Repo::init(root, id("a")).unwrap();
    repo.commit(&msg("start")).unwrap();

    fs::remove_file(root.join("thing")).unwrap();
    write(root, "thing/inner.txt", "dir now\n");
    fs::remove_file(root.join("run.sh")).unwrap();
    repo.snapshot().unwrap();

    repo.undo().unwrap();
    assert_eq!(fs::read_to_string(root.join("thing")).unwrap(), "file\n");
    let mode = fs::metadata(root.join("run.sh")).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "exec bit restored");
}

#[test]
fn undo_of_describe_restores_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path(), id("a")).unwrap();
    repo.describe(&msg("first")).unwrap();
    repo.describe(&msg("second")).unwrap();
    repo.undo().unwrap();
    assert_eq!(repo.working().unwrap().1.meta.intent, "first");
}

#[test]
fn stat_cache_avoids_rehashing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for i in 0..50 {
        write(root, &format!("f{i}.txt"), &format!("{i}\n"));
    }
    // Backdate mtimes so entries are not "racy".
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for i in 0..50 {
        fs::File::options().write(true).open(root.join(format!("f{i}.txt"))).unwrap().set_modified(old).unwrap();
    }
    let repo = Repo::init(root, id("a")).unwrap();
    let s = repo.snapshot().unwrap();
    assert_eq!((s.hashed, s.cached), (0, 50));

    write(root, "f7.txt", "changed\n");
    let s = repo.snapshot().unwrap();
    assert!(s.changed);
    assert_eq!(s.hashed, 1);
}

#[test]
fn cached_tree_build_matches_full_rebuild() {
    use psync_core::store::Store;
    use psync_core::wc::{scan, Index};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in 0..5 {
        for f in 0..5 {
            write(root, &format!("d{d}/sub/f{f}.txt"), &format!("{d}{f}\n"));
        }
    }
    let store_dir = tempfile::tempdir().unwrap();
    let store = Store::open(store_dir.path());
    store.create_dirs().unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let backdate = |p: &Path| fs::File::options().write(true).open(p).unwrap().set_modified(old).unwrap();
    for d in 0..5 {
        for f in 0..5 {
            backdate(&root.join(format!("d{d}/sub/f{f}.txt")));
        }
    }
    let mut index = scan(root, &store, &Index::default(), None).unwrap().index;
    let steps: Vec<Box<dyn Fn()>> = vec![
        Box::new(|| write(root, "d2/sub/f3.txt", "changed\n")),
        Box::new(|| fs::remove_file(root.join("d4/sub/f0.txt")).unwrap()),
        Box::new(|| write(root, "d9/new/deep/x.txt", "new dir\n")),
        Box::new(|| fs::remove_dir_all(root.join("d1")).unwrap()),
        Box::new(|| {}),
    ];
    for step in steps {
        step();
        let incremental = scan(root, &store, &index, None).unwrap();
        let full = scan(root, &store, &Index::default(), None).unwrap();
        assert_eq!(incremental.tree, full.tree, "cached tree differs from a full rebuild");
        index = incremental.index;
    }
    // Once no file is "racy" (modified within the last 2s), a scan with
    // nothing changed does not need to save the index.
    let mut dirs = vec![root.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let t = e.file_type().unwrap();
            if t.is_dir() && e.file_name() != ".psync" {
                dirs.push(e.path());
            } else if t.is_file() {
                backdate(&e.path());
            }
        }
    }
    let settled = scan(root, &store, &index, None).unwrap();
    assert!(settled.index_changed, "new mtimes are recorded once");
    let again = scan(root, &store, &settled.index, None).unwrap();
    assert!(!again.index_changed);
    assert_eq!(again.tree, settled.tree);
}

#[test]
fn generations_and_ancestry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = Repo::init(root, id("a")).unwrap();
    let mut hashes = Vec::new();
    for i in 0..20 {
        write(root, "f.txt", &format!("{i}\n"));
        repo.snapshot().unwrap();
        let ((h, r), _) = repo.commit(&msg(&format!("c{i}"))).unwrap();
        assert_eq!(r.generation, i + 1);
        hashes.push(h);
    }
    assert!(repo.is_ancestor(hashes[3], hashes[15]).unwrap());
    assert!(!repo.is_ancestor(hashes[15], hashes[3]).unwrap());
    assert_eq!(repo.merge_base(hashes[19], hashes[7]).unwrap(), Some(hashes[7]));
}

#[test]
fn watched_scan_matches_full_scan() {
    use psync_core::store::Store;
    use psync_core::wc::{scan, scan_changed, Index};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".psyncignore", "target/\n*.log\n");
    for d in 0..4 {
        for f in 0..3 {
            write(root, &format!("d{d}/sub/f{f}.txt"), &format!("{d}{f}\n"));
        }
    }
    write(root, "top.txt", "top\n");
    let store_dir = tempfile::tempdir().unwrap();
    let store = Store::open(store_dir.path());
    store.create_dirs().unwrap();
    let mut index = scan(root, &store, &Index::default(), None).unwrap().index;

    type Step<'a> = (Box<dyn Fn() + 'a>, Vec<&'a str>);
    let steps: Vec<Step> = vec![
        (Box::new(|| write(root, "d2/sub/f1.txt", "changed\n")), vec!["d2/sub/f1.txt"]),
        (Box::new(|| fs::remove_file(root.join("d0/sub/f0.txt")).unwrap()), vec!["d0/sub/f0.txt"]),
        (Box::new(|| write(root, "new/deep/er/x.txt", "x\n")), vec!["new", "new/deep", "new/deep/er", "new/deep/er/x.txt"]),
        // Some watchers report only the top of a new tree.
        (Box::new(|| write(root, "fresh/a/b.txt", "b\n")), vec!["fresh"]),
        (Box::new(|| write(root, "target/out/bin", "noise")), vec!["target/out/bin"]),
        (Box::new(|| write(root, "d1/debug.log", "noise")), vec!["d1/debug.log"]),
        (Box::new(|| write(root, "d1/sub/added.txt", "a\n")), vec!["d1/sub/added.txt"]),
        // Directory renamed away and another renamed into its place: one event each.
        (
            Box::new(|| {
                fs::rename(root.join("d3"), root.join("moved")).unwrap();
                fs::rename(root.join("fresh"), root.join("d3")).unwrap();
            }),
            vec!["d3", "moved", "fresh"],
        ),
        // File replaced by a directory, and a directory by a file.
        (
            Box::new(|| {
                fs::remove_file(root.join("top.txt")).unwrap();
                write(root, "top.txt/inner.txt", "inner\n");
                fs::remove_dir_all(root.join("new")).unwrap();
                write(root, "new", "now a file\n");
            }),
            vec!["top.txt", "new"],
        ),
        (Box::new(|| fs::remove_dir_all(root.join("d1")).unwrap()), vec!["d1"]),
        (Box::new(|| {}), vec![]),
        (Box::new(|| {}), vec![""]),
    ];
    for (i, (step, changed)) in steps.into_iter().enumerate() {
        step();
        let changed: Vec<String> = changed.into_iter().map(String::from).collect();
        let inc = scan_changed(root, &store, &index, &changed, None).unwrap().expect("incremental");
        let full = scan(root, &store, &Index::default(), None).unwrap();
        assert_eq!(inc.tree, full.tree, "step {i}: tree differs from a full scan");
        let keys = |ix: &Index| ix.entries.iter().map(|(k, e)| (k.clone(), e.hash)).collect::<Vec<_>>();
        assert_eq!(keys(&inc.index), keys(&full.index), "step {i}: entries");
        assert_eq!(inc.index.trees, full.index.trees, "step {i}: tree cache");
        index = inc.index;
    }
    // Changing ignore rules needs a full scan.
    write(root, ".psyncignore", "*.log\n");
    assert!(scan_changed(root, &store, &index, &[".psyncignore".into()], None).unwrap().is_none());
}

#[test]
fn author_agent_is_kept_and_filled_in() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let owner = Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: None };
    write(root, "a.txt", "a\n");
    let repo = Repo::init(root, id("claude")).unwrap();
    repo.commit(&msg("start")).unwrap();
    drop(repo);

    // The owner's own command snapshots the agent's first edits: the change
    // stays credited to the agent.
    write(root, "a.txt", "agent edit\n");
    let r = Repo::discover_as(root, owner.clone()).unwrap();
    r.snapshot().unwrap();
    assert_eq!(r.working().unwrap().1.author.agent.as_deref(), Some("claude"));
    drop(r);
    let r = Repo::discover_as(root, id("claude")).unwrap();
    let ((_, done), _) = r.commit(&msg("agent's work")).unwrap();
    assert_eq!(done.author.agent.as_deref(), Some("claude"));
    drop(r);

    // The owner edits and commits in a change the agent's commit started:
    // the owner is the author.
    let r = Repo::discover_as(root, owner.clone()).unwrap();
    write(root, "b.txt", "b\n");
    r.snapshot().unwrap();
    let ((_, done), _) = r.commit(&msg("owner's work")).unwrap();
    assert_eq!(done.author, owner);

    // A change started by the owner and committed by an agent is the agent's.
    write(root, "c.txt", "c\n");
    r.snapshot().unwrap();
    assert_eq!(r.working().unwrap().1.author.agent, None);
    drop(r);
    let r = Repo::discover_as(root, id("claude")).unwrap();
    r.snapshot().unwrap();
    let ((_, done), _) = r.commit(&msg("finished by the agent")).unwrap();
    assert_eq!(done.author.agent.as_deref(), Some("claude"));
    assert_eq!((done.author.name.as_str(), done.author.email.as_str()), ("Ada", "ada@example.com"));
}
