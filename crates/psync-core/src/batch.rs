//! Batch land (T-33): land several workspaces' committed changes under one
//! check, `psync land claude-1@ codex-2@ --check "<suite>"`.
//!
//! The changes are stacked on main in order: this workspace's committed
//! changes first (if any), then each named workspace's, every change
//! rebased on the one before it (a change already on main, or already in
//! the batch, under the same change id is skipped). Each keeps its change
//! id, author, intent and task, so main shows who did what, and each owner's
//! next `sync` drops what landed. Only committed changes land: what a
//! workspace has not committed stays its own.
//!
//! If a change conflicts with main or with the changes before it, nothing
//! lands and the error names it (its owner syncs and resolves; the others
//! can land without it). Otherwise the combined tree is checked out in this
//! workspace (which must be clean), the check runs once against it, and
//! main moves to the top of the stack with the check recorded there; this
//! workspace is left on top of the new main. On a failed check the
//! workspace's files are put back.

use crate::bail;
use crate::error::{Context, Result};
use crate::hash::{ChangeId, Hash};
use crate::merge::{self, Labels};
use crate::object::{Check, Object, Revision};
use crate::repo::{first_line, LandResult, Repo, MAIN};
use crate::util::now_ms;
use crate::wc;
use std::collections::HashSet;

/// Where each landed change came from (`you`, or the spec it was named by).
pub struct BatchLanded {
    pub result: LandResult,
    /// One per change in the result, in the same order.
    pub sources: Vec<String>,
}

impl Repo {
    /// Land this workspace's and `specs`' committed changes together (see
    /// module docs). `specs` name workspaces: `claude-1@`, `claude-1@laptop`.
    pub fn land_batch(
        &self,
        specs: &[String],
        check: Option<&str>,
        on_wait: &mut dyn FnMut(&crate::queue::LandQueue),
    ) -> Result<BatchLanded> {
        let (_, w) = self.working()?;
        if !w.conflicts.is_empty() {
            bail!("resolve the {} conflict(s) in your working change first (see `psync status`)", w.conflicts.len());
        }
        if !self.changes_of(&w)?.is_empty() {
            bail!("your working change has uncommitted edits: `psync commit` them (or `psync undo`) before landing");
        }
        // The committed tip of each source.
        let mut sources: Vec<(String, Option<Hash>, Option<String>)> = vec![("you".into(), w.parents.first().copied(), None)];
        let view = self.view()?;
        for spec in specs {
            let spec = if spec.contains('@') { spec.clone() } else { format!("{spec}@") };
            let (ws, at) = spec.split_once('@').expect("has @");
            if ws.is_empty() {
                bail!("'{spec}' does not name a workspace: use <workspace>@ or <workspace>@<host>");
            }
            if at.is_empty() && ws == self.workspace {
                continue; // this workspace is always part of the batch
            }
            let head = self.resolve_workspace(&view, ws, at)?;
            let tip = self.store.get_revision(&head)?.parents.first().copied();
            let local = (at.is_empty() && view.workspaces.contains_key(ws)).then(|| ws.to_string());
            if sources.iter().any(|(s, _, _)| *s == spec) {
                bail!("{spec} is named twice");
            }
            sources.push((spec, tip, local));
        }
        if sources.len() < 2 {
            bail!("name the workspaces to land with yours, e.g. `psync land claude-1@ codex-2@`");
        }
        let _turn = if self.land_queue_on() {
            let place = crate::queue::Place::join(&self.dot, &self.workspace, &self.identity, check)?;
            place.wait_for_main(on_wait)?;
            Some(place)
        } else {
            None
        };
        let mut on_disk = w.tree;
        let result = self.land_batch_attempts(&sources, check, &mut on_disk);
        if !matches!(result, Ok(BatchLanded { result: LandResult::Landed { .. }, .. })) && on_disk != w.tree {
            // Nothing landed: the workspace gets its own files back.
            wc::checkout(&self.root, &self.store, on_disk, w.tree, self.eol()?)?;
            self.refresh_index()?;
        }
        result
    }

    fn land_batch_attempts(
        &self,
        sources: &[(String, Option<Hash>, Option<String>)],
        check: Option<&str>,
        on_disk: &mut Hash,
    ) -> Result<BatchLanded> {
        for _ in 0..3 {
            let main = self.main_tip()?.context("there is no main yet: land one workspace first (`psync land`)")?;
            let main_rev = self.store.get_revision(&main)?;
            let (mut parent, mut parent_tree) = (main, main_rev.tree);
            let mut stack: Vec<(Hash, Revision)> = Vec::new();
            let mut from: Vec<String> = Vec::new();
            let mut seen: HashSet<ChangeId> = HashSet::new();
            for (label, tip, _) in sources {
                let Some(tip) = tip else { continue };
                let chain = self.unlanded(*tip, Some(main))?;
                let on_main = match chain.first() {
                    Some((_, first)) => self.change_ids_on_main_since(main, first.parents.first().copied())?,
                    None => HashSet::new(),
                };
                for (h, rev) in chain {
                    if on_main.contains(&rev.change_id) || !seen.insert(rev.change_id) {
                        continue;
                    }
                    if rev.parents.first() == Some(&parent) {
                        parent = h;
                        parent_tree = rev.tree;
                        stack.push((h, rev));
                        from.push(label.clone());
                        continue;
                    }
                    let base = self.parent_tree(&rev)?;
                    let theirs = format!("{label} ({})", &rev.change_id.short()[..8]);
                    let m = merge::merge_trees(&self.store, Some(base), parent_tree, rev.tree, &Labels { ours: "batch", theirs: &theirs })?;
                    if !m.conflicts.is_empty() {
                        let paths: Vec<&str> = m.conflicts.iter().map(|c| c.path.as_str()).collect();
                        let with = if stack.is_empty() { "main" } else { "main or the changes before it in this batch" };
                        bail!(
                            "nothing landed: {label}'s change {} ({}) conflicts with {with} in {}. Land without {label}, or have it `psync sync` and resolve first",
                            &rev.change_id.short()[..8],
                            first_line(&rev.meta.intent),
                            paths.join(", ")
                        );
                    }
                    let mut nr = rev.clone();
                    nr.tree = m.tree;
                    nr.parents = vec![parent];
                    nr.generation = self.generation_for(&nr.parents)?;
                    nr.committer = self.identity.clone();
                    nr.updated_ms = now_ms();
                    let nh = self.store.put(&Object::Revision(nr.clone()))?;
                    parent = nh;
                    parent_tree = nr.tree;
                    stack.push((nh, nr));
                    from.push(label.clone());
                }
            }
            if stack.is_empty() {
                bail!("nothing to land: none of these workspaces has committed changes that main lacks");
            }
            for (h, r) in &stack {
                self.vet(crate::msgcheck::Stage::Land, Some(h), r, Some(&self.identity))?;
            }
            // The check runs here, on exactly the combined tree.
            wc::checkout(&self.root, &self.store, *on_disk, parent_tree, self.eol()?)?;
            *on_disk = parent_tree;
            let run = match check {
                Some(cmd) => Some(self.run_check(cmd, parent_tree)?),
                None => None,
            };
            if let Some(r) = run.as_ref().filter(|r| !r.passed) {
                return Ok(BatchLanded { result: LandResult::CheckFailed { check: r.clone(), changes: stack }, sources: from });
            }
            if let Some(r) = &run {
                let (_, top) = stack.last_mut().expect("non-empty");
                top.meta.checks.push(Check {
                    command: r.command.clone(),
                    passed: true,
                    exit_code: r.exit_code,
                    duration_ms: r.duration_ms,
                    tree: top.tree,
                    time_ms: now_ms(),
                    by: self.identity.clone(),
                });
                let th = self.store.put(&Object::Revision(top.clone()))?;
                stack.last_mut().expect("non-empty").0 = th;
            }
            let top = stack.last().expect("non-empty").0;
            // This workspace continues on top of the new main.
            let (_, w) = self.working()?;
            let mut nw = w.clone();
            nw.tree = parent_tree;
            nw.parents = vec![top];
            nw.generation = self.generation_for(&nw.parents)?;
            let nwh = self.store.put(&Object::Revision(nw))?;
            let ws = self.workspace.clone();
            let mut released = vec![ws.clone()];
            released.extend(sources.iter().filter_map(|(_, _, local)| local.clone()));
            let mut shared = vec![MAIN.to_string(), "claims".to_string()];
            let tasks: HashSet<String> = stack.iter().filter_map(|(_, r)| r.meta.task.clone()).collect();
            shared.extend(tasks.iter().map(|t| format!("task:{t}")));
            let names: Vec<&str> = sources.iter().map(|(s, _, _)| s.as_str()).collect();
            let desc = format!("land {} change(s) from {} onto main", stack.len(), names.join(", "));
            let applied = self.update_view(Some(&ws), desc, None, shared, |view| {
                if view.bookmarks.get(MAIN).copied() != Some(main) {
                    return Ok(false);
                }
                view.bookmarks.insert(MAIN.into(), top);
                view.workspaces.insert(ws.clone(), nwh);
                self.record_landed(view, &stack, &released)?;
                Ok(true)
            })?;
            if applied.is_some() {
                self.refresh_index()?;
                return Ok(BatchLanded { result: LandResult::Landed { main: top, changes: stack, check: run, synced: false }, sources: from });
            }
        }
        bail!("main kept moving while landing (3 attempts); run `psync land` again")
    }
}
