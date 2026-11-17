//! Line-ending conversion (T-27), opt-in.
//!
//! Off by default: files are stored exactly as they are on disk (like git
//! with `autocrlf = false`). Conversion applies to a path when the
//! `text.eol` setting is set, or when an attribute file says so:
//!
//! - `text.eol` = `lf`, `crlf` or `native` (CRLF on Windows, LF elsewhere):
//!   every text file is stored with LF and checked out with that ending
//!   (as if `* text=auto eol=<that>`). Per repository, or global.
//! - `.gitattributes` and `.psyncattributes` at the workspace root (the
//!   second wins), in git's syntax: `pattern attr...` with `text`, `-text`,
//!   `binary`, `text=auto`, `eol=lf`, `eol=crlf` (which implies `text`),
//!   and the old `crlf` / `-crlf`. A pattern with a `/` is matched against
//!   the path from the root, one without against the file name; the last
//!   matching line sets each attribute.
//!
//! A converted file is stored with CRLF replaced by LF, and written out with
//! the chosen ending. `text=auto` (and `text.eol`) only converts text: no NUL
//! byte in the first 8000 bytes. Symlinks and files larger than
//! [`crate::chunk::LARGE`] are never converted. Like git, changing the rules
//! does not touch files that have not changed: they are converted when
//! they are next modified or checked out.

use crate::bail;
use crate::error::Result;
use crate::glob::Glob;
use std::path::Path;

/// Attribute files read at the root, in increasing precedence.
pub const FILE_NAMES: [&str; 2] = [".gitattributes", ".psyncattributes"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Lf,
    Crlf,
}

impl Ending {
    /// The `text.eol` setting's value.
    pub fn parse(value: &str) -> Result<Option<Ending>> {
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "" | "off" | "false" | "none" => None,
            "lf" => Some(Ending::Lf),
            "crlf" => Some(Ending::Crlf),
            "native" | "auto" | "true" => Some(Ending::native()),
            other => bail!("text.eol must be lf, crlf, native or off, not '{other}'"),
        })
    }

    pub fn native() -> Ending {
        if cfg!(windows) { Ending::Crlf } else { Ending::Lf }
    }
}

/// The configured `text.eol` for the repository at `dot`.
pub fn configured(dot: &Path) -> Result<Option<Ending>> {
    match crate::config::get(Some(dot), "text.eol") {
        Some((v, _)) => Ending::parse(&v),
        None => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
    Auto,
    Yes,
    No,
}

#[derive(Debug)]
struct Rule {
    glob: Glob,
    anchored: bool,
    text: Option<Text>,
    eol: Option<Ending>,
}

/// The conversion rules of one workspace.
#[derive(Debug, Default)]
pub struct Eol {
    config: Option<Ending>,
    rules: Vec<Rule>,
}

fn parse(text: &str, rules: &mut Vec<Rule>) {
    for line in text.lines() {
        let mut words = line.split_ascii_whitespace();
        let Some(pattern) = words.next() else { continue };
        if pattern.starts_with('#') || pattern.starts_with('"') || pattern.starts_with('!') || pattern.ends_with('/') {
            continue; // comments; quoted, negated and directory patterns are not supported
        }
        let (mut text, mut eol) = (None, None);
        for w in words {
            match w {
                "text" | "crlf" => text = Some(Text::Yes),
                "-text" | "binary" | "-crlf" => text = Some(Text::No),
                "text=auto" => text = Some(Text::Auto),
                "eol=lf" => eol = Some(Ending::Lf),
                "eol=crlf" => eol = Some(Ending::Crlf),
                _ => {}
            }
        }
        if text.is_none() && eol.is_none() {
            continue;
        }
        let anchored = pattern.contains('/');
        if let Ok(glob) = Glob::new(pattern.strip_prefix('/').unwrap_or(pattern)) {
            rules.push(Rule { glob, anchored, text, eol });
        }
    }
}

/// Whether `data` looks like text (git's test: no NUL near the start).
pub fn is_text(data: &[u8]) -> bool {
    !data[..data.len().min(8000)].contains(&0)
}

impl Eol {
    /// Rules from the `text.eol` setting and the attribute files' texts
    /// (in [`FILE_NAMES`] order).
    pub fn new(config: Option<Ending>, attributes: &[&str]) -> Eol {
        let mut rules = Vec::new();
        for a in attributes {
            parse(a, &mut rules);
        }
        Eol { config, rules }
    }

    /// Rules for the working files under `root`.
    pub fn from_disk(config: Option<Ending>, root: &Path) -> Eol {
        let texts: Vec<String> = FILE_NAMES.iter().filter_map(|n| std::fs::read_to_string(root.join(n)).ok()).collect();
        Eol::new(config, &texts.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Nothing is ever converted.
    pub fn is_off(&self) -> bool {
        self.config.is_none() && self.rules.is_empty()
    }

    /// How `rel` is converted, if it is.
    fn decide(&self, rel: &str) -> Option<(Text, Ending)> {
        if self.is_off() {
            return None;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let (mut text, mut eol) = (None, None);
        for r in &self.rules {
            if r.glob.matches(if r.anchored { rel } else { name }) {
                text = r.text.or(text);
                eol = r.eol.or(eol);
            }
        }
        let text = match (text, eol) {
            (Some(Text::No), _) => return None,
            (Some(t), _) => t,
            (None, Some(_)) => Text::Yes,
            (None, None) => self.config.map(|_| Text::Auto)?,
        };
        Some((text, eol.or(self.config).unwrap_or_else(Ending::native)))
    }

    /// Whether `rel` may be converted at all (then it is read whole).
    pub fn applies(&self, rel: &str) -> bool {
        self.decide(rel).is_some()
    }

    /// The contents to store for working file `rel`, if not `data` itself.
    pub fn to_store(&self, rel: &str, data: &[u8]) -> Option<Vec<u8>> {
        let (text, _) = self.decide(rel)?;
        if (text == Text::Auto && !is_text(data)) || !data.windows(2).any(|w| w == b"\r\n") {
            return None;
        }
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i] == b'\r' && data.get(i + 1) == Some(&b'\n') {
                i += 1;
                continue;
            }
            out.push(data[i]);
            i += 1;
        }
        Some(out)
    }

    /// The contents to write for stored file `rel`, if not `data` itself.
    pub fn to_disk(&self, rel: &str, data: &[u8]) -> Option<Vec<u8>> {
        let (text, ending) = self.decide(rel)?;
        if ending == Ending::Lf || (text == Text::Auto && !is_text(data)) || !data.contains(&b'\n') {
            return None;
        }
        let mut out = Vec::with_capacity(data.len() + data.len() / 32);
        for (i, &b) in data.iter().enumerate() {
            // A CRLF already stored is kept as is.
            if b == b'\n' && (i == 0 || data[i - 1] != b'\r') {
                out.push(b'\r');
            }
            out.push(b);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_unless_configured_or_attributed() {
        let off = Eol::new(None, &[]);
        assert!(off.is_off());
        assert_eq!(off.to_store("a.txt", b"a\r\n"), None);
        assert_eq!(off.to_disk("a.txt", b"a\n"), None);
        // Attributes that say nothing about text do not turn it on.
        assert!(Eol::new(None, &["*.rs diff=rust\n# comment\n"]).is_off());
    }

    #[test]
    fn setting_converts_text_both_ways_and_leaves_binary_alone() {
        let crlf = Eol::new(Some(Ending::Crlf), &[]);
        assert_eq!(crlf.to_store("a.txt", b"a\r\nb\r\nc").unwrap(), b"a\nb\nc");
        assert_eq!(crlf.to_disk("a.txt", b"a\nb\r\nc\n").unwrap(), b"a\r\nb\r\nc\r\n");
        assert_eq!(crlf.to_store("a.bin", b"a\0\r\n"), None);
        assert_eq!(crlf.to_disk("a.bin", b"a\0\n"), None);
        assert_eq!(crlf.to_store("a.txt", b"a\nb\n"), None, "already LF");
        let lf = Eol::new(Some(Ending::Lf), &[]);
        assert_eq!(lf.to_store("a.txt", b"a\r\n").unwrap(), b"a\n");
        assert_eq!(lf.to_disk("a.txt", b"a\n"), None);
    }

    #[test]
    fn attributes_choose_per_path_and_the_last_match_wins() {
        let e = Eol::new(
            None,
            &["*.txt text\n*.bat eol=crlf\n*.sh text eol=lf\ndocs/*.md text=auto eol=crlf\n", "special.txt -text\nvendor/* binary\n"],
        );
        assert_eq!(e.to_store("x/a.txt", b"a\r\n").unwrap(), b"a\n");
        assert_eq!(e.to_store("x/a.txt", b"\0\r\n").unwrap(), b"\0\n", "`text` converts even binary-looking files");
        assert_eq!(e.to_store("special.txt", b"a\r\n"), None);
        assert_eq!(e.to_disk("run.bat", b"x\n").unwrap(), b"x\r\n");
        assert_eq!(e.to_disk("run.sh", b"x\n"), None);
        assert_eq!(e.to_disk("docs/a.md", b"x\n").unwrap(), b"x\r\n");
        assert_eq!(e.to_disk("other/docs/a.md", b"x\n"), None, "anchored at the root");
        assert_eq!(e.to_store("vendor/lib.c", b"a\r\n"), None);
        assert_eq!(e.to_store("a.rs", b"a\r\n"), None, "no rule, no setting");
        // A setting fills in for paths without rules, never against `-text`.
        let e = Eol::new(Some(Ending::Crlf), &["special.txt -text\n*.sh eol=lf\n"]);
        assert_eq!(e.to_disk("a.rs", b"a\n").unwrap(), b"a\r\n");
        assert_eq!(e.to_disk("special.txt", b"a\n"), None);
        assert_eq!(e.to_disk("x.sh", b"a\n"), None);
    }

    #[test]
    fn setting_values() {
        assert_eq!(Ending::parse("CRLF").unwrap(), Some(Ending::Crlf));
        assert_eq!(Ending::parse("off").unwrap(), None);
        assert_eq!(Ending::parse("native").unwrap(), Some(Ending::native()));
        assert!(Ending::parse("dos").is_err());
    }
}
