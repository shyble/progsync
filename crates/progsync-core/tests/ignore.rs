use progsync_core::wc::{check_ignore, ignored_paths};
use progsync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

#[test]
fn check_ignore_names_the_rule_and_ignored_paths_lists_what_commits_leave_out() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", b"*.log\n");
    write(root, ".psyncignore", b"# models\nweights/\n*.bin\n!keep.bin\n");
    write(root, "sub/.psyncignore", b"local.txt\n");
    write(root, "weights/w.pt", b"model");
    write(root, "data/x.bin", b"xx");
    write(root, "keep.bin", b"k");
    write(root, "run.log", b"log");
    write(root, "sub/local.txt", b"l");
    write(root, "src/a.rs", b"a");
    write(root, "nested/.psync", b"{}");

    let v = check_ignore(root, "weights/w.pt").unwrap();
    assert!(v.ignored);
    assert_eq!(v.inside.as_deref(), Some("weights"));
    let r = v.rule.unwrap();
    assert_eq!((r.source.as_str(), r.line, r.pattern.as_str()), (".psyncignore", 2, "weights/"));

    let v = check_ignore(root, "data/x.bin").unwrap();
    assert!(v.ignored && v.inside.is_none());
    assert_eq!(v.rule.unwrap().line, 3);
    let v = check_ignore(root, "keep.bin").unwrap();
    assert!(!v.ignored && v.rule.unwrap().negated, "re-included");
    assert_eq!(check_ignore(root, "run.log").unwrap().rule.unwrap().source, ".gitignore");
    assert_eq!(check_ignore(root, "sub/local.txt").unwrap().rule.unwrap().source, "sub/.psyncignore");
    let v = check_ignore(root, "src/a.rs").unwrap();
    assert!(!v.ignored && v.rule.is_none());
    assert!(check_ignore(root, "src/not-yet.bin").unwrap().ignored, "paths need not exist");
    assert!(check_ignore(root, "nested/x").unwrap().reason.is_some(), "nested workspace");

    let list: Vec<(String, bool)> = ignored_paths(root).unwrap().into_iter().map(|p| (p.path, p.is_dir)).collect();
    assert_eq!(
        list,
        vec![
            ("data/x.bin".to_string(), false),
            ("run.log".into(), false),
            ("sub/local.txt".into(), false),
            ("weights".into(), true),
        ]
    );
}

#[test]
fn commit_refuses_files_above_the_limit_unless_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a.txt", b"a");
    let repo = Repo::init(root, Identity { name: "Ada".into(), email: "a@x".into(), agent: None }).unwrap();
    progsync_core::config::set(&progsync_core::config::repo_path(repo.dot()), "commit.max_file_mb", Some("1")).unwrap();
    assert!(progsync_core::config::set(&progsync_core::config::repo_path(repo.dot()), "commit.max_file_mb", Some("big")).is_err());
    repo.commit(&msg("small")).unwrap();

    write(root, "model.pt", &vec![7u8; 1_500_000]);
    write(root, "b.txt", b"b");
    repo.snapshot().unwrap();
    let err = repo.commit(&msg("with a model")).unwrap_err().to_string();
    assert!(err.contains("model.pt") && err.contains("--allow-large"), "{err}");
    // Committing other paths only, or with allow_large, works.
    repo.commit_paths(&msg("just b"), &["b.txt".into()]).unwrap();
    let ((_, done), _) = repo.commit_with(&msg("model on purpose"), &[], true).unwrap();
    assert_eq!(repo.changes_of(&done).unwrap().len(), 1);
}
