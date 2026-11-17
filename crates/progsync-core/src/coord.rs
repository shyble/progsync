//! Coordination between agents: tasks and claims.
//!
//! Both are shared state in the view, changed only through operations, so
//! every change is in the operation log. Task and claim operations are not
//! tied to a workspace and are never reverted by `undo`.
//!
//! - **Tasks** have a title, an optional acceptance check, dependencies
//!   (`after`), an owner and a status. `land` records the changes that landed
//!   for a task; `task done` runs the acceptance check against main and
//!   records the evidence.
//! - **Claims** are soft locks with an expiry: a workspace announces the paths
//!   it is working on. Overlapping claims are refused (unless forced), and
//!   edits under someone else's claim are reported, not blocked.

use crate::diff;
use crate::hash::Hash;
use crate::object::{Check, Claim, EntryKind, Object, Task, TaskOwner, TaskStatus, View};
use crate::paths::ClaimSpec;
use crate::semantic;
use crate::repo::{CheckRun, MetaUpdate, Repo, MAIN};
use crate::util::now_ms;
use crate::bail;
use crate::error::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_CLAIM_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub hash: Hash,
    pub task: Task,
    /// Dependencies that are not done yet.
    pub blocked_by: Vec<String>,
}

impl TaskInfo {
    /// Open, not blocked, and nobody is working on it.
    pub fn is_ready(&self) -> bool {
        self.task.status == TaskStatus::Open && self.blocked_by.is_empty() && self.task.owner.is_none()
    }

    /// Display status: `blocked` is derived from dependencies.
    pub fn display_status(&self) -> &'static str {
        match self.task.status {
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
            _ if !self.blocked_by.is_empty() => "blocked",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Open => "open",
        }
    }
}

pub enum TaskDone {
    Done(Box<Task>),
    CheckFailed(CheckRun),
}

#[derive(Debug, Clone, Default)]
pub struct TaskCounts {
    pub open: usize,
    pub in_progress: usize,
    pub blocked: usize,
    pub done: usize,
    pub cancelled: usize,
}

/// Fields to change with [`Repo::task_edit`]; `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    /// `Some("")` removes the acceptance check.
    pub check: Option<String>,
    pub after: Option<Vec<String>>,
}

fn validate_task_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !id.starts_with(['.', '-']);
    if !ok {
        bail!("invalid task id '{id}': use letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

/// Natural ordering: T-2 before T-10.
fn natural_key(id: &str) -> (String, u64, String) {
    let split = id.find(|c: char| c.is_ascii_digit()).unwrap_or(id.len());
    let (prefix, rest) = id.split_at(split);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let tail = rest[digits.len()..].to_string();
    (prefix.to_string(), digits.parse().unwrap_or(0), tail)
}

fn task_map(repo: &Repo, view: &View) -> Result<BTreeMap<String, (Hash, Task)>> {
    view.tasks.iter().map(|(id, h)| Ok((id.clone(), (*h, repo.store.get_task(h)?)))).collect()
}

/// Whether `from` depends on `target`, directly or transitively.
fn depends_on(from: &str, target: &str, all: &BTreeMap<String, (Hash, Task)>) -> bool {
    let mut stack = vec![from.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(cur) = stack.pop() {
        if cur == target {
            return true;
        }
        if !seen.insert(cur.clone()) {
            continue;
        }
        if let Some((_, t)) = all.get(&cur) {
            stack.extend(t.after.iter().cloned());
        }
    }
    false
}

fn blockers(task: &Task, all: &BTreeMap<String, (Hash, Task)>) -> Vec<String> {
    task.after
        .iter()
        .filter(|d| all.get(*d).is_none_or(|(_, t)| !t.status.is_finished()))
        .cloned()
        .collect()
}

impl Repo {
    // ---- tasks -----------------------------------------------------------

    /// All tasks in natural id order.
    pub fn tasks(&self) -> Result<Vec<TaskInfo>> {
        let all = task_map(self, &self.view()?)?;
        let mut out: Vec<TaskInfo> = all
            .values()
            .map(|(h, t)| TaskInfo { hash: *h, task: t.clone(), blocked_by: blockers(t, &all) })
            .collect();
        out.sort_by_key(|t| natural_key(&t.task.id));
        Ok(out)
    }

    pub fn task(&self, id: &str) -> Result<TaskInfo> {
        self.tasks()?.into_iter().find(|t| t.task.id == id).with_context(|| format!("no task '{id}'"))
    }

    pub fn task_counts(&self) -> Result<TaskCounts> {
        let mut c = TaskCounts::default();
        for t in self.tasks()? {
            match t.display_status() {
                "done" => c.done += 1,
                "cancelled" => c.cancelled += 1,
                "blocked" => c.blocked += 1,
                "in_progress" => c.in_progress += 1,
                _ => c.open += 1,
            }
        }
        Ok(c)
    }

    /// Create a task. Without `id`, the next `T-<n>` is used.
    pub fn task_add(
        &self,
        title: &str,
        id: Option<&str>,
        description: &str,
        check: Option<String>,
        after: Vec<String>,
    ) -> Result<Task> {
        if title.trim().is_empty() {
            bail!("a task needs a title");
        }
        let mut created = None;
        let view = self.view()?;
        let id = match id {
            Some(i) => i.to_string(),
            None => {
                let n = view
                    .tasks
                    .keys()
                    .filter_map(|k| k.strip_prefix("T-").and_then(|n| n.parse::<u64>().ok()))
                    .max()
                    .unwrap_or(0);
                format!("T-{}", n + 1)
            }
        };
        validate_task_id(&id)?;
        let now = now_ms();
        let task = Task {
            id: id.clone(),
            title: title.trim().to_string(),
            description: description.trim().to_string(),
            check: check.filter(|c| !c.trim().is_empty()),
            after,
            status: TaskStatus::Open,
            owner: None,
            landed: vec![],
            completion: None,
            created_by: self.identity.clone(),
            created_ms: now,
            updated_ms: now,
            previous: None,
            merged_from: vec![],
        };
        self.update_view(None, format!("add task {id}: {}", task.title), None, vec![format!("task:{id}")], |view| {
            if view.tasks.contains_key(&id) {
                bail!("task '{id}' already exists");
            }
            for d in &task.after {
                if !view.tasks.contains_key(d) {
                    bail!("unknown dependency '{d}'");
                }
            }
            view.tasks.insert(id.clone(), self.store.put(&Object::Task(task.clone()))?);
            created = Some(task.clone());
            Ok(true)
        })?;
        Ok(created.expect("inserted"))
    }

    /// Apply `f` to the current version of a task, recording a new version.
    fn modify_task(&self, id: &str, description: String, f: impl FnOnce(&mut Task, &View) -> Result<()>) -> Result<Task> {
        let mut out = None;
        self.update_view(None, description, None, vec![format!("task:{id}")], |view| {
            let h = *view.tasks.get(id).with_context(|| format!("no task '{id}'"))?;
            let mut t = self.store.get_task(&h)?;
            f(&mut t, view)?;
            t.previous = Some(h);
            t.updated_ms = now_ms();
            view.tasks.insert(id.to_string(), self.store.put(&Object::Task(t.clone()))?);
            out = Some(t);
            Ok(true)
        })?;
        Ok(out.expect("modified"))
    }

    /// Take ownership of a task in this workspace and point the working
    /// change at it. Refuses blocked tasks and tasks owned by another live
    /// workspace unless `force`.
    pub fn task_start(&self, id: &str, force: bool) -> Result<Task> {
        let ws = self.workspace.clone();
        let me = self.identity.clone();
        let t = self.modify_task(id, format!("start task {id} in workspace {ws}"), |t, view| {
            if t.status.is_finished() {
                bail!("task {id} is already {}", if t.status == TaskStatus::Done { "done" } else { "cancelled" });
            }
            let all = task_map(self, view)?;
            let blocked = blockers(t, &all);
            if !blocked.is_empty() && !force {
                bail!("task {id} is blocked by {}: finish those first (or pass --force)", blocked.join(", "));
            }
            if let Some(o) = &t.owner {
                if o.workspace != ws && view.workspaces.contains_key(&o.workspace) && !force {
                    bail!(
                        "task {id} is being worked on by {} in workspace {} (pass --force to take it over)",
                        o.by.short(),
                        o.workspace
                    );
                }
            }
            t.owner = Some(TaskOwner { workspace: ws.clone(), by: me.clone(), since_ms: now_ms() });
            t.status = TaskStatus::InProgress;
            Ok(())
        })?;
        self.describe_unvetted(&MetaUpdate { task: Some(id.to_string()), ..Default::default() })?;
        Ok(t)
    }

    /// Give up ownership of a task (it becomes open again unless done).
    pub fn task_drop(&self, id: &str, force: bool) -> Result<Task> {
        let ws = self.workspace.clone();
        self.modify_task(id, format!("drop task {id}"), |t, _| {
            match &t.owner {
                Some(o) if o.workspace != ws && !force => {
                    bail!("task {id} is owned by workspace {} (pass --force to drop it anyway)", o.workspace)
                }
                None => bail!("task {id} has no owner"),
                _ => {}
            }
            t.owner = None;
            if t.status == TaskStatus::InProgress && t.landed.is_empty() {
                t.status = TaskStatus::Open;
            }
            Ok(())
        })
    }

    /// Complete a task. If it has an acceptance check, the check runs in this
    /// workspace, which must be exactly main, and its result is recorded.
    pub fn task_done(&self, id: &str) -> Result<TaskDone> {
        let info = self.task(id)?;
        if info.task.status.is_finished() {
            bail!("task {id} is already {}", info.display_status());
        }
        let evidence = match &info.task.check {
            Some(cmd) => {
                let main = self.main_tip()?.context("there is no main yet: land the task's work first")?;
                let (_, w) = self.working()?;
                if w.parents.first() != Some(&main) || !self.changes_of(&w)?.is_empty() {
                    bail!(
                        "the acceptance check runs against main: land your work and `psync sync` first, so this workspace is exactly main"
                    );
                }
                let tree = self.store.get_revision(&main)?.tree;
                let run = self.run_check(cmd, tree)?;
                if !run.passed {
                    return Ok(TaskDone::CheckFailed(run));
                }
                Some(Check {
                    command: run.command,
                    passed: true,
                    exit_code: run.exit_code,
                    duration_ms: run.duration_ms,
                    tree,
                    time_ms: now_ms(),
                    by: self.identity.clone(),
                })
            }
            None => None,
        };
        let t = self.modify_task(id, format!("complete task {id}"), |t, _| {
            if t.status.is_finished() {
                bail!("task {id} is already finished");
            }
            t.status = TaskStatus::Done;
            t.completion = evidence;
            Ok(())
        })?;
        self.release_where(format!("release claims of finished task {id}"), |c| c.task.as_deref() == Some(id))?;
        Ok(TaskDone::Done(Box::new(t)))
    }

    /// Change a task's title, description, acceptance check or dependencies.
    pub fn task_edit(&self, id: &str, edit: TaskEdit) -> Result<Task> {
        self.modify_task(id, format!("edit task {id}"), |t, view| {
            if let Some(title) = &edit.title {
                if title.trim().is_empty() {
                    bail!("a task needs a title");
                }
                t.title = title.trim().to_string();
            }
            if let Some(d) = &edit.description {
                t.description = d.trim().to_string();
            }
            if let Some(c) = &edit.check {
                t.check = Some(c.trim().to_string()).filter(|c| !c.is_empty());
            }
            if let Some(after) = &edit.after {
                let all = task_map(self, view)?;
                for d in after {
                    if !all.contains_key(d) {
                        bail!("unknown dependency '{d}'");
                    }
                    if depends_on(d, id, &all) {
                        bail!("'{d}' already depends on {id}: that would create a cycle");
                    }
                }
                t.after = after.clone();
            }
            Ok(())
        })
    }

    /// Mark a task as no longer needed. Dependents are unblocked and its
    /// claims are released.
    pub fn task_cancel(&self, id: &str) -> Result<Task> {
        let t = self.modify_task(id, format!("cancel task {id}"), |t, _| {
            if t.status.is_finished() {
                bail!("task {id} is already finished");
            }
            t.status = TaskStatus::Cancelled;
            t.owner = None;
            Ok(())
        })?;
        self.release_where(format!("release claims of cancelled task {id}"), |c| c.task.as_deref() == Some(id))?;
        Ok(t)
    }

    /// The task this workspace's working change belongs to, if any.
    pub fn current_task(&self) -> Result<Option<TaskInfo>> {
        let (_, w) = self.working()?;
        match w.meta.task {
            Some(id) => Ok(self.tasks()?.into_iter().find(|t| t.task.id == id)),
            None => Ok(None),
        }
    }

    // ---- claims ------------------------------------------------------------

    /// Claims that have not expired.
    pub fn active_claims(&self) -> Result<Vec<Claim>> {
        let now = now_ms();
        Ok(self.view()?.claims.into_iter().filter(|c| c.expires_ms > now).collect())
    }

    /// Claim paths for this workspace for `ttl_ms`. Refuses patterns that
    /// overlap another workspace's active claim unless `force`. Re-claiming a
    /// pattern refreshes its expiry.
    pub fn claim(&self, patterns: &[String], ttl_ms: i64, force: bool) -> Result<Vec<Claim>> {
        if patterns.is_empty() {
            bail!("nothing to claim");
        }
        let specs: Vec<ClaimSpec> = patterns.iter().map(|p| ClaimSpec::new(p)).collect::<Result<_>>()?;
        let (_, w) = self.working()?;
        for s in &specs {
            self.check_symbol(s, w.tree)?;
        }
        let mut files: BTreeSet<String> = diff::flatten(&self.store, w.tree)?.into_iter().map(|(p, _)| p).collect();
        if let Some(m) = self.main_tip()? {
            let tree = self.store.get_revision(&m)?.tree;
            files.extend(diff::flatten(&self.store, tree)?.into_iter().map(|(p, _)| p));
        }
        let ws = self.workspace.clone();
        let me = self.identity.clone();
        let task = w.meta.task.clone();
        let names: Vec<&str> = specs.iter().map(|s| s.raw.as_str()).collect();
        let mut created = Vec::new();
        self.update_view(None, format!("claim {} for workspace {ws}", names.join(", ")), None, vec!["claims".into()], |view| {
            let now = now_ms();
            view.claims.retain(|c| c.expires_ms > now);
            let mut clashes = Vec::new();
            for s in &specs {
                for c in view.claims.iter().filter(|c| !c.is_of(&ws)) {
                    let other = ClaimSpec::new(&c.pattern)?;
                    if s.overlaps(&other, files.iter().map(String::as_str)) {
                        clashes.push(format!("{} (claimed by {} in {} as '{}')", s.raw, c.by.short(), c.location(), c.pattern));
                    }
                }
            }
            if !clashes.is_empty() && !force {
                bail!("already claimed: {}. Coordinate first, or pass --force to claim anyway", clashes.join("; "));
            }
            for s in &specs {
                view.claims.retain(|c| !(c.is_of(&ws) && c.pattern == s.raw));
                let c = Claim {
                    pattern: s.raw.clone(),
                    workspace: ws.clone(),
                    by: me.clone(),
                    task: task.clone(),
                    created_ms: now,
                    expires_ms: now + ttl_ms,
                    home: None,
                };
                view.claims.push(c.clone());
                created.push(c);
            }
            touch_claims(view);
            Ok(true)
        })?;
        Ok(created)
    }

    /// Release this workspace's claims on `patterns` (all of them if empty).
    pub fn release(&self, patterns: &[String]) -> Result<usize> {
        let ws = self.workspace.clone();
        let wanted: Vec<String> = patterns.iter().map(|p| ClaimSpec::new(p).map(|s| s.raw)).collect::<Result<_>>()?;
        let desc = if wanted.is_empty() { format!("release all claims of {ws}") } else { format!("release {}", wanted.join(", ")) };
        self.release_where(desc, |c| c.workspace == ws && (wanted.is_empty() || wanted.contains(&c.pattern)))
    }

    /// Release this repository's claims matching `pred` (claims fetched from
    /// other repositories are theirs to release).
    pub(crate) fn release_where(&self, description: String, pred: impl Fn(&Claim) -> bool) -> Result<usize> {
        let mut removed = 0;
        let now = now_ms();
        let pred = |c: &Claim| c.home.is_none() && pred(c);
        if !self.view()?.claims.iter().any(|c| c.expires_ms > now && pred(c)) {
            return Ok(0);
        }
        self.update_view(None, description, None, vec!["claims".into()], |view| {
            let before = view.claims.len();
            view.claims.retain(|c| c.expires_ms > now && !pred(c));
            removed = before - view.claims.len();
            touch_claims(view);
            Ok(true)
        })?;
        Ok(removed)
    }

    /// Paths this workspace changed since it diverged from main (or in its
    /// working change, if there is no main).
    /// A symbol claim must name a declaration that exists, when the file
    /// exists (in the working change or on main) and can be analyzed.
    fn check_symbol(&self, s: &ClaimSpec, working_tree: Hash) -> Result<()> {
        let Some(sym) = &s.symbol else { return Ok(()) };
        let path = &s.path.raw;
        if !semantic::supports(path) {
            bail!("symbol claims need a Rust, Python, JavaScript, TypeScript or Go file; claim '{path}' instead");
        }
        let mut trees = vec![working_tree];
        if let Some(m) = self.main_tip()? {
            trees.push(self.store.get_revision(&m)?.tree);
        }
        let mut known = Vec::new();
        for t in trees {
            let Some(text) = self.file_text(t, path)? else { continue };
            let Some(decls) = semantic::declarations(path, &text) else { return Ok(()) };
            if decls.iter().any(|d| semantic::names(d, sym)) {
                return Ok(());
            }
            known.extend(decls.into_iter().map(|d| d.name));
        }
        if known.is_empty() {
            return Ok(()); // a file not written yet
        }
        known.sort();
        known.dedup();
        known.truncate(30);
        bail!("{path} has no top-level declaration named '{sym}' (it has: {})", known.join(", "))
    }

    fn file_text(&self, tree: Hash, path: &str) -> Result<Option<String>> {
        Ok(match diff::lookup(&self.store, tree, path)? {
            Some((EntryKind::File | EntryKind::Exec, h)) => self.store.get_small_file(&h)?.and_then(|d| String::from_utf8(d).ok()),
            _ => None,
        })
    }

    /// The tree your unlanded work started from (merge base with main).
    fn my_base_tree(&self) -> Result<(Hash, Hash)> {
        let (wh, w) = self.working()?;
        let base = match self.view()?.bookmarks.get(MAIN) {
            Some(m) => match self.merge_base(wh, *m)? {
                Some(b) => self.store.get_revision(&b)?.tree,
                None => self.parent_tree(&w)?,
            },
            None => self.parent_tree(&w)?,
        };
        Ok((base, w.tree))
    }

    pub fn my_changed_paths(&self) -> Result<Vec<String>> {
        let (base, mine) = self.my_base_tree()?;
        Ok(diff::diff_trees(&self.store, Some(base), Some(mine))?.into_iter().map(|c| c.path).collect())
    }

    /// Of `paths`, those under another workspace's active claim.
    pub fn claimed_by_others(&self, paths: &[String]) -> Result<Vec<(String, Claim)>> {
        let mut out = Vec::new();
        let mut trees = None;
        for c in self.active_claims()?.into_iter().filter(|c| !c.is_of(&self.workspace)) {
            let spec = ClaimSpec::new(&c.pattern)?;
            for p in paths.iter().filter(|p| spec.path.matches(p)) {
                if let Some(sym) = &spec.symbol {
                    // Only a warning if your edits change that declaration
                    // (or the file cannot be analyzed).
                    let (base, mine) = match trees {
                        Some(t) => t,
                        None => *trees.insert(self.my_base_tree()?),
                    };
                    let (old, new) = (self.file_text(base, p)?, self.file_text(mine, p)?);
                    if let Some(changed) = semantic::changed_declarations(p, old.as_deref(), new.as_deref()) {
                        if !changed.iter().any(|d| semantic::names(d, sym)) {
                            continue;
                        }
                    }
                }
                out.push((p.clone(), c.clone()));
            }
        }
        Ok(out)
    }
}

/// Record that this repository's claims changed (see `View::claim_stamps`).
pub(crate) fn touch_claims(view: &mut View) {
    let s = view.claim_stamps.entry(String::new()).or_default();
    *s = now_ms().max(*s + 1);
}

/// Parse durations like `90s`, `30m`, `2h`, `1d`, `2w`.
pub fn parse_ttl(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().with_context(|| format!("invalid duration '{s}' (examples: 90s, 30m, 2h, 1d)"))?;
    let ms = match unit {
        "s" => 1000,
        "m" | "" => 60 * 1000,
        "h" => 3600 * 1000,
        "d" => 86_400 * 1000,
        "w" => 7 * 86_400 * 1000,
        _ => bail!("invalid duration unit in '{s}' (use s, m, h, d or w)"),
    };
    Ok(n * ms)
}
