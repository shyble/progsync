//! Git bridge: import a Git branch's history into `main`, and export `main`
//! as Git commits. Uses the `git` command (plumbing only); the Git working
//! tree is never touched.
//!
//! - **Import** reads the branch's first-parent history in one
//!   `git log --raw` pass and all file contents in one `git cat-file --batch`
//!   pass. Each commit becomes a revision with the commit's message as intent,
//!   its author and committer, its dates, and `git_commit` metadata; change
//!   ids are derived from the commit hash, so importing is repeatable. A later
//!   import continues from the commit main was imported from (main must not
//!   have psync-only changes on top). Submodules are skipped.
//! - **Export** writes main's first-parent history as commits on a branch
//!   with `git fast-import`, continuing from the last revision already known
//!   to the Git repository (imported from it, or exported to it before; the
//!   mapping is kept in `.psync/git-export.json`). The branch is only ever
//!   fast-forwarded.

use crate::diff;
use crate::hash::{ChangeId, Hash};
use crate::object::{ChangeMeta, EntryKind, Identity, Leaf, Object, Revision};
use crate::repo::{Repo, MAIN};
use crate::util::atomic_write;
use crate::wc;
use crate::bail;
use crate::error::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct ImportOutcome {
    pub commits: usize,
    pub main: Option<Hash>,
    /// The Git commit main now corresponds to.
    pub git_head: String,
    pub skipped_submodules: usize,
    /// The working change was empty and has been moved onto the new main.
    pub synced: bool,
}

pub struct ExportOutcome {
    pub commits: usize,
    pub branch: String,
    pub git_head: Option<String>,
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn git_out(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir).args(args).output().context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn git_dir_key(dir: &Path) -> Result<String> {
    let d = git_out(dir, &["rev-parse", "--absolute-git-dir"])?;
    Ok(crate::platform::canonicalize(&PathBuf::from(d))?.display().to_string())
}

fn change_id_for(sha: &str) -> ChangeId {
    let h = blake3::hash(format!("git-commit:{sha}").as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h.as_bytes()[..16]);
    ChangeId(b)
}

struct GitCommit {
    sha: String,
    author: Identity,
    author_s: i64,
    committer: Identity,
    committer_s: i64,
    message: String,
    /// (path, new leaf as (git mode, git blob sha), or None if deleted)
    changes: Vec<(String, Option<(String, String)>)>,
}

/// Parse `git log --raw -z` output written with the format below.
fn parse_log(out: &[u8]) -> Result<Vec<GitCommit>> {
    let mut commits = Vec::new();
    for chunk in out.split(|b| *b == 1).filter(|c| !c.is_empty()) {
        let mut f = chunk.split(|b| *b == 0);
        let mut next = || -> Result<String> {
            Ok(String::from_utf8_lossy(f.next().context("truncated git log output")?).into_owned())
        };
        let sha = next()?;
        let (an, ae, at) = (next()?, next()?, next()?);
        let (cn, ce, ct) = (next()?, next()?, next()?);
        let message = next()?;
        let mut changes = Vec::new();
        let rest: Vec<String> = f.map(|b| String::from_utf8_lossy(b).into_owned()).collect();
        let mut i = 0;
        while i < rest.len() {
            let meta = rest[i].trim_start_matches('\n');
            if meta.is_empty() {
                i += 1;
                continue;
            }
            // ":old_mode new_mode old_sha new_sha status" then the path.
            let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            let path = rest.get(i + 1).context("git log: path missing")?.clone();
            i += 2;
            if parts.len() < 5 {
                bail!("unexpected git log line {meta:?}");
            }
            let (new_mode, new_sha, status) = (parts[1], parts[3], parts[4]);
            if status.starts_with('D') {
                changes.push((path, None));
            } else {
                changes.push((path, Some((new_mode.to_string(), new_sha.to_string()))));
            }
        }
        let id = |n: String, e: String| Identity { name: n, email: e, agent: None };
        commits.push(GitCommit {
            sha,
            author: id(an, ae),
            author_s: at.parse().unwrap_or(0),
            committer: id(cn, ce),
            committer_s: ct.parse().unwrap_or(0),
            message,
            changes,
        });
    }
    Ok(commits)
}

/// Read blobs with one `git cat-file --batch`; returns sha -> bytes.
fn read_blobs(dir: &Path, shas: &[String]) -> Result<HashMap<String, Vec<u8>>> {
    let mut out = HashMap::new();
    if shas.is_empty() {
        return Ok(out);
    }
    let mut child = git(dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running git cat-file")?;
    let mut stdin = child.stdin.take().expect("piped");
    let requests: Vec<String> = shas.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for s in requests {
            writeln!(stdin, "{s}")?;
        }
        Ok(())
    });
    let mut r = BufReader::new(child.stdout.take().expect("piped"));
    for sha in shas {
        let mut header = String::new();
        r.read_line(&mut header)?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() != 3 || parts[1] != "blob" {
            bail!("git cat-file: unexpected reply for {sha}: {header:?}");
        }
        let size: usize = parts[2].parse()?;
        let mut data = vec![0u8; size + 1];
        r.read_exact(&mut data)?;
        data.pop(); // trailing newline
        out.insert(sha.clone(), data);
    }
    writer.join().expect("writer thread")?;
    child.wait()?;
    Ok(out)
}

fn kind_for_mode(mode: &str) -> Option<EntryKind> {
    match mode {
        "100644" | "100664" => Some(EntryKind::File),
        "100755" => Some(EntryKind::Exec),
        "120000" => Some(EntryKind::Symlink),
        _ => None, // 160000: submodule
    }
}

fn mode_for(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Exec => "100755",
        EntryKind::Symlink => "120000",
        _ => "100644",
    }
}

impl Repo {
    /// Import (or continue importing) `branch` of the Git repository at
    /// `dir` into main.
    pub fn git_import(&self, dir: &Path, branch: &str) -> Result<ImportOutcome> {
        let tip = git_out(dir, &["rev-parse", "--verify", &format!("{branch}^{{commit}}")])
            .with_context(|| format!("no branch '{branch}' in Git repository {}", dir.display()))?;
        let main = self.main_tip()?;
        let (range, mut files, mut parent): (String, BTreeMap<String, Leaf>, Option<Hash>) = match main {
            None => (tip.clone(), BTreeMap::new(), None),
            Some(m) => {
                let rev = self.store.get_revision(&m)?;
                let Some(from) = rev.meta.extra.get("git_commit").cloned() else {
                    bail!("main has changes that did not come from Git; `git import` can only continue an imported history");
                };
                if from == tip {
                    return Ok(ImportOutcome {
                        commits: 0,
                        main,
                        git_head: tip,
                        skipped_submodules: 0,
                        synced: false,
                    });
                }
                if !git_ok(dir, &["merge-base", "--is-ancestor", &from, &tip]) {
                    bail!("{branch} no longer contains {} (main was imported from it); history was rewritten in Git", &from[..12]);
                }
                let files = diff::flatten(&self.store, rev.tree)?.into_iter().collect();
                (format!("{from}..{tip}"), files, Some(m))
            }
        };
        let out = git(dir)
            .args([
                "log",
                "--first-parent",
                "--reverse",
                "--root",
                "--raw",
                "-z",
                "--no-renames",
                "--no-abbrev",
                "--diff-merges=first-parent",
                "--format=%x01%H%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%B%x00",
                &range,
            ])
            .output()
            .context("running git log")?;
        if !out.status.success() {
            bail!("git log failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let commits = parse_log(&out.stdout)?;

        // All file contents in one pass.
        let mut wanted: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for c in &commits {
            for (_, new) in &c.changes {
                if let Some((mode, sha)) = new {
                    if kind_for_mode(mode).is_some() && seen.insert(sha.clone()) {
                        wanted.push(sha.clone());
                    }
                }
            }
        }
        let blobs = read_blobs(dir, &wanted)?;
        let mut stored: HashMap<String, Hash> = HashMap::new();
        for (sha, data) in &blobs {
            stored.insert(sha.clone(), self.store.put_blob(data)?);
        }

        let mut skipped = 0;
        for c in &commits {
            for (path, new) in &c.changes {
                match new {
                    None => {
                        files.remove(path);
                    }
                    Some((mode, sha)) => match kind_for_mode(mode) {
                        Some(kind) => {
                            files.insert(path.clone(), (kind, stored[sha]));
                        }
                        None => skipped += 1,
                    },
                }
            }
            let tree = wc::build_tree_from(&self.store, files.iter().map(|(p, (k, h))| (p.as_str(), *k, *h)))?;
            let mut meta = ChangeMeta { intent: c.message.trim_end().to_string(), ..Default::default() };
            meta.extra.insert("git_commit".into(), c.sha.clone());
            let parents: Vec<Hash> = parent.into_iter().collect();
            let rev = Revision {
                change_id: change_id_for(&c.sha),
                tree,
                generation: self.generation_for(&parents)?,
                parents,
                meta,
                author: c.author.clone(),
                committer: c.committer.clone(),
                conflicts: vec![],
                created_ms: c.author_s * 1000,
                updated_ms: c.committer_s * 1000,
            };
            parent = Some(self.store.put(&Object::Revision(rev))?);
        }

        let new_main = parent;
        if let Some(nm) = new_main.filter(|nm| Some(*nm) != main) {
            self.update_view(
                None,
                format!("git import: {} commit(s) from {} ({branch})", commits.len(), dir.display()),
                None,
                vec![MAIN.into()],
                |v| {
                    if v.bookmarks.get(MAIN).copied() != main {
                        bail!("main moved during the import; run it again");
                    }
                    v.bookmarks.insert(MAIN.into(), nm);
                    Ok(true)
                },
            )?;
        }
        // An untouched working change follows main, so the files appear.
        let (_, w) = self.working()?;
        let empty = w.meta.intent.is_empty() && w.conflicts.is_empty() && w.tree == self.parent_tree(&w)?;
        let synced = empty && new_main.is_some() && self.sync().is_ok();
        Ok(ImportOutcome { commits: commits.len(), main: new_main, git_head: tip, skipped_submodules: skipped, synced })
    }

    /// Export main's history as commits on `branch` of the Git repository at `dir`.
    /// With `vet`, each revision is first passed to the message check
    /// (see [`crate::msgcheck`]); nothing is written if one is refused.
    pub fn git_export(&self, dir: &Path, branch: &str, vet: bool) -> Result<ExportOutcome> {
        let main = self.main_tip()?.context("nothing to export: there is no main yet")?;
        if !git_ok(dir, &["rev-parse", "--git-dir"]) {
            bail!("{} is not a Git repository (create one with `git init`)", dir.display());
        }
        let key = git_dir_key(dir)?;
        let map_path = self.dot().join("git-export.json");
        let mut all: BTreeMap<String, BTreeMap<String, String>> = std::fs::read(&map_path)
            .ok()
            .and_then(|b| crate::json::from_slice(&b).ok())
            .unwrap_or_default();
        let known = all.entry(key.clone()).or_default();
        let in_git = |sha: &str| git_ok(dir, &["cat-file", "-e", &format!("{sha}^{{commit}}")]);

        // Walk back from main to the newest revision Git already has.
        let mut todo: Vec<(Hash, Revision)> = Vec::new();
        let mut start: Option<String> = None;
        let mut next = Some(main);
        while let Some(h) = next {
            let rev = self.store.get_revision(&h)?;
            let sha = known.get(&h.to_hex()).cloned().or_else(|| rev.meta.extra.get("git_commit").cloned());
            if let Some(s) = sha.filter(|s| in_git(s)) {
                start = Some(s);
                break;
            }
            next = rev.parents.first().copied();
            todo.push((h, rev));
        }
        todo.reverse();
        if vet {
            for (h, rev) in &todo {
                self.vet(crate::msgcheck::Stage::Export, Some(h), rev, None)?;
            }
        }
        let refname = format!("refs/heads/{branch}");
        let current = git_out(dir, &["rev-parse", "--verify", "-q", &refname]).ok().filter(|s| !s.is_empty());
        if todo.is_empty() {
            return Ok(ExportOutcome { commits: 0, branch: branch.into(), git_head: start });
        }
        match (&current, &start) {
            (Some(cur), Some(s)) if cur != s && !git_ok(dir, &["merge-base", "--is-ancestor", cur, s]) => {
                bail!("branch {branch} has commits that are not on psync's main; export to another branch")
            }
            (Some(_), None) => bail!("branch {branch} already exists and shares no history with psync's main; export to a new branch"),
            _ => {}
        }

        let mut stream: Vec<u8> = Vec::new();
        let mut prev_tree = match &start {
            Some(_) => Some(self.store.get_revision(&todo[0].1.parents[0])?.tree),
            None => None,
        };
        for (i, (_, rev)) in todo.iter().enumerate() {
            let msg = if rev.meta.intent.is_empty() { "(no description)".to_string() } else { rev.meta.intent.clone() };
            let msg = if msg.ends_with('\n') { msg } else { format!("{msg}\n") };
            let who = |id: &Identity, ms: i64| format!("{} <{}> {} +0000", id.name, id.email, ms.div_euclid(1000));
            write!(stream, "commit {refname}\nmark :{}\n", i + 1)?;
            writeln!(stream, "author {}", who(&rev.author, rev.created_ms))?;
            writeln!(stream, "committer {}", who(&rev.committer, rev.updated_ms))?;
            write!(stream, "data {}\n{msg}", msg.len())?;
            if i == 0 {
                if let Some(s) = &start {
                    writeln!(stream, "from {s}")?;
                }
            }
            for c in diff::diff_trees(&self.store, prev_tree, Some(rev.tree))? {
                if c.path.contains('\n') || c.path.starts_with('"') {
                    bail!("cannot export path {:?} to Git", c.path);
                }
                match c.new {
                    None => writeln!(stream, "D {}", c.path)?,
                    Some((kind, h)) => {
                        let data = self.store.get_blob(&h)?;
                        write!(stream, "M {} inline {}\ndata {}\n", mode_for(kind), c.path, data.len())?;
                        stream.extend_from_slice(&data);
                        stream.push(b'\n');
                    }
                }
            }
            stream.push(b'\n');
            prev_tree = Some(rev.tree);
        }
        let marks = tempfile_path(self.dot());
        let mut child = git(dir)
            .args(["fast-import", "--quiet", &format!("--export-marks={}", marks.display())])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running git fast-import")?;
        child.stdin.take().expect("piped").write_all(&stream)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&marks);
            bail!("git fast-import failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let marks_text = std::fs::read_to_string(&marks)?;
        let _ = std::fs::remove_file(&marks);
        let mut head = None;
        for line in marks_text.lines() {
            let Some((m, sha)) = line.split_once(' ') else { continue };
            let n: usize = m.trim_start_matches(':').parse()?;
            if let Some((h, _)) = todo.get(n - 1) {
                known.insert(h.to_hex(), sha.to_string());
                if n == todo.len() {
                    head = Some(sha.to_string());
                }
            }
        }
        atomic_write(&map_path, &crate::json::to_vec_pretty(&all)?)?;
        Ok(ExportOutcome { commits: todo.len(), branch: branch.into(), git_head: head })
    }
}

fn tempfile_path(dot: &Path) -> PathBuf {
    dot.join("tmp").join(format!("git-marks-{}", crate::util::random_hex(16)))
}
