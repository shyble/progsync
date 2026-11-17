use progsync_core::gc::{check, gc};
use progsync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn id() -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some("agent".into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn object_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for d in fs::read_dir(root.join(".psync/objects")).unwrap().flatten() {
        out.extend(fs::read_dir(d.path()).unwrap().flatten().map(|f| f.path()).filter(|p| p.is_file()));
    }
    out
}

/// Make every object, pack (and temp file) look two days old.
fn age_everything(root: &Path) {
    let old = SystemTime::now() - Duration::from_secs(2 * 86_400);
    let tmp = fs::read_dir(root.join(".psync/tmp")).unwrap().flatten().map(|e| e.path());
    for p in object_files(root).into_iter().chain(tmp) {
        if p.is_dir() {
            continue;
        }
        fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(old).unwrap();
    }
}

/// Whether the store has `h`, loose or packed.
fn exists(repo: &Repo, h: progsync_core::Hash) -> bool {
    repo.store.read_raw(&h).is_ok()
}

fn object_path(root: &Path, h: progsync_core::Hash) -> std::path::PathBuf {
    let hex = h.to_hex();
    root.join(".psync/objects").join(&hex[..2]).join(&hex[2..])
}

#[test]
fn check_reports_missing_and_damaged_objects_and_gc_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a.txt", "alpha\n");
    write(root, "src/b.txt", "beta\n");
    let repo = Repo::init(root, id()).unwrap();
    repo.commit(&msg("first")).unwrap();
    let r = check(&repo).unwrap();
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert!(r.objects >= 6);

    let blob = repo.store.put_blob(b"alpha\n").unwrap();
    fs::remove_file(object_path(root, blob)).unwrap();
    let (_, rev) = repo.working().unwrap();
    let tree = repo.store.get_tree(&rev.tree).unwrap();
    let sub = tree.entries.iter().find(|e| e.name == "src").unwrap().hash;
    fs::write(object_path(root, sub), b"garbage").unwrap();

    let r = check(&repo).unwrap();
    assert_eq!(r.problems.len(), 2, "{:?}", r.problems);
    assert!(r.problems.iter().any(|p| p.starts_with(&format!("file {}: missing", blob.short()))), "{:?}", r.problems);
    assert!(r.problems.iter().any(|p| p.starts_with(&format!("tree {}: damaged", sub.short()))), "{:?}", r.problems);
    let err = gc(&repo, None, false).unwrap_err().to_string();
    assert!(err.contains("nothing was removed"), "{err}");
}

#[test]
fn gc_keep_forgets_old_operations_and_their_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "f.txt", "v0\n");
    let repo = Repo::init(root, id()).unwrap();
    repo.commit(&msg("base")).unwrap();
    // Intermediate snapshots of the working change: only the operation log
    // refers to them.
    for v in ["v1", "v2"] {
        write(root, "f.txt", &format!("{v}\n"));
        repo.snapshot().unwrap();
    }
    write(root, "f.txt", "v3\n");
    repo.snapshot().unwrap();
    repo.commit(&msg("final")).unwrap();
    let v1 = repo.store.put_blob(b"v1\n").unwrap();
    let v3 = repo.store.put_blob(b"v3\n").unwrap();

    // Young unreachable objects are kept; without --keep everything is
    // reachable anyway.
    let r = gc(&repo, None, false).unwrap();
    assert_eq!((r.removed, r.forgotten_ops), (0, 0));
    assert!(r.horizon.is_none());

    age_everything(root);
    let ops_before = repo.op_log(usize::MAX).unwrap().len();
    let dry = gc(&repo, Some(0), true).unwrap();
    assert!(dry.removed > 0 && dry.forgotten_ops == ops_before - 1);
    assert!(repo.op_horizon().unwrap().is_none(), "dry run changes nothing");
    assert!(exists(&repo, v1));

    let r = gc(&repo, Some(0), false).unwrap();
    assert_eq!((r.removed, r.forgotten_ops), (dry.removed, dry.forgotten_ops));
    assert!(!exists(&repo, v1), "intermediate snapshot removed");
    assert!(exists(&repo, v3));
    assert_eq!(repo.op_log(usize::MAX).unwrap().len(), 1);
    let c = check(&repo).unwrap();
    assert!(c.problems.is_empty(), "{:?}", c.problems);
    assert_eq!(c.unreachable, 0);
    // Change history is intact; the operation history ends at the horizon.
    let intents: Vec<String> = repo.log_all(10).unwrap().into_iter().map(|(_, r)| r.meta.intent).collect();
    assert!(intents.contains(&"base".to_string()) && intents.contains(&"final".to_string()), "{intents:?}");
    assert!(repo.undo().is_err());
    // Work continues normally.
    write(root, "f.txt", "v4\n");
    repo.snapshot().unwrap();
    repo.commit(&msg("after gc")).unwrap();
    assert!(repo.undo().is_ok());
    assert!(check(&repo).unwrap().problems.is_empty());
}

#[test]
fn gc_removes_old_orphans_and_temp_files_but_keeps_reused_objects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a.txt", "a\n");
    let repo = Repo::init(root, id()).unwrap();
    // Left behind by an interrupted command.
    let orphan = repo.store.put_blob(b"never recorded").unwrap();
    let reused = repo.store.put_blob(b"recorded later").unwrap();
    fs::write(root.join(".psync/tmp/0123abcd"), b"half-written").unwrap();
    age_everything(root);
    // A command writes the same contents again: the old object is reused
    // (and freshened), not rewritten.
    assert_eq!(repo.store.put_blob(b"recorded later").unwrap(), reused);

    let r = gc(&repo, None, false).unwrap();
    assert_eq!((r.removed, r.temp_files, r.young), (1, 1, 1));
    assert!(!exists(&repo, orphan));
    assert!(exists(&repo, reused));
    assert!(!root.join(".psync/tmp/0123abcd").exists());
    assert!(check(&repo).unwrap().problems.is_empty());
}

/// One agent keeps committing (reusing contents that only forgotten
/// operations referred to) while another workspace runs `gc --keep 0` over
/// and over: the repository stays complete.
#[test]
fn gc_runs_safely_while_another_workspace_commits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("main");
    fs::create_dir(&root).unwrap();
    write(&root, "f.txt", "start\n");
    let repo = Repo::init(&root, id()).unwrap();
    let variants: Vec<String> = (0..6).map(|i| format!("variant {i}\n").repeat(50)).collect();
    for v in &variants {
        write(&root, "f.txt", v);
        write(&root, &format!("dir{}/g.txt", v.len() % 3), v);
        repo.snapshot().unwrap();
    }
    repo.commit(&msg("base")).unwrap();
    let other = repo.workspace_add("gc", None, None).unwrap().info.path.unwrap();
    age_everything(&root);

    std::thread::scope(|s| {
        let committer = s.spawn(|| {
            for i in 0..40 {
                let v = &variants[i % variants.len()];
                write(&root, "f.txt", v);
                write(&root, &format!("dir{}/g.txt", i % 3), v);
                repo.snapshot().unwrap();
                if i % 4 == 0 {
                    repo.commit(&msg(&format!("c{i}"))).unwrap();
                }
            }
        });
        let gc_repo = Repo::discover_as(&other, id()).unwrap();
        let mut i = 0;
        while !committer.is_finished() {
            gc(&gc_repo, Some(0), false).unwrap();
            progsync_core::gc::auto_with(&gc_repo, 0, 2).unwrap();
            i += 1;
        }
        assert!(i > 1, "gc ran while the other workspace committed");
    });
    let c = check(&repo).unwrap();
    assert!(c.problems.is_empty(), "{:?}", c.problems);
}

#[test]
fn packed_objects_serve_every_command_and_transfers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    fs::create_dir(&root).unwrap();
    write(&root, "a.txt", "one\n");
    write(&root, "lib/b.rs", "fn b() {}\n");
    let repo = Repo::init(&root, id()).unwrap();
    repo.commit(&msg("first")).unwrap();
    repo.land(None).unwrap();
    assert!(progsync_core::gc::auto_with(&repo, 0, 16).unwrap());
    let c = check(&repo).unwrap();
    assert!(c.problems.is_empty(), "{:?}", c.problems);
    assert_eq!((c.packs, c.loose), (1, 0));

    // Reading, writing and undoing on top of packed objects.
    write(&root, "a.txt", "two\n");
    repo.snapshot().unwrap();
    assert_eq!(repo.status().unwrap().changes.len(), 1);
    repo.commit(&msg("second")).unwrap();
    repo.undo().unwrap();
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "two\n");
    // Packs are merged once there are too many.
    assert!(progsync_core::gc::auto_with(&repo, 0, 16).unwrap());
    assert!(progsync_core::gc::auto_with(&repo, 0, 1).unwrap());
    let c = check(&repo).unwrap();
    assert!(c.problems.is_empty(), "{:?}", c.problems);
    assert_eq!((c.packs, c.loose), (1, 0));

    // Another repository fetches from the packed one.
    let dst = dir.path().join("dst");
    let clone = Repo::clone_from(root.to_str().unwrap(), &dst, id()).unwrap();
    assert_eq!(fs::read_to_string(dst.join("lib/b.rs")).unwrap(), "fn b() {}\n");
    assert!(check(&clone).unwrap().problems.is_empty());

    // A damaged pack is reported.
    let packs = root.join(".psync/objects/pack");
    let idx = fs::read_dir(&packs).unwrap().flatten().find(|e| e.path().extension().is_some_and(|x| x == "idx")).unwrap().path();
    let mut b = fs::read(&idx).unwrap();
    let n = b.len();
    b[n - 1] ^= 1;
    fs::write(&idx, b).unwrap();
    drop(repo);
    let c = check(&Repo::discover_as(&root, id()).unwrap()).unwrap();
    assert!(c.problems.iter().any(|p| p.contains("index is damaged")), "{:?}", c.problems);
}
