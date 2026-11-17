//! Three-way merge of text files and trees.
//!
//! Text: hunks from base->ours and base->theirs are combined. Hunks that do
//! not overlap merge cleanly (even on adjacent lines); identical edits on both
//! sides merge cleanly; anything else becomes a conflict block with
//! diff3-style markers (ours / base / theirs).
//!
//! Trees: identical subtrees are skipped by hash, so merging is proportional
//! to what changed, not to repository size.

use crate::diff::{self, Leaf};
use crate::hash::Hash;
use crate::object::{Conflict, ConflictKind, EntryKind, Object, Tree, TreeEntry};
use crate::store::Store;
use crate::error::Result;
use std::collections::BTreeSet;

pub struct Labels<'a> {
    pub ours: &'a str,
    pub theirs: &'a str,
}

#[derive(Debug)]
struct Hunk {
    start: usize,
    end: usize,
    side_start: usize,
    side_end: usize,
}

impl Hunk {
    fn delta(&self) -> isize {
        (self.side_end - self.side_start) as isize - (self.end - self.start) as isize
    }
}

fn hunks(base: &[&str], side: &[&str]) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for e in crate::linediff::diff(base, side) {
        let (bs, be, ss, se) = (e.old.start, e.old.end, e.new.start, e.new.end);
        if let Some(last) = out.last_mut() {
            if last.end == bs && last.side_end == ss {
                last.end = be;
                last.side_end = se;
                continue;
            }
        }
        out.push(Hunk { start: bs, end: be, side_start: ss, side_end: se });
    }
    out
}

fn overlaps(rs: usize, re: usize, h: &Hunk) -> bool {
    (h.start < re && rs < h.end) || h.start == rs
}

/// The lines of one side covering base region `[rs, re)`, and the line-count
/// delta the side's hunks in that region introduce.
fn region<'a, 'b>(lines: &'b [&'a str], hs: &[Hunk], rs: usize, re: usize, delta: isize) -> (&'b [&'a str], isize) {
    let d: isize = hs.iter().map(Hunk::delta).sum();
    let s = (rs as isize + delta) as usize;
    let e = (re as isize + delta + d) as usize;
    (&lines[s..e], d)
}

fn push_section(out: &mut String, lines: &[&str]) {
    for l in lines {
        out.push_str(l);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Merge three versions of a text. Returns the merged text and the number of
/// conflict blocks in it.
pub fn merge_text(base: &str, ours: &str, theirs: &str, labels: &Labels) -> (String, usize) {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let o: Vec<&str> = ours.split_inclusive('\n').collect();
    let t: Vec<&str> = theirs.split_inclusive('\n').collect();
    let ho = hunks(&b, &o);
    let ht = hunks(&b, &t);

    let mut out = String::with_capacity(ours.len().max(theirs.len()));
    let mut conflicts = 0;
    let (mut pos, mut io, mut it) = (0usize, 0usize, 0usize);
    let (mut delta_o, mut delta_t) = (0isize, 0isize);
    loop {
        let rs = match (ho.get(io), ht.get(it)) {
            (None, None) => break,
            (Some(a), None) => a.start,
            (None, Some(c)) => c.start,
            (Some(a), Some(c)) => a.start.min(c.start),
        };
        for l in &b[pos..rs] {
            out.push_str(l);
        }
        let (so, st) = (io, it);
        let mut re = rs;
        let mut grew = true;
        while grew {
            grew = false;
            while let Some(h) = ho.get(io).filter(|h| overlaps(rs, re, h)) {
                re = re.max(h.end);
                io += 1;
                grew = true;
            }
            while let Some(h) = ht.get(it).filter(|h| overlaps(rs, re, h)) {
                re = re.max(h.end);
                it += 1;
                grew = true;
            }
        }
        let (ours_part, d_o) = region(&o, &ho[so..io], rs, re, delta_o);
        let (theirs_part, d_t) = region(&t, &ht[st..it], rs, re, delta_t);
        delta_o += d_o;
        delta_t += d_t;
        if so == io {
            theirs_part.iter().for_each(|l| out.push_str(l));
        } else if st == it || ours_part == theirs_part {
            ours_part.iter().for_each(|l| out.push_str(l));
        } else {
            conflicts += 1;
            let mut block = format!("<<<<<<< {}\n", labels.ours);
            push_section(&mut block, ours_part);
            block.push_str("||||||| base\n");
            push_section(&mut block, &b[rs..re]);
            block.push_str("=======\n");
            push_section(&mut block, theirs_part);
            block.push_str(&format!(">>>>>>> {}\n", labels.theirs));
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block);
        }
        pos = re;
    }
    for l in &b[pos..] {
        out.push_str(l);
    }
    (out, conflicts)
}

/// Whether text still contains conflict markers written by [`merge_text`].
pub fn has_markers(data: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(data) else { return false };
    let mut open = false;
    for line in s.lines() {
        if line.starts_with("<<<<<<< ") {
            open = true;
        } else if open && line.starts_with(">>>>>>> ") {
            return true;
        }
    }
    false
}

fn as_text(data: &[u8]) -> Option<&str> {
    if data[..data.len().min(8000)].contains(&0) {
        return None;
    }
    std::str::from_utf8(data).ok()
}

pub struct TreeMerge {
    pub tree: Hash,
    pub conflicts: Vec<Conflict>,
}

/// Three-way merge of trees. `base` may be absent (unrelated histories).
pub fn merge_trees(store: &Store, base: Option<Hash>, ours: Hash, theirs: Hash, labels: &Labels) -> Result<TreeMerge> {
    let mut conflicts = Vec::new();
    let tree = match merge_dir(store, base, Some(ours), Some(theirs), "", labels, &mut conflicts)? {
        Some(h) => h,
        None => store.put(&Object::Tree(Tree::default()))?,
    };
    Ok(TreeMerge { tree, conflicts })
}

fn merge_dir(
    store: &Store,
    b: Option<Hash>,
    o: Option<Hash>,
    t: Option<Hash>,
    prefix: &str,
    labels: &Labels,
    conflicts: &mut Vec<Conflict>,
) -> Result<Option<Hash>> {
    if o == t {
        return Ok(o);
    }
    if b == o {
        return Ok(t);
    }
    if b == t {
        return Ok(o);
    }
    let load = |h: Option<Hash>| -> Result<Vec<TreeEntry>> {
        Ok(match h {
            Some(h) => store.get_tree(&h)?.entries,
            None => Vec::new(),
        })
    };
    let (eb, eo, et) = (load(b)?, load(o)?, load(t)?);
    let names: BTreeSet<&str> = eb.iter().chain(&eo).chain(&et).map(|e| e.name.as_str()).collect();
    let find = |v: &[TreeEntry], name: &str| v.iter().find(|e| e.name == name).cloned();
    let mut entries = Vec::new();
    for name in names {
        let path = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        let merged = merge_entry(store, find(&eb, name), find(&eo, name), find(&et, name), &path, labels, conflicts)?;
        if let Some((kind, hash)) = merged {
            entries.push(TreeEntry { name: name.to_string(), kind, hash });
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(store.put(&Object::Tree(Tree { entries }))?))
}

fn leaf_of(e: &Option<TreeEntry>) -> Option<Leaf> {
    e.as_ref().filter(|e| e.kind != EntryKind::Tree).map(|e| (e.kind, e.hash))
}

fn tree_of(e: &Option<TreeEntry>) -> Option<Hash> {
    e.as_ref().filter(|e| e.kind == EntryKind::Tree).map(|e| e.hash)
}

fn merge_entry(
    store: &Store,
    b: Option<TreeEntry>,
    o: Option<TreeEntry>,
    t: Option<TreeEntry>,
    path: &str,
    labels: &Labels,
    conflicts: &mut Vec<Conflict>,
) -> Result<Option<Leaf>> {
    let key = |e: &Option<TreeEntry>| e.as_ref().map(|e| (e.kind, e.hash));
    if key(&o) == key(&t) {
        return Ok(key(&o));
    }
    if key(&b) == key(&o) {
        return Ok(key(&t));
    }
    if key(&b) == key(&t) {
        return Ok(key(&o));
    }
    let (o_tree, t_tree) = (tree_of(&o), tree_of(&t));
    // Directory on at least one side, and nothing that is a file on the other.
    if (o_tree.is_some() || t_tree.is_some()) && leaf_of(&o).is_none() && leaf_of(&t).is_none() {
        let h = merge_dir(store, tree_of(&b), o_tree, t_tree, path, labels, conflicts)?;
        return Ok(h.map(|h| (EntryKind::Tree, h)));
    }
    let conflict = |kind, written| Conflict {
        path: path.to_string(),
        kind,
        base: leaf_of(&b),
        ours: leaf_of(&o),
        theirs: leaf_of(&t),
        written,
    };
    if o_tree.is_some() || t_tree.is_some() {
        conflicts.push(conflict(ConflictKind::FileDirectory, None));
        return Ok(key(&o));
    }
    match (leaf_of(&o), leaf_of(&t)) {
        (Some((ok, oh)), Some((tk, th))) => {
            let base_kind = leaf_of(&b).map(|(k, _)| k);
            let kind = if base_kind == Some(ok) { tk } else { ok };
            let texts = if ok == EntryKind::Symlink || tk == EntryKind::Symlink {
                None
            } else {
                // Large files are merged as binary.
                let base_data = match leaf_of(&b) {
                    Some((EntryKind::Symlink, _)) | None => Some(Vec::new()),
                    Some((_, bh)) => store.get_small_file(&bh)?,
                };
                let (od, td) = (store.get_small_file(&oh)?, store.get_small_file(&th)?);
                let text = |d: &Option<Vec<u8>>| d.as_deref().and_then(as_text).map(str::to_string);
                match (text(&base_data), text(&od), text(&td)) {
                    (Some(bt), Some(ot), Some(tt)) => {
                        let (merged, n) = merge_text(&bt, &ot, &tt, labels);
                        // Line conflicts in source code: try merging by declaration.
                        match crate::semantic::merge(path, &bt, &ot, &tt) {
                            Some(s) if n > 0 => Some((s, 0)),
                            _ => Some((merged, n)),
                        }
                    }
                    _ => None,
                }
            };
            match texts {
                Some((merged, 0)) => Ok(Some((kind, store.put_blob(merged.as_bytes())?))),
                Some((merged, _)) => {
                    let h = store.put_blob(merged.as_bytes())?;
                    conflicts.push(conflict(ConflictKind::Content, Some(h)));
                    Ok(Some((kind, h)))
                }
                None => {
                    conflicts.push(conflict(ConflictKind::Binary, Some(oh)));
                    Ok(Some((ok, oh)))
                }
            }
        }
        (None, Some((tk, th))) => {
            conflicts.push(conflict(ConflictKind::DeleteModify, Some(th)));
            Ok(Some((tk, th)))
        }
        (Some((ok, oh)), None) => {
            conflicts.push(conflict(ConflictKind::DeleteModify, Some(oh)));
            Ok(Some((ok, oh)))
        }
        (None, None) => Ok(None),
    }
}

/// Whether a recorded conflict is still present in `tree`.
pub fn still_conflicted(store: &Store, tree: Hash, c: &Conflict) -> Result<bool> {
    let current = diff::lookup(store, tree, &c.path)?;
    Ok(match c.kind {
        ConflictKind::Content => match current {
            None => false,
            Some((EntryKind::Tree, _)) => false,
            Some((_, h)) => Some(h) == c.written || store.get_small_file(&h)?.is_some_and(|d| has_markers(&d)),
        },
        ConflictKind::Binary | ConflictKind::DeleteModify => current.map(|(_, h)| h) == c.written,
        ConflictKind::FileDirectory => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Labels = Labels { ours: "ours", theirs: "theirs" };

    #[test]
    fn disjoint_edits_merge_cleanly() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\n";
        assert_eq!(merge_text(base, ours, theirs, &L), ("A\nb\nc\nd\nE\n".into(), 0));
    }

    #[test]
    fn adjacent_edits_merge_cleanly() {
        let (m, n) = merge_text("a\nb\n", "A\nb\n", "a\nB\n", &L);
        assert_eq!((m.as_str(), n), ("A\nB\n", 0));
    }

    #[test]
    fn identical_edits_merge_cleanly() {
        let (m, n) = merge_text("a\nb\n", "a\nX\n", "a\nX\n", &L);
        assert_eq!((m.as_str(), n), ("a\nX\n", 0));
    }

    #[test]
    fn insertions_on_both_sides_far_apart() {
        let (m, n) = merge_text("a\nb\nc\n", "o\na\nb\nc\n", "a\nb\nc\nt\n", &L);
        assert_eq!((m.as_str(), n), ("o\na\nb\nc\nt\n", 0));
    }

    #[test]
    fn same_line_edits_conflict_with_markers() {
        let (m, n) = merge_text("x\nkeep\n", "ours\nkeep\n", "theirs\nkeep\n", &L);
        assert_eq!(n, 1);
        assert_eq!(m, "<<<<<<< ours\nours\n||||||| base\nx\n=======\ntheirs\n>>>>>>> theirs\nkeep\n");
        assert!(has_markers(m.as_bytes()));
    }

    #[test]
    fn insertion_at_same_point_conflicts() {
        let (_, n) = merge_text("a\nb\n", "a\no\nb\n", "a\nt\nb\n", &L);
        assert_eq!(n, 1);
    }

    #[test]
    fn missing_trailing_newline_is_handled() {
        let (m, n) = merge_text("a\nb", "A\nb", "a\nb", &L);
        assert_eq!((m.as_str(), n), ("A\nb", 0));
    }
}
