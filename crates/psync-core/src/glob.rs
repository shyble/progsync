//! Glob patterns over `/`-separated paths.
//!
//! - `*` matches any run of characters except `/`; `?` one character except `/`;
//! - `[abc]`, `[a-z]`, `[!a]` / `[^a]`: one character from (or not from) a set,
//!   never `/`;
//! - `**` as a whole path component: `**/x` (any directories, or none,
//!   before `x`), `a/**/b` (any directories between, or none), `a/**`
//!   (everything inside `a`); elsewhere `**` is the same as `*`;
//! - `\x` matches `x` literally;
//! - `{a,b}` alternatives (only with [`Glob::with_braces`]).
//!
//! Patterns made only of literal text, `*.ext` or `prefix*` are matched with
//! plain string comparisons; others with a small dynamic program.

use crate::bail;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Lit(char),
    /// `?`
    One,
    /// `*`: any characters but `/`.
    Star,
    /// `**` at the end (after `/`, or alone): anything, `/` included.
    All,
    /// `**/`: nothing, or anything ending in `/`.
    Dirs,
    Class { negated: bool, ranges: Vec<(char, char)> },
}

#[derive(Debug, Clone)]
enum Kind {
    Exact(String),
    /// `*suffix` (suffix without `/`).
    Suffix(String),
    /// `prefix*` (prefix without `/`).
    Prefix(String),
    General(Vec<Tok>),
}

/// A compiled pattern (with brace alternatives: any of them).
#[derive(Debug, Clone)]
pub struct Glob {
    alts: Vec<Kind>,
}

impl Glob {
    /// A pattern without brace alternatives (`{`, `}` are literal).
    pub fn new(pattern: &str) -> Result<Glob> {
        Ok(Glob { alts: vec![compile(pattern)?] })
    }

    /// A pattern where `{a,b}` lists alternatives.
    pub fn with_braces(pattern: &str) -> Result<Glob> {
        let alts = expand_braces(pattern).iter().map(|p| compile(p)).collect::<Result<_>>()?;
        Ok(Glob { alts })
    }

    pub fn matches(&self, text: &str) -> bool {
        self.alts.iter().any(|k| match k {
            Kind::Exact(s) => text == s,
            Kind::Suffix(s) => text.ends_with(s.as_str()) && !text[..text.len() - s.len()].contains('/'),
            Kind::Prefix(s) => text.starts_with(s.as_str()) && !text[s.len()..].contains('/'),
            Kind::General(toks) => run(toks, text),
        })
    }
}

/// Whether a pattern uses any glob syntax (so it is not a plain path).
pub fn has_magic(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{', '\\'])
}

fn compile(p: &str) -> Result<Kind> {
    let chars: Vec<char> = p.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let Some(&n) = chars.get(i + 1) else { bail!("pattern '{p}' ends with a lone '\\'") };
                toks.push(Tok::Lit(n));
                i += 2;
            }
            '?' => {
                toks.push(Tok::One);
                i += 1;
            }
            '*' => {
                let mut j = i;
                while j < chars.len() && chars[j] == '*' {
                    j += 1;
                }
                let at_start = i == 0 || chars[i - 1] == '/';
                if j - i >= 2 && at_start && j == chars.len() {
                    toks.push(Tok::All);
                    i = j;
                } else if j - i >= 2 && at_start && chars[j] == '/' {
                    toks.push(Tok::Dirs);
                    i = j + 1;
                } else {
                    toks.push(Tok::Star);
                    i = j;
                }
            }
            '[' => match class(&chars, i) {
                Some((tok, end)) => {
                    toks.push(tok);
                    i = end;
                }
                None => {
                    toks.push(Tok::Lit('['));
                    i += 1;
                }
            },
            _ => {
                toks.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    let lit = |ts: &[Tok]| -> Option<String> {
        ts.iter().map(|t| if let Tok::Lit(c) = t { Some(*c) } else { None }).collect()
    };
    Ok(if let Some(s) = lit(&toks) {
        Kind::Exact(s)
    } else if let Some(s) = toks.split_first().filter(|(f, _)| **f == Tok::Star).and_then(|(_, r)| lit(r)).filter(|s| !s.contains('/')) {
        Kind::Suffix(s)
    } else if let Some(s) = toks.split_last().filter(|(l, _)| **l == Tok::Star).and_then(|(_, r)| lit(r)).filter(|s| !s.contains('/')) {
        Kind::Prefix(s)
    } else {
        Kind::General(toks)
    })
}

/// A `[...]` class starting at `chars[i]`; `None` if it is not closed.
fn class(chars: &[char], i: usize) -> Option<(Tok, usize)> {
    let mut j = i + 1;
    let negated = matches!(chars.get(j), Some('!' | '^'));
    if negated {
        j += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let mut c = *chars.get(j)?;
        if c == ']' && !first {
            return Some((Tok::Class { negated, ranges }, j + 1));
        }
        first = false;
        if c == '\\' {
            j += 1;
            c = *chars.get(j)?;
        }
        if chars.get(j + 1) == Some(&'-') && chars.get(j + 2).is_some_and(|&e| e != ']') {
            let mut e = chars[j + 2];
            let mut skip = 3;
            if e == '\\' {
                e = *chars.get(j + 3)?;
                skip = 4;
            }
            ranges.push((c, e));
            j += skip;
        } else {
            ranges.push((c, c));
            j += 1;
        }
    }
}

fn class_matches(negated: bool, ranges: &[(char, char)], c: char) -> bool {
    c != '/' && ranges.iter().any(|&(a, b)| a <= c && c <= b) != negated
}

/// Match by dynamic programming: `ok[t][s]` = tokens from `t` match text
/// from `s`. Computed right to left, one row per token.
fn run(toks: &[Tok], text: &str) -> bool {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    // Row for "no tokens left": matches only the empty rest.
    let mut next: Vec<bool> = (0..=n).map(|s| s == n).collect();
    let mut cur = vec![false; n + 1];
    for t in toks.iter().rev() {
        match t {
            Tok::Lit(_) | Tok::One | Tok::Class { .. } => {
                cur[n] = false;
                for s in 0..n {
                    let c = cs[s];
                    let ok = match t {
                        Tok::Lit(l) => c == *l,
                        Tok::One => c != '/',
                        Tok::Class { negated, ranges } => class_matches(*negated, ranges, c),
                        _ => unreachable!(),
                    };
                    cur[s] = ok && next[s + 1];
                }
            }
            Tok::Star => {
                cur[n] = next[n];
                for s in (0..n).rev() {
                    cur[s] = next[s] || (cs[s] != '/' && cur[s + 1]);
                }
            }
            Tok::All => {
                cur[n] = next[n];
                for s in (0..n).rev() {
                    cur[s] = next[s] || cur[s + 1];
                }
            }
            Tok::Dirs => {
                // Nothing, or any run ending in '/'.
                let mut slash_then = false;
                cur[n] = next[n];
                for s in (0..n).rev() {
                    slash_then = slash_then || (cs[s] == '/' && next[s + 1]);
                    cur[s] = next[s] || slash_then;
                }
            }
        }
        std::mem::swap(&mut cur, &mut next);
    }
    next[0]
}

/// `a{b,c}d` -> `abd`, `acd` (nested braces allowed; unbalanced braces are
/// taken literally).
fn expand_braces(p: &str) -> Vec<String> {
    let cs: Vec<char> = p.chars().collect();
    let mut depth = 0;
    let mut open = None;
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\\' => i += 1,
            '{' => {
                if depth == 0 {
                    open = Some(i);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let o = open.expect("set with depth");
                    let (pre, post): (String, String) = (cs[..o].iter().collect(), cs[i + 1..].iter().collect());
                    let mut parts = Vec::new();
                    let (mut d, mut start) = (0, o + 1);
                    let mut k = o + 1;
                    while k < i {
                        match cs[k] {
                            '\\' => k += 1,
                            '{' => d += 1,
                            '}' => d -= 1,
                            ',' if d == 0 => {
                                parts.push(cs[start..k].iter().collect::<String>());
                                start = k + 1;
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    parts.push(cs[start..i].iter().collect());
                    return parts.iter().flat_map(|m| expand_braces(&format!("{pre}{m}{post}"))).collect();
                }
            }
            _ => {}
        }
        i += 1;
    }
    vec![p.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        Glob::with_braces(p).unwrap().matches(t)
    }

    #[test]
    fn basics() {
        assert!(m("a.rs", "a.rs") && !m("a.rs", "b.rs"));
        assert!(m("*.rs", "a.rs") && m("*.rs", ".rs") && !m("*.rs", "src/a.rs"));
        assert!(m("src/*", "src/a") && !m("src/*", "src/a/b"));
        assert!(m("a?c", "abc") && !m("a?c", "a/c") && !m("a?c", "ac"));
        assert!(m("f[0-9].txt", "f7.txt") && !m("f[0-9].txt", "fa.txt"));
        assert!(m("f[!0-9]", "fa") && !m("f[!0-9]", "f1") && m("f[^0-9]", "fa"));
        assert!(m("[]x]", "]") && m("[a-]", "-"));
        assert!(m("a[", "a[") && m("\\*", "*") && !m("\\*", "x"));
        assert!(m("x*y*z", "xaybz") && !m("x*y*z", "xa/ybz"));
        assert!(m("ä?", "äö"));
    }

    #[test]
    fn globstar() {
        assert!(m("**/*.md", "README.md") && m("**/*.md", "docs/a/b.md"));
        assert!(m("a/**/b", "a/b") && m("a/**/b", "a/x/y/b") && !m("a/**/b", "ab") && !m("a/**/b", "a/xb"));
        assert!(m("a/**", "a/x") && m("a/**", "a/x/y") && !m("a/**", "b/x"));
        assert!(m("**", "any/thing"));
        assert!(m("a**b", "axxb") && !m("a**b", "ax/b"), "** inside a component is *");
        assert!(m("**/target", "target") && m("**/target", "x/target") && !m("**/target", "xtarget"));
    }

    #[test]
    fn braces() {
        assert!(m("src/{a,b}.rs", "src/a.rs") && m("src/{a,b}.rs", "src/b.rs") && !m("src/{a,b}.rs", "src/c.rs"));
        assert!(m("{x,y{1,2}}", "y2") && m("a{,b}", "a"));
        assert!(m("a{b", "a{b"), "unbalanced brace is literal");
        assert!(Glob::new("{a,b}").unwrap().matches("{a,b}"), "no braces in plain mode");
    }
}
