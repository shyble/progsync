//! `log --long`: the expanded view.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_psync");

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("PSYNC_USER_NAME", "Ada")
        .env("PSYNC_AGENT", "test")
        .env("NO_COLOR", "1")
        .arg("-C")
        .arg(dir.join("repo"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn long_log_shows_details_files_and_the_whole_intent() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["task", "add", "Rate limits"]);
    std::fs::write(repo.join("a.txt"), "a\nb\n").unwrap();
    let intent = "Add rate limiting\n\nA token bucket per client, refilled every second, so that one noisy client cannot starve the others.";
    ok(dir.path(), &["commit", "-m", intent, "--task", "T-1", "--meta", "model=opus"]);

    let text = ok(dir.path(), &["log", "--long"]);
    for want in ["Author     test for Ada", "Task       T-1 Rate limits", "model      opus", "Files      1 file  +2 -0", "A a.txt  +2 -0", "    Add rate limiting\n"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(text.contains("\n      A token bucket per client"), "the body, indented:\n{text}");
    assert!(text.lines().all(|l| l.chars().count() <= 80), "intent is wrapped:\n{text}");
    assert!(text.lines().all(|l| !l.ends_with(' ')), "no trailing spaces:\n{text}");
    assert!(!text.lines().last().unwrap().starts_with('│'), "the rail ends at the oldest change:\n{text}");
    assert_eq!(ok(dir.path(), &["log", "-l", "-n", "1"]).matches("Author").count(), 1);

    let json = ok(dir.path(), &["--json", "log", "-l"]);
    assert!(json.contains("\"files\"") && json.contains("\"a.txt\""), "{json}");
    assert!(ok(dir.path(), &["log"]).lines().count() == 2, "the one-line view is unchanged");
}
