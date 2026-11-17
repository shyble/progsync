//! Line diff: Myers' O((N+M)·D) algorithm in linear space ("middle snake"
//! divide and conquer, E. Myers 1986, as in GNU diff).
//!
//! Lines are compared as small integers (each distinct line interned once),
//! and a common prefix and suffix are stripped before the search. The result
//! is the list of changed regions; everything between them is unchanged.

use std::collections::HashMap;
use std::ops::Range;

/// One changed region: `old` lines were replaced by `new` lines (either may
/// be empty: a pure insertion or deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Lines of `s`, each keeping its trailing `\n` (the last may have none).
pub fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// The changed regions between `old` and `new`, in order, with no two
/// regions touching (adjacent changes are joined).
pub fn diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Edit> {
    let mut ids: HashMap<&'a str, u32> = HashMap::with_capacity(old.len() + new.len());
    let mut intern = |l: &'a str| -> u32 {
        let next = ids.len() as u32;
        *ids.entry(l).or_insert(next)
    };
    let a: Vec<u32> = old.iter().map(|l| intern(l)).collect();
    let b: Vec<u32> = new.iter().map(|l| intern(l)).collect();
    diff_ids(&a, &b)
}

/// [`diff`] on interned sequences (ids below `a.len() + b.len()`).
pub fn diff_ids(a: &[u32], b: &[u32]) -> Vec<Edit> {
    // A line that occurs on one side only can never be unchanged: leave such
    // lines out of the search (as GNU diff does), which keeps the result
    // minimal and makes a rewritten file cheap instead of the worst case.
    let ids = a.iter().chain(b).map(|&i| i as usize + 1).max().unwrap_or(0);
    let (mut in_a, mut in_b) = (vec![false; ids], vec![false; ids]);
    a.iter().for_each(|&i| in_a[i as usize] = true);
    b.iter().for_each(|&i| in_b[i as usize] = true);
    let ka: Vec<usize> = (0..a.len()).filter(|&i| in_b[a[i] as usize]).collect();
    let kb: Vec<usize> = (0..b.len()).filter(|&j| in_a[b[j] as usize]).collect();
    let ra: Vec<u32> = ka.iter().map(|&i| a[i]).collect();
    let rb: Vec<u32> = kb.iter().map(|&j| b[j]).collect();

    let mut raw = Vec::new();
    let size = ra.len() + rb.len() + 2;
    let (mut vf, mut vb) = (V::new(size), V::new(size));
    conquer(&ra, 0..ra.len(), &rb, 0..rb.len(), &mut vf, &mut vb, &mut raw);

    // Unchanged lines of the reduced search, in original positions; the
    // gaps between them are the changes.
    let mut edits = Vec::new();
    let (mut pa, mut pb) = (0, 0); // next unmatched line, original positions
    let (mut ri, mut rj) = (0, 0); // same, reduced positions
    let mut matched = |ri: usize, rj: usize, edits: &mut Vec<Edit>| {
        let (x, y) = (ka[ri], kb[rj]);
        if x > pa || y > pb {
            edits.push(Edit { old: pa..x, new: pb..y });
        }
        (pa, pb) = (x + 1, y + 1);
    };
    for e in &raw {
        while ri < e.old.start {
            matched(ri, rj, &mut edits);
            ri += 1;
            rj += 1;
        }
        ri = e.old.end;
        rj = e.new.end;
    }
    while ri < ra.len() {
        matched(ri, rj, &mut edits);
        ri += 1;
        rj += 1;
    }
    if pa < a.len() || pb < b.len() {
        edits.push(Edit { old: pa..a.len(), new: pb..b.len() });
    }
    // Place ambiguous blocks, then join what now touches.
    slide(a, b, &mut edits);
    join(edits)
}

/// Merge edits that touch into one.
fn join(edits: Vec<Edit>) -> Vec<Edit> {
    let mut out: Vec<Edit> = Vec::with_capacity(edits.len());
    for e in edits {
        match out.last_mut() {
            Some(last) if last.old.end == e.old.start && last.new.end == e.new.start => {
                last.old.end = e.old.end;
                last.new.end = e.new.end;
            }
            _ => out.push(e),
        }
    }
    out
}

/// Place each ambiguous pure insertion or deletion (a block `X…X` can show
/// as the first or the last `X`): next to a neighbouring change if it can
/// reach one, so they form one hunk; otherwise as far down as it goes. Of
/// equally short diffs this reads best for code: an added function after the
/// closing `}` of the one before, not before it.
fn slide(a: &[u32], b: &[u32], edits: &mut [Edit]) {
    for i in 0..edits.len() {
        let (prev_old, prev_new) = if i > 0 { (edits[i - 1].old.end, edits[i - 1].new.end) } else { (0, 0) };
        let next_old = edits.get(i + 1).map_or(a.len(), |n| n.old.start);
        let e = &edits[i];
        let ins = e.old.is_empty();
        if ins == e.new.is_empty() {
            continue; // a replacement (or nothing): fixed in place
        }
        // The block's lines, and whether it can move one line down / up.
        let block = |e: &Edit| if ins { (b, e.new.clone()) } else { (a, e.old.clone()) };
        let can_down = |e: &Edit| {
            let (seq, r) = block(e);
            e.old.end < next_old && r.end < seq.len() && seq[r.start] == seq[r.end]
        };
        let can_up = |e: &Edit| {
            let (seq, r) = block(e);
            e.old.start > prev_old && e.new.start > prev_new && seq[r.start - 1] == seq[r.end - 1]
        };
        let shift = |e: &mut Edit, by: isize| {
            let mv = |r: &mut Range<usize>| *r = (r.start as isize + by) as usize..(r.end as isize + by) as usize;
            mv(&mut e.old);
            mv(&mut e.new);
        };
        let mut down = e.clone();
        while can_down(&down) {
            shift(&mut down, 1);
        }
        let joins_next = down.old.end == next_old && i + 1 < edits.len();
        if !joins_next {
            let mut up = e.clone();
            while can_up(&up) {
                shift(&mut up, -1);
            }
            if i > 0 && up.old.start == prev_old && up.new.start == prev_new {
                edits[i] = up;
                continue;
            }
        }
        edits[i] = down;
    }
}

/// Furthest-reaching x per diagonal k, indexed from -size to size.
struct V {
    v: Vec<usize>,
    off: isize,
}

impl V {
    fn new(size: usize) -> V {
        V { v: vec![0; 2 * size + 1], off: size as isize }
    }
}

impl std::ops::Index<isize> for V {
    type Output = usize;
    fn index(&self, k: isize) -> &usize {
        &self.v[(k + self.off) as usize]
    }
}

impl std::ops::IndexMut<isize> for V {
    fn index_mut(&mut self, k: isize) -> &mut usize {
        &mut self.v[(k + self.off) as usize]
    }
}

fn conquer(a: &[u32], mut ar: Range<usize>, b: &[u32], mut br: Range<usize>, vf: &mut V, vb: &mut V, out: &mut Vec<Edit>) {
    while ar.start < ar.end && br.start < br.end && a[ar.start] == b[br.start] {
        ar.start += 1;
        br.start += 1;
    }
    while ar.start < ar.end && br.start < br.end && a[ar.end - 1] == b[br.end - 1] {
        ar.end -= 1;
        br.end -= 1;
    }
    if ar.is_empty() && br.is_empty() {
        return;
    }
    if ar.is_empty() || br.is_empty() {
        out.push(Edit { old: ar, new: br });
        return;
    }
    match middle_snake(a, ar.clone(), b, br.clone(), vf, vb) {
        // A split strictly inside the box; both halves are smaller.
        Some((x, y)) if (x, y) != (ar.start, br.start) && (x, y) != (ar.end, br.end) => {
            conquer(a, ar.start..x, b, br.start..y, vf, vb, out);
            conquer(a, x..ar.end, b, y..br.end, vf, vb, out);
        }
        // Not expected; replacing the whole box is still a correct diff.
        _ => out.push(Edit { old: ar, new: br }),
    }
}

/// A point the shortest edit path through the box passes through, found by
/// running the search from both corners until the paths meet.
fn middle_snake(a: &[u32], ar: Range<usize>, b: &[u32], br: Range<usize>, vf: &mut V, vb: &mut V) -> Option<(usize, usize)> {
    let (n, m) = (ar.len(), br.len());
    let delta = n as isize - m as isize;
    let odd = delta & 1 == 1;
    vf[1] = 0;
    vb[1] = 0;
    let d_max = (n + m).div_ceil(2) as isize + 1;
    for d in 0..d_max {
        // Forward: from the top-left corner.
        let mut k = d;
        while k >= -d {
            let mut x = if k == -d || (k != d && vf[k - 1] < vf[k + 1]) { vf[k + 1] } else { vf[k - 1] + 1 };
            let y = (x as isize - k) as usize;
            let (x0, y0) = (x, y);
            let mut yy = y;
            while x < n && yy < m && a[ar.start + x] == b[br.start + yy] {
                x += 1;
                yy += 1;
            }
            vf[k] = x;
            if odd && (k - delta).abs() < d && vf[k] + vb[-(k - delta)] >= n {
                return Some((ar.start + x0, br.start + y0));
            }
            k -= 2;
        }
        // Backward: from the bottom-right corner (x, y count from the end).
        let mut k = d;
        while k >= -d {
            let mut x = if k == -d || (k != d && vb[k - 1] < vb[k + 1]) { vb[k + 1] } else { vb[k - 1] + 1 };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[ar.end - x - 1] == b[br.end - y - 1] {
                x += 1;
                y += 1;
            }
            vb[k] = x;
            if !odd && (k - delta).abs() <= d && vb[k] + vf[-(k - delta)] >= n {
                return Some((ar.end - x, br.end - y));
            }
            k -= 2;
        }
    }
    None
}

/// Unified diff (git style: `---`/`+++` headers, `@@` hunks with `context`
/// unchanged lines around each change). Empty when the texts are equal.
pub fn unified(old_text: &str, new_text: &str, old_name: &str, new_name: &str, context: usize) -> String {
    let (a, b) = (split_lines(old_text), split_lines(new_text));
    let edits = diff(&a, &b);
    let mut s = String::new();
    if edits.is_empty() {
        return s;
    }
    s.push_str(&format!("--- {old_name}\n+++ {new_name}\n"));
    let mut i = 0;
    while i < edits.len() {
        // Changes this close share a hunk (their contexts would overlap).
        let mut j = i;
        while j + 1 < edits.len() && edits[j + 1].old.start - edits[j].old.end <= 2 * context {
            j += 1;
        }
        let (first, last) = (&edits[i], &edits[j]);
        let old_start = first.old.start.saturating_sub(context);
        let new_start = first.new.start - (first.old.start - old_start);
        let old_end = (last.old.end + context).min(a.len());
        let new_end = last.new.end + (old_end - last.old.end);
        s.push_str(&format!("@@ -{} +{} @@\n", range(old_start, old_end), range(new_start, new_end)));
        let mut pos = old_start;
        for e in &edits[i..=j] {
            for l in &a[pos..e.old.start] {
                line(&mut s, ' ', l);
            }
            for l in &a[e.old.clone()] {
                line(&mut s, '-', l);
            }
            for l in &b[e.new.clone()] {
                line(&mut s, '+', l);
            }
            pos = e.old.end;
        }
        for l in &a[pos..old_end] {
            line(&mut s, ' ', l);
        }
        i = j + 1;
    }
    s
}

/// A hunk range: `start,len` (1-based), or just `start` for one line; an
/// empty range names the line before it.
fn range(start: usize, end: usize) -> String {
    match end - start {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        len => format!("{},{len}", start + 1),
    }
}

fn line(s: &mut String, tag: char, l: &str) {
    s.push(tag);
    s.push_str(l);
    if !l.ends_with('\n') {
        s.push_str("\n\\ No newline at end of file\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply edits to `old` and check the result is `new`; return the
    /// number of changed lines (deleted + inserted).
    fn check(old: &[&str], new: &[&str]) -> usize {
        let edits = diff(old, new);
        let mut out: Vec<&str> = Vec::new();
        let mut pos = 0;
        let mut changed = 0;
        for e in &edits {
            assert!(e.old.start >= pos, "edits out of order: {edits:?}");
            assert!(!(e.old.is_empty() && e.new.is_empty()), "empty edit");
            out.extend(&old[pos..e.old.start]);
            out.extend(&new[e.new.clone()]);
            changed += e.old.len() + e.new.len();
            pos = e.old.end;
        }
        out.extend(&old[pos..]);
        assert_eq!(out, new, "edits {edits:?} do not turn old into new");
        changed
    }

    /// Length of the longest common subsequence (to check minimality).
    fn lcs(a: &[&str], b: &[&str]) -> usize {
        let mut t = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                t[i][j] = if a[i - 1] == b[j - 1] { t[i - 1][j - 1] + 1 } else { t[i - 1][j].max(t[i][j - 1]) };
            }
        }
        t[a.len()][b.len()]
    }

    #[test]
    fn small_cases() {
        let cases: &[(&[&str], &[&str])] = &[
            (&[], &[]),
            (&["a"], &[]),
            (&[], &["a"]),
            (&["a"], &["b"]),
            (&["a", "b", "c"], &["a", "b", "c"]),
            (&["a", "b", "c", "a", "b", "b", "a"], &["c", "b", "a", "b", "a", "c"]),
            (&["x", "a", "b"], &["a", "b", "y"]),
        ];
        for (a, b) in cases {
            let changed = check(a, b);
            assert_eq!(changed, a.len() + b.len() - 2 * lcs(a, b), "not minimal for {a:?} -> {b:?}");
        }
    }

    #[test]
    fn random_sequences_are_minimal() {
        let mut s = 0x2545f4914f6cdd1du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let alphabet = ["a", "b", "c", "d", "e"];
        for _ in 0..3000 {
            let la = (next() % 25) as usize;
            let a: Vec<&str> = (0..la).map(|_| alphabet[(next() % 5) as usize]).collect();
            // b: a with random edits, or unrelated.
            let mut b = a.clone();
            for _ in 0..next() % 6 {
                let p = (next() as usize) % (b.len() + 1);
                match next() % 3 {
                    0 if p < b.len() => {
                        b.remove(p);
                    }
                    1 if p < b.len() => b[p] = alphabet[(next() % 5) as usize],
                    _ => b.insert(p, alphabet[(next() % 5) as usize]),
                }
            }
            let changed = check(&a, &b);
            assert_eq!(changed, a.len() + b.len() - 2 * lcs(&a, &b), "not minimal for {a:?} -> {b:?}");
        }
    }

    #[test]
    fn ambiguous_blocks_go_last() {
        // Adding a function after another: the new block is "}\n\nfn b() {\n"
        // or "\nfn b() {\n}\n"; the second reads right.
        let old = split_lines("fn a() {\n}\n");
        let new = split_lines("fn a() {\n}\n\nfn b() {\n}\n");
        assert_eq!(diff(&old, &new), [Edit { old: 2..2, new: 2..5 }]);
        // Same for deletions.
        assert_eq!(diff(&new, &old), [Edit { old: 2..5, new: 2..2 }]);
        // A block that can reach a neighbouring change joins it instead.
        let old = split_lines("x\na\nb\n");
        let new = split_lines("y\na\na\nb\n");
        assert_eq!(diff(&old, &new), [Edit { old: 0..1, new: 0..2 }]);
    }

    #[test]
    fn unified_format() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
        let new = "1\n2\nthree\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n";
        let want = "--- a/f\n+++ b/f\n@@ -1,6 +1,6 @@\n 1\n 2\n-3\n+three\n 4\n 5\n 6\n@@ -18,3 +18,4 @@\n 18\n 19\n 20\n+21\n";
        assert_eq!(unified(old, new, "a/f", "b/f", 3), want);
        // Close changes share a hunk.
        let near = "1\n2\nthree\n4\n5\n6\n7\n8\n9\nten\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
        assert!(unified(old, near, "a", "b", 3).contains("@@ -1,13 +1,13 @@\n"));
        // New file, missing final newline, single-line range.
        assert_eq!(unified("", "x", "/dev/null", "b/f", 3), "--- /dev/null\n+++ b/f\n@@ -0,0 +1 @@\n+x\n\\ No newline at end of file\n");
        assert_eq!(unified("a\n", "a\n", "a", "b", 3), "");
    }
}
