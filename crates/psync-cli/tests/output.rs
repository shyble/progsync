//! Writing output to a reader that has gone away.

use std::path::Path;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_psync");

/// Run psync with stdout a pipe whose reading end is already closed (as
/// after `| head` exits): (exit code, stderr).
fn run_into_closed_pipe(dir: &Path, args: &[&str]) -> (Option<i32>, String) {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(BIN)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("PSYNC_USER_NAME", "Ada")
        .env("PSYNC_AGENT", "test")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn a_closed_output_pipe_ends_the_command_quietly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    assert!(Command::new(BIN).env("XDG_CONFIG_HOME", dir.path().join("cfg")).arg("-C").arg(dir.path()).arg("init").output().unwrap().status.success());

    for args in [&["status"][..], &["--json", "log"], &["--help"], &["--version"]] {
        assert_eq!(run_into_closed_pipe(dir.path(), args), (Some(0), String::new()), "{args:?}");
    }
    // A failing command keeps its exit code.
    assert_eq!(run_into_closed_pipe(dir.path(), &["--json", "show", "nosuchrev"]), (Some(1), String::new()));
}
