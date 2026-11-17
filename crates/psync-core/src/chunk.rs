//! Content-defined chunking for large files (FastCDC-style, with
//! normalized chunking).
//!
//! A file larger than [`LARGE`] is stored as chunks (ordinary blobs) plus a
//! chunk list ([`crate::object::Chunked`]). Where a chunk ends depends only on
//! the bytes near the cut (a rolling "gear" hash over the last 64 bytes), so
//! an edit changes only the chunks around it and the rest are shared with
//! the previous version. Files are split while being read: at most
//! [`MAX`] bytes are held at a time.
//!
//! The parameters and the gear table are part of every large file's hash:
//! they must not change.

use crate::error::Result;
use std::io::Read;

/// Files larger than this are chunked.
pub const LARGE: usize = 4 << 20;
/// Chunk sizes: at least `MIN` (except a file's last chunk), around `AVG`,
/// at most `MAX` (so a chunk is never itself large).
pub const MIN: usize = 256 << 10;
pub const AVG: usize = 1 << 20;
pub const MAX: usize = 4 << 20;

// A cut needs the top 22 bits of the hash to be zero before `AVG` (less
// likely) and the top 18 after it (more likely): sizes cluster around `AVG`.
const MASK_S: u64 = !0 << (64 - 22);
const MASK_L: u64 = !0 << (64 - 18);

const GEAR: [u64; 256] = gear();

/// 256 fixed pseudo-random values (SplitMix64 from a fixed seed).
const fn gear() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut s: u64 = 0x7073_796e_635f_6364; // "psync_cd"
    let mut i = 0;
    while i < 256 {
        s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        t[i] = z ^ (z >> 31);
        i += 1;
    }
    t
}

/// Length of the chunk that starts `data`. Looks at most at `MAX` bytes, so
/// for a stream it is enough to pass `MAX` bytes (or all that is left).
pub fn cut(data: &[u8]) -> usize {
    let n = data.len();
    if n <= MIN {
        return n;
    }
    let (avg, max) = (n.min(AVG), n.min(MAX));
    let mut h: u64 = 0;
    let mut i = MIN;
    while i < avg {
        h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
        if h & MASK_S == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < max {
        h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
        if h & MASK_L == 0 {
            return i + 1;
        }
        i += 1;
    }
    max
}

/// Split `start` followed by the rest of `r` into chunks, calling `f` with
/// each in order.
pub fn split(r: &mut dyn Read, start: Vec<u8>, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut buf = start;
    let mut eof = false;
    loop {
        if !eof && buf.len() < MAX {
            let want = MAX - buf.len();
            let got = r.take(want as u64).read_to_end(&mut buf)?;
            eof = got < want;
        }
        if buf.is_empty() {
            return Ok(());
        }
        let n = cut(&buf);
        f(&buf[..n])?;
        buf.drain(..n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes.
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    fn chunks(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        // A reader that returns little at a time, as pipes and sockets do.
        struct Slow<'a>(&'a [u8]);
        impl Read for Slow<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(self.0.len()).min(70_001);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        split(&mut Slow(data), Vec::new(), |c| {
            out.push(c.to_vec());
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn chunks_cover_the_input_within_bounds() {
        let data = noise(20 << 20, 1);
        let cs = chunks(&data);
        assert_eq!(cs.concat(), data);
        assert!(cs[..cs.len() - 1].iter().all(|c| c.len() >= MIN && c.len() <= MAX));
        let avg = data.len() / cs.len();
        assert!(avg > AVG / 2 && avg < AVG * 2, "average chunk {avg}");
        // Streaming and whole-buffer splitting agree.
        let mut whole = Vec::new();
        let mut rest = &data[..];
        while !rest.is_empty() {
            let n = cut(rest);
            whole.push(rest[..n].to_vec());
            rest = &rest[n..];
        }
        assert_eq!(whole, cs);
        // Uniform input has no cut points: maximal chunks.
        assert!(chunks(&vec![0u8; 9 << 20])[..2].iter().all(|c| c.len() == MAX));
    }

    #[test]
    fn an_edit_changes_only_nearby_chunks() {
        let data = noise(16 << 20, 2);
        let mut edited = data.clone();
        edited.splice(8 << 20..(8 << 20) + 10, b"inserted bytes, a different length".iter().copied());
        let (a, b) = (chunks(&data), chunks(&edited));
        let shared = b.iter().filter(|c| a.contains(c)).count();
        assert!(shared + 2 >= b.len(), "{shared} of {} chunks shared", b.len());
    }
}
