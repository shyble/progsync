//! The land queue, `land.queue` (T-32).
#![cfg(unix)]

use psync_core::config;
use psync_core::queue::{self, LandQueue, Place};
use psync_core::repo::LandResult;
use psync_core::util::now_ms;
use psync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};

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

/// A repository with main, the queue on, and workspaces `names`.
fn setup(names: &[&str]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("p");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("a.txt"), "a\n").unwrap();
    let r = Repo::init(&p, id("lead")).unwrap();
    r.commit(&msg("start")).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    config::set(&config::repo_path(r.dot()), queue::KEY, Some("true")).unwrap();
    let dirs = names.iter().map(|n| r.workspace_add(n, None, None).unwrap().info.path.unwrap()).collect();
    (tmp, p, dirs)
}

#[test]
fn queued_lands_run_each_check_once() {
    let (tmp, _p, dirs) = setup(&["w1", "w2", "w3"]);
    let runs = tmp.path().join("runs");
    // Each check notes its workspace and takes a while, so without the
    // queue the later lands would find main moved and run their check again.
    let check = format!("echo $PSYNC_WORKSPACE >> {}; sleep 1", runs.display());
    let start = Arc::new(Barrier::new(dirs.len()));
    let saw_holder = Arc::new(Mutex::new(false));
    let threads: Vec<_> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let (d, check, start, saw_holder) = (d.clone(), check.clone(), start.clone(), saw_holder.clone());
            std::thread::spawn(move || {
                let r = open(&d, &format!("agent-{i}"));
                fs::write(d.join(format!("f{i}.txt")), "x\n").unwrap();
                r.snapshot().unwrap();
                r.commit(&msg(&format!("change {i}"))).unwrap();
                start.wait();
                let mut on_wait = |q: &LandQueue| {
                    if q.holder.is_some() {
                        *saw_holder.lock().unwrap() = true;
                    }
                };
                match r.land_with(Some(&check), &mut on_wait).unwrap() {
                    LandResult::Landed { .. } => {}
                    _ => panic!("not landed"),
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let ran = fs::read_to_string(&runs).unwrap();
    assert_eq!(ran.lines().count(), 3, "one check per land:\n{ran}");
    assert!(*saw_holder.lock().unwrap(), "the waiting lands saw who held main");
    let r = open(&dirs[0], "agent-0");
    assert!(r.land_queue().is_empty(), "everyone left the queue");
    let main = r.store.get_revision(&r.main_tip().unwrap().unwrap()).unwrap();
    assert_eq!(main.generation, 4, "all three landed");
}

#[test]
fn a_lease_of_a_process_that_died_runs_out() {
    let (_tmp, p, dirs) = setup(&["w1"]);
    let r = open(&dirs[0], "agent-1");
    // Left behind by a land that crashed, last renewed long ago.
    let old = now_ms() - queue::EXPIRE_MS - 1;
    let stale = format!(
        r#"{{"ticket":"0000000000001-dead","workspace":"gone","by":{{"name":"X"}},"pid":1,"since_ms":{old},"holding_ms":{old},"renewed_ms":{old}}}"#
    );
    fs::create_dir_all(p.join(".psync/land/wait")).unwrap();
    fs::write(p.join(".psync/land/holder.json"), stale).unwrap();
    assert!(r.land_queue().is_empty(), "an expired lease is not shown");
    fs::write(dirs[0].join("b.txt"), "b\n").unwrap();
    r.snapshot().unwrap();
    r.commit(&msg("b")).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
}

#[test]
fn a_lease_renews_while_its_land_runs() {
    let (_tmp, p, _dirs) = setup(&[]);
    let dot = p.join(".psync");
    let place = Place::join(&dot, "w1", &id("a"), Some("long suite")).unwrap();
    let behind = Place::join(&dot, "w2", &id("b"), None).unwrap();
    place.wait_for_main(&mut |_| {}).unwrap();
    let q = queue::read(&dot);
    let h = q.holder.clone().unwrap();
    assert_eq!((h.workspace.as_str(), h.check.as_deref()), ("w1", Some("long suite")));
    assert_eq!(q.waiting.iter().map(|e| e.workspace.as_str()).collect::<Vec<_>>(), ["w2"]);
    std::thread::sleep(std::time::Duration::from_millis(queue::RENEW_MS as u64 + 600));
    assert!(queue::read(&dot).holder.unwrap().renewed_ms > h.renewed_ms, "renewed");
    drop(place);
    behind.wait_for_main(&mut |_| {}).unwrap();
    assert_eq!(queue::read(&dot).holder.unwrap().workspace, "w2", "the next in line holds main");
}
