//! Structure-aware merge for source files (tree-sitter).
//!
//! Used when the line-based three-way merge of a file conflicts. Each version
//! is split into its top-level declarations (functions, types, impls,
//! classes, imports, ...), keyed by kind and name, and merged declaration by
//! declaration:
//!
//! - a declaration changed on one side only takes that side's version;
//! - one changed on both sides is line-merged on its own (less context, so
//!   fewer false conflicts), and must merge cleanly;
//! - declarations added on both sides are all kept (the most common conflict
//!   between agents: both append a function at the same place);
//! - deleted on one side and unchanged on the other: deleted.
//!
//! Order follows "ours"; declarations new in "theirs" go after the
//! declaration they follow there. Anything doubtful (a declaration deleted
//! on one side and modified on the other, the same new name added with
//! different bodies, a both-sides edit that still conflicts, a parse error
//! in any version or in the result) returns `None`, and the caller keeps the
//! line merge with its conflict markers.
//!
//! Supported: Rust, Python, JavaScript, TypeScript/TSX, Go.

use crate::merge::{merge_text, Labels};
use std::collections::{HashMap, HashSet};
use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
}

fn lang_for(path: &str) -> Option<Lang> {
    let ext = path.rsplit_once('.')?.1;
    Some(match ext {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "go" => Lang::Go,
        _ => return None,
    })
}

fn language(lang: Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// Whether `path` is in a language the structural merge understands.
pub fn supports(path: &str) -> bool {
    lang_for(path).is_some()
}

struct Item {
    key: String,
    /// The declaration with attached comments, without trailing whitespace.
    text: String,
    /// Whitespace up to the next declaration (or the end of the file).
    gap: String,
    last: bool,
}

struct Split {
    /// Text before the first declaration.
    header: String,
    items: Vec<Item>,
}

fn parse(lang: Lang, src: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language(lang)).ok()?;
    let tree = parser.parse(src, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

/// Comments and attributes attach to the declaration that follows them.
fn attaches_forward(kind: &str) -> bool {
    matches!(kind, "comment" | "line_comment" | "block_comment" | "attribute_item" | "decorator")
}

fn text<'a>(n: Node, src: &'a str) -> &'a str {
    &src[n.byte_range()]
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn field<'a>(n: Node, name: &str, src: &'a str) -> Option<&'a str> {
    n.child_by_field_name(name).map(|c| text(c, src))
}

/// The first named child of `n` with one of `kinds`.
fn child_of_kind<'t>(n: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    let mut c = n.walk();
    let found = n.named_children(&mut c).find(|ch| kinds.contains(&ch.kind()));
    found
}

/// Identity of a top-level declaration: kind plus name (or, for things
/// without a name, their normalized text).
fn key(lang: Lang, n: Node, src: &str) -> String {
    let kind = n.kind();
    let name: Option<String> = match (lang, kind) {
        (Lang::Rust, "impl_item") => {
            let ty = field(n, "type", src).unwrap_or("");
            Some(match field(n, "trait", src) {
                Some(tr) => format!("{} for {}", squash(tr), squash(ty)),
                None => squash(ty),
            })
        }
        (Lang::Python, "decorated_definition") => {
            n.child_by_field_name("definition").map(|d| key(lang, d, src))
        }
        (Lang::Python, "expression_statement") => {
            child_of_kind(n, &["assignment"]).and_then(|a| field(a, "left", src)).map(squash)
        }
        (Lang::JavaScript | Lang::TypeScript | Lang::Tsx, "export_statement") => match n.child_by_field_name("declaration") {
            Some(d) => Some(key(lang, d, src)),
            None => Some(squash(text(n, src))),
        },
        (Lang::JavaScript | Lang::TypeScript | Lang::Tsx, "lexical_declaration" | "variable_declaration") => {
            child_of_kind(n, &["variable_declarator"]).and_then(|d| field(d, "name", src)).map(String::from)
        }
        (Lang::Go, "method_declaration") => {
            Some(format!("{} {}", squash(field(n, "receiver", src).unwrap_or("")), field(n, "name", src).unwrap_or("")))
        }
        (Lang::Go, "type_declaration" | "const_declaration" | "var_declaration") => {
            child_of_kind(n, &["type_spec", "type_alias", "const_spec", "var_spec"])
                .and_then(|s| field(s, "name", src))
                .map(String::from)
        }
        _ => field(n, "name", src).map(String::from),
    };
    let name = name.unwrap_or_else(|| squash(text(n, src).lines().next().unwrap_or("")));
    format!("{kind}:{name}")
}

fn split(lang: Lang, src: &str) -> Option<Split> {
    let tree = parse(lang, src)?;
    let root = tree.root_node();
    let mut starts: Vec<(usize, String)> = Vec::new();
    let mut pending: Option<usize> = None;
    let mut c = root.walk();
    for child in root.children(&mut c) {
        if attaches_forward(child.kind()) {
            pending.get_or_insert(child.start_byte());
            continue;
        }
        if !child.is_named() {
            continue;
        }
        let start = pending.take().unwrap_or(child.start_byte());
        starts.push((start, key(lang, child, src)));
    }
    if starts.is_empty() {
        return None;
    }
    // Repeated keys (e.g. two `impl Display for X`?) are numbered in order.
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut items = Vec::with_capacity(starts.len());
    for (i, (start, k)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(src.len(), |(s, _)| *s);
        let n = seen.entry(k.clone()).or_insert(0);
        *n += 1;
        let key = if *n == 1 { k.clone() } else { format!("{k}#{n}") };
        let full = &src[*start..end];
        let body = full.trim_end();
        items.push(Item {
            key,
            text: body.to_string(),
            gap: full[body.len()..].to_string(),
            last: i + 1 == starts.len(),
        });
    }
    Some(Split { header: src[..starts[0].0].to_string(), items })
}

/// A top-level declaration: `kind` (tree-sitter node kind) and `name`.
pub struct Declaration {
    pub kind: String,
    pub name: String,
    pub text: String,
}

/// The top-level declarations of a source file, or `None` if its language is
/// not supported or it does not parse.
pub fn declarations(path: &str, src: &str) -> Option<Vec<Declaration>> {
    let s = split(lang_for(path)?, src)?;
    Some(
        s.items
            .into_iter()
            .map(|i| {
                let (kind, name) = i.key.split_once(':').unwrap_or(("", &i.key));
                Declaration { kind: kind.to_string(), name: name.to_string(), text: i.text }
            })
            .collect(),
    )
}

/// Whether declaration `d` is the one a claim's `symbol` names: by name
/// (`login`, `Display for User`) or by kind and name (`function_item:login`).
pub fn names(d: &Declaration, symbol: &str) -> bool {
    d.name == symbol || format!("{}:{}", d.kind, d.name) == symbol
}

/// Declarations added, removed or changed between two versions of a file
/// (`None` = file absent), or `None` if either version cannot be analyzed.
pub fn changed_declarations(path: &str, old: Option<&str>, new: Option<&str>) -> Option<Vec<Declaration>> {
    let old = match old {
        Some(s) => declarations(path, s)?,
        None => vec![],
    };
    let new = match new {
        Some(s) => declarations(path, s)?,
        None => vec![],
    };
    let key = |d: &Declaration| format!("{}:{}", d.kind, d.name);
    let before: HashMap<String, &str> = old.iter().map(|d| (key(d), d.text.as_str())).collect();
    let after: HashSet<String> = new.iter().map(key).collect();
    let mut out: Vec<Declaration> = Vec::new();
    for d in &new {
        if before.get(&key(d)) != Some(&d.text.as_str()) {
            out.push(Declaration { kind: d.kind.clone(), name: d.name.clone(), text: d.text.clone() });
        }
    }
    out.extend(old.into_iter().filter(|d| !after.contains(&key(d))));
    Some(out)
}

/// Three-way merge of one piece of text; `None` if it conflicts.
fn merge3(base: Option<&str>, ours: &str, theirs: &str) -> Option<String> {
    if ours == theirs || base == Some(theirs) {
        return Some(ours.to_string());
    }
    if base == Some(ours) {
        return Some(theirs.to_string());
    }
    let (merged, conflicts) = merge_text(base?, ours, theirs, &Labels { ours: "ours", theirs: "theirs" });
    (conflicts == 0).then_some(merged)
}

/// Structural merge of a source file; `None` when it cannot merge cleanly.
pub fn merge(path: &str, base: &str, ours: &str, theirs: &str) -> Option<String> {
    let lang = lang_for(path)?;
    let (b, o, t) = (split(lang, base)?, split(lang, ours)?, split(lang, theirs)?);
    let header = merge3(Some(&b.header), &o.header, &t.header)?;
    let bm: HashMap<&str, &str> = b.items.iter().map(|i| (i.key.as_str(), i.text.as_str())).collect();
    let tm: HashMap<&str, &str> = t.items.iter().map(|i| (i.key.as_str(), i.text.as_str())).collect();
    let om: HashSet<&str> = o.items.iter().map(|i| i.key.as_str()).collect();

    // Separator between declarations: what ours mostly uses.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for i in o.items.iter().filter(|i| !i.last) {
        *counts.entry(i.gap.as_str()).or_insert(0) += 1;
    }
    let sep = counts.into_iter().max_by_key(|(g, n)| (*n, g.len())).map_or("\n", |(g, _)| g).to_string();
    let gap_of = |i: &Item| if i.last || !i.gap.contains('\n') { sep.clone() } else { i.gap.clone() };

    // Ours' order, each declaration merged with theirs: (key, text, gap).
    let mut out: Vec<(String, String, String)> = Vec::new();
    for item in &o.items {
        let k = item.key.as_str();
        match (bm.get(k), tm.get(k)) {
            (None, None) => out.push((k.into(), item.text.clone(), gap_of(item))),
            // Deleted by theirs: fine only if ours left it unchanged.
            (Some(bt), None) => {
                if *bt != item.text {
                    return None;
                }
            }
            (b, Some(tt)) => out.push((k.into(), merge3(b.copied(), &item.text, tt)?, gap_of(item))),
        }
    }
    // Theirs' new declarations, after the one they follow there.
    for (i, item) in t.items.iter().enumerate() {
        let k = item.key.as_str();
        if om.contains(k) {
            continue;
        }
        if let Some(bt) = bm.get(k) {
            // Deleted by ours: fine only if theirs left it unchanged.
            if *bt != item.text {
                return None;
            }
            continue;
        }
        let anchor = t.items[..i].iter().rev().find_map(|p| out.iter().position(|(ok, _, _)| *ok == p.key));
        let mut pos = anchor.map_or(0, |a| a + 1);
        // Keep ours' own additions right after the anchor together.
        while pos < out.len() && !bm.contains_key(out[pos].0.as_str()) && !tm.contains_key(out[pos].0.as_str()) {
            pos += 1;
        }
        out.insert(pos, (k.into(), item.text.clone(), gap_of(item)));
    }

    // The file ends the way ours ends.
    let end = o.items.last().map_or(String::new(), |i| i.gap.clone());
    let mut result = header;
    let n = out.len();
    for (i, (_, text, gap)) in out.into_iter().enumerate() {
        result.push_str(&text);
        result.push_str(if i + 1 == n { &end } else { &gap });
    }
    parse(lang, &result)?;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "use std::fmt;\n\nfn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";

    #[test]
    fn both_sides_append_functions() {
        let ours = format!("{BASE}\nfn mine() {{}}\n");
        let theirs = format!("{BASE}\nfn theirs() {{}}\n");
        // The line merge conflicts: both inserted at the end.
        assert!(merge_text(BASE, &ours, &theirs, &Labels { ours: "o", theirs: "t" }).1 > 0);
        let m = merge("x.rs", BASE, &ours, &theirs).unwrap();
        assert!(m.contains("fn mine() {}") && m.contains("fn theirs() {}"), "{m}");
        assert!(m.find("fn mine").unwrap() < m.find("fn theirs").unwrap());
        assert!(m.starts_with("use std::fmt;"));
    }

    #[test]
    fn edits_to_different_functions_and_additions_combine() {
        let ours = BASE.replace("    1\n", "    10\n") + "\nfn c() {}\n";
        let theirs = BASE.replace("    2\n", "    20\n") + "\n/// Docs.\n#[inline]\nfn d() {}\n";
        let m = merge("x.rs", BASE, &ours, &theirs).unwrap();
        for s in ["    10\n", "    20\n", "fn c() {}", "/// Docs.\n#[inline]\nfn d() {}"] {
            assert!(m.contains(s), "missing {s:?} in\n{m}");
        }
    }

    #[test]
    fn real_conflicts_are_refused() {
        // Same function changed differently on both sides.
        let ours = BASE.replace("    1\n", "    10\n");
        let theirs = BASE.replace("    1\n", "    11\n");
        assert!(merge("x.rs", BASE, &ours, &theirs).is_none());
        // Deleted on one side, modified on the other.
        let ours = BASE.replace("fn b() {\n    2\n}\n", "");
        let theirs = BASE.replace("    2\n", "    22\n");
        assert!(merge("x.rs", BASE, &ours, &theirs).is_none());
        // Same new name, different bodies.
        assert!(merge("x.rs", BASE, &format!("{BASE}\nfn n() {{ 1 }}\n"), &format!("{BASE}\nfn n() {{ 2 }}\n")).is_none());
        // Parse errors, unknown languages.
        assert!(merge("x.rs", BASE, "fn (", BASE).is_none());
        assert!(merge("x.txt", BASE, BASE, BASE).is_none());
    }

    #[test]
    fn deletion_on_one_side_applies() {
        let ours = BASE.replace("fn b() {\n    2\n}\n", "") + "fn c() {}\n";
        let theirs = format!("{BASE}\nfn d() {{}}\n");
        let m = merge("x.rs", BASE, &ours, &theirs).unwrap();
        assert!(!m.contains("fn b()") && m.contains("fn c()") && m.contains("fn d()"), "{m}");
    }

    #[test]
    fn other_languages() {
        let py = "import os\n\n\ndef a():\n    return 1\n";
        let m = merge(
            "m.py",
            py,
            &format!("{py}\n\ndef mine():\n    pass\n"),
            &format!("{py}\n\n@cache\ndef theirs():\n    pass\n"),
        )
        .unwrap();
        assert!(m.contains("def mine") && m.contains("@cache\ndef theirs"), "{m}");

        let ts = "import { x } from './x';\n\nexport function a(): number {\n  return 1;\n}\n";
        let m = merge(
            "m.ts",
            ts,
            &format!("{ts}\nexport const mine = 1;\n"),
            &format!("{ts}\ninterface Theirs {{ y: number }}\n"),
        )
        .unwrap();
        assert!(m.contains("export const mine") && m.contains("interface Theirs"), "{m}");

        let go = "package main\n\nimport \"fmt\"\n\nfunc a() {\n\tfmt.Println(1)\n}\n";
        let m = merge(
            "m.go",
            go,
            &format!("{go}\nfunc (s *S) Mine() {{}}\n"),
            &format!("{go}\ntype Theirs struct{{}}\n"),
        )
        .unwrap();
        assert!(m.contains("func (s *S) Mine()") && m.contains("type Theirs struct{}"), "{m}");
    }
}
