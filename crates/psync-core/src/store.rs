//! Content-addressed object store. New objects are *loose* files,
//! `.psync/objects/<2 hex>/<62 hex>`, each zstd-compressed; `gc` and automatic
//! packing move them into pack files (`.psync/objects/pack/`, see
//! [`crate::pack`]). Reads look for a loose file first, then in the packs
//! (re-reading the pack directory when it changed, as another process may
//! have just packed the object). Writes are atomic (temp file + rename) and
//! idempotent, so concurrent writers never corrupt the store.
//!
//! Durability: new objects are flushed (see [`crate::platform::barrier`])
//! before `op_head` moves to an operation that refers to them, so after a
//! crash every object reachable from `op_head` is intact.
//!
//! Garbage collection ([`crate::gc`]) deletes unreachable objects older than
//! [`GRACE_MS`]. A writer that finds an object already present relies on it
//! without writing it, so it *freshens* the object (sets its modification
//! time to now) when it is older than half the grace period: an object in
//! use is never old enough to be collected. A packed object is freshened by
//! freshening its pack.
//!
//! Files larger than [`crate::chunk::LARGE`] are stored as chunks plus a
//! chunk list, written and read a chunk at a time ([`Store::put_reader`],
//! [`Store::write_file_to`]).

use crate::hash::Hash;
use crate::object::{Chunked, Object, Operation, Revision, Task, Tree, FORMAT_VERSION, TAG_BLOB};
use crate::bail;
use crate::error::{Context, Result};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use crate::pack::{Pack, PackSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

const ZSTD_LEVEL: i32 = 3;

/// Unreachable objects (and temp files) younger than this are kept by `gc`:
/// they may belong to a command that has not recorded its operation yet.
pub const GRACE_MS: i64 = 86_400_000;

/// Whether a modification time is older than half the grace period (an
/// object or pack relied on then needs freshening).
fn is_old(t: Option<SystemTime>) -> bool {
    t.and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age > Duration::from_millis(GRACE_MS as u64 / 2))
}

/// Large files being stored right now (see [`Store::put_reader`]).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

struct Active;

impl Active {
    fn enter() -> Active {
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        Active
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The first two bytes of a compressed object, with a decompression context
/// reused per thread (`gc` and pushes ask this of every file).
fn first_bytes(raw: &[u8]) -> Result<[u8; 2]> {
    use zstd::stream::raw::{Decoder, InBuffer, Operation, OutBuffer};
    thread_local! {
        static DECODER: std::cell::RefCell<Option<Decoder<'static>>> = const { std::cell::RefCell::new(None) };
    }
    DECODER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let dec = match cell.as_mut() {
            Some(d) => d,
            None => cell.insert(Decoder::new()?),
        };
        dec.reinit()?;
        let mut out = [0u8; 2];
        let mut input = InBuffer::around(raw);
        let mut output = OutBuffer::around(&mut out[..]);
        while output.pos() < 2 {
            let before = (input.pos(), output.pos());
            dec.run(&mut input, &mut output)?;
            if (input.pos(), output.pos()) == before {
                bail!("too short");
            }
        }
        Ok(out)
    })
}

pub struct Store {
    objects: PathBuf,
    tmp: PathBuf,
    /// Directories that got new objects since the last [`Store::sync`].
    dirty: Mutex<HashSet<PathBuf>>,
    /// Open packs, the pack directory's modification time and when it was
    /// last looked at; `None` until first needed.
    packs: RwLock<Option<(PackSet, Option<SystemTime>, std::time::Instant)>>,
    /// Packs freshened by this handle (once is enough).
    freshened: Mutex<HashSet<String>>,
}

impl Store {
    pub fn open(dot: &Path) -> Store {
        Store {
            objects: dot.join("objects"),
            tmp: dot.join("tmp"),
            dirty: Mutex::default(),
            packs: RwLock::default(),
            freshened: Mutex::default(),
        }
    }

    pub fn create_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.objects)?;
        fs::create_dir_all(&self.tmp)?;
        Ok(())
    }

    pub(crate) fn path_for(&self, h: &Hash) -> PathBuf {
        let hex = h.to_hex();
        self.objects.join(&hex[..2]).join(&hex[2..])
    }

    pub(crate) fn tmp_dir(&self) -> &Path {
        &self.tmp
    }

    pub(crate) fn pack_dir(&self) -> PathBuf {
        self.objects.join("pack")
    }

    /// The open packs (opening them if not done yet; with `refresh`, also
    /// re-reading the directory if it changed).
    pub(crate) fn packs(&self, refresh: bool) -> Vec<Arc<Pack>> {
        if let Some((set, _, checked)) = self.packs.read().unwrap().as_ref() {
            // Long-lived handles (the watcher) look again now and then, so
            // packs removed by a gc elsewhere are let go.
            if !refresh && checked.elapsed() < Duration::from_secs(1) {
                return set.packs.clone();
            }
        }
        let mut guard = self.packs.write().unwrap();
        let dir = self.pack_dir();
        let mtime = fs::metadata(&dir).and_then(|m| m.modified()).ok();
        let (set, seen, checked) = guard.get_or_insert_with(|| (PackSet::default(), None, std::time::Instant::now()));
        *checked = std::time::Instant::now();
        // A directory's modification time can be coarse (1 s on some file
        // systems): within 2 s of a change, always look.
        let recent = mtime.and_then(|t| SystemTime::now().duration_since(t).ok()).is_none_or(|a| a < Duration::from_secs(2));
        if seen.is_none() || *seen != mtime || recent {
            set.refresh(&dir);
            *seen = mtime;
        }
        set.packs.clone()
    }

    /// Packs that could not be opened, with why.
    pub(crate) fn broken_packs(&self) -> Vec<String> {
        self.packs(true);
        self.packs.read().unwrap().as_ref().map(|(s, _, _)| s.broken.clone()).unwrap_or_default()
    }

    /// The pack holding `h`, if any (re-reading the pack directory first
    /// when not found and `refresh`).
    fn find_packed(&self, h: &Hash, refresh: bool) -> Option<Arc<Pack>> {
        let found = self.packs(false).into_iter().find(|p| p.contains(h));
        if found.is_some() || !refresh {
            return found;
        }
        self.packs(true).into_iter().find(|p| p.contains(h))
    }

    /// Whether the object exists; freshens it if old (see module docs).
    fn present(&self, h: &Hash, refresh: bool) -> bool {
        let path = self.path_for(h);
        if let Ok(md) = fs::metadata(&path) {
            if is_old(md.modified().ok()) {
                // Gone meanwhile (collected or packed): look again below.
                if let Err(e) = crate::platform::touch(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return true;
                    }
                    return self.find_packed(h, true).is_some_and(|p| self.freshen_pack(&p));
                }
            }
            return true;
        }
        self.find_packed(h, refresh).is_some_and(|p| self.freshen_pack(&p))
    }

    /// Freshen a pack whose object is being relied on; false if the pack is
    /// gone (the caller then treats the object as absent).
    fn freshen_pack(&self, p: &Pack) -> bool {
        if self.freshened.lock().unwrap().contains(&p.name) || !is_old(p.modified()) {
            return true;
        }
        match crate::platform::touch(&p.path) {
            Ok(()) => {
                self.freshened.lock().unwrap().insert(p.name.clone());
                true
            }
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    }

    /// Move a finished temp file into place as `path`, flushing it as part of
    /// the current batch.
    fn install(&self, tmp: &Path, file: &File, path: &Path) -> Result<()> {
        crate::platform::sync_new_file(file)?;
        let dir = path.parent().expect("object path has a parent");
        let mut dirty = vec![dir.to_path_buf()];
        match fs::create_dir(dir) {
            Ok(()) => dirty.push(self.objects.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
        fs::rename(tmp, path)?;
        self.dirty.lock().unwrap().extend(dirty);
        Ok(())
    }

    /// Flush the directories that got new objects through this handle, so
    /// that with [`crate::platform::barrier`] (issued by the `op_head` write)
    /// all objects written so far are durable.
    pub fn sync(&self) -> Result<()> {
        let dirs: Vec<PathBuf> = std::mem::take(&mut *self.dirty.lock().unwrap()).into_iter().collect();
        let failed: Vec<_> = crate::util::par_map(dirs, |d| match crate::platform::sync_new_dir(&d) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some((d, e)),
            _ => None,
        })
        .into_iter()
        .flatten()
        .collect();
        if let Some((d, e)) = failed.into_iter().next() {
            return Err(e).with_context(|| format!("flushing {}", d.display()));
        }
        Ok(())
    }

    /// Whether the object exists, without freshening it (for `gc`/`check`).
    pub(crate) fn contains(&self, h: &Hash) -> bool {
        self.path_for(h).exists() || self.find_packed(h, true).is_some()
    }

    /// Write `raw` as a loose object even if it is packed (taking it out of
    /// packs that `gc` is about to drop), with modification time `mtime`.
    pub(crate) fn write_loose(&self, h: &Hash, raw: &[u8], mtime: Option<SystemTime>) -> Result<()> {
        let path = self.path_for(h);
        if !path.exists() {
            let tmp = self.tmp.join(crate::util::random_hex(16));
            let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(raw)?;
            if let Some(t) = mtime {
                f.set_modified(t)?;
            }
            self.install(&tmp, &f, &path)?;
        }
        Ok(())
    }

    /// Make everything written so far durable before what follows (e.g.
    /// deleting other copies of the objects).
    pub(crate) fn barrier(&self) -> Result<()> {
        self.sync()?;
        let marker = self.tmp.join(format!("barrier-{}", crate::util::random_hex(8)));
        let f = File::create(&marker)?;
        let r = crate::platform::barrier(&f);
        drop(f);
        let _ = fs::remove_file(&marker);
        Ok(r?)
    }

    pub fn has(&self, h: &Hash) -> bool {
        self.present(h, true)
    }

    /// Store file contents (chunked if large, see [`crate::chunk`]).
    pub fn put_blob(&self, data: &[u8]) -> Result<Hash> {
        if data.len() > crate::chunk::LARGE {
            return Ok(self.put_reader(&mut &data[..])?.0);
        }
        self.put_whole(data)
    }

    /// Store the contents read from `r`, holding at most a few chunks in
    /// memory. Returns the file's hash and, for a large file, the hash it
    /// would have as a single blob (as stored before large files were
    /// chunked), so an unchanged old file can keep its hash.
    pub fn put_reader(&self, r: &mut dyn Read) -> Result<(Hash, Option<Hash>)> {
        let mut start = Vec::new();
        r.take(crate::chunk::LARGE as u64 + 1).read_to_end(&mut start)?;
        if start.len() <= crate::chunk::LARGE {
            return Ok((self.put_whole(&start)?, None));
        }
        let mut whole = blake3::Hasher::new();
        whole.update(&[FORMAT_VERSION, TAG_BLOB]);
        // Chunks are compressed and written by worker threads while the
        // file is read and split; at most a few wait in between. Large
        // files stored at the same time share the processors.
        let _active = Active::enter();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let workers = (cores / ACTIVE.load(Ordering::Relaxed).max(1)).clamp(1, 8);
        let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, Vec<u8>)>(workers);
        let rx = Mutex::new(rx);
        let stored: Mutex<Vec<(usize, Result<Hash>)>> = Mutex::default();
        let mut lens = Vec::new();
        let split = std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((i, chunk)) = job else { break };
                    let h = self.put_whole(&chunk);
                    stored.lock().unwrap().push((i, h));
                });
            }
            let r = crate::chunk::split(r, start, |c| {
                whole.update(c);
                lens.push(c.len() as u64);
                tx.send((lens.len() - 1, c.to_vec())).map_err(|_| crate::err!("chunk writers stopped"))
            });
            drop(tx);
            r
        });
        split?;
        let mut stored = stored.into_inner().unwrap();
        stored.sort_by_key(|(i, _)| *i);
        let mut list = Chunked { size: lens.iter().sum(), chunks: Vec::with_capacity(lens.len()) };
        for ((_, h), len) in stored.into_iter().zip(lens) {
            list.chunks.push((h?, len));
        }
        let h = self.put(&Object::Chunked(list))?;
        Ok((h, Some(Hash(*whole.finalize().as_bytes()))))
    }

    /// Store `data` as one blob, without copying it into another buffer.
    fn put_whole(&self, data: &[u8]) -> Result<Hash> {
        let header = [FORMAT_VERSION, TAG_BLOB];
        let mut hasher = blake3::Hasher::new();
        hasher.update(&header);
        hasher.update(data);
        let h = Hash(*hasher.finalize().as_bytes());
        self.write_if_absent(&h, &[&header, data])?;
        Ok(h)
    }

    pub fn put(&self, obj: &Object) -> Result<Hash> {
        let enc = obj.encode()?;
        let h = Hash::of(&enc);
        self.write_if_absent(&h, &[&enc])?;
        Ok(h)
    }

    /// Copy one object's stored bytes from another store (same format), if
    /// absent here. Used to transfer objects between local repositories.
    pub fn copy_from(&self, other: &Store, h: &Hash) -> Result<()> {
        if self.present(h, false) {
            return Ok(());
        }
        self.put_raw(h, &other.read_raw(h)?)
    }

    fn write_if_absent(&self, h: &Hash, parts: &[&[u8]]) -> Result<()> {
        // Not re-reading the pack directory: at worst an object packed a
        // moment ago is written loose again (gc removes the copy).
        if self.present(h, false) {
            return Ok(());
        }
        let path = self.path_for(h);
        let tmp = self.tmp.join(crate::util::random_hex(16));
        let f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut enc = zstd::Encoder::new(BufWriter::new(f), ZSTD_LEVEL)?;
        for p in parts {
            enc.write_all(p)?;
        }
        let f = enc.finish()?.into_inner().map_err(|e| e.into_error())?;
        self.install(&tmp, &f, &path)
    }

    /// An object's stored (compressed) bytes, for transfer.
    pub fn read_raw(&self, h: &Hash) -> Result<Vec<u8>> {
        match fs::read(self.path_for(h)) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match self.find_packed(h, true) {
                Some(p) => p.read(h).expect("the pack has it"),
                None => bail!("object {} not found", h.short()),
            },
            Err(e) => Err(e).with_context(|| format!("reading object {}", h.short())),
        }
    }

    /// Decode stored bytes received from elsewhere, checking they are the
    /// object `h` claims to be.
    pub fn verify_raw(h: &Hash, raw: &[u8]) -> Result<Object> {
        let data = zstd::decode_all(raw).with_context(|| format!("object {} is corrupt", h.short()))?;
        if Hash::of(&data) != *h {
            bail!("object {} does not match its hash", h.short());
        }
        Object::decode(&data)
    }

    /// Store bytes already verified with `verify_raw` (no-op if present).
    pub fn put_raw(&self, h: &Hash, raw: &[u8]) -> Result<()> {
        if self.present(h, false) {
            return Ok(());
        }
        let path = self.path_for(h);
        let tmp = self.tmp.join(crate::util::random_hex(16));
        let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(raw)?;
        self.install(&tmp, &f, &path)
    }

    /// Write received bytes to a temp file, to be stored later with
    /// [`Store::put_staged`] (once what the object refers to is stored).
    pub(crate) fn stage(&self, raw: &[u8]) -> Result<PathBuf> {
        let tmp = self.tmp.join(crate::util::random_hex(16));
        fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        Ok(tmp)
    }

    /// Store a staged object (its bytes were verified before staging).
    pub(crate) fn put_staged(&self, h: &Hash, tmp: &Path) -> Result<()> {
        if self.present(h, false) {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        let path = self.path_for(h);
        let f = File::open(tmp)?;
        self.install(tmp, &f, &path)
    }

    /// The kind tag of a stored object (reads only its first bytes).
    pub(crate) fn tag_of(&self, h: &Hash) -> Result<u8> {
        // The first block (at most 128 KiB) holds the start.
        const PREFIX: u64 = (128 << 10) + 64;
        let raw = match File::open(self.path_for(h)) {
            Ok(f) => {
                let mut raw = Vec::new();
                f.take(PREFIX).read_to_end(&mut raw).with_context(|| format!("reading object {}", h.short()))?;
                raw
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match self.find_packed(h, true) {
                Some(p) => p.read_prefix(h, PREFIX).expect("the pack has it")?,
                None => self.read_raw(h)?,
            },
            Err(e) => return Err(e).with_context(|| format!("reading object {}", h.short())),
        };
        let head = first_bytes(&raw).with_context(|| format!("object {} is corrupt", h.short()))?;
        Ok(head[1])
    }

    /// Every object in the store, loose or packed (each once).
    pub(crate) fn list(&self) -> Result<Vec<Hash>> {
        let mut all: HashSet<Hash> = self.list_loose()?.into_iter().collect();
        for p in self.packs(true) {
            all.extend(p.hashes());
        }
        Ok(all.into_iter().collect())
    }

    /// The loose objects (by file name; unrelated files are skipped).
    pub(crate) fn list_loose(&self) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        for d in fs::read_dir(&self.objects).with_context(|| format!("reading {}", self.objects.display()))? {
            let d = d?;
            let prefix = d.file_name().to_string_lossy().into_owned();
            if prefix.len() != 2 || !d.file_type()?.is_dir() {
                continue;
            }
            for f in fs::read_dir(d.path())? {
                let name = f?.file_name();
                if let Ok(h) = Hash::from_hex(&format!("{prefix}{}", name.to_string_lossy())) {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }

    pub fn get(&self, h: &Hash) -> Result<Object> {
        let data = match File::open(self.path_for(h)) {
            Ok(f) => zstd::decode_all(BufReader::new(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => zstd::decode_all(&self.read_raw(h)?[..]),
            Err(e) => return Err(e).with_context(|| format!("reading object {}", h.short())),
        };
        Object::decode(&data.with_context(|| format!("object {} is corrupt", h.short()))?)
    }

    /// A file's whole contents (a large file is assembled in memory: see
    /// [`Store::write_file_to`] and [`Store::get_small_file`]).
    pub fn get_blob(&self, h: &Hash) -> Result<Vec<u8>> {
        match self.get(h)? {
            Object::Blob(b) => Ok(b),
            Object::Chunked(list) => {
                let mut out = Vec::with_capacity(usize::try_from(list.size).unwrap_or(0));
                self.write_chunks(h, &list, &mut out)?;
                Ok(out)
            }
            o => bail!("expected file at {}, found {}", h.short(), o.kind_name()),
        }
    }

    /// A file's contents, or `None` for a large (chunked) file: for diffs
    /// and merges, which treat large files as binary.
    pub fn get_small_file(&self, h: &Hash) -> Result<Option<Vec<u8>>> {
        match self.get(h)? {
            Object::Blob(b) => Ok(Some(b)),
            Object::Chunked(_) => Ok(None),
            o => bail!("expected file at {}, found {}", h.short(), o.kind_name()),
        }
    }

    /// A file's size in bytes, without reading a large file's chunks.
    pub fn file_size(&self, h: &Hash) -> Result<u64> {
        match self.get(h)? {
            Object::Blob(b) => Ok(b.len() as u64),
            Object::Chunked(list) => Ok(list.size),
            o => bail!("expected file at {}, found {}", h.short(), o.kind_name()),
        }
    }

    /// Write a file's contents to `w`, a chunk at a time.
    pub fn write_file_to(&self, h: &Hash, w: &mut dyn Write) -> Result<()> {
        match self.get(h)? {
            Object::Blob(b) => Ok(w.write_all(&b)?),
            Object::Chunked(list) => self.write_chunks(h, &list, w),
            o => bail!("expected file at {}, found {}", h.short(), o.kind_name()),
        }
    }

    fn write_chunks(&self, h: &Hash, list: &Chunked, w: &mut dyn Write) -> Result<()> {
        for (c, len) in &list.chunks {
            let data = match self.get(c)? {
                Object::Blob(b) if b.len() as u64 == *len => b,
                _ => bail!("file {}: chunk {} is not a chunk of {len} bytes", h.short(), c.short()),
            };
            w.write_all(&data)?;
        }
        Ok(())
    }

    pub fn get_tree(&self, h: &Hash) -> Result<Tree> {
        match self.get(h)? {
            Object::Tree(t) => Ok(t),
            o => bail!("expected tree at {}, found {}", h.short(), o.kind_name()),
        }
    }

    pub fn get_revision(&self, h: &Hash) -> Result<Revision> {
        match self.get(h)? {
            Object::Revision(r) => Ok(r),
            o => bail!("expected revision at {}, found {}", h.short(), o.kind_name()),
        }
    }

    pub fn get_operation(&self, h: &Hash) -> Result<Operation> {
        match self.get(h)? {
            Object::Operation(op) => Ok(op),
            o => bail!("expected operation at {}, found {}", h.short(), o.kind_name()),
        }
    }

    pub fn get_task(&self, h: &Hash) -> Result<Task> {
        match self.get(h)? {
            Object::Task(t) => Ok(t),
            o => bail!("expected task at {}, found {}", h.short(), o.kind_name()),
        }
    }
}
