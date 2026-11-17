use progsync_core::watch::{self, DaemonOptions, OnSettled};
use progsync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn id() -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some("watcher-test".into()) }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn start(root: &Path, ws_dir: &Path, on_settled: Option<OnSettled>) -> std::thread::JoinHandle<()> {
    start_with(root, ws_dir, on_settled, watch::PER_DIRECTORY)
}

fn start_with(root: &Path, ws_dir: &Path, on_settled: Option<OnSettled>, per_directory: bool) -> std::thread::JoinHandle<()> {
    let opts = DaemonOptions { root: root.to_path_buf(), ws_dir: ws_dir.to_path_buf(), on_settled, per_directory };
    let h = std::thread::spawn(move || watch::run(opts).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !watch::is_running(ws_dir) {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    h
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("app");
    write(&root, "src/a.rs", "a\n");
    write(&root, ".psyncignore", "target/\n");
    let repo = Repo::init(&root, id()).unwrap();
    repo.commit(&MetaUpdate { intent: Some("start".into()), ..Default::default() }).unwrap();
    let ws_dir = repo.workspace_dir();
    (tmp, root.canonicalize().unwrap(), ws_dir)
}

fn changed_paths(repo: &Repo) -> Vec<String> {
    repo.status().unwrap().changes.into_iter().map(|c| c.path).collect()
}

#[test]
fn snapshots_use_the_watcher_and_see_every_change() {
    for per_directory in [false, true] {
        see_every_change(per_directory);
    }
}

fn see_every_change(per_directory: bool) {
    // Forced on macOS, per-directory watching restarts the FSEvents
    // stream for each new watch, which can drop the sync cookie: answers
    // then fall back to full scans (still correct). inotify drops nothing.
    let native = !per_directory || watch::PER_DIRECTORY;
    let (_tmp, root, ws_dir) = setup();
    let daemon = start_with(&root, &ws_dir, None, per_directory);
    let repo = Repo::discover_as(&root, id()).unwrap();
    // First scan with a daemon is full (no token yet); later ones are not.
    assert!(!repo.snapshot().unwrap().watched);
    assert!(repo.snapshot().unwrap().watched || !native);

    // Changes made right before the command are seen (cookie sync).
    write(&root, "src/a.rs", "a changed\n");
    write(&root, "src/new/deep/b.rs", "b\n");
    write(&root, "target/out.o", "ignored\n");
    let s = repo.snapshot().unwrap();
    assert!((s.watched || !native) && s.changed, "per_directory={per_directory} {s:?}");
    assert_eq!(changed_paths(&repo), vec!["src/a.rs", "src/new/deep/b.rs"]);

    fs::remove_dir_all(root.join("src/new")).unwrap();
    let s = repo.snapshot().unwrap();
    assert!((s.watched || !native) && s.changed, "per_directory={per_directory} {s:?}");
    assert_eq!(changed_paths(&repo), vec!["src/a.rs"]);

    // Undo rewrites files on disk; the watcher's view stays correct.
    repo.undo().unwrap();
    assert!(repo.snapshot().unwrap().watched || !native);
    assert_eq!(changed_paths(&repo), vec!["src/a.rs", "src/new/deep/b.rs"]);

    assert!(watch::stop(&ws_dir));
    daemon.join().unwrap();
    assert!(!watch::info_path(&ws_dir).exists());
    // Without a daemon, scans are full again and still correct.
    write(&root, "src/c.rs", "c\n");
    let s = repo.snapshot().unwrap();
    assert!(!s.watched && s.changed);
}

#[test]
fn only_one_daemon_per_workspace() {
    let (_tmp, root, ws_dir) = setup();
    let daemon = start(&root, &ws_dir, None);
    let second = watch::run(DaemonOptions { root: root.clone(), ws_dir: ws_dir.clone(), on_settled: None, per_directory: false });
    assert!(second.is_err());
    assert!(watch::stop(&ws_dir));
    daemon.join().unwrap();
    assert!(!watch::stop(&ws_dir), "nothing left to stop");
}

#[test]
fn daemon_snapshots_when_edits_settle_and_waits_for_busy_workspaces() {
    let (_tmp, root, ws_dir) = setup();
    let r = root.clone();
    let daemon = start(&root, &ws_dir, Some(Box::new(move || Ok(Repo::snapshot_if_idle(&r, id())?.is_some()))));

    // While a command holds the workspace, the daemon does not snapshot.
    let held = Repo::discover_as(&root, id()).unwrap();
    assert!(Repo::snapshot_if_idle(&root, id()).unwrap().is_none());
    write(&root, "src/live.rs", "live\n");
    std::thread::sleep(Duration::from_millis(800));
    assert!(held.op_log(1).unwrap()[0].1.description.contains("commit"), "no snapshot while busy");
    drop(held);

    // Once free, the pending edits are snapshotted without any command.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let repo = Repo::discover_as(&root, id()).unwrap();
        let last = repo.op_log(1).unwrap().remove(0).1;
        if last.description.ends_with("(watcher)") {
            assert_eq!(changed_paths(&repo), vec!["src/live.rs"]);
            break;
        }
        drop(repo);
        assert!(Instant::now() < deadline, "watcher never snapshotted");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(watch::status(&ws_dir).unwrap().snapshots_taken >= 1);
    watch::stop(&ws_dir);
    daemon.join().unwrap();
}

#[test]
fn removing_a_workspace_stops_its_daemon() {
    let (_tmp, root, _) = setup();
    let repo = Repo::discover_as(&root, id()).unwrap();
    let added = repo.workspace_add("a", None, None).unwrap();
    let a_root = added.info.path.unwrap();
    let a_dir = Repo::discover_as(&a_root, id()).unwrap().workspace_dir();
    let daemon = start(&a_root, &a_dir, None);
    repo.workspace_remove("a").unwrap();
    daemon.join().unwrap();
    assert!(!a_root.exists());
}

/// Both ways of watching: the whole tree, and directory by directory (as
/// on Linux), which must skip ignored directories and follow new ones.
#[test]
fn ignored_directories_are_not_watched_or_recorded() {
    for per_directory in [false, true] {
        // Forced on macOS, per-directory watching restarts the FSEvents
        // stream for each new watch, which can drop the sync cookie: answers
        // then fall back to full scans (still correct). inotify drops nothing.
        let native = !per_directory || watch::PER_DIRECTORY;
        let (_tmp, root, ws_dir) = setup();
        write(&root, "target/debug/old.o", "built\n");
        write(&root, "node/x.js", "x\n");
        let daemon = start_with(&root, &ws_dir, None, per_directory);
        let repo = Repo::discover_as(&root, id()).unwrap();
        repo.snapshot().unwrap();
        repo.commit(&MetaUpdate { intent: Some("node".into()), ..Default::default() }).unwrap();
        let st = watch::status(&ws_dir).unwrap();
        if per_directory {
            // The root, src and node; not target/ or .psync.
            assert_eq!(st.directories, Some(3), "per_directory");
        } else {
            assert_eq!(st.directories, None);
        }

        // A build writes under target/: nothing is remembered.
        let before = watch::status(&ws_dir).unwrap().paths;
        for i in 0..50 {
            write(&root, &format!("target/debug/deps/f{i}.o"), "obj\n");
        }
        assert!(repo.snapshot().unwrap().watched || !native);
        let st = watch::status(&ws_dir).unwrap();
        assert_eq!(st.paths, before, "per_directory={per_directory}");
        // Watching the whole tree, the events arrive and are dropped; watching
        // directory by directory, they never arrive.
        assert_eq!(st.ignored > 0, !per_directory, "{} ignored events", st.ignored);

        // New directories are followed, however deep, and seen at once.
        write(&root, "src/new/deep/b.rs", "b\n");
        let s = repo.snapshot().unwrap();
        assert!((s.watched || !native) && s.changed, "per_directory={per_directory}");
        assert_eq!(changed_paths(&repo), vec!["src/new/deep/b.rs"]);
        std::thread::sleep(Duration::from_millis(300));
        write(&root, "src/new/deep/c.rs", "c\n");
        assert!(repo.snapshot().unwrap().watched || !native);
        assert_eq!(changed_paths(&repo), vec!["src/new/deep/b.rs", "src/new/deep/c.rs"], "per_directory={per_directory}");

        // Un-ignoring target/ makes it tracked, and watched from then on.
        write(&root, ".psyncignore", "");
        repo.snapshot().unwrap();
        assert!(changed_paths(&repo).contains(&"target/debug/old.o".to_string()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while per_directory && watch::status(&ws_dir).unwrap().directories != Some(8) {
            assert!(Instant::now() < deadline, "target/ never watched: {:?}", watch::status(&ws_dir).unwrap().directories);
            std::thread::sleep(Duration::from_millis(20));
        }
        write(&root, "target/debug/later.o", "later\n");
        assert!(repo.snapshot().unwrap().watched || !native);
        assert!(changed_paths(&repo).contains(&"target/debug/later.o".to_string()), "per_directory={per_directory}");

        // Removed directories leave nothing behind.
        fs::remove_dir_all(root.join("src/new")).unwrap();
        assert!(repo.snapshot().unwrap().watched || !native);
        assert!(!changed_paths(&repo).iter().any(|p| p.starts_with("src/new")));
        drop(repo);
        assert!(watch::stop(&ws_dir));
        daemon.join().unwrap();
    }
}
