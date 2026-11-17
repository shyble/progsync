//! Pack files: many objects in one file, so a repository is a few large
//! files instead of one small file per object (each of which costs a disk
//! block, an inode and a directory entry).
//!
//! `.psync/objects/pack/<name>.pack` holds objects' stored bytes (exactly what
//! a loose object file holds: zstd-compressed) one after another;
//! `<name>.idx` maps hashes to their place:
//!
//! ```text
//! pack: "PSPK" u32 version | raw object bytes ...
//! idx:  "PSIX" u32 version | u64 count | 256 x u32 fan-out (number of
//!       hashes whose first byte is <= i) | count x (32-byte hash, u64
//!       offset, u64 length), sorted by hash | u64 pack length |
//!       32-byte BLAKE3 of everything before
//! ```
//!
//! All integers are little-endian. A pack is visible once its index exists:
//! the pack is written and flushed first, then the index. New objects are
//! always written loose (cheap, and safe with concurrent writers); `gc` and
//! automatic packing (see [`crate::gc`]) move them into packs.

use crate::bail;
use crate::error::{Context, Result};
use crate::hash::Hash;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PACK_MAGIC: &[u8; 4] = b"PSPK";
const IDX_MAGIC: &[u8; 4] = b"PSIX";
const VERSION: u32 = 1;
const PACK_HEADER: u64 = 8;
const ENTRY: usize = 48;
const FANOUT: usize = 16;

/// One pack file and its index (held in memory).
pub struct Pack {
    pub name: String,
    pub path: PathBuf,
    file: File,
    count: usize,
    /// The index file's bytes; entries start at [`entries_at`].
    idx: Vec<u8>,
}

fn entries_at() -> usize {
    FANOUT + 256 * 4
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

impl Pack {
    /// Open `<dir>/<name>.idx` and its pack, checking the index's checksum
    /// and the pack's length.
    pub fn open(dir: &Path, name: &str) -> Result<Pack> {
        let idx_path = dir.join(format!("{name}.idx"));
        let idx = fs::read(&idx_path).with_context(|| format!("reading {}", idx_path.display()))?;
        let damaged = || crate::err!("pack {name}: index is damaged");
        if idx.len() < entries_at() + 8 + 32 || &idx[..4] != IDX_MAGIC {
            return Err(damaged());
        }
        if u32_at(&idx, 4) != VERSION {
            bail!("pack {name}: unsupported version {}", u32_at(&idx, 4));
        }
        let count = usize::try_from(u64_at(&idx, 8)).map_err(|_| damaged())?;
        let end = entries_at() + count.checked_mul(ENTRY).ok_or_else(damaged)?;
        if idx.len() != end + 8 + 32 || blake3::hash(&idx[..end + 8]).as_bytes() != &idx[end + 8..] {
            return Err(damaged());
        }
        let path = dir.join(format!("{name}.pack"));
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        if file.metadata()?.len() != u64_at(&idx, end) {
            bail!("pack {name}: pack file has the wrong length (truncated?)");
        }
        Ok(Pack { name: name.to_string(), path, file, count, idx })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn hash_at(&self, i: usize) -> &[u8] {
        let at = entries_at() + i * ENTRY;
        &self.idx[at..at + 32]
    }

    /// The hashes in this pack, sorted.
    pub fn hashes(&self) -> impl Iterator<Item = Hash> + '_ {
        (0..self.count).map(|i| Hash(self.hash_at(i).try_into().expect("32 bytes")))
    }

    /// Offset and length of `h`'s bytes in the pack.
    fn find(&self, h: &Hash) -> Option<(u64, u64)> {
        let first = h.0[0] as usize;
        let lo = if first == 0 { 0 } else { u32_at(&self.idx, FANOUT + (first - 1) * 4) as usize };
        let hi = u32_at(&self.idx, FANOUT + first * 4) as usize;
        let (mut lo, mut hi) = (lo, hi.min(self.count));
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.hash_at(mid).cmp(&h.0[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let at = entries_at() + mid * ENTRY + 32;
                    return Some((u64_at(&self.idx, at), u64_at(&self.idx, at + 8)));
                }
            }
        }
        None
    }

    /// The stored size of `h`, if this pack has it.
    pub fn size_of(&self, h: &Hash) -> Option<u64> {
        self.find(h).map(|(_, len)| len)
    }

    pub fn contains(&self, h: &Hash) -> bool {
        self.find(h).is_some()
    }

    /// The stored bytes of `h`, if this pack has it.
    pub fn read(&self, h: &Hash) -> Option<Result<Vec<u8>>> {
        let (off, len) = self.find(h)?;
        let mut buf = vec![0u8; len as usize];
        Some(
            crate::platform::read_at(&self.file, &mut buf, off)
                .with_context(|| format!("reading object {} from pack {}", h.short(), self.name))
                .map(|()| buf),
        )
    }

    /// At most the first `max` stored bytes of `h`, if this pack has it.
    pub fn read_prefix(&self, h: &Hash, max: u64) -> Option<Result<Vec<u8>>> {
        let (off, len) = self.find(h)?;
        let mut buf = vec![0u8; len.min(max) as usize];
        Some(
            crate::platform::read_at(&self.file, &mut buf, off)
                .with_context(|| format!("reading object {} from pack {}", h.short(), self.name))
                .map(|()| buf),
        )
    }

    /// When the pack was last written or freshened.
    pub fn modified(&self) -> Option<std::time::SystemTime> {
        fs::metadata(&self.path).and_then(|m| m.modified()).ok()
    }
}

/// Write a pack of `objects` (hash, stored bytes) into `dir`, through temp
/// files in `tmp`. Returns its name (derived from its contents). The files are
/// flushed but the caller must still issue a barrier before relying on it
/// (e.g. before deleting the objects' other copies).
pub fn write(dir: &Path, tmp: &Path, objects: impl Iterator<Item = Result<(Hash, Vec<u8>)>>) -> Result<String> {
    fs::create_dir_all(dir)?;
    let tmp_pack = tmp.join(format!("pack-{}", crate::util::random_hex(16)));
    let mut entries: Vec<(Hash, u64, u64)> = Vec::new();
    let result = (|| -> Result<String> {
        let mut w = BufWriter::new(File::create(&tmp_pack)?);
        w.write_all(PACK_MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        let mut off = PACK_HEADER;
        for obj in objects {
            let (h, raw) = obj?;
            w.write_all(&raw)?;
            entries.push((h, off, raw.len() as u64));
            off += raw.len() as u64;
        }
        let f = w.into_inner().map_err(|e| e.into_error())?;
        crate::platform::sync_new_file(&f)?;
        entries.sort_unstable_by_key(|e| e.0);
        entries.dedup_by_key(|e| e.0);

        let mut idx = Vec::with_capacity(entries_at() + entries.len() * ENTRY + 40);
        idx.extend_from_slice(IDX_MAGIC);
        idx.extend_from_slice(&VERSION.to_le_bytes());
        idx.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        let mut fanout = [0u32; 256];
        for (h, _, _) in &entries {
            fanout[h.0[0] as usize] += 1;
        }
        let mut total = 0;
        for n in fanout {
            total += n;
            idx.extend_from_slice(&total.to_le_bytes());
        }
        for (h, o, l) in &entries {
            idx.extend_from_slice(&h.0);
            idx.extend_from_slice(&o.to_le_bytes());
            idx.extend_from_slice(&l.to_le_bytes());
        }
        idx.extend_from_slice(&off.to_le_bytes());
        let sum = *blake3::hash(&idx).as_bytes();
        idx.extend_from_slice(&sum);
        let name = blake3::hash(&idx[entries_at()..]).to_hex()[..32].to_string();

        let tmp_idx = tmp.join(format!("idx-{}", crate::util::random_hex(16)));
        {
            let mut f = File::create(&tmp_idx)?;
            f.write_all(&idx)?;
            crate::platform::sync_new_file(&f)?;
        }
        // The pack first: a pack is visible once its index is there.
        fs::rename(&tmp_pack, dir.join(format!("{name}.pack")))?;
        fs::rename(&tmp_idx, dir.join(format!("{name}.idx")))?;
        crate::platform::sync_new_dir(dir)?;
        Ok(name)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_pack);
    }
    result
}

/// Names of the packs in `dir` (those with an index).
pub fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".idx")).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Delete a pack (its index first, so it disappears as a whole).
pub fn remove(dir: &Path, name: &str) -> Result<()> {
    for ext in ["idx", "pack"] {
        match fs::remove_file(dir.join(format!("{name}.{ext}"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing pack {name}")),
        }
    }
    Ok(())
}

/// The packs a store has open, refreshed when the directory changes.
#[derive(Default)]
pub struct PackSet {
    pub packs: Vec<Arc<Pack>>,
    /// Packs that failed to open, with why (reported by `check`).
    pub broken: Vec<String>,
    names: Vec<String>,
}

impl PackSet {
    /// Re-read the pack directory; keeps packs already open. Returns whether
    /// anything changed.
    pub fn refresh(&mut self, dir: &Path) -> bool {
        let names = names(dir);
        if names == self.names {
            return false;
        }
        let mut packs = Vec::new();
        let mut broken = Vec::new();
        for n in &names {
            if let Some(p) = self.packs.iter().find(|p| &p.name == n) {
                packs.push(p.clone());
                continue;
            }
            match Pack::open(dir, n) {
                Ok(p) => packs.push(Arc::new(p)),
                // A pack whose index appeared before its pack finished being
                // removed, or a damaged one.
                Err(e) => broken.push(format!("{e:#}")),
            }
        }
        self.packs = packs;
        self.broken = broken;
        self.names = names;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_open_find_and_detect_damage() {
        let dir = tempfile::tempdir().unwrap();
        let (packs, tmp) = (dir.path().join("pack"), dir.path().to_path_buf());
        let objects: Vec<(Hash, Vec<u8>)> = (0..300u32).map(|i| (Hash::of(&i.to_le_bytes()), vec![i as u8; i as usize % 7])).collect();
        let name = write(&packs, &tmp, objects.clone().into_iter().map(Ok)).unwrap();
        let p = Pack::open(&packs, &name).unwrap();
        assert_eq!(p.len(), 300);
        for (h, raw) in &objects {
            assert_eq!(&p.read(h).unwrap().unwrap(), raw);
        }
        assert!(p.read(&Hash::of(b"absent")).is_none());
        let mut sorted: Vec<Hash> = objects.iter().map(|o| o.0).collect();
        sorted.sort();
        assert_eq!(p.hashes().collect::<Vec<_>>(), sorted);
        assert_eq!(write(&packs, &tmp, objects.clone().into_iter().map(Ok)).unwrap(), name, "same contents, same name");

        let idx = packs.join(format!("{name}.idx"));
        let mut bytes = fs::read(&idx).unwrap();
        bytes[2000] ^= 1;
        fs::write(&idx, &bytes).unwrap();
        assert!(Pack::open(&packs, &name).is_err());
    }
}
