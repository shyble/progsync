//! Path patterns used by claims and partial commits.
//!
//! A pattern is relative to the workspace root and matches:
//! - itself (`src/auth.rs`), and
//! - everything below it (`src/auth` or `src/auth/` matches `src/auth/x.rs`).
//!
//! Glob syntax (`*`, `?`, `[..]`, `**`, `{a,b}`; see [`crate::glob`]) is
//! supported; `*` does not cross `/`.

use crate::bail;
use crate::error::Result;
use crate::glob::Glob;

#[derive(Debug, Clone)]
pub struct PathSpec {
    /// Normalized pattern text.
    pub raw: String,
    /// The pattern itself, and everything below it.
    globs: [Glob; 2],
}

impl PathSpec {
    pub fn new(pattern: &str) -> Result<PathSpec> {
        let pattern_slashed = crate::platform::slash_path(pattern);
        let norm = pattern_slashed.trim().trim_start_matches("./").trim_end_matches('/');
        if norm.is_empty() || norm.starts_with('/') || norm.split('/').any(|c| c == "..") {
            bail!("invalid path pattern '{pattern}': use a path relative to the workspace root");
        }
        let globs = [Glob::with_braces(norm)?, Glob::with_braces(&format!("{norm}/**"))?];
        Ok(PathSpec { raw: norm.to_string(), globs })
    }

    pub fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|g| g.matches(path))
    }

    /// Whether two patterns could cover a common path, judged against known
    /// `files` and against each other's literal text (for paths not yet created).
    pub fn overlaps<'a>(&self, other: &PathSpec, files: impl IntoIterator<Item = &'a str>) -> bool {
        self.matches(&other.raw) || other.matches(&self.raw) || files.into_iter().any(|f| self.matches(f) && other.matches(f))
    }
}

/// A claim pattern: a path pattern, optionally narrowed to one top-level
/// declaration of a source file: `src/auth.rs::login`.
#[derive(Debug, Clone)]
pub struct ClaimSpec {
    pub path: PathSpec,
    pub symbol: Option<String>,
    /// Normalized text (`path` or `path::symbol`).
    pub raw: String,
}

impl ClaimSpec {
    pub fn new(pattern: &str) -> Result<ClaimSpec> {
        let (path, symbol) = match pattern.split_once("::") {
            Some((p, s)) => {
                let s = s.trim();
                if s.is_empty() {
                    bail!("missing symbol after '::' in '{pattern}'");
                }
                (p, Some(s.to_string()))
            }
            None => (pattern, None),
        };
        let path = PathSpec::new(path)?;
        if symbol.is_some() && path.raw.contains(['*', '?', '[', '{']) {
            bail!("a symbol claim needs one file, not a glob: '{pattern}'");
        }
        let raw = match &symbol {
            Some(s) => format!("{}::{s}", path.raw),
            None => path.raw.clone(),
        };
        Ok(ClaimSpec { path, symbol, raw })
    }

    /// Two claims overlap if their paths do and they do not name different
    /// symbols (a whole-file claim overlaps every symbol in the file).
    pub fn overlaps<'a>(&self, other: &ClaimSpec, files: impl IntoIterator<Item = &'a str>) -> bool {
        let symbols_clash = match (&self.symbol, &other.symbol) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        symbols_clash && self.path.overlaps(&other.path, files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_claims() {
        let a = ClaimSpec::new("./src/auth.rs::login").unwrap();
        assert_eq!((a.path.raw.as_str(), a.symbol.as_deref(), a.raw.as_str()), ("src/auth.rs", Some("login"), "src/auth.rs::login"));
        let none: [&str; 0] = [];
        assert!(a.overlaps(&ClaimSpec::new("src/auth.rs::login").unwrap(), none));
        assert!(!a.overlaps(&ClaimSpec::new("src/auth.rs::logout").unwrap(), none));
        assert!(a.overlaps(&ClaimSpec::new("src/").unwrap(), none), "whole directory covers the symbol");
        assert!(!a.overlaps(&ClaimSpec::new("src/api.rs::login").unwrap(), none));
        assert!(ClaimSpec::new("src/*.rs::login").is_err());
        assert!(ClaimSpec::new("src/a.rs::").is_err());
    }

    #[test]
    fn paths_dirs_and_globs() {
        let f = PathSpec::new("src/auth.rs").unwrap();
        assert!(f.matches("src/auth.rs") && !f.matches("src/auth.rsx"));
        let d = PathSpec::new("src/auth/").unwrap();
        assert!(d.matches("src/auth/login.rs") && d.matches("src/auth/deep/x.rs") && !d.matches("src/authz.rs"));
        let g = PathSpec::new("src/*.rs").unwrap();
        assert!(g.matches("src/a.rs") && !g.matches("src/sub/a.rs"));
        let gg = PathSpec::new("**/*.md").unwrap();
        assert!(gg.matches("README.md") && gg.matches("docs/x/y.md"));
        assert!(PathSpec::new("../x").is_err() && PathSpec::new("/abs").is_err());
    }

    #[test]
    fn overlap() {
        let a = PathSpec::new("src/").unwrap();
        let b = PathSpec::new("src/new_file.rs").unwrap();
        assert!(a.overlaps(&b, []));
        let c = PathSpec::new("src/*.rs").unwrap();
        let d = PathSpec::new("src/x/").unwrap();
        assert!(!c.overlaps(&d, ["src/x/a.rs", "src/b.rs"]));
        let e = PathSpec::new("**/*.rs").unwrap();
        assert!(e.overlaps(&d, ["src/x/a.rs"]));
    }
}
