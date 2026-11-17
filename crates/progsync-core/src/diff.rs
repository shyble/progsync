//! Tree diffs (skipping identical subtrees by hash) and content diffs.

use crate::hash::Hash;
use crate::object::{EntryKind, TreeEntry};
use crate::store::Store;
use crate::error::Result;
use std::cmp::Ordering;

pub use crate::object::Leaf;

/// Look up one path in a tree.
pub fn lookup(store: &Store, tree: Hash, path: &str) -> Result<Option<Leaf>> {
    let mut cur = tree;
    let mut parts = path.split('/').peekable();
    while let Some(name) = parts.next() {
        let t = store.get_tree(&cur)?;
        let Some(e) = t.entries.iter().find(|e| e.name == name) else { return Ok(None) };
        match (e.kind, parts.peek().is_some()) {
            (EntryKind::Tree, true) => cur = e.hash,
            (EntryKind::Tree, false) => return Ok(Some((EntryKind::Tree, e.hash))),
            (_, true) => return Ok(None),
            (k, false) => return Ok(Some((k, e.hash))),
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}
crate::json_enum!(FileStatus { Added = "added", Modified = "modified", Deleted = "deleted" });

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub old: Option<Leaf>,
    pub new: Option<Leaf>,
}

impl FileChange {
    pub fn status(&self) -> FileStatus {
        match (&self.old, &self.new) {
            (None, _) => FileStatus::Added,
            (_, None) => FileStatus::Deleted,
            _ => FileStatus::Modified,
        }
    }
}

/// File-level differences between two trees, in path order.
pub fn diff_trees(store: &Store, a: Option<Hash>, b: Option<Hash>) -> Result<Vec<FileChange>> {
    let mut out = Vec::new();
    diff_rec(store, a, b, "", &mut out)?;
    Ok(out)
}

fn diff_rec(
    store: &Store,
    a: Option<Hash>,
    b: Option<Hash>,
    prefix: &str,
    out: &mut Vec<FileChange>,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let ta = match a {
        Some(h) => store.get_tree(&h)?.entries,
        None => Vec::new(),
    };
    let tb = match b {
        Some(h) => store.get_tree(&h)?.entries,
        None => Vec::new(),
    };
    let (mut i, mut j) = (0, 0);
    while i < ta.len() || j < tb.len() {
        let ord = match (ta.get(i), tb.get(j)) {
            (Some(x), Some(y)) => x.name.cmp(&y.name),
            (Some(_), None) => Ordering::Less,
            _ => Ordering::Greater,
        };
        match ord {
            Ordering::Less => {
                diff_entry(store, Some(&ta[i]), None, prefix, out)?;
                i += 1;
            }
            Ordering::Greater => {
                diff_entry(store, None, Some(&tb[j]), prefix, out)?;
                j += 1;
            }
            Ordering::Equal => {
                diff_entry(store, Some(&ta[i]), Some(&tb[j]), prefix, out)?;
                i += 1;
                j += 1;
            }
        }
    }
    Ok(())
}

fn split(e: Option<&TreeEntry>) -> (Option<Hash>, Option<Leaf>) {
    match e {
        Some(e) if e.kind == EntryKind::Tree => (Some(e.hash), None),
        Some(e) => (None, Some((e.kind, e.hash))),
        None => (None, None),
    }
}

fn diff_entry(
    store: &Store,
    x: Option<&TreeEntry>,
    y: Option<&TreeEntry>,
    prefix: &str,
    out: &mut Vec<FileChange>,
) -> Result<()> {
    if x == y {
        return Ok(());
    }
    let name = &x.or(y).expect("at least one side").name;
    let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
    let (xt, xl) = split(x);
    let (yt, yl) = split(y);
    if xl != yl {
        out.push(FileChange { path: path.clone(), old: xl, new: yl });
    }
    if xt.is_some() || yt.is_some() {
        diff_rec(store, xt, yt, &path, out)?;
    }
    Ok(())
}

/// Every leaf in a tree, as (path, kind, hash).
pub fn flatten(store: &Store, tree: Hash) -> Result<Vec<(String, Leaf)>> {
    Ok(diff_trees(store, None, Some(tree))?
        .into_iter()
        .map(|c| (c.path, c.new.expect("added entries have a new side")))
        .collect())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LineStat {
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}
crate::json_struct!(LineStat { added, removed, binary });

/// A side's contents; `None` for a large file (shown as binary).
fn content(store: &Store, side: Option<Leaf>) -> Result<Option<Vec<u8>>> {
    match side {
        Some((EntryKind::Symlink, h)) => store.get_blob(&h).map(Some),
        Some((_, h)) => store.get_small_file(&h),
        None => Ok(Some(Vec::new())),
    }
}

fn as_text(data: &[u8]) -> Option<&str> {
    if data[..data.len().min(8000)].contains(&0) {
        return None;
    }
    std::str::from_utf8(data).ok()
}

pub fn line_stat(store: &Store, c: &FileChange) -> Result<LineStat> {
    let (a, b) = (content(store, c.old)?, content(store, c.new)?);
    let (Some(ta), Some(tb)) = (a.as_deref().and_then(as_text), b.as_deref().and_then(as_text)) else {
        return Ok(LineStat { binary: true, ..Default::default() });
    };
    let mut st = LineStat::default();
    let (la, lb) = (crate::linediff::split_lines(ta), crate::linediff::split_lines(tb));
    for e in crate::linediff::diff(&la, &lb) {
        st.removed += e.old.len();
        st.added += e.new.len();
    }
    Ok(st)
}

/// Unified diff of one file (git-style headers).
pub fn unified(store: &Store, c: &FileChange) -> Result<String> {
    let (a, b) = (content(store, c.old)?, content(store, c.new)?);
    let old_name = if c.old.is_some() { format!("a/{}", c.path) } else { "/dev/null".into() };
    let new_name = if c.new.is_some() { format!("b/{}", c.path) } else { "/dev/null".into() };
    let (Some(ta), Some(tb)) = (a.as_deref().and_then(as_text), b.as_deref().and_then(as_text)) else {
        return Ok(format!("Binary files {old_name} and {new_name} differ\n"));
    };
    let mut s = String::new();
    if let (Some((ka, _)), Some((kb, _))) = (c.old, c.new) {
        if ka != kb {
            s.push_str(&format!("type changed: {ka:?} -> {kb:?}\n"));
        }
    }
    s.push_str(&crate::linediff::unified(ta, tb, &old_name, &new_name, 3));
    Ok(s)
}
