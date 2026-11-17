//! Ignore rules: `.psyncignore` and `.gitignore` files in any directory of
//! the workspace, plus the user's global ignore file
//! (`$XDG_CONFIG_HOME/psync/ignore`, default `~/.config/psync/ignore`).
//!
//! The syntax is git's: one pattern per line; `#` starts a comment; `!`
//! re-includes; a trailing `/` matches directories only; a pattern with a
//! `/` at the start or in the middle is relative to the file's directory,
//! one without matches a name at any depth below it; `\` escapes
//! (`\#`, `\!`, `\ ` for a trailing space). See [`crate::glob`] for the globs.
//!
//! Precedence: the deepest directory with a matching rule decides; within a
//! directory the last matching rule wins, and `.psyncignore` rules come after
//! (so win over) `.gitignore` rules. The global file has the lowest
//! precedence. As in git, files inside an ignored directory cannot be
//! re-included (the directory is not entered).

use crate::glob::Glob;
use std::path::Path;
use std::sync::Arc;

/// Ignore files read in each directory, in increasing precedence.
pub const FILE_NAMES: [&str; 2] = [".gitignore", ".psyncignore"];

#[derive(Debug)]
struct Rule {
    glob: Glob,
    negated: bool,
    dir_only: bool,
    /// Match the path relative to the rule's directory (else: the name).
    anchored: bool,
    /// Where it comes from: ignore file (relative to the root, or the global
    /// file's path), line number, and the line as written.
    source: Arc<str>,
    line: usize,
    text: String,
}

/// The rule that decided whether a path is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    /// The ignore file, relative to the workspace root (or the global file's path).
    pub source: String,
    pub line: usize,
    pub pattern: String,
    /// A `!` rule: the path is re-included.
    pub negated: bool,
}

/// Parse ignore-file text; invalid patterns are skipped (as git does).
fn parse(text: &str, source: &str) -> Vec<Rule> {
    let source: Arc<str> = Arc::from(source);
    let mut rules = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Trailing spaces are dropped unless escaped.
        let mut end = line.len();
        while end > 0 && line.as_bytes()[end - 1] == b' ' && !(end >= 2 && line.as_bytes()[end - 2] == b'\\') {
            end -= 1;
        }
        let mut p = &line[..end];
        if p.is_empty() {
            continue;
        }
        let negated = p.starts_with('!');
        if negated {
            p = &p[1..];
        }
        let dir_only = p.ends_with('/') && !p.ends_with("\\/");
        let p = p.strip_suffix('/').filter(|_| dir_only).unwrap_or(p);
        let anchored = p.contains('/');
        let p = p.strip_prefix('/').unwrap_or(p);
        if p.is_empty() {
            continue;
        }
        if let Ok(glob) = Glob::new(p) {
            rules.push(Rule { glob, negated, dir_only, anchored, source: source.clone(), line: i + 1, text: line[..end].to_string() });
        }
    }
    rules
}

/// Rules from one directory's ignore files.
#[derive(Debug)]
struct Level {
    /// The directory, relative to the workspace root ("" for the root).
    dir: String,
    rules: Vec<Rule>,
}

impl Level {
    /// The rule here that decides `rel` (a path below `dir`), if any.
    fn decide(&self, rel: &str, name: &str, is_dir: bool) -> Option<&Rule> {
        let below = if self.dir.is_empty() { rel } else { &rel[self.dir.len() + 1..] };
        self.rules.iter().rev().find(|r| (!r.dir_only || is_dir) && r.glob.matches(if r.anchored { below } else { name }))
    }
}

/// The rules in effect in one directory: its own, then its parents'.
#[derive(Debug, Default, Clone)]
pub struct Ignores {
    chain: Option<Arc<Node>>,
    global: Option<Arc<Level>>,
}

#[derive(Debug)]
struct Node {
    level: Level,
    parent: Option<Arc<Node>>,
}

impl Ignores {
    /// The rules at the workspace root: the global ignore file only (the
    /// root's own files are added with [`Ignores::enter`] like any directory's).
    pub fn root() -> Ignores {
        let global = crate::platform::config_home()
            .map(|h| h.join("psync").join("ignore"))
            .and_then(|p| std::fs::read_to_string(&p).ok().map(|t| parse(&t, &p.display().to_string())))
            .filter(|r| !r.is_empty())
            .map(|rules| Arc::new(Level { dir: String::new(), rules }));
        Ignores { chain: None, global }
    }

    /// The rules inside directory `dir` (relative), given the names of the
    /// ignore files it contains; reads them from `abs`.
    pub fn enter(&self, dir: &str, abs: &Path, present: &[&str]) -> Ignores {
        let mut rules = Vec::new();
        for f in FILE_NAMES {
            if present.contains(&f) {
                if let Ok(t) = std::fs::read_to_string(abs.join(f)) {
                    let source = if dir.is_empty() { f.to_string() } else { format!("{dir}/{f}") };
                    rules.extend(parse(&t, &source));
                }
            }
        }
        if rules.is_empty() {
            return self.clone();
        }
        let level = Level { dir: dir.to_string(), rules };
        Ignores { chain: Some(Arc::new(Node { level, parent: self.chain.clone() })), global: self.global.clone() }
    }

    /// Whether `rel` (relative to the root; `name` is its last component)
    /// is ignored.
    pub fn ignored(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        self.rule_for(rel, name, is_dir).is_some_and(|r| !r.negated)
    }

    /// The rule that decides `rel`, if any (a `!` rule re-includes it).
    pub fn explain(&self, rel: &str, name: &str, is_dir: bool) -> Option<RuleMatch> {
        self.rule_for(rel, name, is_dir).map(|r| RuleMatch {
            source: r.source.to_string(),
            line: r.line,
            pattern: r.text.clone(),
            negated: r.negated,
        })
    }

    fn rule_for(&self, rel: &str, name: &str, is_dir: bool) -> Option<&Rule> {
        let mut node = self.chain.as_deref();
        while let Some(n) = node {
            if let Some(r) = n.level.decide(rel, name, is_dir) {
                return Some(r);
            }
            node = n.parent.as_deref();
        }
        self.global.as_ref().and_then(|g| g.decide(rel, name, is_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Ignores {
        Ignores { chain: Some(Arc::new(Node { level: Level { dir: String::new(), rules: parse(text, ".gitignore") }, parent: None })), global: None }
    }

    #[test]
    fn gitignore_semantics() {
        let ig = rules("# comment\n*.log\n!keep.log\ntarget/\n/top.txt\ndocs/*.tmp\n\\#hash\nspace\\ \n");
        let f = |p: &str| ig.ignored(p, p.rsplit('/').next().unwrap(), false);
        let d = |p: &str| ig.ignored(p, p.rsplit('/').next().unwrap(), true);
        assert!(f("a.log") && f("x/y/a.log") && !f("keep.log") && !f("x/keep.log"));
        assert!(d("target") && d("sub/target") && !f("target"), "dir-only pattern");
        assert!(f("top.txt") && !f("sub/top.txt"), "leading / anchors");
        assert!(f("docs/a.tmp") && !f("x/docs/a.tmp") && !f("docs/x/a.tmp"), "a middle / anchors too");
        assert!(f("#hash") && f("space ") && !f("comment"));
    }

    #[test]
    fn deeper_files_and_psyncignore_win() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.gen\n!special.gen\n").unwrap();
        std::fs::write(dir.path().join(".psyncignore"), "special.gen\n").unwrap();
        let root = Ignores::default().enter("", dir.path(), &[".gitignore", ".psyncignore"]);
        assert!(root.ignored("a.gen", "a.gen", false));
        assert!(root.ignored("special.gen", "special.gen", false), ".psyncignore beats .gitignore");
        let sub = tempfile::tempdir().unwrap();
        std::fs::write(sub.path().join(".gitignore"), "!*.gen\n").unwrap();
        let deeper = root.enter("src", sub.path(), &[".gitignore"]);
        assert!(!deeper.ignored("src/a.gen", "a.gen", false), "deeper rules decide first");
        assert!(root.ignored("src/a.gen", "a.gen", false));
    }
}
