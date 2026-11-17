use crate::bail;
use crate::error::Result;
use std::fmt;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Content address of an object: BLAKE3 of its encoded bytes.
/// Written as hex in JSON.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(data: &[u8]) -> Self {
        Hash(*blake3::hash(data).as_bytes())
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 15) as usize] as char);
        }
        s
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        if s.len() != 64 {
            bail!("invalid hash: {s:?}");
        }
        let mut out = [0u8; 32];
        for (i, pair) in s.as_bytes().chunks(2).enumerate() {
            out[i] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Hash(out))
    }

    /// 16-hex-digit prefix, used in output.
    pub fn short(&self) -> String {
        self.to_hex()[..16].to_string()
    }
}

fn hex_nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => bail!("invalid hex digit {:?}", c as char),
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

/// Stable identity of a change. Survives amends and rewrites, unlike the
/// revision hash. Rendered with the letters `k..z` so it can never be
/// confused with a hex revision hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeId(pub [u8; 16]);

impl ChangeId {
    pub fn random() -> Self {
        let mut out = [0u8; 16];
        crate::platform::random_bytes(&mut out);
        ChangeId(out)
    }

    /// 12-letter prefix, used in output and accepted as input.
    pub fn short(&self) -> String {
        self.to_string()[..12].to_string()
    }

    pub fn is_change_id_text(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| (b'k'..=b'z').contains(&b))
    }

    pub fn parse(s: &str) -> Result<Self> {
        if s.len() != 32 || !Self::is_change_id_text(s) {
            bail!("invalid change id: {s:?}");
        }
        let b = s.as_bytes();
        let mut out = [0u8; 16];
        for (i, o) in out.iter_mut().enumerate() {
            *o = ((b[2 * i] - b'k') << 4) | (b[2 * i + 1] - b'k');
        }
        Ok(ChangeId(out))
    }
}

impl crate::json::ToJson for Hash {
    /// As 64 hex digits.
    fn write_json(&self, out: &mut String) {
        crate::json::write_str(out, &self.to_hex());
    }
}

impl crate::json::FromJson for Hash {
    fn from_json(v: &crate::json::Value) -> Result<Self> {
        Hash::from_hex(v.as_str().ok_or_else(|| crate::err!("expected a hash (hex string)"))?)
    }
}

impl crate::json::ToJson for ChangeId {
    /// As 32 letters `k`-`z`.
    fn write_json(&self, out: &mut String) {
        crate::json::write_str(out, &self.to_string());
    }
}

impl crate::json::FromJson for ChangeId {
    fn from_json(v: &crate::json::Value) -> Result<Self> {
        ChangeId::parse(v.as_str().ok_or_else(|| crate::err!("expected a change id (string)"))?)
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::with_capacity(32);
        for b in self.0 {
            s.push((b'k' + (b >> 4)) as char);
            s.push((b'k' + (b & 15)) as char);
        }
        f.write_str(&s)
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({})", self.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let h = Hash::of(b"hello");
        assert_eq!(Hash::from_hex(&h.to_hex()).unwrap(), h);
    }

    #[test]
    fn change_id_alphabet() {
        let id = ChangeId::random();
        assert!(ChangeId::is_change_id_text(&id.to_string()));
        assert_eq!(id.to_string().len(), 32);
        assert_eq!(ChangeId::parse(&id.to_string()).unwrap(), id);
    }
}
