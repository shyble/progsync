//! Integrity check (`psync check`) and garbage collection (`psync gc`).
//!
//! Both walk what the repository refers to: every kept operation (from
//! `op_head` back to the oldest kept one), the views they record (working
//! changes, bookmarks, tasks), and from there revisions, their parents,
//! trees, files, conflict sides and checks.
//!
//! `gc` deletes the objects that walk does not reach, unless they are younger
//! than [`GRACE_MS`]: those may belong to a command that has not recorded its
//! operation yet (writers freshen objects they reuse, see [`crate::store`]).
//! Young objects and the workspaces' stat caches count as extra roots, so a
//! kept object never refers to a deleted one. Objects are deleted referrers
//! first, so even an interrupted `gc` leaves every remaining object complete,
//! which fetching from other repositories relies on.
//!
//! `gc` also repacks: afterwards all reachable objects are in one pack file
//! (see [`crate::pack`]) and only recent or unreachable ones are loose.
//! Between gcs, [`auto`] moves loose objects into packs once there are many.
//!
//! `gc --keep AGE` also forgets operations older than AGE (`undo` and
//! `op restore` cannot go back further); the current state and all change
//! history stay. What only the forgotten operations referred to, such as
//! intermediate snapshots of working changes, becomes collectable.

use crate::bail;
use crate::error::{Context, Result};
use crate::hash::Hash;
use crate::pack;
use crate::object::{EntryKind, Object, TAG_BLOB, TAG_CHUNKED, TAG_OPERATION, TAG_REVISION, TAG_TASK, TAG_TREE};
use crate::repo::Repo;
use crate::store::{Store, GRACE_MS};
use crate::util::{now_ms, par_map};
use std::collections::{HashMap, HashSet};
use std::fs::{self, TryLockError};
use std::time::{Duration, SystemTime};

/// The kind of object a reference points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A file: a blob, or a chunk list for a large file.
    Blob,
    /// A chunk of a large file: always a blob.
    Chunk,
    Tree,
    Revision,
    Operation,
    Task,
}

impl Kind {
    fn of_entry(k: EntryKind) -> Kind {
        if k == EntryKind::Tree {
            Kind::Tree
        } else {
            Kind::Blob
        }
    }

    fn of_tag(tag: u8) -> Option<Kind> {
        Some(match tag {
            TAG_BLOB | TAG_CHUNKED => Kind::Blob,
            TAG_TREE => Kind::Tree,
            TAG_REVISION => Kind::Revision,
            TAG_OPERATION => Kind::Operation,
            TAG_TASK => Kind::Task,
            _ => return None,
        })
    }

    fn of(obj: &Object) -> Kind {
        match obj {
            Object::Blob(_) | Object::Chunked(_) => Kind::Blob,
            Object::Tree(_) => Kind::Tree,
            Object::Revision(_) => Kind::Revision,
            Object::Operation(_) => Kind::Operation,
            Object::Task(_) => Kind::Task,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Blob => "file",
            Kind::Chunk => "chunk",
            Kind::Tree => "tree",
            Kind::Revision => "revision",
            Kind::Operation => "operation",
            Kind::Task => "task",
        }
    }
}

/// The objects `obj` refers to. An operation's parent is not included: the
/// kept operations are walked explicitly (see [`kept_ops`]).
fn refs(obj: &Object) -> Vec<(Hash, Kind)> {
    let mut out = Vec::new();
    match obj {
        Object::Blob(_) => {}
        Object::Chunked(c) => out.extend(c.chunks.iter().map(|(h, _)| (*h, Kind::Chunk))),
        Object::Tree(t) => out.extend(t.entries.iter().map(|e| (e.hash, Kind::of_entry(e.kind)))),
        Object::Revision(r) => {
            out.push((r.tree, Kind::Tree));
            out.extend(r.parents.iter().map(|p| (*p, Kind::Revision)));
            out.extend(r.meta.checks.iter().map(|c| (c.tree, Kind::Tree)));
            for c in &r.conflicts {
                out.extend([&c.base, &c.ours, &c.theirs].into_iter().flatten().map(|(k, h)| (*h, Kind::of_entry(*k))));
                out.extend(c.written.map(|h| (h, Kind::Blob)));
            }
        }
        Object::Operation(op) => {
            let v = &op.view;
            let heads = v.remote_heads.iter().map(|r| &r.rev);
            let revs = v.workspaces.values().chain(v.bookmarks.values()).chain(v.revs.values()).chain(v.parked.values()).chain(heads);
            out.extend(revs.map(|h| (*h, Kind::Revision)));
            out.extend(v.tasks.values().map(|h| (*h, Kind::Task)));
        }
        Object::Task(t) => {
            out.extend(t.previous.iter().chain(&t.merged_from).map(|h| (*h, Kind::Task)));
            out.extend(t.completion.iter().map(|c| (c.tree, Kind::Tree)));
        }
    }
    out
}

/// The kept operations, newest first: from `op_head` to the horizon (or the
/// first operation).
fn kept_ops(repo: &Repo) -> Result<Vec<(Hash, crate::object::Operation)>> {
    repo.op_log(usize::MAX)
}

/// A reference to check: the object, what it should be, and who refers to it.
type Ref = (Hash, Kind, Option<(Hash, Kind)>);

#[derive(Default)]
struct Walk {
    seen: HashSet<Hash>,
    problems: Vec<String>,
    /// Bytes read (compressed), when verifying.
    bytes: u64,
}

impl Walk {
    /// Visit everything reachable from `roots`. With `verify`, every object
    /// (files too) is read whole and checked against its hash; otherwise
    /// files are only checked to exist. With `tolerant`, missing or broken
    /// objects are not problems (for roots that are only caches).
    fn run(&mut self, store: &Store, roots: Vec<Ref>, verify: bool, tolerant: bool) {
        let mut frontier: Vec<Ref> = roots.into_iter().filter(|(h, _, _)| self.seen.insert(*h)).collect();
        while !frontier.is_empty() {
            let results = par_map(frontier, |(h, want, by)| visit(store, h, want, verify).map_err(|e| describe(h, want, by, e)));
            frontier = Vec::new();
            for r in results {
                match r {
                    Ok((h, kind, bytes, children)) => {
                        self.bytes += bytes;
                        frontier.extend(children.into_iter().filter(|(c, _)| self.seen.insert(*c)).map(|(c, k)| (c, k, Some((h, kind)))));
                    }
                    Err(msg) if !tolerant => self.problems.push(msg),
                    Err(_) => {}
                }
            }
        }
    }
}

fn describe(h: Hash, want: Kind, by: Option<(Hash, Kind)>, e: crate::error::Error) -> String {
    let by = by.map(|(b, k)| format!(" (in {} {})", k.name(), b.short())).unwrap_or_default();
    format!("{} {}: {e}{by}", want.name(), h.short())
}

/// A visited object: its hash and kind, bytes read and references.
type Visited = (Hash, Kind, u64, Vec<(Hash, Kind)>);

/// Read one object.
fn visit(store: &Store, h: Hash, want: Kind, verify: bool) -> Result<Visited> {
    if want == Kind::Chunk && !verify {
        if !store.contains(&h) {
            bail!("missing");
        }
        return Ok((h, want, 0, vec![]));
    }
    if want == Kind::Blob && !verify {
        // Only a large file's chunk list is read (for its chunks).
        match store.tag_of(&h) {
            Ok(TAG_BLOB) => return Ok((h, want, 0, vec![])),
            Ok(_) => {}
            Err(_) if !store.contains(&h) => bail!("missing"),
            Err(_) => bail!("damaged (cannot be decompressed)"),
        }
    }
    let raw = store.read_raw(&h).map_err(|e| if store.contains(&h) { e } else { crate::err!("missing") })?;
    let data = zstd::decode_all(&raw[..]).map_err(|_| crate::err!("damaged (cannot be decompressed)"))?;
    if verify && Hash::of(&data) != h {
        bail!("damaged (contents do not match its hash)");
    }
    let obj = Object::decode(&data).map_err(|e| crate::err!("damaged ({e})"))?;
    let got = match (&obj, want) {
        (Object::Blob(_), Kind::Chunk) => Kind::Chunk,
        _ => Kind::of(&obj),
    };
    if got != want {
        bail!("is a {}, not a {}", got.name(), want.name());
    }
    Ok((h, want, raw.len() as u64, refs(&obj)))
}

/// The roots: kept operations (and what their views refer to).
fn op_roots(ops: &[(Hash, crate::object::Operation)]) -> Vec<Ref> {
    ops.iter().map(|(h, _)| (*h, Kind::Operation, None)).collect()
}

/// Tree and file hashes recorded in the workspaces' stat caches.
fn index_roots(repo: &Repo) -> Vec<Ref> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(repo.dot().join("workspaces")) else { return out };
    for d in dirs.flatten() {
        let index = crate::wc::Index::load(&d.path().join("index"));
        out.extend(index.entries.values().map(|e| (e.hash, Kind::of_entry(e.kind), None)));
        out.extend(index.trees.values().map(|h| (*h, Kind::Tree, None)));
    }
    out
}

pub struct CheckReport {
    /// Reachable objects checked.
    pub objects: usize,
    /// Their stored (compressed) size.
    pub bytes: u64,
    /// Operations in the kept history.
    pub operations: usize,
    /// Objects present but not reachable (what `gc` can remove, if old).
    pub unreachable: usize,
    /// Pack files, and objects stored loose (outside packs).
    pub packs: usize,
    pub loose: usize,
    pub problems: Vec<String>,
}

/// Verify everything the repository refers to: each object is present,
/// intact (matches its hash) and of the expected kind.
pub fn check(repo: &Repo) -> Result<CheckReport> {
    let mut problems = Vec::new();
    let ops = match kept_ops(repo) {
        Ok(ops) => ops,
        Err(e) => {
            problems.push(format!("operation log: {e:#}"));
            vec![]
        }
    };
    let mut walk = Walk::default();
    walk.run(&repo.store, op_roots(&ops), true, false);
    problems.append(&mut walk.problems);
    for d in fs::read_dir(repo.dot().join("workspaces")).context("reading workspaces")?.flatten() {
        let name = d.file_name().to_string_lossy().into_owned();
        if repo.workspace_meta(&name).is_none() {
            problems.push(format!("workspace {name}: meta.json is missing or unreadable"));
        }
    }
    problems.extend(repo.store.broken_packs());
    let present = repo.store.list()?;
    let unreachable = present.iter().filter(|h| !walk.seen.contains(h)).count();
    Ok(CheckReport {
        objects: walk.seen.len(),
        bytes: walk.bytes,
        operations: ops.len(),
        unreachable,
        packs: repo.store.packs(true).len(),
        loose: repo.store.list_loose()?.len(),
        problems,
    })
}

#[derive(Debug, Default)]
pub struct GcReport {
    /// Unreachable objects removed (or that would be, with `dry_run`).
    pub removed: usize,
    pub removed_bytes: u64,
    /// Unreachable objects kept because they are younger than the grace period.
    pub young: usize,
    /// Objects kept.
    pub kept: usize,
    /// Leftover temp files removed.
    pub temp_files: usize,
    /// Operations forgotten by `keep`.
    pub forgotten_ops: usize,
    /// The oldest kept operation and its time, if history is limited.
    pub horizon: Option<(Hash, i64)>,
    /// Reachable objects, all in one pack afterwards.
    pub packed: usize,
    pub packs_before: usize,
    pub packs_after: usize,
    pub loose_before: usize,
}

/// Remove unreachable objects and leftover temp files. With `keep_ms`, first
/// forget operations older than that. With `dry_run`, change nothing and
/// report what would be removed.
pub fn gc(repo: &Repo, keep_ms: Option<i64>, dry_run: bool) -> Result<GcReport> {
    let Some(_lock) = try_gc_lock(repo)? else { bail!("another gc is running") };
    let started = now_ms();
    let mut report = GcReport::default();

    let mut ops = kept_ops(repo)?;
    if let Some(keep) = keep_ms {
        let cutoff = started - keep;
        // Keep the operations newer than the cutoff, and always the latest.
        let n = ops.iter().position(|(_, op)| op.time_ms < cutoff).unwrap_or(ops.len()).max(1);
        if n < ops.len() {
            report.forgotten_ops = ops.len() - n;
            ops.truncate(n);
            let horizon = ops[n - 1].0;
            if !dry_run {
                repo.set_op_horizon(horizon)?;
            }
        }
    }
    if keep_ms.is_some() || repo.op_horizon()?.is_some() {
        let (h, op) = ops.last().expect("at least the latest operation is kept");
        report.horizon = Some((*h, op.time_ms));
    }

    let mut walk = Walk::default();
    walk.run(&repo.store, op_roots(&ops), false, false);
    if !walk.problems.is_empty() {
        bail!(
            "the repository has missing or damaged objects; nothing was removed. Run `psync check` for details (first: {})",
            walk.problems[0]
        );
    }
    let reachable: HashSet<Hash> = walk.seen.clone();
    walk.run(&repo.store, index_roots(repo), false, true);

    // Where every object is, and how old: a loose file's own time, a packed
    // object's newest pack's time.
    let store = &repo.store;
    let packs = store.packs(true);
    let loose: HashMap<Hash, (Option<SystemTime>, u64)> = par_map(store.list_loose()?, |h| {
        let md = fs::metadata(store.path_for(&h)).ok();
        (h, (md.as_ref().and_then(|m| m.modified().ok()), md.map_or(0, |m| m.len())))
    })
    .into_iter()
    .collect();
    let mut age: HashMap<Hash, (Option<SystemTime>, u64)> = HashMap::new();
    for p in &packs {
        let t = p.modified();
        for h in p.hashes() {
            let e = age.entry(h).or_insert((t, p.size_of(&h).unwrap_or(0)));
            e.0 = e.0.max(t);
        }
    }
    age.extend(loose.iter().map(|(h, v)| (*h, *v)));
    let total = age.len();

    // Unreachable objects, split by age. Young ones (and what they refer to)
    // are kept.
    let young: Vec<Ref> =
        age.iter().filter(|(h, (t, _))| !walk.seen.contains(h) && !is_old(*t)).filter_map(|(h, _)| Some((*h, kind_of(store, h)?, None))).collect();
    walk.run(store, young, false, true);
    let doomed: Vec<Hash> = age.keys().filter(|h| !walk.seen.contains(h)).copied().collect();
    report.kept = total - doomed.len();
    report.young = report.kept - reachable.len().min(report.kept);
    report.removed = doomed.len();
    report.removed_bytes = doomed.iter().map(|h| age[h].1).sum();
    // Kept but unreachable objects that are only packed: they leave the packs
    // (which will hold only reachable objects) and become loose again, with
    // their age, so the grace period still runs out for them.
    let unpack: HashSet<Hash> = walk.seen.iter().filter(|h| !reachable.contains(h) && age.contains_key(h) && !loose.contains_key(h)).copied().collect();
    // Repack unless the reachable objects are already exactly one pack.
    let repack = !(packs.len() == 1 && packs[0].len() == reachable.len() && reachable.iter().all(|h| packs[0].contains(h) && !loose.contains_key(h)));
    report.packed = reachable.len();
    report.packs_before = packs.len();
    report.loose_before = loose.len();

    report.temp_files = remove_temp_files(repo, dry_run)?;
    if dry_run {
        return Ok(report);
    }

    // 1. The new pack, and unpacked objects: written and flushed before
    //    anything is deleted.
    let new_pack = if repack && !reachable.is_empty() {
        let mut order: Vec<Hash> = reachable.iter().copied().collect();
        order.sort();
        Some(pack::write(&store.pack_dir(), store.tmp_dir(), order.into_iter().map(|h| Ok((h, store.read_raw(&h)?))))?)
    } else {
        None
    };
    for h in &unpack {
        store.write_loose(h, &store.read_raw(h)?, age[h].0)?;
    }
    store.barrier()?;

    // 2. Doomed objects that must stay after all: those of an old pack
    //    freshened since we looked (a writer relied on one of its objects),
    //    and, below, loose ones freshened meanwhile; with everything they
    //    refer to.
    let graph = doomed_graph(store, &doomed);
    let old_packs: Vec<_> = if repack { packs.iter().filter(|p| Some(&p.name) != new_pack.as_ref()).collect() } else { vec![] };
    let mut spared: HashSet<Hash> = HashSet::new();
    let spare = |h: Hash, spared: &mut HashSet<Hash>| {
        let mut stack = vec![h];
        while let Some(x) = stack.pop() {
            if spared.insert(x) {
                stack.extend(graph.get(&x).into_iter().flatten().copied());
            }
        }
    };
    let freshened_packs = |spared: &mut HashSet<Hash>| {
        for p in old_packs.iter().filter(|p| !is_old(p.modified())) {
            for h in p.hashes().filter(|h| graph.contains_key(h)) {
                spare(h, spared);
            }
        }
    };
    freshened_packs(&mut spared);

    // 3. Doomed loose objects, referrers first.
    for h in delete_order(&graph) {
        if spared.contains(&h) || !loose.contains_key(&h) {
            continue;
        }
        let path = store.path_for(&h);
        if is_old(fs::metadata(&path).and_then(|m| m.modified()).ok()) {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            }
        } else {
            spare(h, &mut spared);
        }
    }

    // 4. Old packs: first take out the spared objects only they hold, then
    //    drop the loose copies of what the new pack holds, then the packs.
    freshened_packs(&mut spared);
    for h in spared.iter().filter(|h| !loose.contains_key(h)) {
        if let Some(p) = old_packs.iter().find(|p| p.contains(h)) {
            store.write_loose(h, &store.read_raw(h)?, p.modified())?;
        }
    }
    store.barrier()?;
    if new_pack.is_some() {
        for h in loose.keys().filter(|h| reachable.contains(h)) {
            let _ = fs::remove_file(store.path_for(h));
        }
    }
    for p in &old_packs {
        pack::remove(&store.pack_dir(), &p.name)?;
    }
    report.removed = doomed.iter().filter(|h| !spared.contains(h)).count();
    report.removed_bytes = doomed.iter().filter(|h| !spared.contains(h)).map(|h| age[h].1).sum();
    report.packs_after = store.packs(true).len();
    Ok(report)
}

fn is_old(t: Option<SystemTime>) -> bool {
    t.and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|a| a > Duration::from_millis(GRACE_MS as u64))
}

/// Loose objects beyond which a command packs them (estimated from one
/// fan-out directory, as git does).
const AUTO_LOOSE: usize = 4096;
/// Packs beyond which automatic packing merges them into one.
const AUTO_PACKS: usize = 16;

/// Automatic packing (the CLI runs it in the background after commands, as
/// `psync gc --auto`): when there are many loose objects,
/// move them all into a new pack; when there are many packs, merge them.
/// Nothing is deleted that is not in a pack. Skipped while a gc runs.
pub fn auto(repo: &Repo) -> Result<bool> {
    auto_with(repo, AUTO_LOOSE, AUTO_PACKS)
}

/// Whether [`auto`] has work to do (cheap: one directory listing or two).
/// False while a gc (or another automatic packing) runs.
pub fn auto_needed(repo: &Repo) -> bool {
    let (loose, packs) = needed(&repo.store, AUTO_LOOSE, AUTO_PACKS);
    (loose || packs) && try_gc_lock(repo).ok().flatten().is_some()
}

/// (Too many loose objects, too many packs.)
fn needed(store: &Store, max_loose: usize, max_packs: usize) -> (bool, bool) {
    let sample = fs::read_dir(store.path_for(&Hash([0x17; 32])).parent().expect("fan-out dir")).map_or(0, |d| d.count());
    // The one-directory estimate is too rough for small thresholds (tests).
    let estimate = if max_loose < 2048 { store.list_loose().map_or(0, |l| l.len()) } else { sample * 256 };
    (estimate > max_loose, pack::names(&store.pack_dir()).len() > max_packs)
}

/// [`auto`] with explicit thresholds: pack when the estimated number of
/// loose objects exceeds `max_loose`; merge packs when there are more than
/// `max_packs`.
pub fn auto_with(repo: &Repo, max_loose: usize, max_packs: usize) -> Result<bool> {
    let store = &repo.store;
    let (many_loose, many_packs) = needed(store, max_loose, max_packs);
    if !many_loose && !many_packs {
        return Ok(false);
    }
    let Some(_lock) = try_gc_lock(repo)? else { return Ok(false) };
    let loose = store.list_loose()?;
    let merge: Vec<_> = if many_packs { store.packs(true) } else { vec![] };
    let mut all: Vec<Hash> = loose.clone();
    for p in &merge {
        all.extend(p.hashes());
    }
    all.sort();
    all.dedup();
    let name = pack::write(&store.pack_dir(), store.tmp_dir(), all.into_iter().map(|h| Ok((h, store.read_raw(&h)?))))?;
    store.barrier()?;
    for h in &loose {
        let _ = fs::remove_file(store.path_for(h));
    }
    for p in merge.iter().filter(|p| p.name != name) {
        pack::remove(&store.pack_dir(), &p.name)?;
    }
    store.packs(true);
    Ok(true)
}

/// The gc lock, if no other gc holds it.
fn try_gc_lock(repo: &Repo) -> Result<Option<fs::File>> {
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(repo.dot().join("gc.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e).context("locking gc.lock"),
    }
}

fn kind_of(store: &Store, h: &Hash) -> Option<Kind> {
    store.tag_of(h).ok().and_then(Kind::of_tag)
}

/// What each doomed object refers to among the doomed.
fn doomed_graph(store: &Store, doomed: &[Hash]) -> HashMap<Hash, Vec<Hash>> {
    let set: HashSet<Hash> = doomed.iter().copied().collect();
    par_map(doomed.to_vec(), |h| {
        let children = match store.tag_of(&h).ok().filter(|t| *t != TAG_BLOB).and_then(Kind::of_tag) {
            None => vec![],
            Some(_) => store.get(&h).map(|o| refs(&o).into_iter().map(|(c, _)| c).filter(|c| set.contains(c)).collect()).unwrap_or_default(),
        };
        (h, children)
    })
    .into_iter()
    .collect()
}

/// The objects of `graph` ordered so that an object is deleted before
/// everything it refers to (files last), keeping the remaining objects
/// complete.
fn delete_order(graph: &HashMap<Hash, Vec<Hash>>) -> Vec<Hash> {
    let mut indegree: HashMap<Hash, usize> = graph.keys().map(|h| (*h, 0)).collect();
    for cs in graph.values() {
        for c in cs {
            *indegree.get_mut(c).expect("child is doomed") += 1;
        }
    }
    let mut ready: Vec<Hash> = indegree.iter().filter(|(_, n)| **n == 0).map(|(h, _)| *h).collect();
    let mut out = Vec::with_capacity(indegree.len());
    while let Some(h) = ready.pop() {
        out.push(h);
        for c in &graph[&h] {
            let n = indegree.get_mut(c).expect("child is doomed");
            *n -= 1;
            if *n == 0 {
                ready.push(*c);
            }
        }
    }
    out
}

/// Remove temp files older than the grace period: in `.psync/tmp`, and the
/// `*.tmp.<pid>` files of interrupted atomic writes.
fn remove_temp_files(repo: &Repo, dry_run: bool) -> Result<usize> {
    let mut dirs = vec![repo.store.tmp_dir().to_path_buf(), repo.dot().to_path_buf()];
    if let Ok(ws) = fs::read_dir(repo.dot().join("workspaces")) {
        dirs.extend(ws.flatten().map(|d| d.path()));
    }
    let grace = Duration::from_millis(GRACE_MS as u64);
    let mut n = 0;
    for (i, dir) in dirs.iter().enumerate() {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_temp = if i == 0 { true } else { name.contains(".tmp.") };
            let Ok(md) = e.metadata() else { continue };
            let old = md.modified().ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|a| a > grace);
            if is_temp && md.is_file() && old {
                if !dry_run {
                    let _ = fs::remove_file(e.path());
                }
                n += 1;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Tree, TreeEntry};

    #[test]
    fn referrers_are_deleted_before_what_they_refer_to() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        store.create_dirs().unwrap();
        let blob = store.put_blob(b"x").unwrap();
        let entry = |name: &str, kind, hash| TreeEntry { name: name.into(), kind, hash };
        let inner = store.put(&Object::Tree(Tree { entries: vec![entry("x", EntryKind::File, blob)] })).unwrap();
        let outer = store.put(&Object::Tree(Tree { entries: vec![entry("d", EntryKind::Tree, inner), entry("y", EntryKind::File, blob)] })).unwrap();
        for doomed in [vec![blob, inner, outer], vec![outer, blob, inner], vec![inner, outer, blob]] {
            assert_eq!(delete_order(&doomed_graph(&store, &doomed)), vec![outer, inner, blob]);
        }
    }
}
