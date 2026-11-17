//! `archive`: a revision's files as a tar stream, for building or deploying
//! exactly that revision elsewhere.
//!
//! The output is POSIX ustar with pax extended headers where ustar falls
//! short (paths over 100 bytes, link targets over 100 bytes, files of 8 GiB
//! or more). Like `git archive`, it starts with a pax global header whose
//! `comment` is the revision hash, so an archive says what it was built from.
//! It is reproducible: entries are in path order, every timestamp is the
//! revision's, owners are 0, and modes are 0644/0755.
//!
//! Files are written as stored (no `text.eol` conversion), and large files
//! are streamed chunk by chunk.

use crate::bail;
use crate::diff;
use crate::error::Result;
use crate::hash::Hash;
use crate::object::EntryKind;
use crate::paths::PathSpec;
use crate::store::Store;
use std::collections::BTreeSet;
use std::io::Write;

const BLOCK: usize = 512;
/// The largest size an ustar header holds (11 octal digits).
const USTAR_MAX_SIZE: u64 = 0o77777777777;

pub struct ArchiveStats {
    pub files: usize,
    /// Bytes of file content (not counting headers).
    pub bytes: u64,
}

/// Write `tree` (of revision `revision`, dated `mtime` seconds) as a tar to
/// `w`, every path under `prefix` (e.g. `app-1.2/`), keeping only paths
/// matching one of `specs` (all when empty).
pub fn write_tar(
    store: &Store,
    tree: Hash,
    revision: Hash,
    mtime: u64,
    prefix: &str,
    specs: &[PathSpec],
    w: &mut dyn Write,
) -> Result<ArchiveStats> {
    let prefix = match prefix.trim_matches('/') {
        "" => String::new(),
        p => format!("{p}/"),
    };
    let files: Vec<(String, (EntryKind, Hash))> = diff::flatten(store, tree)?
        .into_iter()
        .filter(|(p, _)| specs.is_empty() || specs.iter().any(|s| s.matches(p)))
        .collect();
    for s in specs {
        if !files.iter().any(|(p, _)| s.matches(p)) {
            bail!("'{}' matches no file in that revision", s.raw);
        }
    }

    let mut t = Tar { w, mtime };
    t.pax(b'g', "pax_global_header", &[("comment", revision.to_string().as_bytes())])?;
    if !prefix.is_empty() {
        t.entry(&prefix, b'5', 0o755, 0, b"")?;
    }
    let mut dirs_written: BTreeSet<String> = BTreeSet::new();
    let mut stats = ArchiveStats { files: 0, bytes: 0 };
    for (path, (kind, hash)) in &files {
        // Parent directories first, each once (paths come sorted).
        let mut at = 0;
        while let Some(i) = path[at..].find('/') {
            let dir = &path[..at + i];
            if dirs_written.insert(dir.to_string()) {
                t.entry(&format!("{prefix}{dir}/"), b'5', 0o755, 0, b"")?;
            }
            at += i + 1;
        }
        let name = format!("{prefix}{path}");
        match kind {
            EntryKind::Symlink => {
                let target = store.get_blob(hash)?;
                t.entry(&name, b'2', 0o777, 0, &target)?;
            }
            EntryKind::File | EntryKind::Exec => {
                let size = store.file_size(hash)?;
                let mode = if *kind == EntryKind::Exec { 0o755 } else { 0o644 };
                t.entry(&name, b'0', mode, size, b"")?;
                let mut counted = Counting { w: &mut *t.w, n: 0 };
                store.write_file_to(hash, &mut counted)?;
                if counted.n != size {
                    bail!("{path}: stored size {size} but {} bytes read", counted.n);
                }
                t.pad(size)?;
                stats.files += 1;
                stats.bytes += size;
            }
            EntryKind::Tree => {}
        }
    }
    // End of archive: two zero blocks.
    t.w.write_all(&[0u8; BLOCK * 2])?;
    t.w.flush()?;
    Ok(stats)
}

struct Tar<'a> {
    w: &'a mut dyn Write,
    mtime: u64,
}

impl Tar<'_> {
    /// One entry's header, preceded by a pax header for what ustar cannot hold.
    fn entry(&mut self, name: &str, kind: u8, mode: u32, size: u64, link: &[u8]) -> Result<()> {
        let mut pax: Vec<(&str, &[u8])> = Vec::new();
        let size_text = size.to_string();
        if name.len() > 100 {
            pax.push(("path", name.as_bytes()));
        }
        if link.len() > 100 {
            pax.push(("linkpath", link));
        }
        if size > USTAR_MAX_SIZE {
            pax.push(("size", size_text.as_bytes()));
        }
        if !pax.is_empty() {
            self.pax(b'x', "PaxHeader", &pax)?;
        }
        let mut h = [0u8; BLOCK];
        put(&mut h[0..100], &name.as_bytes()[..name.len().min(100)]);
        octal(&mut h[100..108], mode as u64);
        octal(&mut h[108..116], 0); // uid
        octal(&mut h[116..124], 0); // gid
        octal(&mut h[124..136], if size > USTAR_MAX_SIZE { 0 } else { size });
        octal(&mut h[136..148], self.mtime);
        h[156] = kind;
        put(&mut h[157..257], &link[..link.len().min(100)]);
        put(&mut h[257..263], b"ustar\0");
        put(&mut h[263..265], b"00");
        checksum(&mut h);
        self.w.write_all(&h)?;
        Ok(())
    }

    /// A pax extended header (`x`: for the next entry, `g`: global).
    fn pax(&mut self, kind: u8, name: &str, records: &[(&str, &[u8])]) -> Result<()> {
        let mut body = Vec::new();
        for (key, value) in records {
            // "<len> <key>=<value>\n", where <len> counts itself.
            let rest = key.len() + value.len() + 3;
            let mut len = rest + 1;
            while len.to_string().len() + rest != len {
                len = len.to_string().len() + rest;
            }
            body.extend_from_slice(format!("{len} {key}=").as_bytes());
            body.extend_from_slice(value);
            body.push(b'\n');
        }
        let mut h = [0u8; BLOCK];
        put(&mut h[0..100], name.as_bytes());
        octal(&mut h[100..108], 0o644);
        octal(&mut h[108..116], 0);
        octal(&mut h[116..124], 0);
        octal(&mut h[124..136], body.len() as u64);
        octal(&mut h[136..148], self.mtime);
        h[156] = kind;
        put(&mut h[257..263], b"ustar\0");
        put(&mut h[263..265], b"00");
        checksum(&mut h);
        self.w.write_all(&h)?;
        self.w.write_all(&body)?;
        self.pad(body.len() as u64)
    }

    /// Zero-fill to the end of the current block.
    fn pad(&mut self, len: u64) -> Result<()> {
        let rem = (len % BLOCK as u64) as usize;
        if rem != 0 {
            self.w.write_all(&[0u8; BLOCK][..BLOCK - rem])?;
        }
        Ok(())
    }
}

fn put(field: &mut [u8], value: &[u8]) {
    field[..value.len()].copy_from_slice(value);
}

/// Octal digits, zero-padded, NUL-terminated, filling the field.
fn octal(field: &mut [u8], n: u64) {
    let digits = format!("{n:0width$o}", width = field.len() - 1);
    put(field, digits.as_bytes());
    field[field.len() - 1] = 0;
}

fn checksum(h: &mut [u8; BLOCK]) {
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let digits = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(digits.as_bytes());
}

struct Counting<'a> {
    w: &'a mut dyn Write,
    n: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.w.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_lengths_count_themselves() {
        let mut out = Vec::new();
        let mut t = Tar { w: &mut out, mtime: 0 };
        t.pax(b'x', "PaxHeader", &[("path", "a".repeat(95).as_bytes()), ("comment", b"abc")]).unwrap();
        let body = &out[BLOCK..];
        let mut at = 0;
        for _ in 0..2 {
            let sp = body[at..].iter().position(|&b| b == b' ').unwrap();
            let len: usize = std::str::from_utf8(&body[at..at + sp]).unwrap().parse().unwrap();
            assert_eq!(body[at + len - 1], b'\n', "record of {len} bytes ends with a newline");
            at += len;
        }
        assert_eq!(out.len() % BLOCK, 0);
    }

    #[test]
    fn header_checksum_and_octal_fields() {
        let mut f = [0u8; 8];
        octal(&mut f, 0o755);
        assert_eq!(&f, b"0000755\0");
        let mut out = Vec::new();
        Tar { w: &mut out, mtime: 1 }.entry("a.txt", b'0', 0o644, 3, b"").unwrap();
        let h: &[u8] = &out[..BLOCK];
        let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
        let sum: u32 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { b' ' as u32 } else { b as u32 }).sum();
        assert_eq!(stored, sum);
    }
}
