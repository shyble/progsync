//! The land queue (T-32): with `land.queue true`, one land at a time holds
//! main, so a long check runs once instead of again after every land that
//! overtook it.
//!
//! - A land first joins the queue: `.psync/land/wait/<ticket>.json`, tickets
//!   ordered by when they joined. When main is free and its ticket is the
//!   first live one, it takes the **lease** (`.psync/land/holder.json`),
//!   then syncs, runs its check and moves main as usual, and gives the lease
//!   up when it is done (landed, refused, failed).
//! - Every entry (waiting or holding) is renewed every [`RENEW_MS`] by a
//!   thread of the process that owns it, so a lease lasts as long as the
//!   land, however long its check takes. An entry not renewed for
//!   [`EXPIRE_MS`] belongs to a process that died, and is ignored (and
//!   removed by the next land that looks).
//! - Changes to the files happen under `.psync/land/lock`, so taking the
//!   lease is atomic and a renewal never overwrites another land's lease.
//!
//! The queue orders the lands of one repository (its workspaces). Other
//! repositories land into their own main and meet at `push`/`pull`.

use crate::error::{Context, Result};
use crate::object::Identity;
use crate::repo::Repo;
use crate::util::{atomic_write, now_ms, random_hex};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub const KEY: &str = "land.queue";
pub const RENEW_MS: i64 = 5_000;
pub const EXPIRE_MS: i64 = 60_000;
const POLL: Duration = Duration::from_millis(250);

/// One land in the queue (waiting, or holding main).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ticket: String,
    pub workspace: String,
    pub by: Identity,
    pub pid: u32,
    /// The land check it runs (or will), if any.
    pub check: Option<String>,
    /// When it joined the queue.
    pub since_ms: i64,
    /// When it took the lease (holder only).
    pub holding_ms: Option<i64>,
    pub renewed_ms: i64,
}

crate::json_struct!(Entry { ticket, workspace, by, pid, check #default ?Option::is_none, since_ms, holding_ms #default ?Option::is_none, renewed_ms });

impl Entry {
    fn live(&self, now: i64) -> bool {
        now - self.renewed_ms < EXPIRE_MS
    }
}

/// Who holds main, and who waits, in order.
#[derive(Clone, Debug, Default)]
pub struct LandQueue {
    pub holder: Option<Entry>,
    pub waiting: Vec<Entry>,
}

impl LandQueue {
    pub fn is_empty(&self) -> bool {
        self.holder.is_none() && self.waiting.is_empty()
    }
}

fn dir(dot: &Path) -> PathBuf {
    dot.join("land")
}
fn holder_path(dot: &Path) -> PathBuf {
    dir(dot).join("holder.json")
}
fn wait_dir(dot: &Path) -> PathBuf {
    dir(dot).join("wait")
}
fn wait_path(dot: &Path, ticket: &str) -> PathBuf {
    wait_dir(dot).join(format!("{ticket}.json"))
}

fn read_entry(path: &Path) -> Option<Entry> {
    fs::read(path).ok().and_then(|b| crate::json::from_slice(&b).ok())
}

fn write_entry(path: &Path, e: &Entry) -> Result<()> {
    atomic_write(path, &crate::json::to_vec(e)?)
}

/// Run `f` under the queue's lock.
fn locked<T>(dot: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    fs::create_dir_all(wait_dir(dot))?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir(dot).join("lock"))?;
    lock.lock().context("locking the land queue")?;
    f()
}

/// The queue as it is now (live entries only).
pub fn read(dot: &Path) -> LandQueue {
    let now = now_ms();
    let holder = read_entry(&holder_path(dot)).filter(|e| e.live(now));
    let mut waiting: Vec<Entry> = fs::read_dir(wait_dir(dot))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|d| read_entry(&d.path()))
        .filter(|e| e.live(now) && holder.as_ref().is_none_or(|h| h.ticket != e.ticket))
        .collect();
    waiting.sort_by(|a, b| a.ticket.cmp(&b.ticket));
    LandQueue { holder, waiting }
}

/// A place in the queue; once [`Place::wait_for_main`] returns, the lease on
/// main. Renewed until dropped; dropping it leaves the queue.
pub struct Place {
    dot: PathBuf,
    ticket: String,
    stop: Arc<AtomicBool>,
    renewer: Option<JoinHandle<()>>,
}

impl Place {
    /// Join the queue.
    pub fn join(dot: &Path, workspace: &str, by: &Identity, check: Option<&str>) -> Result<Place> {
        let now = now_ms();
        // Tickets sort by time joined; the random part breaks ties.
        let ticket = format!("{now:013}-{}", random_hex(8));
        let entry = Entry {
            ticket: ticket.clone(),
            workspace: workspace.into(),
            by: by.clone(),
            pid: std::process::id(),
            check: check.map(String::from),
            since_ms: now,
            holding_ms: None,
            renewed_ms: now,
        };
        locked(dot, || write_entry(&wait_path(dot, &ticket), &entry))?;
        let stop = Arc::new(AtomicBool::new(false));
        let renewer = {
            let (dot, ticket, stop) = (dot.to_path_buf(), ticket.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut last = now_ms();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(POLL);
                    if now_ms() - last >= RENEW_MS {
                        let _ = renew(&dot, &ticket);
                        last = now_ms();
                    }
                }
            })
        };
        Ok(Place { dot: dot.to_path_buf(), ticket, stop, renewer: Some(renewer) })
    }

    /// Wait until this place holds main. `on_wait` is called with the queue
    /// whenever it changes while waiting.
    pub fn wait_for_main(&self, on_wait: &mut dyn FnMut(&LandQueue)) -> Result<()> {
        let mut shown: Option<(Option<String>, Vec<String>)> = None;
        loop {
            if self.try_take()? {
                return Ok(());
            }
            let q = read(&self.dot);
            let key = (q.holder.as_ref().map(|h| h.ticket.clone()), q.waiting.iter().map(|w| w.ticket.clone()).collect());
            if shown.as_ref() != Some(&key) {
                on_wait(&q);
                shown = Some(key);
            }
            std::thread::sleep(POLL);
        }
    }

    /// Take the lease if main is free and this is the first live ticket.
    fn try_take(&self) -> Result<bool> {
        let dot = &self.dot;
        locked(dot, || {
            let now = now_ms();
            let holder = read_entry(&holder_path(dot));
            if holder.as_ref().is_some_and(|h| h.ticket == self.ticket) {
                return Ok(true);
            }
            if holder.as_ref().is_some_and(|h| h.live(now)) {
                return Ok(false);
            }
            let mut first: Option<Entry> = None;
            for d in fs::read_dir(wait_dir(dot))?.flatten() {
                let Some(e) = read_entry(&d.path()) else { continue };
                if !e.live(now) {
                    let _ = fs::remove_file(d.path()); // its process is gone
                } else if first.as_ref().is_none_or(|f| e.ticket < f.ticket) {
                    first = Some(e);
                }
            }
            match first {
                Some(mut e) if e.ticket == self.ticket => {
                    e.holding_ms = Some(now);
                    e.renewed_ms = now;
                    write_entry(&holder_path(dot), &e)?;
                    let _ = fs::remove_file(wait_path(dot, &self.ticket));
                    Ok(true)
                }
                // A place renews its own entry, so it is always there.
                _ => Ok(false),
            }
        })
    }
}

/// Renew `ticket`'s entry, wherever it is.
fn renew(dot: &Path, ticket: &str) -> Result<()> {
    locked(dot, || {
        let now = now_ms();
        let hp = holder_path(dot);
        if let Some(mut h) = read_entry(&hp).filter(|h| h.ticket == ticket) {
            h.renewed_ms = now;
            return write_entry(&hp, &h);
        }
        let wp = wait_path(dot, ticket);
        if let Some(mut w) = read_entry(&wp) {
            w.renewed_ms = now;
            write_entry(&wp, &w)?;
        }
        Ok(())
    })
}

impl Drop for Place {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.renewer.take() {
            let _ = t.join();
        }
        let _ = locked(&self.dot, || {
            let hp = holder_path(&self.dot);
            if read_entry(&hp).is_some_and(|h| h.ticket == self.ticket) {
                fs::remove_file(&hp)?;
            }
            let _ = fs::remove_file(wait_path(&self.dot, &self.ticket));
            Ok(())
        });
    }
}

impl Repo {
    /// Whether lands wait their turn (`land.queue`).
    pub fn land_queue_on(&self) -> bool {
        crate::config::get(Some(&self.dot), KEY).is_some_and(|(v, _)| v == "true")
    }

    /// Who holds main and who waits to land.
    pub fn land_queue(&self) -> LandQueue {
        read(&self.dot)
    }
}
