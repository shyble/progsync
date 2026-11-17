//! Working copy: fast scanning into a tree, and checkout of a tree.
//!
//! Scanning is incremental: a stat cache (the index) lets unchanged files be
//! reused without reading them, and changed files are hashed in parallel.
//! With a file watcher (see `watch`), only the reported paths are looked at
//! (`scan_changed`) instead of walking the whole directory.

use crate::diff::{self, FileChange};
use crate::eol::{Ending, Eol};
use crate::hash::Hash;
use crate::object::{EntryKind, Object, Tree, TreeEntry};
use crate::store::Store;
use crate::util::now_ns;
use crate::bail;
use crate::error::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::fs;
use std::io::ErrorKind;
use crate::ignores::{self, Ignores};
use crate::platform;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Files modified this close to a scan are re-hashed next time, since their
/// mtime cannot prove they did not change again within the timestamp
/// granularity ("racy" entries, as in git).
const RACY_NS: i64 = 2_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub kind: EntryKind,
    pub hash: Hash,
    pub size: u64,
    pub mtime_ns: i64,
    pub ino: u64,
}

/// Stat cache. Purely an optimization: losing it only costs a full re-hash.
#[derive(Default, Debug)]
pub struct Index {
    pub scanned_at_ns: i64,
    pub entries: BTreeMap<String, IndexEntry>,
    /// Directory path ("" for the root) -> tree hash, so unchanged
    /// directories are not re-encoded on the next scan.
    pub trees: BTreeMap<String, Hash>,
    /// Position in the file watcher's event stream this index is current
    /// as of; the next scan only needs to look at paths changed since.
    pub watch: Option<WatchToken>,
}

/// A file watcher's position: daemon instance + event sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchToken {
    pub instance: String,
    pub seq: u64,
}
crate::json_struct!(WatchToken { instance, seq });

impl Index {
    pub fn load(path: &Path) -> Index {
        fs::read(path).ok().and_then(|b| Index::decode(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::util::replace_file(path, &self.encode())
    }

    /// Encoding ([`crate::bin`]): scan time; entries as (path, kind, hash,
    /// size, mtime, inode); trees as (directory, hash); watcher position.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = crate::bin::Writer::default();
        w.i64(self.scanned_at_ns);
        w.len(self.entries.len());
        for (path, e) in &self.entries {
            w.str(path);
            e.kind.write(&mut w);
            w.hash(&e.hash);
            w.u64(e.size);
            w.i64(e.mtime_ns);
            w.u64(e.ino);
        }
        w.len(self.trees.len());
        for (dir, h) in &self.trees {
            w.str(dir);
            w.hash(h);
        }
        w.bool(self.watch.is_some());
        if let Some(t) = &self.watch {
            w.str(&t.instance);
            w.u64(t.seq);
        }
        w.buf
    }

    pub fn decode(data: &[u8]) -> Result<Index> {
        let mut r = crate::bin::Reader::new(data);
        let scanned_at_ns = r.i64()?;
        let mut entries = BTreeMap::new();
        for _ in 0..r.len(37)? {
            let path = r.str()?;
            let e = IndexEntry {
                kind: EntryKind::read(&mut r)?,
                hash: r.hash()?,
                size: r.u64()?,
                mtime_ns: r.i64()?,
                ino: r.u64()?,
            };
            entries.insert(path, e);
        }
        let mut trees = BTreeMap::new();
        for _ in 0..r.len(33)? {
            let dir = r.str()?;
            trees.insert(dir, r.hash()?);
        }
        let watch = if r.bool()? { Some(WatchToken { instance: r.str()?, seq: r.u64()? }) } else { None };
        r.finish()?;
        Ok(Index { scanned_at_ns, entries, trees, watch })
    }
}

pub struct ScanResult {
    pub tree: Hash,
    pub index: Index,
    /// Files read and hashed in this scan.
    pub hashed: usize,
    /// Files reused from the stat cache.
    pub cached: usize,
    /// Whether the index differs from the one passed in (and needs saving).
    pub index_changed: bool,
    /// Only watcher-reported paths were examined (no full directory walk).
    pub incremental: bool,
}

impl ScanResult {
    /// Save the index only if it changed.
    pub fn save_index(&self, path: &Path) -> Result<()> {
        if self.index_changed {
            self.index.save(path)?;
        }
        Ok(())
    }
}

struct Found {
    rel: String,
    path: PathBuf,
    kind: EntryKind,
    size: u64,
    mtime_ns: i64,
    ino: u64,
}

fn mtime_ns(md: &fs::Metadata) -> i64 {
    platform::mtime_ns(md)
}


/// Walk the working directory (honoring ignore files, see [`crate::ignores`]), store
/// new contents, and return the resulting root tree. `eol` is the `text.eol`
/// setting (see [`crate::eol`]).
pub fn scan(root: &Path, store: &Store, old: &Index, eol: Option<Ending>) -> Result<ScanResult> {
    let started = now_ns();
    let t = std::time::Instant::now();
    let found = walk(root)?;
    let t_walk = t.elapsed();
    let observed = hash_found(store, old, found, &Eol::from_disk(eol, root))?;
    let mut entries = BTreeMap::new();
    let mut dirty: HashSet<String> = HashSet::new();
    let (mut hashed, mut cached) = (0, 0);
    for (rel, entry, was_cached) in observed {
        if was_cached {
            cached += 1;
        } else {
            hashed += 1;
            mark_dirty(&mut dirty, &rel);
        }
        entries.insert(rel, entry);
    }
    let mut removed = 0;
    for rel in old.entries.keys().filter(|k| !entries.contains_key(*k)) {
        removed += 1;
        mark_dirty(&mut dirty, rel);
    }
    finish(store, old, started, entries, dirty, (hashed, cached, removed), false, t, t_walk)
}

/// Like `scan`, but only looks at `changed` paths (relative, as reported by a
/// file watcher since `old` was made) instead of walking everything. Returns
/// `None` when a full scan is needed instead (e.g. an ignore file changed).
///
/// A changed path that is a known file is re-stat'd directly. A known
/// directory is rescanned recursively (it may have been replaced by a
/// rename). Anything else (new, ignored, or under an ignored directory)
/// causes its nearest known directory to be re-listed, which finds new files
/// and directories while applying ignore rules exactly as a full walk would.
pub fn scan_changed(root: &Path, store: &Store, old: &Index, changed: &[String], eol: Option<Ending>) -> Result<Option<ScanResult>> {
    if !old.trees.contains_key("") {
        return Ok(None);
    }
    let started = now_ns();
    let t = std::time::Instant::now();
    let mut relist: HashSet<String> = HashSet::new();
    let mut subtree: HashSet<String> = HashSet::new();
    let mut direct: Vec<Found> = Vec::new();
    let mut gone: Vec<&str> = Vec::new();
    for p in changed {
        // Ignore rules or nested workspaces changed, or an odd path: rescan all.
        let name = p.rsplit('/').next().unwrap_or("");
        let needs_full = matches!(name, ".gitignore" | ".psyncignore" | ".psync")
            || p.split('/').any(|c| c.is_empty() || c == "." || c == "..");
        if needs_full && !p.is_empty() {
            return Ok(None);
        }
        if p.is_empty() {
            relist.insert(String::new());
        } else if old.entries.contains_key(p) {
            let path = root.join(p);
            match fs::symlink_metadata(&path) {
                Ok(md) if md.is_file() || md.file_type().is_symlink() => direct.push(found(p.clone(), path, &md)),
                Ok(_) => {
                    // Became a directory (or something untrackable).
                    relist.insert(parent(p).to_string());
                    subtree.insert(p.clone());
                }
                Err(e) if e.kind() == ErrorKind::NotFound => gone.push(p),
                Err(e) => return Err(crate::error::Error::from(e).context(format!("reading {p}"))),
            }
        } else if old.trees.contains_key(p.as_str()) {
            subtree.insert(p.clone());
        } else {
            let mut a = parent(p);
            while !old.trees.contains_key(a) {
                a = parent(a);
            }
            relist.insert(a.to_string());
        }
    }

    let regions = std::sync::Arc::new(Regions { relist, subtree, known: old.trees.keys().cloned().collect() });
    let mut found = Vec::new();
    if !regions.relist.is_empty() || !regions.subtree.is_empty() {
        found = walk_regions(root, regions.clone())?;
    }
    let t_walk = t.elapsed();
    // Directly stat'd files that a walked region also covers are already in `found`.
    let walked: HashSet<&str> = found.iter().map(|f| f.rel.as_str()).collect();
    let direct: Vec<Found> =
        direct.into_iter().filter(|f| !regions.covers_file(&f.rel) && !walked.contains(f.rel.as_str())).collect();
    drop(walked);
    found.extend(direct);

    // Old entries in the examined regions are replaced by what was found.
    let mut stale: Vec<String> = gone.iter().map(|s| s.to_string()).collect();
    for d in &regions.relist {
        stale.extend(direct_children(&old.entries, d).into_iter().map(|(name, _)| join(d, name)));
    }
    for d in &regions.subtree {
        let prefix = format!("{d}/");
        stale.extend(
            old.entries
                .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
                .take_while(|(k, _)| k.starts_with(&prefix))
                .map(|(k, _)| k.clone()),
        );
    }

    let observed = hash_found(store, old, found, &Eol::from_disk(eol, root))?;
    let mut entries = old.entries.clone();
    let mut dirty: HashSet<String> = HashSet::new();
    for k in &stale {
        entries.remove(k);
    }
    let (mut hashed, mut cached) = (0, 0);
    let mut seen: HashSet<String> = HashSet::new();
    for (rel, entry, was_cached) in observed {
        if was_cached {
            cached += 1;
        } else {
            hashed += 1;
            mark_dirty(&mut dirty, &rel);
        }
        seen.insert(rel.clone());
        entries.insert(rel, entry);
    }
    let mut removed = 0;
    for k in stale.iter().filter(|k| !seen.contains(*k)) {
        if old.entries.contains_key(k) && !entries.contains_key(k) {
            removed += 1;
            mark_dirty(&mut dirty, k);
        }
    }
    finish(store, old, started, entries, dirty, (hashed, cached, removed), true, t, t_walk).map(Some)
}

#[allow(clippy::too_many_arguments)]
fn finish(
    store: &Store,
    old: &Index,
    started: i64,
    entries: BTreeMap<String, IndexEntry>,
    dirty: HashSet<String>,
    (hashed, cached, removed): (usize, usize, usize),
    incremental: bool,
    t: std::time::Instant,
    t_walk: std::time::Duration,
) -> Result<ScanResult> {
    let t_hash = t.elapsed();
    let (tree, trees) = if old.trees.contains_key("") {
        rebuild_dirty(store, &entries, &dirty, &old.trees)?
    } else {
        let mut trees = BTreeMap::new();
        let tree = build_tree_cached(store, &entries, &dirty, &old.trees, &mut trees)?;
        (tree, trees)
    };
    let mut index = Index { scanned_at_ns: started, entries, trees, watch: old.watch.clone() };
    let index_changed = hashed > 0 || removed > 0 || index.trees != old.trees;
    if !index_changed {
        // Nothing changed: keep the old scan time so trust in cached entries is unchanged.
        index.scanned_at_ns = old.scanned_at_ns;
    }
    if std::env::var_os("PSYNC_TRACE").is_some() {
        eprintln!(
            "trace scan{}: walk+stat {:?}, hash {:?}, trees {:?} ({} files, {hashed} hashed)",
            if incremental { " (watched)" } else { "" },
            t_walk,
            t_hash - t_walk,
            t.elapsed() - t_hash,
            index.entries.len()
        );
    }
    Ok(ScanResult { tree, index, hashed, cached, index_changed, incremental })
}

/// Hash the found files that the stat cache cannot vouch for.
fn hash_found(store: &Store, old: &Index, found: Vec<Found>, eol: &Eol) -> Result<Vec<(String, IndexEntry, bool)>> {
    let one = |mut f: Found| -> Result<Option<(String, IndexEntry, bool)>> {
        // Without an executable bit (Windows), files keep the recorded kind.
        if !platform::HAS_EXEC_BIT && f.kind == EntryKind::File {
            if let Some(e) = old.entries.get(&f.rel) {
                if e.kind == EntryKind::Exec {
                    f.kind = EntryKind::Exec;
                }
            }
        }
        if let Some(e) = old.entries.get(&f.rel) {
            if e.kind == f.kind
                && e.size == f.size
                && e.mtime_ns == f.mtime_ns
                && e.ino == f.ino
                && f.mtime_ns + RACY_NS < old.scanned_at_ns
            {
                return Ok(Some((f.rel, e.clone(), true)));
            }
        }
        let gone = |e: std::io::Error| -> Result<Option<Hash>> {
            match e.kind() {
                ErrorKind::NotFound => Ok(None),
                _ => Err(crate::error::Error::from(e).context(format!("reading {}", f.rel))),
            }
        };
        let hash = match f.kind {
            EntryKind::Symlink => match platform::link_target_bytes(&f.path) {
                Ok(d) => Some(store.put_blob(&d)?),
                Err(e) => gone(e)?,
            },
            // Converted line endings: read whole (never a large file).
            _ if f.size <= crate::chunk::LARGE as u64 && eol.applies(&f.rel) => match fs::read(&f.path) {
                Ok(d) => Some(store.put_blob(eol.to_store(&f.rel, &d).as_deref().unwrap_or(&d))?),
                Err(e) => gone(e)?,
            },
            // Streamed: a large file is never held in memory whole.
            _ => match fs::File::open(&f.path) {
                Ok(mut file) => {
                    let (hash, whole) = store.put_reader(&mut file).map_err(|e| e.context(format!("reading {}", f.rel)))?;
                    // A large file stored whole by an older version keeps
                    // that hash while unchanged.
                    Some(match (whole, old.entries.get(&f.rel)) {
                        (Some(w), Some(e)) if e.hash == w => w,
                        _ => hash,
                    })
                }
                Err(e) => gone(e)?,
            },
        };
        let Some(hash) = hash else { return Ok(None) };
        let entry = IndexEntry { kind: f.kind, hash, size: f.size, mtime_ns: f.mtime_ns, ino: f.ino };
        Ok(Some((f.rel, entry, false)))
    };
    let results: Vec<Result<Option<_>>> =
        if found.len() > 64 { crate::util::par_map(found, one) } else { found.into_iter().map(one).collect() };
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        out.extend(r?);
    }
    Ok(out)
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map_or("", |(d, _)| d)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Keys of `map` directly inside directory `dir` (not deeper), as
/// `(name, value)`. Skips over subdirectories by seeking past them, so the
/// cost is proportional to the number of direct children.
fn direct_children<'a, V>(map: &'a BTreeMap<String, V>, dir: &str) -> Vec<(&'a str, &'a V)> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut out = Vec::new();
    let mut from = prefix.clone();
    loop {
        let mut next = None;
        for (k, v) in map.range::<str, _>((Bound::Included(from.as_str()), Bound::Unbounded)) {
            if !k.starts_with(&prefix) {
                break;
            }
            let rest = &k[prefix.len()..];
            match rest.find('/') {
                None if !rest.is_empty() => out.push((rest, v)),
                None => {}
                // Every key under "<prefix><sub>/" sorts before "<prefix><sub>0".
                Some(i) => {
                    next = Some(format!("{prefix}{}0", &rest[..i]));
                    break;
                }
            }
        }
        match next {
            Some(f) => from = f,
            None => return out,
        }
    }
}

/// Re-encode only the dirty directories (deepest first); every other
/// directory keeps its tree hash from `cache`. Requires that every existing
/// directory is either in `cache` or dirty, which holds when `cache` was
/// built from the previous entries.
fn rebuild_dirty(
    store: &Store,
    entries: &BTreeMap<String, IndexEntry>,
    dirty: &HashSet<String>,
    cache: &BTreeMap<String, Hash>,
) -> Result<(Hash, BTreeMap<String, Hash>)> {
    let mut trees = cache.clone();
    let mut order: Vec<&String> = dirty.iter().collect();
    order.sort_by_key(|d| std::cmp::Reverse(if d.is_empty() { 0 } else { d.matches('/').count() + 1 }));
    for d in order {
        let mut items: Vec<TreeEntry> = direct_children(entries, d)
            .into_iter()
            .map(|(name, e)| TreeEntry { name: name.to_string(), kind: e.kind, hash: e.hash })
            .collect();
        items.extend(
            direct_children(&trees, d)
                .into_iter()
                .map(|(name, h)| TreeEntry { name: name.to_string(), kind: EntryKind::Tree, hash: *h }),
        );
        if items.is_empty() && !d.is_empty() {
            trees.remove(d.as_str());
            continue;
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        let h = store.put(&Object::Tree(Tree { entries: items }))?;
        trees.insert(d.clone(), h);
    }
    Ok((trees[""], trees))
}

/// The parts of the working directory `scan_changed` re-examines.
struct Regions {
    /// Directories whose direct children are re-listed.
    relist: HashSet<String>,
    /// Directories rescanned recursively.
    subtree: HashSet<String>,
    /// Directories known from the previous scan.
    known: HashSet<String>,
}

impl Regions {
    /// Whether directory `d` lies in a fully re-examined area: inside a
    /// rescanned subtree, or a directory unknown before whose parent is
    /// re-listed (a new directory), or inside one.
    fn covers_dir(&self, d: &str) -> bool {
        let mut cur = d;
        loop {
            if self.subtree.contains(cur) {
                return true;
            }
            if cur.is_empty() {
                return false;
            }
            let p = parent(cur);
            if !self.known.contains(cur) && self.relist.contains(p) {
                return true;
            }
            cur = p;
        }
    }

    /// Whether the file at `rel` is re-examined (so its old entry is replaced).
    fn covers_file(&self, rel: &str) -> bool {
        let d = parent(rel);
        self.relist.contains(d) || self.covers_dir(d)
    }

    /// Whether the walk must enter directory `d`.
    fn enter(&self, d: &str) -> bool {
        self.relist.contains(d)
            || self.covers_dir(d)
            || self.relist.iter().chain(&self.subtree).any(|r| is_ancestor_dir(d, r))
    }
}

fn is_ancestor_dir(a: &str, d: &str) -> bool {
    d.len() > a.len() && d.starts_with(a) && d.as_bytes()[a.len()] == b'/'
}

fn found(rel: String, path: PathBuf, md: &fs::Metadata) -> Found {
    let kind = if md.file_type().is_symlink() {
        EntryKind::Symlink
    } else if platform::is_executable(md) {
        EntryKind::Exec
    } else {
        EntryKind::File
    };
    Found { rel, path, kind, size: md.len(), mtime_ns: mtime_ns(md), ino: platform::inode(md) }
}

fn mark_dirty(dirty: &mut HashSet<String>, rel: &str) {
    let mut cur = rel;
    while let Some((parent, _)) = cur.rsplit_once('/') {
        if !dirty.insert(parent.to_string()) {
            return;
        }
        cur = parent;
    }
    dirty.insert(String::new());
}

/// Why a path is or is not tracked, as [`check_ignore`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreVerdict {
    pub ignored: bool,
    /// The rule that decided (for an ignored directory above the path: that
    /// directory's rule). A `!` rule here means the path is re-included.
    pub rule: Option<crate::ignores::RuleMatch>,
    /// An ignored directory containing the path (its files are never read).
    pub inside: Option<String>,
    /// Skipped for another reason: `.psync`/`.git`, or a nested workspace.
    pub reason: Option<String>,
}

/// Whether `rel` (relative to the root; it need not exist) is ignored, and
/// which rule decides it, applying ignore files exactly as a scan does.
pub fn check_ignore(root: &Path, rel: &str) -> Result<IgnoreVerdict> {
    let rel = rel.trim_start_matches("./");
    let dir_hint = rel.ends_with('/');
    let rel = rel.trim_matches('/');
    let mut v = IgnoreVerdict { ignored: false, rule: None, inside: None, reason: None };
    if rel.is_empty() {
        return Ok(v);
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let mut ignores = Ignores::root();
    let mut abs = root.to_path_buf();
    for (i, name) in parts.iter().enumerate() {
        let here = parts[..i].join("/");
        let present: Vec<&str> = ignores::FILE_NAMES.into_iter().filter(|f| abs.join(f).is_file()).collect();
        if !present.is_empty() {
            ignores = ignores.enter(&here, &abs, &present);
        }
        let cur = parts[..=i].join("/");
        let last = i + 1 == parts.len();
        abs = abs.join(name);
        if *name == ".psync" || *name == ".git" {
            v.ignored = true;
            v.reason = Some(format!("{name} is never tracked"));
            return Ok(v);
        }
        let is_dir = if last { abs.is_dir() || dir_hint } else { true };
        if is_dir && abs.join(".psync").exists() {
            v.ignored = true;
            v.reason = Some(format!("{cur} is a nested workspace or repository (it tracks itself)"));
            return Ok(v);
        }
        let rule = ignores.explain(&cur, name, is_dir);
        if rule.as_ref().is_some_and(|r| !r.negated) {
            v.ignored = true;
            v.rule = rule;
            if !last {
                v.inside = Some(cur);
            }
            return Ok(v);
        }
        if last {
            v.rule = rule;
        }
    }
    Ok(v)
}

/// An ignored file or directory, as `status --ignored` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredPath {
    pub path: String,
    pub is_dir: bool,
    /// Size in bytes (files only: an ignored directory is not entered).
    pub size: Option<u64>,
}

/// The ignored files and directories of the workspace (an ignored directory
/// is listed once and not entered), sorted by path.
pub fn ignored_paths(root: &Path) -> Result<Vec<IgnoredPath>> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), root.to_path_buf(), Ignores::root())];
    while let Some((rel, abs, ignores)) = stack.pop() {
        let mut entries = Vec::new();
        for e in fs::read_dir(&abs).with_context(|| format!("listing {}", abs.display()))? {
            let e = e?;
            let Some(name) = e.file_name().to_str().map(String::from) else { continue };
            if name != ".psync" && name != ".git" {
                entries.push((name, e));
            }
        }
        let present: Vec<&str> = ignores::FILE_NAMES.into_iter().filter(|f| entries.iter().any(|(n, _)| n == f)).collect();
        let ignores = if present.is_empty() { ignores } else { ignores.enter(&rel, &abs, &present) };
        for (name, e) in entries {
            let Ok(ft) = e.file_type() else { continue };
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if ft.is_dir() {
                if e.path().join(".psync").exists() {
                    continue;
                }
                if ignores.ignored(&path, &name, true) {
                    out.push(IgnoredPath { path, is_dir: true, size: None });
                } else {
                    stack.push((path, e.path(), ignores.clone()));
                }
            } else if ignores.ignored(&path, &name, false) {
                let size = fs::symlink_metadata(e.path()).ok().map(|m| m.len());
                out.push(IgnoredPath { path, is_dir: false, size });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A directory waiting to be listed.
struct DirJob {
    rel: String,
    abs: PathBuf,
    ignores: Ignores,
}

/// List one directory: its tracked files go to `files`, the directories
/// to descend into are returned. Applies the ignore files found here.
fn list_dir(job: DirJob, enter: &(dyn Fn(&str) -> bool + Sync), files: &mut Vec<Found>) -> Result<Vec<DirJob>> {
    let rd = match fs::read_dir(&job.abs) {
        Ok(rd) => rd,
        // Removed while we walked: nothing to track there any more.
        Err(e) if e.kind() == ErrorKind::NotFound && !job.rel.is_empty() => return Ok(Vec::new()),
        Err(e) => return Err(crate::error::Error::from(e).context(format!("listing {}", job.abs.display()))),
    };
    let mut entries = Vec::new();
    for e in rd {
        let e = e.with_context(|| format!("listing {}", job.abs.display()))?;
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            bail!("non-UTF-8 path not supported: {}", e.path().display());
        };
        if name == ".psync" || name == ".git" {
            continue;
        }
        entries.push((name.to_string(), e));
    }
    let present: Vec<&str> = ignores::FILE_NAMES.into_iter().filter(|f| entries.iter().any(|(n, _)| n == f)).collect();
    let ignores = if present.is_empty() { job.ignores } else { job.ignores.enter(&job.rel, &job.abs, &present) };
    let mut dirs = Vec::new();
    for (name, e) in entries {
        let Ok(ft) = e.file_type() else { continue };
        let rel = if job.rel.is_empty() { name.clone() } else { format!("{}/{name}", job.rel) };
        if ft.is_dir() {
            let abs = e.path();
            // Nested workspaces and repositories track themselves.
            if ignores.ignored(&rel, &name, true) || abs.join(".psync").exists() || !enter(&rel) {
                continue;
            }
            dirs.push(DirJob { rel, abs, ignores: ignores.clone() });
        } else if (ft.is_file() || ft.is_symlink()) && !ignores.ignored(&rel, &name, false) {
            let path = e.path();
            match fs::symlink_metadata(&path) {
                Ok(md) => files.push(found(rel, path, &md)),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(crate::error::Error::from(e).context(format!("reading {rel}"))),
            }
        }
    }
    Ok(dirs)
}

/// Walk the workspace from the root, applying ignore rules, entering only
/// directories `enter` accepts. Parallel over directories unless
/// `sequential` (cheaper for a handful of directories). Order unspecified.
fn walk_with(root: &Path, enter: &(dyn Fn(&str) -> bool + Sync), sequential: bool) -> Result<Vec<Found>> {
    let first = DirJob { rel: String::new(), abs: root.to_path_buf(), ignores: Ignores::root() };
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get());
    if sequential || threads == 1 {
        let (mut out, mut stack) = (Vec::new(), vec![first]);
        while let Some(job) = stack.pop() {
            stack.extend(list_dir(job, enter, &mut out)?);
        }
        return Ok(out);
    }
    // A shared stack of directories; `pending` counts directories queued or
    // being listed, so workers know when the walk is complete.
    struct State {
        stack: Vec<DirJob>,
        pending: usize,
        error: Option<crate::error::Error>,
    }
    let state = Mutex::new(State { stack: vec![first], pending: 1, error: None });
    let wake = std::sync::Condvar::new();
    let found = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let job = {
                            let mut st = state.lock().unwrap();
                            loop {
                                if st.error.is_some() || st.pending == 0 {
                                    wake.notify_all();
                                    return out;
                                }
                                if let Some(j) = st.stack.pop() {
                                    break j;
                                }
                                st = wake.wait(st).unwrap();
                            }
                        };
                        let result = list_dir(job, enter, &mut out);
                        let mut st = state.lock().unwrap();
                        st.pending -= 1;
                        match result {
                            Ok(dirs) => {
                                st.pending += dirs.len();
                                st.stack.extend(dirs);
                            }
                            Err(e) => {
                                st.error.get_or_insert(e);
                            }
                        }
                        wake.notify_all();
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect::<Vec<Found>>()
    });
    if let Some(e) = state.into_inner().unwrap().error {
        return Err(e);
    }
    Ok(found)
}

/// Sequential walk restricted to `regions` (the walk still starts at the
/// root, so ignore files are applied exactly as in a full walk).
fn walk_regions(root: &Path, regions: std::sync::Arc<Regions>) -> Result<Vec<Found>> {
    let found = walk_with(root, &|d: &str| regions.enter(d), true)?;
    Ok(found.into_iter().filter(|f| regions.covers_file(&f.rel)).collect())
}

/// Parallel directory walk + stat. Order of results is unspecified.
fn walk(root: &Path) -> Result<Vec<Found>> {
    walk_with(root, &|_: &str| true, false)
}

enum Node {
    Leaf(EntryKind, Hash),
    Dir(BTreeMap<String, Node>),
}

fn build_tree_cached(
    store: &Store,
    entries: &BTreeMap<String, IndexEntry>,
    dirty: &HashSet<String>,
    cache: &BTreeMap<String, Hash>,
    out: &mut BTreeMap<String, Hash>,
) -> Result<Hash> {
    let mut root = BTreeMap::new();
    for (rel, e) in entries {
        insert(&mut root, rel, e.kind, e.hash);
    }
    write_dir_cached(store, &root, "", dirty, cache, out)
}

fn write_dir_cached(
    store: &Store,
    dir: &BTreeMap<String, Node>,
    path: &str,
    dirty: &HashSet<String>,
    cache: &BTreeMap<String, Hash>,
    out: &mut BTreeMap<String, Hash>,
) -> Result<Hash> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, node) in dir {
        let (kind, hash) = match node {
            Node::Leaf(k, h) => (*k, *h),
            Node::Dir(sub) => {
                let child = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
                (EntryKind::Tree, write_dir_cached(store, sub, &child, dirty, cache, out)?)
            }
        };
        entries.push(TreeEntry { name: name.clone(), kind, hash });
    }
    let h = match cache.get(path) {
        Some(h) if !dirty.contains(path) => *h,
        _ => store.put(&Object::Tree(Tree { entries }))?,
    };
    out.insert(path.to_string(), h);
    Ok(h)
}

/// Build (and store) a tree from `(path, kind, hash)` leaves.
pub fn build_tree_from<'a>(store: &Store, leaves: impl Iterator<Item = (&'a str, EntryKind, Hash)>) -> Result<Hash> {
    let mut root = BTreeMap::new();
    for (rel, kind, hash) in leaves {
        insert(&mut root, rel, kind, hash);
    }
    write_dir(store, &root)
}

/// `base` with the given file changes applied (their `new` side).
pub fn apply_changes(store: &Store, base: Hash, changes: &[FileChange]) -> Result<Hash> {
    let mut files: BTreeMap<String, diff::Leaf> = diff::flatten(store, base)?.into_iter().collect();
    for c in changes {
        match c.new {
            Some(leaf) => {
                files.insert(c.path.clone(), leaf);
            }
            None => {
                files.remove(&c.path);
            }
        }
    }
    build_tree_from(store, files.iter().map(|(p, (k, h))| (p.as_str(), *k, *h)))
}

fn insert(dir: &mut BTreeMap<String, Node>, path: &str, kind: EntryKind, hash: Hash) {
    match path.split_once('/') {
        None => {
            dir.insert(path.to_string(), Node::Leaf(kind, hash));
        }
        Some((first, rest)) => {
            let child = dir.entry(first.to_string()).or_insert_with(|| Node::Dir(BTreeMap::new()));
            if let Node::Dir(sub) = child {
                insert(sub, rest, kind, hash);
            }
        }
    }
}

fn write_dir(store: &Store, dir: &BTreeMap<String, Node>) -> Result<Hash> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, node) in dir {
        let (kind, hash) = match node {
            Node::Leaf(k, h) => (*k, *h),
            Node::Dir(sub) => (EntryKind::Tree, write_dir(store, sub)?),
        };
        entries.push(TreeEntry { name: name.clone(), kind, hash });
    }
    store.put(&Object::Tree(Tree { entries }))
}

#[derive(Debug, Default)]
pub struct CheckoutStats {
    pub written: usize,
    pub removed: usize,
}

/// The line-ending rules for checking out `tree`: its own attribute files.
fn eol_of_tree(store: &Store, tree: Hash, config: Option<Ending>) -> Result<Eol> {
    let mut texts = Vec::new();
    for name in crate::eol::FILE_NAMES {
        if let Some((EntryKind::File | EntryKind::Exec, h)) = diff::lookup(store, tree, name)? {
            if let Some(d) = store.get_small_file(&h)? {
                texts.push(String::from_utf8_lossy(&d).into_owned());
            }
        }
    }
    Ok(Eol::new(config, &texts.iter().map(String::as_str).collect::<Vec<_>>()))
}

/// Transform the working directory from tree `from` into tree `to`, touching
/// only the files that differ. Untracked/ignored files are left alone. `eol`
/// is the `text.eol` setting.
pub fn checkout(root: &Path, store: &Store, from: Hash, to: Hash, eol: Option<Ending>) -> Result<CheckoutStats> {
    let changes = diff::diff_trees(store, Some(from), Some(to))?;
    let eol = eol_of_tree(store, to, eol)?;
    let mut stats = CheckoutStats::default();

    // Remove deleted and replaced entries first, so file <-> directory
    // transitions work.
    for c in &changes {
        if c.old.is_none() {
            continue;
        }
        let p = root.join(&c.path);
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(crate::error::Error::from(e).context(format!("removing {}", c.path))),
        }
        if c.new.is_none() {
            stats.removed += 1;
            prune_empty_parents(root, &p);
        }
    }

    let writes: Vec<&FileChange> = changes.iter().filter(|c| c.new.is_some()).collect();
    stats.written = writes.len();
    crate::util::par_map(writes, |c| {
        let (kind, hash) = c.new.expect("filtered");
        write_entry(root, store, &c.path, kind, hash, &eol).with_context(|| format!("writing {}", c.path))
    })
    .into_iter()
    .collect::<Result<()>>()?;
    Ok(stats)
}

/// An existing workspace whose files may be cloned instead of written.
pub struct CloneSource<'a> {
    pub root: &'a Path,
    pub index: &'a Index,
}

pub struct Materialized {
    pub index: Index,
    pub files: usize,
    /// Files created as copy-on-write clones (APFS clonefile / reflink).
    pub cloned: usize,
}

/// Populate an empty directory with `tree`. Files whose content matches the
/// clone source (per its stat cache) are copied with `fs::copy`, which clones
/// copy-on-write on APFS/btrfs/XFS: no data is duplicated on disk.
pub fn materialize(root: &Path, store: &Store, tree: Hash, source: Option<CloneSource>, eol: Option<Ending>) -> Result<Materialized> {
    let started = now_ns();
    let eol = eol_of_tree(store, tree, eol)?;
    let files = diff::flatten(store, tree)?;
    let results: Vec<Result<(String, IndexEntry, bool)>> =
        crate::util::par_map(files.into_iter().collect(), |(rel, (kind, hash))| {
            let dst = root.join(&rel);
            let cloned = kind != EntryKind::Symlink
                && source.as_ref().is_some_and(|src| try_clone(src, &rel, kind, hash, &dst));
            if !cloned {
                write_entry(root, store, &rel, kind, hash, &eol).with_context(|| format!("writing {rel}"))?;
            }
            let md = fs::symlink_metadata(&dst)?;
            let entry = IndexEntry { kind, hash, size: md.len(), mtime_ns: mtime_ns(&md), ino: platform::inode(&md) };
            Ok((rel, entry, cloned))
        });
    let mut index = Index { scanned_at_ns: started, ..Default::default() };
    let mut cloned = 0;
    for r in results {
        let (rel, entry, was_cloned) = r?;
        cloned += was_cloned as usize;
        index.entries.insert(rel, entry);
    }
    Ok(Materialized { files: index.entries.len(), index, cloned })
}

fn try_clone(src: &CloneSource, rel: &str, kind: EntryKind, hash: Hash, dst: &Path) -> bool {
    let Some(e) = src.index.entries.get(rel) else { return false };
    if e.hash != hash || e.kind != kind {
        return false;
    }
    let sp = src.root.join(rel);
    let Ok(md) = fs::symlink_metadata(&sp) else { return false };
    if md.len() != e.size || mtime_ns(&md) != e.mtime_ns || platform::inode(&md) != e.ino {
        return false;
    }
    if let Some(parent) = dst.parent() {
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    fs::copy(&sp, dst).is_ok()
}

fn write_entry(root: &Path, store: &Store, rel: &str, kind: EntryKind, hash: Hash, eol: &Eol) -> Result<()> {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    match kind {
        EntryKind::Symlink => platform::symlink(&store.get_blob(&hash)?, &p)?,
        EntryKind::File | EntryKind::Exec => {
            let converted = match eol.applies(rel) {
                true => store.get_small_file(&hash)?.and_then(|d| eol.to_disk(rel, &d)),
                false => None,
            };
            match converted {
                Some(d) => fs::write(&p, d)?,
                None => store.write_file_to(&hash, &mut fs::File::create(&p)?)?,
            }
            platform::set_executable(&p, kind == EntryKind::Exec)?;
        }
        EntryKind::Tree => bail!("unexpected tree entry at {rel}"),
    }
    Ok(())
}

fn prune_empty_parents(root: &Path, p: &Path) {
    let mut cur = p.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) || fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}
