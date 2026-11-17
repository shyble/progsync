//! File watcher daemon: one background process per workspace that records
//! which paths changed (from FSEvents/inotify), so a snapshot only looks at
//! those paths instead of walking the whole directory.
//!
//! Protocol: newline-delimited JSON over a Unix socket (a loopback TCP port on
//! Windows); the first line is the daemon's key from `daemon.json`. A client asks for the
//! paths changed since its token (stored in the workspace index). To be sure
//! every change made before the request has been delivered, the daemon writes
//! a *cookie* file into a watched directory and answers only once it sees the
//! cookie's own event (as git's fsmonitor and watchman do); events arrive in
//! order, so everything earlier has been recorded by then. If anything is
//! uncertain (different daemon instance, dropped events, overflow, cookie
//! timeout), the answer says `full` and the client does a full scan.
//!
//! The daemon can also snapshot the workspace when edits settle, so other
//! agents see this workspace's work live (`context`).
//!
//! Events under ignored directories (`target/`, `node_modules/`), nested
//! workspaces and `.git` are dropped by the same rules as a scan's walk
//! ([`Filter`]), so a build does not flood the daemon or trigger snapshots.
//! Where watching a tree costs one kernel watch per directory (inotify),
//! the daemon watches each directory the walk would enter separately and
//! skips ignored ones ([`PER_DIRECTORY`]); FSEvents and Windows watch the
//! whole tree for free.

use crate::store::Store;
use crate::util::now_ms;
use crate::wc::{self, Index, ScanResult, WatchToken};
use crate::bail;
use crate::error::{Context, Result};
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecursiveMode, Watcher};
use crate::ignores::{Ignores, FILE_NAMES};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use crate::json;
use crate::platform::{self, IpcListener, IpcStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Beyond this many distinct changed paths, the daemon forgets them and
/// clients fall back to one full scan.
const MAX_PATHS: usize = 100_000;
/// Replaying more paths than this is worth saving the index for.
const SAVE_AFTER_PATHS: usize = 256;
const COOKIE_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Quiet time after the last event before the daemon snapshots.
const SETTLE: Duration = Duration::from_millis(300);

/// Whether the daemon watches directories one by one (skipping ignored
/// ones) instead of the whole tree: where the kernel needs a watch per
/// directory anyway (inotify on Linux), so ignored trees would use up
/// `fs.inotify.max_user_watches`.
pub const PER_DIRECTORY: bool = cfg!(not(any(target_os = "macos", windows)));

/// Written to `<workspace dir>/daemon.json` while a daemon runs.
#[derive(Clone, Debug)]
pub struct DaemonInfo {
    pub pid: u32,
    /// Where clients connect: a socket path (Unix) or `127.0.0.1:port`.
    pub address: String,
    /// Clients send this first, so only those who can read `daemon.json`
    /// (owner-only) can talk to the daemon.
    pub key: String,
    pub instance: String,
    pub root: PathBuf,
    pub started_ms: i64,
    /// Whether the daemon snapshots the workspace when edits settle.
    pub snapshots: bool,
}
crate::json_struct!(DaemonInfo { pid, address @"socket", key #default, instance, root, started_ms, snapshots });

#[derive(Clone, Debug)]
pub struct DaemonStatus {
    pub info: DaemonInfo,
    /// File-system events received.
    pub events: u64,
    /// Distinct paths currently remembered.
    pub paths: usize,
    pub snapshots_taken: u64,
    pub last_error: Option<String>,
    /// Events dropped because a scan would not look there (ignored paths).
    pub ignored: u64,
    /// Directories watched one by one (`None`: the whole tree is watched).
    pub directories: Option<usize>,
}
crate::json_struct!(DaemonStatus { info, events, paths, snapshots_taken, last_error, ignored #default, directories #default });

/// Changed paths since a token, and the new token.
#[derive(Clone, Debug)]
pub struct Answer {
    pub token: WatchToken,
    /// The token could not be served: do a full scan.
    pub full: bool,
    pub paths: Vec<String>,
}
crate::json_struct!(Answer { token, full, paths });

#[derive(Debug)]
enum Request {
    /// Followed by a second line, `{"token": ...}`, sent while the daemon
    /// syncs (so the client can load its index meanwhile).
    Since,
    Status,
    Stop,
}

impl crate::json::ToJson for Request {
    /// `{"op": "since"}` etc.
    fn write_json(&self, out: &mut String) {
        let op = match self {
            Request::Since => "since",
            Request::Status => "status",
            Request::Stop => "stop",
        };
        crate::json!({ "op": op }).write_json(out);
    }
}

impl crate::json::FromJson for Request {
    fn from_json(v: &crate::json::Value) -> Result<Self> {
        Ok(match v["op"].as_str() {
            Some("since") => Request::Since,
            Some("status") => Request::Status,
            Some("stop") => Request::Stop,
            other => crate::bail!("unknown request {other:?}"),
        })
    }
}

pub fn info_path(ws_dir: &Path) -> PathBuf {
    ws_dir.join("daemon.json")
}

/// Unix socket paths are limited to ~104 bytes, so the socket lives in the
/// temp directory under a name derived from the workspace directory.
fn socket_path(ws_dir: &Path) -> PathBuf {
    let id = blake3::hash(ws_dir.as_os_str().as_encoded_bytes()).to_hex();
    let name = format!("psync-{}.sock", &id[..16]);
    let p = std::env::temp_dir().join(&name);
    if p.as_os_str().len() < 100 {
        p
    } else {
        PathBuf::from("/tmp").join(name)
    }
}

// ---- client ---------------------------------------------------------------

/// Connect to the workspace's daemon and authenticate.
fn connect(ws_dir: &Path) -> Option<IpcStream> {
    let info: DaemonInfo = crate::json::from_slice(&fs::read(info_path(ws_dir)).ok()?).ok()?;
    let mut s = platform::ipc_connect(&info.address).ok()?;
    s.set_read_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    s.write_all(format!("{}\n", info.key).as_bytes()).ok()?;
    Some(s)
}

fn call<T: crate::json::FromJson>(ws_dir: &Path, req: &Request) -> Option<T> {
    let mut s = connect(ws_dir)?;
    let mut line = crate::json::to_vec(req).ok()?;
    line.push(b'\n');
    s.write_all(&line).ok()?;
    let mut buf = String::new();
    BufReader::new(s).read_line(&mut buf).ok()?;
    crate::json::from_str(&buf).ok()
}

/// Paths changed since `token`, or `None` if no daemon is running.
pub fn since(ws_dir: &Path, token: Option<&WatchToken>) -> Option<Answer> {
    begin(ws_dir)?.finish(token)
}

struct TokenLine {
    token: Option<WatchToken>,
}
crate::json_struct!(TokenLine { token });

/// A query in flight: the daemon is syncing with the file system while the
/// caller does other work.
pub struct Pending {
    stream: IpcStream,
}

/// Start a query, or `None` if no daemon is running.
pub fn begin(ws_dir: &Path) -> Option<Pending> {
    let mut stream = connect(ws_dir)?;
    let mut line = crate::json::to_vec(&Request::Since).ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    Some(Pending { stream })
}

impl Pending {
    /// Paths changed since `token`.
    pub fn finish(mut self, token: Option<&WatchToken>) -> Option<Answer> {
        let mut line = crate::json::to_vec(&TokenLine { token: token.cloned() }).ok()?;
        line.push(b'\n');
        self.stream.write_all(&line).ok()?;
        let mut buf = String::new();
        BufReader::new(self.stream).read_line(&mut buf).ok()?;
        crate::json::from_str(&buf).ok()
    }
}

pub fn status(ws_dir: &Path) -> Option<DaemonStatus> {
    call(ws_dir, &Request::Status)
}

pub fn is_running(ws_dir: &Path) -> bool {
    status(ws_dir).is_some()
}

/// Ask the daemon to exit and wait (briefly) until it has. Returns whether
/// one was running.
pub fn stop(ws_dir: &Path) -> bool {
    if call::<crate::json::Value>(ws_dir, &Request::Stop).is_none() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while info_path(ws_dir).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Scan a workspace, using its daemon (if one is running) to look only at
/// changed paths. Falls back to a full scan whenever the daemon cannot vouch
/// for the index.
pub fn scan(root: &Path, store: &Store, ws_dir: &Path) -> Result<ScanResult> {
    let t = Instant::now();
    let pending = begin(ws_dir);
    let old = Index::load(&ws_dir.join("index"));
    // ws_dir is <repo>/.psync/workspaces/<name>.
    let eol = match ws_dir.parent().and_then(Path::parent) {
        Some(dot) => crate::eol::configured(dot)?,
        None => None,
    };
    let t_load = t.elapsed();
    let answer = pending.and_then(|p| p.finish(old.watch.as_ref()));
    if std::env::var_os("PSYNC_TRACE").is_some() {
        eprintln!(
            "trace index load {t_load:?}, watcher answer after {:?} ({:?} paths)",
            t.elapsed(),
            answer.as_ref().map(|a| a.paths.len())
        );
    }
    let Some(answer) = answer else {
        let mut r = wc::scan(root, store, &old, eol)?;
        if r.index.watch.take().is_some() {
            r.index_changed = true;
        }
        return Ok(r);
    };
    let usable = !answer.full && old.watch.is_some();
    let incremental = if usable { wc::scan_changed(root, store, &old, &answer.paths, eol)? } else { None };
    let mut r = match incremental {
        Some(r) => r,
        None => wc::scan(root, store, &old, eol)?,
    };
    if !usable || !r.incremental || answer.paths.len() > SAVE_AFTER_PATHS {
        r.index_changed = true;
    }
    r.index.watch = Some(answer.token);
    Ok(r)
}

// ---- daemon ---------------------------------------------------------------

/// Called when edits have settled. Returns `Ok(false)` if the workspace was
/// busy and the snapshot should be retried later.
pub type OnSettled = Box<dyn FnMut() -> Result<bool> + Send>;

pub struct DaemonOptions {
    /// Workspace root.
    pub root: PathBuf,
    /// The workspace's metadata directory (`.psync/workspaces/<name>`).
    pub ws_dir: PathBuf,
    pub on_settled: Option<OnSettled>,
    /// Watch directories one by one; normally [`PER_DIRECTORY`].
    pub per_directory: bool,
}

#[derive(Default)]
struct State {
    seq: u64,
    /// Tokens older than this cannot be served (events were dropped).
    lost_before: u64,
    paths: HashMap<String, u64>,
    /// Cookies written and not yet seen, and those seen.
    cookies_pending: HashSet<u64>,
    cookies_seen: HashSet<u64>,
    next_cookie: u64,
    events: u64,
    /// Changes not yet snapshotted, and when the last one arrived.
    unsettled: Option<Instant>,
    snapshots_taken: u64,
    last_error: Option<String>,
    ignored: u64,
    /// A directory could not be watched: every answer says `full`.
    incomplete: bool,
    stop: bool,
}

impl State {
    fn record(&mut self, rel: &str) {
        self.seq += 1;
        self.paths.insert(rel.to_string(), self.seq);
        if self.paths.len() > MAX_PATHS {
            self.lose();
        }
        self.unsettled = Some(Instant::now());
    }

    /// Forget individual paths: every older token now needs a full scan.
    fn lose(&mut self) {
        self.seq += 1;
        self.lost_before = self.seq;
        self.paths.clear();
        self.unsettled = Some(Instant::now());
    }
}

type Shared = Arc<(Mutex<State>, Condvar)>;

fn split(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

/// Which paths a scan looks at, by the same rules as its walk (ignore
/// files, nested workspaces, `.psync`, `.git`), with what each directory
/// decided remembered. And, when watching directory by directory, which are
/// watched.
struct Filter {
    root: PathBuf,
    /// Directory -> the ignore rules inside it, or `None` if a scan does not
    /// enter it.
    dirs: BTreeMap<String, Option<Ignores>>,
    watched: HashSet<String>,
}

impl Filter {
    fn new(root: PathBuf) -> Filter {
        Filter { root, dirs: BTreeMap::new(), watched: HashSet::new() }
    }

    /// The rules inside directory `rel`, or `None` if a scan skips it.
    fn dir(&mut self, rel: &str) -> Option<Ignores> {
        if let Some(d) = self.dirs.get(rel) {
            return d.clone();
        }
        let abs = self.root.join(rel);
        let outer = if rel.is_empty() {
            Some(Ignores::root())
        } else {
            let (parent, name) = split(rel);
            self.dir(parent).filter(|ig| {
                !matches!(name, ".psync" | ".git") && !ig.ignored(rel, name, true) && !abs.join(".psync").exists()
            })
        };
        let inside = outer.map(|ig| {
            let present: Vec<&str> = FILE_NAMES.into_iter().filter(|f| abs.join(f).exists()).collect();
            if present.is_empty() {
                ig
            } else {
                ig.enter(rel, &abs, &present)
            }
        });
        self.dirs.insert(rel.to_string(), inside.clone());
        inside
    }

    /// Forget what was decided at and below directory `rel`.
    fn forget(&mut self, rel: &str) {
        if rel.is_empty() {
            self.dirs.clear();
            return;
        }
        self.dirs.remove(rel);
        let below = format!("{rel}/");
        let doomed: Vec<String> =
            self.dirs.range::<str, _>((Bound::Included(below.as_str()), Bound::Unbounded)).map(|(k, _)| k).take_while(|k| k.starts_with(&below)).cloned().collect();
        for k in doomed {
            self.dirs.remove(&k);
        }
    }

    /// Whether an event at `rel` can matter to a scan; and whether `rel` is
    /// now a directory a scan enters.
    fn wanted(&mut self, rel: &str) -> (bool, bool) {
        let (parent, name) = split(rel);
        let Some(ig) = self.dir(parent) else { return (false, false) };
        if FILE_NAMES.contains(&name) || name == ".psync" {
            // Rules (or nested workspaces) changed: decide everything again.
            self.dirs.clear();
            return (true, false);
        }
        match fs::symlink_metadata(self.root.join(rel)) {
            Ok(md) if md.is_dir() => {
                // Maybe a new directory moved in, ignore files and all.
                self.forget(rel);
                let entered = self.dir(rel).is_some();
                (entered, entered)
            }
            Ok(_) => (!ig.ignored(rel, name, false), false),
            // Gone: a scan needs to hear of it.
            Err(_) => {
                self.forget(rel);
                (true, false)
            }
        }
    }

    /// The directories at and below `from` that a scan enters and that are
    /// not watched yet.
    fn unwatched(&mut self, from: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![from.to_string()];
        while let Some(d) = stack.pop() {
            if self.dir(&d).is_none() {
                continue;
            }
            if !self.watched.contains(&d) {
                out.push(d.clone());
            }
            let Ok(rd) = fs::read_dir(self.root.join(&d)) else { continue };
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    if let Some(n) = e.file_name().to_str() {
                        stack.push(if d.is_empty() { n.to_string() } else { format!("{d}/{n}") });
                    }
                }
            }
        }
        out
    }
}

/// Work for the thread that adds and removes per-directory watches (the
/// event callback cannot: the watcher may be waiting for it).
enum WatchJob {
    /// Watch this directory and the unwatched ones below it.
    Tree(String),
    /// Rules changed: watch what is now entered, stop watching the rest.
    All,
}

type SharedWatcher = Arc<Mutex<Option<notify::RecommendedWatcher>>>;

/// Watch the directories at and below `from` not watched yet. Each is
/// recorded as changed once watched, so a later scan looks at what changed
/// before its watch existed.
fn watch_tree(filter: &Mutex<Filter>, watcher: &SharedWatcher, shared: &Shared, from: &str) {
    let dirs = filter.lock().unwrap().unwatched(from);
    let root = filter.lock().unwrap().root.clone();
    let mut added = Vec::new();
    let mut failed = None;
    {
        let mut w = watcher.lock().unwrap();
        let Some(w) = w.as_mut() else { return };
        for d in dirs {
            match w.watch(&root.join(&d), RecursiveMode::NonRecursive) {
                Ok(()) => added.push(d),
                // Removed meanwhile: nothing to watch.
                Err(_) if !root.join(&d).is_dir() => {}
                Err(e) => failed = Some(format!("cannot watch {}: {e} (on Linux, raise fs.inotify.max_user_watches)", root.join(&d).display())),
            }
        }
    }
    filter.lock().unwrap().watched.extend(added.iter().cloned());
    let (m, cv) = &**shared;
    let mut st = m.lock().unwrap();
    if !from.is_empty() {
        for d in &added {
            st.record(d);
        }
    }
    if let Some(e) = failed {
        st.incomplete = true;
        st.last_error = Some(e);
    }
    cv.notify_all();
}

/// After a rule change: stop watching directories a scan no longer enters
/// (or that are gone), then watch the ones it now enters.
fn rewatch(filter: &Mutex<Filter>, watcher: &SharedWatcher, shared: &Shared) {
    let (root, dropped) = {
        let mut f = filter.lock().unwrap();
        let watched: Vec<String> = f.watched.iter().cloned().collect();
        let dropped: Vec<String> = watched.into_iter().filter(|d| f.dir(d).is_none() || !f.root.join(d).is_dir()).collect();
        for d in &dropped {
            f.watched.remove(d);
        }
        (f.root.clone(), dropped)
    };
    if let Some(w) = watcher.lock().unwrap().as_mut() {
        for d in dropped {
            let _ = w.unwatch(&root.join(d));
        }
    }
    watch_tree(filter, watcher, shared, "");
}

/// Run the daemon in the foreground until stopped or the workspace is gone.
pub fn run(opts: DaemonOptions) -> Result<()> {
    let DaemonOptions { root, ws_dir, mut on_settled, per_directory } = opts;
    let root = platform::canonicalize(&root).with_context(|| format!("no such directory: {}", root.display()))?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(ws_dir.join("daemon.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("a watcher is already running for this workspace"),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    let cookies = ws_dir.join("cookies");
    let _ = fs::remove_dir_all(&cookies);
    fs::create_dir_all(&cookies)?;
    let cookies = platform::canonicalize(&cookies)?;
    let instance = crate::util::random_hex(16);
    let shared: Shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
    let filter = Arc::new(Mutex::new(Filter::new(root.clone())));
    let (jobs, job_queue) = std::sync::mpsc::channel::<WatchJob>();

    let watcher: SharedWatcher = Arc::new(Mutex::new(None));
    *watcher.lock().unwrap() = Some({
        let (shared, root, cookies, filter) = (shared.clone(), root.clone(), cookies.clone(), filter.clone());
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let (m, cv) = &*shared;
            let mut st = m.lock().unwrap();
            st.events += 1;
            match res {
                Err(_) => st.lose(),
                // Reading a file changes nothing (inotify reports opens and
                // reads; recording them would make every scan, which reads
                // ignore files and hashes files, look like a change).
                Ok(ev) if matches!(ev.kind, EventKind::Access(a) if a != AccessKind::Close(AccessMode::Write)) => {}
                Ok(ev) => {
                    if ev.need_rescan() {
                        st.lose();
                    }
                    for p in &ev.paths {
                        if let Ok(c) = p.strip_prefix(&cookies) {
                            if let Some(n) = c.to_str().and_then(|s| s.parse().ok()) {
                                if st.cookies_pending.remove(&n) {
                                    st.cookies_seen.insert(n);
                                }
                            }
                            continue;
                        }
                        let Ok(rel) = p.strip_prefix(&root) else { continue };
                        let r = match rel.to_str().map(platform::slash_path) {
                            Some(r) if r == ".psync" || r.starts_with(".psync/") => continue,
                            Some(r) => r,
                            None => {
                                st.lose();
                                continue;
                            }
                        };
                        if r.is_empty() {
                            st.record(&r);
                            continue;
                        }
                        let mut f = filter.lock().unwrap();
                        let rules_changed = matches!(split(&r).1, ".gitignore" | ".psyncignore" | ".psync");
                        let (wanted, entered) = f.wanted(&r);
                        if per_directory {
                            if rules_changed {
                                let _ = jobs.send(WatchJob::All);
                            } else if entered {
                                let _ = jobs.send(WatchJob::Tree(r.clone()));
                            } else if f.watched.contains(&r) && !root.join(&r).is_dir() {
                                // Gone (the kernel dropped its watches).
                                let below = format!("{r}/");
                                f.watched.retain(|d| d != &r && !d.starts_with(&below));
                            }
                        }
                        drop(f);
                        if wanted {
                            st.record(&r);
                        } else {
                            st.ignored += 1;
                        }
                    }
                }
            }
            cv.notify_all();
        })?
    });
    if per_directory {
        watch_tree(&filter, &watcher, &shared, "");
        watcher.lock().unwrap().as_mut().expect("just made").watch(&cookies, RecursiveMode::NonRecursive)?;
    } else {
        watcher.lock().unwrap().as_mut().expect("just made").watch(&root, RecursiveMode::Recursive)?;
        if !cookies.starts_with(&root) {
            watcher.lock().unwrap().as_mut().expect("just made").watch(&cookies, RecursiveMode::NonRecursive)?;
        }
    }
    let watch_thread = {
        let (filter, watcher, shared) = (filter.clone(), watcher.clone(), shared.clone());
        std::thread::spawn(move || {
            while let Ok(job) = job_queue.recv() {
                match job {
                    WatchJob::Tree(d) => watch_tree(&filter, &watcher, &shared, &d),
                    WatchJob::All => rewatch(&filter, &watcher, &shared),
                }
            }
        })
    };

    let socket = socket_path(&ws_dir);
    let (listener, address) = IpcListener::bind(&socket).with_context(|| format!("listening at {}", socket.display()))?;
    let info = DaemonInfo {
        pid: std::process::id(),
        address: address.clone(),
        key: crate::util::random_hex(16),
        instance: instance.clone(),
        root: root.clone(),
        started_ms: now_ms(),
        snapshots: on_settled.is_some(),
    };
    let tmp_info = ws_dir.join(format!("daemon.json.{}", std::process::id()));
    fs::write(&tmp_info, crate::json::to_vec_pretty(&info)?)?;
    platform::restrict_to_owner(&tmp_info)?;
    fs::rename(&tmp_info, info_path(&ws_dir))?;

    let accept = {
        let (shared, info, cookies, filter) = (shared.clone(), info.clone(), cookies.clone(), filter.clone());
        std::thread::spawn(move || {
            loop {
                let stream = listener.accept();
                if shared.0.lock().unwrap().stop {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (shared, info, cookies, filter) = (shared.clone(), info.clone(), cookies.clone(), filter.clone());
                std::thread::spawn(move || {
                    let directories = || per_directory.then(|| filter.lock().unwrap().watched.len());
                    let _ = serve(stream, &shared, &info, &cookies, &directories);
                });
            }
        })
    };

    // Main loop: snapshot when edits settle; exit when stopped or orphaned.
    let (m, cv) = &*shared;
    loop {
        let settled = {
            let mut st = m.lock().unwrap();
            let wait = match st.unsettled {
                Some(t) if on_settled.is_some() => SETTLE.saturating_sub(t.elapsed()).max(Duration::from_millis(1)),
                _ => Duration::from_secs(1),
            };
            st = cv.wait_timeout(st, wait).unwrap().0;
            if st.stop {
                break;
            }
            match st.unsettled {
                Some(t) if on_settled.is_some() && t.elapsed() >= SETTLE => {
                    st.unsettled = None;
                    true
                }
                _ => false,
            }
        };
        if !ws_dir.is_dir() || !root.is_dir() {
            break;
        }
        if settled {
            let f = on_settled.as_mut().expect("checked");
            let outcome = f();
            let mut st = m.lock().unwrap();
            match outcome {
                Ok(true) => st.snapshots_taken += 1,
                Ok(false) => {
                    // Busy: retry after the next quiet period.
                    st.unsettled.get_or_insert_with(Instant::now);
                }
                Err(e) => st.last_error = Some(format!("{e:#}")),
            }
        }
    }

    // Shut down: wake the accept loop so its thread exits, then clean up.
    m.lock().unwrap().stop = true;
    let _ = platform::ipc_connect(&address);
    let _ = accept.join();
    // Dropping the watcher drops the event callback and with it the job
    // sender, which ends the watch thread.
    let w = watcher.lock().unwrap().take();
    drop(w);
    let _ = watch_thread.join();
    let ours = fs::read(info_path(&ws_dir))
        .ok()
        .and_then(|b| crate::json::from_slice::<DaemonInfo>(&b).ok())
        .is_some_and(|i| i.instance == instance);
    if ours {
        #[cfg(unix)]
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_file(info_path(&ws_dir));
    }
    let _ = fs::remove_dir_all(&cookies);
    Ok(())
}

fn serve(stream: IpcStream, shared: &Shared, info: &DaemonInfo, cookies: &Path, directories: &dyn Fn() -> Option<usize>) -> Result<()> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end() != info.key {
        bail!("client did not authenticate");
    }
    line.clear();
    reader.read_line(&mut line)?;
    let req: Request = crate::json::from_str(&line)?;
    let (m, cv) = &**shared;
    let reply = match req {
        Request::Since => {
            let synced = sync(shared, cookies)?;
            line.clear();
            reader.read_line(&mut line)?;
            let TokenLine { token } = crate::json::from_str(&line)?;
            crate::json::to_value(answer(shared, info, synced, token))?
        }
        Request::Status => {
            let st = m.lock().unwrap();
            crate::json::to_value(DaemonStatus {
                info: info.clone(),
                events: st.events,
                paths: st.paths.len(),
                snapshots_taken: st.snapshots_taken,
                last_error: st.last_error.clone(),
                ignored: st.ignored,
                directories: directories(),
            })?
        }
        Request::Stop => {
            m.lock().unwrap().stop = true;
            cv.notify_all();
            json!({ "stopping": true })
        }
    };
    let mut out = crate::json::to_vec(&reply)?;
    out.push(b'\n');
    (&stream).write_all(&out)?;
    Ok(())
}

/// Wait until every file-system change made before now has been recorded:
/// write a cookie file and wait for its event. False on timeout.
fn sync(shared: &Shared, cookies: &Path) -> Result<bool> {
    let (m, cv) = &**shared;
    let n = {
        let mut st = m.lock().unwrap();
        st.next_cookie += 1;
        let n = st.next_cookie;
        st.cookies_pending.insert(n);
        n
    };
    // Sync: once the cookie's event is seen, every earlier change is recorded.
    let cookie = cookies.join(n.to_string());
    fs::write(&cookie, b"")?;
    let deadline = Instant::now() + COOKIE_TIMEOUT;
    let mut st = m.lock().unwrap();
    let synced = loop {
        if st.cookies_seen.remove(&n) {
            break true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break false;
        }
        st = cv.wait_timeout(st, left).unwrap().0;
    };
    st.cookies_pending.remove(&n);
    drop(st);
    let _ = fs::remove_file(&cookie);
    Ok(synced)
}

fn answer(shared: &Shared, info: &DaemonInfo, synced: bool, token: Option<WatchToken>) -> Answer {
    let st = shared.0.lock().unwrap();
    let now = WatchToken { instance: info.instance.clone(), seq: st.seq };
    let servable = token.filter(|t| synced && !st.incomplete && t.instance == info.instance && t.seq >= st.lost_before);
    let answer = match servable {
        Some(t) => Answer {
            token: now,
            full: false,
            paths: st.paths.iter().filter(|(_, s)| **s > t.seq).map(|(p, _)| p.clone()).collect(),
        },
        None => Answer { token: now, full: true, paths: vec![] },
    };
    answer
}
