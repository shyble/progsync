//! The message check (T-31): a project's rules for what a change's record
//! may say, enforced by a script of its own.
//!
//! `psync config commit.check "scripts/msg-check"` runs that command (in the
//! workspace, with `sh -c` / `cmd /C`) for each change, with the change's
//! whole record as one JSON object on stdin:
//!
//! ```json
//! {"stage": "commit", "revision": null, "change_id": "wvxsyk...",
//!  "intent": "...", "task": "T-1", "meta": {"k": "v"},
//!  "author": {"name": "...", "email": "...", "agent": "..."},
//!  "committer": {...}, "checks": [...], "landed_by": null}
//! ```
//!
//! A non-zero exit refuses the change, with the script's output as the
//! reason. Stages:
//!
//! - `commit` and `describe`: the change as it is about to be recorded
//!   (`revision` is null; at `describe` the intent may still be empty);
//! - `land`: every change about to go onto main, before the land check
//!   runs, with `landed_by` the identity the land will record;
//! - `export`: every revision `git export` is about to write as a commit.
//!
//! The environment has `PSYNC_WORKSPACE` and `PSYNC_STAGE`.

use crate::bail;
use crate::config;
use crate::error::{Context, Result};
use crate::hash::Hash;
use crate::object::{Identity, Revision};
use crate::repo::Repo;
use std::io::Write;
use std::process::Stdio;

pub const KEY: &str = "commit.check";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Commit,
    Describe,
    Land,
    Export,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Commit => "commit",
            Stage::Describe => "describe",
            Stage::Land => "land",
            Stage::Export => "export",
        }
    }
}

/// The JSON record a message check reads.
pub fn record(stage: Stage, revision: Option<&Hash>, rev: &Revision, landed_by: Option<&Identity>) -> Result<String> {
    let v = crate::json!({
        "stage": stage.name(),
        "revision": revision,
        "change_id": rev.change_id,
        "intent": rev.meta.intent,
        "task": rev.meta.task,
        "meta": rev.meta.extra,
        "author": rev.author,
        "committer": rev.committer,
        "checks": rev.meta.checks,
        "landed_by": landed_by,
    });
    crate::json::to_string(&v)
}

impl Repo {
    /// The configured message check, if any.
    pub fn commit_check(&self) -> Option<String> {
        config::get(Some(&self.dot), KEY).map(|(v, _)| v).filter(|v| !v.trim().is_empty())
    }

    /// Run the message check on one change; an error if it refuses it.
    pub(crate) fn vet(&self, stage: Stage, revision: Option<&Hash>, rev: &Revision, landed_by: Option<&Identity>) -> Result<()> {
        let Some(cmd) = self.commit_check() else { return Ok(()) };
        let input = record(stage, revision, rev, landed_by)?;
        let mut child = crate::platform::shell(&cmd)
            .current_dir(&self.root)
            .env("PSYNC_WORKSPACE", &self.workspace)
            .env("PSYNC_STAGE", stage.name())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {KEY} `{cmd}`"))?;
        // A script that exits without reading its input is fine.
        let _ = child.stdin.take().expect("piped").write_all(input.as_bytes());
        let out = child.wait_with_output().with_context(|| format!("running {KEY} `{cmd}`"))?;
        if out.status.success() {
            return Ok(());
        }
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let reason = if lines.is_empty() {
            format!("exit {}", out.status.code().map_or("by signal".into(), |c| c.to_string()))
        } else {
            lines[lines.len().saturating_sub(20)..].join("\n")
        };
        let hint = match stage {
            Stage::Commit | Stage::Describe => "change the message or metadata (-m, --meta k=) and try again".to_string(),
            Stage::Land => format!(
                "fix it with `psync describe -r {} -m ...` (or --meta k= to drop a field), then land again",
                &rev.change_id.short()[..8]
            ),
            Stage::Export => "it is already on main, so it cannot be changed; to export it anyway, pass --no-commit-check".to_string(),
        };
        bail!("{KEY} `{cmd}` refused change {} at {}:\n{reason}\n({hint})", &rev.change_id.short()[..8], stage.name())
    }
}
