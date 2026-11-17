//! psync's compact binary encoding, used for trees (part of every tree's
//! hash, so it must never change for a given `FORMAT_VERSION`) and for the
//! stat-cache index.
//!
//! - unsigned integers and lengths: LEB128 varint (7 bits per byte, low
//!   bits first, high bit set on every byte but the last);
//! - signed integers: zigzag, then varint;
//! - strings and byte strings: length, then the bytes (strings are UTF-8);
//! - enums: variant number as a varint;
//! - hashes: the 32 raw bytes;
//! - options: byte 0 (none) or 1 followed by the value;
//! - sequences and maps: count, then the items (maps as key, value pairs).
//!
//! The layout is the same as the `postcard` format psync used before, so
//! existing trees keep their hashes.

use crate::bail;
use crate::error::{Context, Result};
use crate::hash::Hash;

/// Builds an encoded value.
#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn u64(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    pub fn i64(&mut self, v: i64) {
        self.u64(((v << 1) ^ (v >> 63)) as u64);
    }

    pub fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }

    pub fn str(&mut self, s: &str) {
        self.len(s.len());
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn hash(&mut self, h: &Hash) {
        self.buf.extend_from_slice(&h.0);
    }

    pub fn bool(&mut self, b: bool) {
        self.buf.push(b as u8);
    }
}

/// Reads an encoded value; every read fails cleanly on truncated input.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).context("truncated data")?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u64(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.take(1)?[0];
            // The tenth byte may only carry the top bit of a u64.
            if shift == 63 && b > 1 {
                bail!("varint overflows 64 bits");
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("varint longer than 10 bytes")
    }

    pub fn i64(&mut self) -> Result<i64> {
        let u = self.u64()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    /// A count of items that each take at least `min_item` bytes, so a
    /// corrupt count cannot make the caller allocate unbounded memory.
    pub fn len(&mut self, min_item: usize) -> Result<usize> {
        let n = usize::try_from(self.u64()?).ok().context("length too large")?;
        if n.saturating_mul(min_item.max(1)) > self.data.len() - self.pos {
            bail!("length {n} exceeds the data");
        }
        Ok(n)
    }

    pub fn str(&mut self) -> Result<String> {
        let n = self.len(1)?;
        Ok(std::str::from_utf8(self.take(n)?).context("invalid UTF-8 in string")?.to_string())
    }

    pub fn hash(&mut self) -> Result<Hash> {
        Ok(Hash(self.take(32)?.try_into().expect("took 32 bytes")))
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            b => bail!("invalid boolean byte {b}"),
        }
    }

    /// Fail unless all input was consumed.
    pub fn finish(&self) -> Result<()> {
        if self.pos != self.data.len() {
            bail!("{} unexpected trailing bytes", self.data.len() - self.pos);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip() {
        let us = [0, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX - 1, u64::MAX];
        let is = [0, 1, -1, 63, -64, 64, -65, i64::MAX, i64::MIN, 1_700_000_000_000_000_000];
        let mut w = Writer::default();
        for &u in &us {
            w.u64(u);
        }
        for &i in &is {
            w.i64(i);
        }
        let mut r = Reader::new(&w.buf);
        for &u in &us {
            assert_eq!(r.u64().unwrap(), u);
        }
        for &i in &is {
            assert_eq!(r.i64().unwrap(), i);
        }
        r.finish().unwrap();
        // Known encodings.
        let enc = |u: u64| {
            let mut w = Writer::default();
            w.u64(u);
            w.buf
        };
        assert_eq!(enc(300), [0xac, 0x02]);
        assert_eq!(enc(u64::MAX).len(), 10);
    }

    #[test]
    fn corrupt_input_is_an_error() {
        assert!(Reader::new(&[0x80]).u64().is_err()); // truncated
        assert!(Reader::new(&[0xff; 11]).u64().is_err()); // too long
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]).u64().is_err());
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0x0f]).len(1).is_err()); // count beyond the data
        assert!(Reader::new(&[2, 0xc3, 0x28]).str().is_err()); // invalid UTF-8
        assert!(Reader::new(&[1, 2]).finish().is_err());
    }
}
