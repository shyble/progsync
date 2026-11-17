//! The repository: workspaces, changes, and the operation log.
//!
//! Model (borrowed in spirit from jj):
//! - Each workspace (a directory) has a *working change* (`@`). There is no
//!   staging area; every command first snapshots the directory into `@`.
//! - A change has a stable [`ChangeId`]; each amend creates a new immutable
//!   [`Revision`] of it.
//! - Every state change is an [`Operation`] appended to the operation log.
//!
//! Library callers: methods operate on the last snapshot. Call
//! [`Repo::snapshot`] first to pick up edits on disk (the CLI does this before
//! every command).
//!
//! Multi-agent rules:
//! - Only workspace `W` changes `view.workspaces[W]`; operations record which
//!   workspace they changed. `undo` and `op restore` are scoped to the
//!   current workspace, so one agent can never roll back another's work.
//! - Each workspace has its own lock and stat cache; the repository lock is
//!   held only for the instant an operation is appended. Agents in different
//!   workspaces scan and hash in parallel.

use crate::config;
use crate::diff::{self, FileChange};
use crate::hash::{ChangeId, Hash};
use crate::merge::{self, Labels};
use crate::object::{ChangeMeta, Check, Conflict, EntryKind, Identity, Object, Operation, Revision, Tree, View};
use crate::paths::PathSpec;
use crate::store::Store;
use crate::util::{atomic_write, now_ms};
use crate::wc::{self, CloneSource, Index};
use crate::watch;
use crate::bail;
use crate::error::{Context as _, Result};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const DOT_DIR: &str = ".psync";
pub const DEFAULT_WORKSPACE: &str = "default";
/// The shared line that `land` advances and `sync` rebases onto.
pub const MAIN: &str = "main";
pub(crate) const REPO_FORMAT: &str = "2";

/// Changes to apply to a change's metadata. Empty strings clear a field.
#[derive(Debug, Clone, Default)]
pub struct MetaUpdate {
    pub intent: Option<String>,
    pub task: Option<String>,
    pub extra: Vec<(String, String)>,
}

impl MetaUpdate {
    pub fn is_empty(&self) -> bool {
        self.intent.is_none() && self.task.is_none() && self.extra.is_empty()
    }

    pub fn apply(&self, m: &mut ChangeMeta) {
        if let Some(i) = &self.intent {
            m.intent = i.trim().to_string();
        }
        if let Some(t) = &self.task {
            m.task = if t.is_empty() { None } else { Some(t.clone()) };
        }
        for (k, v) in &self.extra {
            if v.is_empty() {
                m.extra.remove(k);
            } else {
                m.extra.insert(k.clone(), v.clone());
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotOutcome {
    /// Whether the working change was amended.
    pub changed: bool,
    pub files: usize,
    pub hashed: usize,
    pub cached: usize,
    /// Only paths reported by the file watcher were examined.
    pub watched: bool,
}

pub struct Status {
    pub working: (Hash, Revision),
    pub parent: Option<(Hash, Revision)>,
    pub changes: Vec<FileChange>,
}

pub struct UndoOutcome {
    pub op: Hash,
    pub undone: Hash,
    pub undone_description: String,
}

/// Contents of the `.psync` *file* in a secondary workspace directory.
struct WorkspaceLink {
    repo: PathBuf,
    workspace: String,
}
crate::json_struct!(WorkspaceLink { repo, workspace });

/// Local (per-machine) record of a workspace: `.psync/workspaces/<name>/meta.json`.
#[derive(Clone, Debug)]
pub struct WorkspaceMeta {
    pub path: PathBuf,
    pub created_ms: i64,
    pub created_by: Identity,
}
crate::json_struct!(WorkspaceMeta { path, created_ms, created_by });

pub struct WorkspaceInfo {
    pub name: String,
    pub path: Option<PathBuf>,
    pub rev: Hash,
    pub revision: Revision,
}

pub struct AddedWorkspace {
    pub info: WorkspaceInfo,
    pub files: usize,
    pub cloned: usize,
}

/// Another workspace, as seen from the current one.
pub struct PeerSummary {
    pub info: WorkspaceInfo,
    /// Files the peer changed since its history diverged from ours.
    pub files: Vec<String>,
    /// Of those, files we have changed too since the divergence.
    pub overlap: Vec<String>,
    /// For another repository's workspace (named `workspace@host`): where
    /// that repository is.
    pub place: Option<String>,
}

/// A compact briefing for an agent: where it is and what others are doing.
pub struct Briefing {
    pub workspace: String,
    pub root: PathBuf,
    pub identity: Identity,
    pub working: (Hash, Revision),
    pub files: Vec<String>,
    /// The main tip, and how many committed changes we have that main lacks
    /// (ahead) and main has that we lack (behind).
    pub main: Option<(Hash, Revision)>,
    pub ahead: usize,
    pub behind: usize,
    pub recent: Vec<(Hash, Revision)>,
    pub peers: Vec<PeerSummary>,
}

pub struct SyncOutcome {
    pub main: Hash,
    pub up_to_date: bool,
    /// Committed changes rebased cleanly onto main (new revisions).
    pub rebased: Vec<(Hash, Revision)>,
    /// Committed changes that conflicted with main and were folded into the
    /// working change, so their conflicts can be resolved in one place.
    pub folded: Vec<Revision>,
    /// Unresolved conflicts now in the working change.
    pub conflicts: Vec<Conflict>,
}

#[derive(Debug, Clone)]
pub struct CheckRun {
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Last lines of combined stdout/stderr.
    pub output_tail: String,
}

pub enum LandResult {
    Landed { main: Hash, changes: Vec<(Hash, Revision)>, check: Option<CheckRun>, synced: bool },
    /// Syncing onto main produced conflicts; nothing was landed.
    Conflicts(SyncOutcome),
    /// The check failed; nothing was landed (the workspace is synced).
    CheckFailed { check: CheckRun, changes: Vec<(Hash, Revision)> },
}

/// What [`Repo::identify`] reports.
pub struct Identification {
    /// The last commit: the working change's first parent.
    pub revision: Hash,
    pub commit: Revision,
    pub working: ChangeId,
    /// Files that differ from the last commit (within the paths asked about).
    pub changed: Vec<String>,
    /// Whether the last commit is on main (`None`: there is no main).
    pub on_main: Option<bool>,
}

/// One path written (or removed) by [`Repo::restore`].
pub struct RestoredFile {
    pub path: String,
    /// `None`: the path does not exist in the source, so it was removed.
    pub leaf: Option<crate::object::Leaf>,
    pub size: Option<u64>,
}

pub struct RestoreOutcome {
    pub source: Hash,
    pub files: Vec<RestoredFile>,
}

pub struct RevertOutcome {
    pub reverted: Hash,
    pub files: usize,
    pub conflicts: Vec<Conflict>,
}

pub struct MergeOutcome {
    pub merged: Hash,
    pub base: Option<Hash>,
    pub files: usize,
    pub conflicts: Vec<Conflict>,
}

pub struct Repo {
    /// Root of the current workspace.
    pub root: PathBuf,
    /// The repository's `.psync` directory (shared by all workspaces).
    pub(crate) dot: PathBuf,
    pub(crate) workspace: String,
    pub store: Store,
    pub(crate) identity: Identity,
    /// Exclusive lock on this workspace for the lifetime of the handle.
    /// `None` for a handle on another repository (a remote).
    _ws_lock: Option<File>,
}

fn validate_workspace_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !name.starts_with(['.', '-'])
        && !name.ends_with('-');
    if !ok {
        bail!("invalid workspace name '{name}': use letters, digits, '.', '_' or '-' (not leading, and not a trailing '-')");
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().create(true).truncate(false).write(true).open(path)?)
}

impl Repo {
    // ---- opening ---------------------------------------------------------

    /// Create a repository at `root`, recording its current contents as the
    /// first change of the default workspace.
    pub fn init(root: &Path, identity: Identity) -> Result<Repo> {
        let root = crate::platform::canonicalize(root).with_context(|| format!("no such directory: {}", root.display()))?;
        let dot = root.join(DOT_DIR);
        if dot.exists() {
            bail!("already a psync repository: {}", root.display());
        }
        let ws_dir = dot.join("workspaces").join(DEFAULT_WORKSPACE);
        fs::create_dir_all(&ws_dir)?;
        fs::write(dot.join("format"), format!("{REPO_FORMAT}\n"))?;
        let store = Store::open(&dot);
        store.create_dirs()?;
        let now = now_ms();
        let meta = WorkspaceMeta { path: root.clone(), created_ms: now, created_by: identity.clone() };
        atomic_write(&ws_dir.join("meta.json"), &crate::json::to_vec_pretty(&meta)?)?;

        let repo = Repo::open(dot, DEFAULT_WORKSPACE.into(), root, identity)?;
        let scan = wc::scan(&repo.root, &repo.store, &Index::default(), repo.eol()?)?;
        let rev = Revision {
            change_id: ChangeId::random(),
            tree: scan.tree,
            parents: vec![],
            meta: ChangeMeta::default(),
            author: repo.identity.clone(),
            committer: repo.identity.clone(),
            conflicts: vec![],
            generation: 1,
            created_ms: now,
            updated_ms: now,
        };
        let rev_hash = repo.store.put(&Object::Revision(rev))?;
        let mut view = View::default();
        view.workspaces.insert(DEFAULT_WORKSPACE.into(), rev_hash);
        let op = Operation {
            parent: None,
            view,
            time_ms: now,
            actor: repo.identity.clone(),
            workspace: Some(DEFAULT_WORKSPACE.into()),
            description: "init repository".into(),
            undo_of: None,
            shared: vec![],
        };
        let op_hash = repo.store.put(&Object::Operation(op))?;
        repo.set_op_head(op_hash)?;
        scan.save_index(&repo.index_path())?;
        Ok(repo)
    }

    /// Find the workspace containing `start`: returns (repo dot dir, workspace, workspace root).
    /// Takes no lock.
    pub fn locate(start: &Path) -> Result<(PathBuf, String, PathBuf)> {
        let start = crate::platform::canonicalize(start).with_context(|| format!("no such directory: {}", start.display()))?;
        for dir in start.ancestors() {
            let p = dir.join(DOT_DIR);
            if p.is_dir() {
                return Ok((p, DEFAULT_WORKSPACE.into(), dir.to_path_buf()));
            }
            if p.is_file() {
                let link: WorkspaceLink = crate::json::from_slice(&fs::read(&p)?)
                    .with_context(|| format!("corrupt workspace link {}", p.display()))?;
                return Ok((link.repo, link.workspace, dir.to_path_buf()));
            }
        }
        bail!("not a psync repository (or any parent): {}", start.display())
    }

    /// Open the workspace containing `start`; the owner identity comes from config.
    pub fn discover(start: &Path, agent: Option<String>) -> Result<Repo> {
        let (dot, ws, root) = Repo::locate(start)?;
        let identity = config::identity(Some(&dot), agent);
        Repo::open(dot, ws, root, identity)
    }

    /// Snapshot the workspace containing `start` unless a command is running
    /// in it (then `Ok(None)`). Used by the file watcher.
    pub fn snapshot_if_idle(start: &Path, identity: Identity) -> Result<Option<SnapshotOutcome>> {
        let (dot, ws, root) = Repo::locate(start)?;
        let Some(repo) = Repo::open_with(dot, ws, root, identity, false)? else { return Ok(None) };
        let ws_dir = repo.ws_dir(&repo.workspace);
        snapshot_workspace(&repo, &repo.workspace, &repo.root, &ws_dir, " (watcher)").map(Some)
    }

    /// Open the workspace containing `start` acting as `identity`.
    pub fn discover_as(start: &Path, identity: Identity) -> Result<Repo> {
        let (dot, ws, root) = Repo::locate(start)?;
        Repo::open(dot, ws, root, identity)
    }

    fn open(dot: PathBuf, workspace: String, root: PathBuf, identity: Identity) -> Result<Repo> {
        Ok(Repo::open_with(dot, workspace, root, identity, true)?.expect("waited for the lock"))
    }

    /// A handle on the repository containing `start` that takes no
    /// workspace lock, for maintenance that never touches a working copy
    /// (background packing), so it never makes a command wait.
    pub fn open_unlocked(start: &Path) -> Result<Repo> {
        let (dot, workspace, root) = Repo::locate(start)?;
        Repo::open_handle(dot, workspace, root, Identity::default())
    }

    /// A handle without a workspace lock (for a remote repository).
    pub(crate) fn open_handle(dot: PathBuf, workspace: String, root: PathBuf, identity: Identity) -> Result<Repo> {
        let format = fs::read_to_string(dot.join("format")).unwrap_or_default();
        if format.trim() != REPO_FORMAT {
            bail!("repository format {:?} in {} is not supported by this psync", format.trim(), dot.display());
        }
        Ok(Repo { store: Store::open(&dot), root, dot, workspace, identity, _ws_lock: None })
    }

    /// Open a workspace, waiting for its lock if `wait`, else returning
    /// `None` when another command holds it.
    fn open_with(dot: PathBuf, workspace: String, root: PathBuf, identity: Identity, wait: bool) -> Result<Option<Repo>> {
        let format = fs::read_to_string(dot.join("format")).unwrap_or_default();
        if format.trim() != REPO_FORMAT {
            bail!(
                "repository format {:?} in {} is not supported by this psync (expects {REPO_FORMAT}); re-create it with `psync init`",
                format.trim(),
                dot.display()
            );
        }
        let ws_dir = dot.join("workspaces").join(&workspace);
        if !ws_dir.is_dir() {
            bail!("workspace '{workspace}' no longer exists in {}", dot.display());
        }
        let lock = open_lock(&ws_dir.join("lock"))?;
        if wait {
            lock.lock().context("locking workspace")?;
        } else {
            match lock.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        Ok(Some(Repo { store: Store::open(&dot), root, dot, workspace, identity, _ws_lock: Some(lock) }))
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn dot(&self) -> &Path {
        &self.dot
    }

    fn ws_dir(&self, ws: &str) -> PathBuf {
        self.dot.join("workspaces").join(ws)
    }

    /// This workspace's metadata directory (`.psync/workspaces/<name>`).
    pub fn workspace_dir(&self) -> PathBuf {
        self.ws_dir(&self.workspace)
    }

    /// Scan this workspace (through its file watcher, if one is running).
    fn scan_own(&self) -> Result<wc::ScanResult> {
        watch::scan(&self.root, &self.store, &self.ws_dir(&self.workspace))
    }

    fn index_path(&self) -> PathBuf {
        self.ws_dir(&self.workspace).join("index")
    }

    // ---- operation log -------------------------------------------------

    pub fn op_head(&self) -> Result<Hash> {
        let s = fs::read_to_string(self.dot.join("op_head")).context("reading op_head")?;
        Hash::from_hex(s.trim())
    }

    /// Move `op_head`, first making every object written so far durable.
    fn set_op_head(&self, h: Hash) -> Result<()> {
        self.store.sync()?;
        atomic_write(&self.dot.join("op_head"), format!("{h}\n").as_bytes())
    }

    /// The oldest operation still kept, if `gc --keep` forgot older ones:
    /// its parent is treated as absent.
    pub fn op_horizon(&self) -> Result<Option<Hash>> {
        match fs::read_to_string(self.dot.join("op_horizon")) {
            Ok(s) => Ok(Some(Hash::from_hex(s.trim()).context("reading op_horizon")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading op_horizon"),
        }
    }

    pub(crate) fn set_op_horizon(&self, h: Hash) -> Result<()> {
        atomic_write(&self.dot.join("op_horizon"), format!("{h}\n").as_bytes())
    }

    /// The parent of operation `h` within the kept history.
    fn op_parent(op: &Operation, h: Hash, horizon: Option<Hash>) -> Option<Hash> {
        if horizon == Some(h) {
            None
        } else {
            op.parent
        }
    }

    pub fn current_op(&self) -> Result<(Hash, Operation)> {
        let h = self.op_head()?;
        Ok((h, self.store.get_operation(&h)?))
    }

    pub fn view(&self) -> Result<View> {
        Ok(self.current_op()?.1.view)
    }

    /// Atomically set (or remove) one workspace's entry in the view, appending
    /// an operation. Holds the repository lock only for this step.
    pub(crate) fn set_entry(&self, ws: &str, rev: Option<Hash>, description: String, undo_of: Option<Hash>) -> Result<Hash> {
        let h = self.update_view(Some(ws), description, undo_of, vec![], |view| {
            match rev {
                Some(r) => view.workspaces.insert(ws.to_string(), r),
                None => {
                    view.parked.remove(ws);
                    view.workspaces.remove(ws)
                }
            };
            Ok(true)
        })?;
        Ok(h.expect("update always applies"))
    }

    /// Read-modify-write of the view under the repository lock. `f` returns
    /// false to abort (e.g. a compare-and-swap that lost a race).
    pub(crate) fn update_view(
        &self,
        ws: Option<&str>,
        description: String,
        undo_of: Option<Hash>,
        shared: Vec<String>,
        f: impl FnOnce(&mut View) -> Result<bool>,
    ) -> Result<Option<Hash>> {
        let lock = open_lock(&self.dot.join("lock"))?;
        lock.lock().context("locking repository")?;
        let (parent, op) = self.current_op()?;
        let mut view = op.view;
        let workspaces = view.workspaces.clone();
        if !f(&mut view)? {
            return Ok(None);
        }
        if view.workspaces != workspaces {
            // Other repositories learn of the change (see `remote`).
            crate::coord::touch_claims(&mut view);
        }
        let op = Operation {
            parent: Some(parent),
            view,
            time_ms: now_ms(),
            actor: self.identity.clone(),
            workspace: ws.map(String::from),
            description,
            undo_of,
            shared,
        };
        let h = self.store.put(&Object::Operation(op))?;
        self.set_op_head(h)?;
        Ok(Some(h))
    }

    pub fn op_log(&self, limit: usize) -> Result<Vec<(Hash, Operation)>> {
        let mut out = Vec::new();
        let horizon = self.op_horizon()?;
        let mut next = Some(self.op_head()?);
        while let Some(h) = next {
            if out.len() >= limit {
                break;
            }
            let op = self.store.get_operation(&h)?;
            next = Repo::op_parent(&op, h, horizon);
            out.push((h, op));
        }
        Ok(out)
    }

    pub fn resolve_op(&self, prefix: &str) -> Result<Hash> {
        let matches: Vec<Hash> = self
            .op_log(usize::MAX)?
            .into_iter()
            .map(|(h, _)| h)
            .filter(|h| h.to_hex().starts_with(prefix))
            .collect();
        match matches.len() {
            0 => bail!("no operation matches '{prefix}'"),
            1 => Ok(matches[0]),
            n => bail!("operation '{prefix}' is ambiguous ({n} matches); use a longer prefix"),
        }
    }

    /// Undo the most recent operation of *this workspace*. Operations of other
    /// workspaces are untouched. Repeated undos walk further back.
    pub fn undo(&self) -> Result<UndoOutcome> {
        let me = Some(self.workspace.as_str());
        let horizon = self.op_horizon()?;
        let horizon_ms = match horizon {
            Some(h) => Some(self.store.get_operation(&h)?.time_ms),
            None => None,
        };
        let mut cursor = Some(self.op_head()?);
        let target = loop {
            let h = cursor.context("nothing to undo in this workspace")?;
            let op = self.store.get_operation(&h)?;
            if op.workspace.as_deref() != me {
                cursor = Repo::op_parent(&op, h, horizon);
            } else if let Some(undone) = op.undo_of {
                // The undone operation may be older than the kept history.
                cursor = match self.store.get_operation(&undone) {
                    Ok(u) if horizon_ms.is_none_or(|t| u.time_ms >= t) => Repo::op_parent(&u, undone, horizon),
                    _ => None,
                };
            } else {
                break (h, op);
            }
        };
        let (target_hash, target_op) = target;
        if !target_op.shared.is_empty() {
            bail!(
                "the last operation here changed shared {} ({}); it cannot be undone because other agents may build on it",
                target_op.shared.join(", "),
                target_op.description
            );
        }
        let before = match Repo::op_parent(&target_op, target_hash, horizon) {
            Some(p) => self.store.get_operation(&p)?.view.workspaces.get(&self.workspace).copied(),
            None => None,
        };
        let before = before.context("nothing to undo in this workspace")?;
        let op = self.move_to(before, format!("undo operation {}", target_hash.short()), Some(target_hash))?;
        Ok(UndoOutcome { op, undone: target_hash, undone_description: target_op.description })
    }

    /// Restore this workspace to its state at operation `op`.
    pub fn op_restore(&self, op: Hash) -> Result<Hash> {
        let view = self.store.get_operation(&op)?.view;
        let rev = *view
            .workspaces
            .get(&self.workspace)
            .with_context(|| format!("workspace '{}' did not exist at operation {}", self.workspace, op.short()))?;
        self.move_to(rev, format!("restore to operation {}", op.short()), None)
    }

    /// Point this workspace at `rev`, updating the files on disk.
    fn move_to(&self, rev: Hash, description: String, undo_of: Option<Hash>) -> Result<Hash> {
        let (_, current) = self.working()?;
        let target = self.store.get_revision(&rev)?;
        wc::checkout(&self.root, &self.store, current.tree, target.tree, self.eol()?)?;
        let h = self.set_entry(&self.workspace.clone(), Some(rev), description, undo_of)?;
        self.refresh_index()?;
        Ok(h)
    }

    // ---- working change ------------------------------------------------

    pub fn working(&self) -> Result<(Hash, Revision)> {
        self.working_of(&self.workspace)
    }

    pub fn working_of(&self, ws: &str) -> Result<(Hash, Revision)> {
        let view = self.view()?;
        let h = *view.workspaces.get(ws).with_context(|| format!("no workspace named '{ws}'"))?;
        Ok((h, self.store.get_revision(&h)?))
    }

    /// Record the working directory into the working change.
    /// The `text.eol` setting (see [`crate::eol`]).
    pub(crate) fn eol(&self) -> Result<Option<crate::eol::Ending>> {
        crate::eol::configured(&self.dot)
    }

    pub fn snapshot(&self) -> Result<SnapshotOutcome> {
        snapshot_workspace(self, &self.workspace, &self.root, &self.ws_dir(&self.workspace), "")
    }

    /// An empty change (no intent, no file changes) created by someone else —
    /// e.g. by `workspace add` — is credited to whoever first puts work in it.
    /// A snapshot by a command run without an agent cannot tell who made the
    /// edits (it may be the owner checking on an agent's work), so it never
    /// takes a change from an agent acting for the same owner.
    fn claim_if_unclaimed(&self, rev: &mut Revision, snapshot: bool) -> Result<()> {
        if rev.author == self.identity || !rev.meta.intent.is_empty() {
            return Ok(());
        }
        if snapshot && self.identity.agent.is_none() && rev.author.agent.is_some() && same_owner(&rev.author, &self.identity) {
            return Ok(());
        }
        let parent_tree = match rev.parents.first() {
            Some(p) => self.store.get_revision(p)?.tree,
            None => self.empty_tree()?,
        };
        if parent_tree == rev.tree {
            rev.author = self.identity.clone();
        }
        Ok(())
    }

    /// Update the metadata of the working change (vetted by the message
    /// check, see [`crate::msgcheck`]).
    pub fn describe(&self, update: &MetaUpdate) -> Result<(Hash, Revision)> {
        self.describe_with(update, true)
    }

    /// [`Repo::describe`] without the message check (for psync's own
    /// updates, such as `task start` setting the task).
    pub(crate) fn describe_unvetted(&self, update: &MetaUpdate) -> Result<(Hash, Revision)> {
        self.describe_with(update, false)
    }

    /// Update the metadata of `spec`: the working change, or one of this
    /// workspace's committed changes not yet on main. The changes after it
    /// are re-recorded on top (their trees are unchanged).
    pub fn describe_rev(&self, spec: &str, update: &MetaUpdate) -> Result<(Hash, Revision)> {
        let target = self.resolve(spec)?;
        let (wh, w) = self.working()?;
        if target == wh {
            return self.describe(update);
        }
        let stack = match w.parents.first() {
            Some(tip) => self.unlanded(*tip, self.main_tip()?)?,
            None => vec![],
        };
        let Some(i) = stack.iter().position(|(h, _)| *h == target) else {
            bail!("'{spec}' is not one of your changes waiting to land: describe can change @ or your committed changes not yet on main");
        };
        let mut rev = stack[i].1.clone();
        update.apply(&mut rev.meta);
        if rev.meta.intent.is_empty() {
            bail!("a committed change needs an intent");
        }
        rev.committer = self.identity.clone();
        rev.updated_ms = now_ms();
        self.vet(crate::msgcheck::Stage::Describe, None, &rev, None)?;
        let h = self.store.put(&Object::Revision(rev.clone()))?;
        let mut parent = h;
        for (_, later) in &stack[i + 1..] {
            let mut later = later.clone();
            later.parents[0] = parent;
            parent = self.store.put(&Object::Revision(later))?;
        }
        let mut nw = w;
        nw.parents[0] = parent;
        let nwh = self.store.put(&Object::Revision(nw))?;
        self.set_entry(&self.workspace, Some(nwh), format!("describe change {}", rev.change_id.short()), None)?;
        Ok((h, rev))
    }

    fn describe_with(&self, update: &MetaUpdate, vet: bool) -> Result<(Hash, Revision)> {
        let (_, mut rev) = self.working()?;
        self.claim_if_unclaimed(&mut rev, false)?;
        update.apply(&mut rev.meta);
        rev.committer = self.identity.clone();
        rev.updated_ms = now_ms();
        if vet {
            self.vet(crate::msgcheck::Stage::Describe, None, &rev, None)?;
        }
        let h = self.store.put(&Object::Revision(rev.clone()))?;
        self.set_entry(&self.workspace, Some(h), format!("describe change {}", rev.change_id.short()), None)?;
        Ok((h, rev))
    }

    /// Finalize the working change with `update` applied, and start a new,
    /// empty working change on top of it. Returns (finished, new working).
    pub fn commit(&self, update: &MetaUpdate) -> Result<((Hash, Revision), (Hash, Revision))> {
        self.commit_paths(update, &[])
    }

    /// Like [`Repo::commit`], but only changes under `paths` (paths,
    /// directories or globs) go into the committed change; the rest stay in
    /// the new working change. Empty `paths` commits everything.
    pub fn commit_paths(&self, update: &MetaUpdate, paths: &[String]) -> Result<((Hash, Revision), (Hash, Revision))> {
        self.commit_with(update, paths, false)
    }

    /// Like [`Repo::commit_paths`]; `allow_large` skips the
    /// `commit.max_file_mb` guard.
    pub fn commit_with(
        &self,
        update: &MetaUpdate,
        paths: &[String],
        allow_large: bool,
    ) -> Result<((Hash, Revision), (Hash, Revision))> {
        let (_, mut done) = self.working()?;
        let full_tree = done.tree;
        let mut remaining_conflicts = Vec::new();
        if !paths.is_empty() {
            let specs: Vec<PathSpec> = paths.iter().map(|p| PathSpec::new(p)).collect::<Result<_>>()?;
            let selected: Vec<FileChange> =
                self.changes_of(&done)?.into_iter().filter(|c| specs.iter().any(|s| s.matches(&c.path))).collect();
            for s in &specs {
                if !selected.iter().any(|c| s.matches(&c.path)) {
                    bail!("'{}' matches no changed file", s.raw);
                }
            }
            done.tree = wc::apply_changes(&self.store, self.parent_tree(&done)?, &selected)?;
            let (in_commit, rest): (Vec<_>, Vec<_>) =
                done.conflicts.drain(..).partition(|c| specs.iter().any(|s| s.matches(&c.path)));
            done.conflicts = in_commit;
            remaining_conflicts = rest;
        }
        if !allow_large {
            self.check_file_sizes(&done)?;
        }
        self.claim_if_unclaimed(&mut done, false)?;
        // A change nobody has named yet belongs to whoever commits it, among
        // agents of the same owner: snapshots cannot tell who made the edits.
        if done.meta.intent.is_empty() && same_owner(&done.author, &self.identity) {
            done.author = self.identity.clone();
        }
        update.apply(&mut done.meta);
        if done.meta.intent.is_empty() {
            bail!("commit needs an intent: pass -m \"<what this change does>\"");
        }
        if !done.conflicts.is_empty() {
            let paths: Vec<&str> = done.conflicts.iter().map(|c| c.path.as_str()).collect();
            bail!(
                "{} unresolved conflict(s): {}. Edit the files to remove the conflict markers (or run `psync resolve <path>`), then commit",
                paths.len(),
                paths.join(", ")
            );
        }
        let now = now_ms();
        done.committer = self.identity.clone();
        done.updated_ms = now;
        self.vet(crate::msgcheck::Stage::Commit, None, &done, None)?;
        let done_hash = self.store.put(&Object::Revision(done.clone()))?;
        let mut next = self.new_revision(full_tree, done_hash, ChangeMeta { task: done.meta.task.clone(), ..Default::default() })?;
        next.conflicts = remaining_conflicts;
        let next_hash = self.store.put(&Object::Revision(next.clone()))?;
        let first_line = done.meta.intent.lines().next().unwrap_or_default().to_string();
        self.set_entry(
            &self.workspace,
            Some(next_hash),
            format!("commit change {}: {first_line}", done.change_id.short()),
            None,
        )?;
        Ok(((done_hash, done), (next_hash, next)))
    }

    pub(crate) fn new_revision(&self, tree: Hash, parent: Hash, meta: ChangeMeta) -> Result<Revision> {
        let now = now_ms();
        Ok(Revision {
            change_id: ChangeId::random(),
            tree,
            parents: vec![parent],
            meta,
            author: self.identity.clone(),
            committer: self.identity.clone(),
            conflicts: vec![],
            generation: self.generation_for(&[parent])?,
            created_ms: now,
            updated_ms: now,
        })
    }

    fn generation_of(&self, h: &Hash) -> Result<u64> {
        Ok(self.store.get_revision(h)?.generation)
    }

    /// Generation for a revision with these parents.
    pub(crate) fn generation_for(&self, parents: &[Hash]) -> Result<u64> {
        let mut g = 0;
        for p in parents {
            g = g.max(self.generation_of(p)?);
        }
        Ok(g + 1)
    }

    pub fn status(&self) -> Result<Status> {
        let working = self.working()?;
        let parent = match working.1.parents.first() {
            Some(p) => Some((*p, self.store.get_revision(p)?)),
            None => None,
        };
        let changes = self.changes_of(&working.1)?;
        Ok(Status { working, parent, changes })
    }

    /// Files changed by a revision relative to its first parent.
    pub fn changes_of(&self, rev: &Revision) -> Result<Vec<FileChange>> {
        let base = match rev.parents.first() {
            Some(p) => self.store.get_revision(p)?.tree,
            None => self.empty_tree()?,
        };
        diff::diff_trees(&self.store, Some(base), Some(rev.tree))
    }

    fn empty_tree(&self) -> Result<Hash> {
        self.store.put(&Object::Tree(Tree::default()))
    }

    // ---- history -------------------------------------------------------

    /// First-parent history starting at `start`.
    pub fn log(&self, start: Hash, limit: usize) -> Result<Vec<(Hash, Revision)>> {
        let mut out = Vec::new();
        let mut next = Some(start);
        while let Some(h) = next {
            if out.len() >= limit {
                break;
            }
            let rev = self.store.get_revision(&h)?;
            next = rev.parents.first().copied();
            out.push((h, rev));
        }
        Ok(out)
    }

    /// Every revision reachable from any workspace (here or, as last
    /// fetched, in other repositories), parked working change or rev,
    /// newest first.
    pub fn log_all(&self, limit: usize) -> Result<Vec<(Hash, Revision)>> {
        let v = self.view()?;
        let heads: Vec<Hash> = v
            .workspaces
            .values()
            .chain(v.parked.values())
            .chain(v.revs.values())
            .copied()
            .chain(v.remote_heads.iter().map(|h| h.rev))
            .collect();
        let mut all = self.reachable(&heads)?;
        all.sort_by(|a, b| b.1.created_ms.cmp(&a.1.created_ms).then(b.0.cmp(&a.0)));
        all.truncate(limit);
        Ok(all)
    }

    fn reachable(&self, heads: &[Hash]) -> Result<Vec<(Hash, Revision)>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = heads.to_vec();
        while let Some(h) = stack.pop() {
            if !seen.insert(h) {
                continue;
            }
            let rev = self.store.get_revision(&h)?;
            stack.extend(rev.parents.iter().copied());
            out.push((h, rev));
        }
        Ok(out)
    }

    /// Resolve a revision spec:
    /// - `@` — this workspace's working change; `<workspace>@` — another's,
    ///   here or in another repository; `<workspace>@<host>` (see `lines`)
    /// - a bookmark name, e.g. `main`, or a rev (`psync rev`)
    /// - a change-id prefix (letters k-z) or revision-hash prefix (hex),
    ///   searched across all workspaces' histories
    /// - each trailing `-` moves to the parent, e.g. `@-`, `claude-2@--`
    pub fn resolve(&self, spec: &str) -> Result<Hash> {
        let base = spec.trim_end_matches('-');
        let ups = spec.len() - base.len();
        let view = self.view()?;
        let mut h = if base == "@" {
            self.working()?.0
        } else if let Some(b) = view.bookmarks.get(base).or_else(|| view.revs.get(base)) {
            *b
        } else if let Some((ws, at)) = base.split_once('@') {
            self.resolve_workspace(&view, ws, at)?
        } else if ChangeId::is_change_id_text(base) {
            self.find_revision(base, |_, rev| rev.change_id.to_string().starts_with(base))?
        } else if !base.is_empty() && base.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            self.find_revision(base, |h, _| h.to_hex().starts_with(base))?
        } else {
            bail!("cannot resolve '{spec}': use @, <workspace>@, main, a rev, a change id, or a revision hash (append '-' for the parent)");
        };
        for _ in 0..ups {
            let rev = self.store.get_revision(&h)?;
            h = *rev.parents.first().with_context(|| format!("'{spec}': revision {} has no parent", h.short()))?;
        }
        Ok(h)
    }

    fn find_revision(&self, what: &str, pred: impl Fn(&Hash, &Revision) -> bool) -> Result<Hash> {
        // Prefer the current workspace's own history, so a change id that was
        // amended elsewhere resolves to the version we are building on.
        let (mine, _) = self.working()?;
        let own: Vec<Hash> = self.reachable(&[mine])?.into_iter().filter(|(h, r)| pred(h, r)).map(|(h, _)| h).collect();
        let matches = if own.is_empty() {
            let heads: Vec<Hash> = self.view()?.workspaces.values().copied().collect();
            self.reachable(&heads)?.into_iter().filter(|(h, r)| pred(h, r)).map(|(h, _)| h).collect()
        } else {
            own
        };
        match matches.len() {
            0 => bail!("no revision matches '{what}'"),
            1 => Ok(matches[0]),
            n => bail!("'{what}' is ambiguous ({n} matches); use a longer prefix"),
        }
    }

    /// Whether `a` is an ancestor of (or equal to) `b`.
    pub fn is_ancestor(&self, a: Hash, b: Hash) -> Result<bool> {
        let g = self.generation_of(&a)?;
        Ancestry::new(self, b)?.contains(a, g)
    }

    /// Nearest common ancestor: ancestors of `b` are visited from the highest
    /// generation down, and the first one that is also an ancestor of `a`
    /// wins. Both walks stop at that generation instead of visiting all history.
    pub fn merge_base(&self, a: Hash, b: Hash) -> Result<Option<Hash>> {
        let mut ours = Ancestry::new(self, a)?;
        let mut heap = BinaryHeap::from([(self.generation_of(&b)?, b)]);
        let mut seen = HashSet::new();
        while let Some((g, h)) = heap.pop() {
            if !seen.insert(h) {
                continue;
            }
            if ours.contains(h, g)? {
                return Ok(Some(h));
            }
            for p in self.store.get_revision(&h)?.parents {
                heap.push((self.generation_of(&p)?, p));
            }
        }
        Ok(None)
    }

    // ---- main: sync, land, merge, resolve -------------------------------

    pub fn main_tip(&self) -> Result<Option<Hash>> {
        Ok(self.view()?.bookmarks.get(MAIN).copied())
    }

    /// Changes on the first-parent chain ending at `tip` that `main` does not
    /// contain, oldest first.
    pub(crate) fn unlanded(&self, tip: Hash, main: Option<Hash>) -> Result<Vec<(Hash, Revision)>> {
        let mut landed = match main {
            Some(m) => Some(Ancestry::new(self, m)?),
            None => None,
        };
        let mut out = Vec::new();
        let mut next = Some(tip);
        while let Some(h) = next {
            let rev = self.store.get_revision(&h)?;
            if let Some(l) = landed.as_mut() {
                if l.contains(h, rev.generation)? {
                    break;
                }
            }
            next = rev.parents.first().copied();
            out.push((h, rev));
        }
        out.reverse();
        Ok(out)
    }

    pub(crate) fn parent_tree(&self, rev: &Revision) -> Result<Hash> {
        match rev.parents.first() {
            Some(p) => Ok(self.store.get_revision(p)?.tree),
            None => self.empty_tree(),
        }
    }

    /// (main tip, committed changes we have that main lacks, main changes we lack).
    pub fn ahead_behind(&self) -> Result<Option<(Hash, usize, usize)>> {
        let Some(main) = self.main_tip()? else { return Ok(None) };
        let (wh, w) = self.working()?;
        let ahead = match w.parents.first() {
            Some(p) => self.unlanded(*p, Some(main))?.len(),
            None => 0,
        };
        let behind = self.unlanded(main, Some(wh))?.len();
        Ok(Some((main, ahead, behind)))
    }

    /// Rebase this workspace's unlanded committed changes and its working
    /// change onto the tip of main, updating files on disk. Conflicts never
    /// abort: they are written with markers and recorded on the working
    /// change. A committed change that conflicts is folded, with everything
    /// after it, into the working change so all conflicts are in one place.
    /// Change ids on main's first-parent line above `base` (all of main if
    /// `base` is `None`).
    pub(crate) fn change_ids_on_main_since(&self, main: Hash, base: Option<Hash>) -> Result<HashSet<ChangeId>> {
        let mut below = match base {
            Some(b) => Some(Ancestry::new(self, b)?),
            None => None,
        };
        let mut ids = HashSet::new();
        let mut next = Some(main);
        while let Some(h) = next {
            let rev = self.store.get_revision(&h)?;
            if let Some(b) = below.as_mut() {
                if b.contains(h, rev.generation)? {
                    break;
                }
            }
            ids.insert(rev.change_id);
            next = rev.parents.first().copied();
        }
        Ok(ids)
    }

    pub fn sync(&self) -> Result<SyncOutcome> {
        let main = self.main_tip()?.context("there is no main yet: land a change first with `psync land`")?;
        let (_, w) = self.working()?;
        self.rebase_onto(main, &w)
    }

    fn rebase_onto(&self, main: Hash, w: &Revision) -> Result<SyncOutcome> {
        if w.parents.first() == Some(&main) {
            return Ok(SyncOutcome {
                main,
                up_to_date: true,
                rebased: vec![],
                folded: vec![],
                conflicts: w.conflicts.clone(),
            });
        }
        let main_rev = self.store.get_revision(&main)?;
        let main_label = format!("main ({})", &main_rev.change_id.short()[..8]);
        let mut stack = match w.parents.first() {
            Some(p) => self.unlanded(*p, Some(main))?,
            None => vec![],
        };
        // Changes already on main under the same change id (e.g. landed again
        // after a pull moved them back into a workspace) are not re-applied.
        if let Some((_, first)) = stack.first() {
            let on_main = self.change_ids_on_main_since(main, first.parents.first().copied())?;
            stack.retain(|(_, r)| !on_main.contains(&r.change_id));
        }
        let (mut parent, mut parent_tree) = (main, main_rev.tree);
        let mut rebased = Vec::new();
        let mut folded = Vec::new();
        let mut work_base = self.parent_tree(w)?;
        for (i, (h, rev)) in stack.iter().enumerate() {
            if rev.parents.first() == Some(&parent) {
                // Already on the right parent (nothing below it was rewritten).
                parent = *h;
                parent_tree = rev.tree;
                continue;
            }
            let base = self.parent_tree(rev)?;
            let label = format!("{} ({})", first_line(&rev.meta.intent), &rev.change_id.short()[..8]);
            let m = merge::merge_trees(&self.store, Some(base), parent_tree, rev.tree, &Labels { ours: &main_label, theirs: &label })?;
            if !m.conflicts.is_empty() {
                work_base = base;
                folded = stack[i..].iter().map(|(_, r)| r.clone()).collect();
                break;
            }
            let mut nr = rev.clone();
            nr.tree = m.tree;
            nr.parents = vec![parent];
            nr.generation = self.generation_for(&nr.parents)?;
            nr.committer = self.identity.clone();
            nr.updated_ms = now_ms();
            let nh = self.store.put(&Object::Revision(nr.clone()))?;
            rebased.push((nh, nr));
            parent = nh;
            parent_tree = m.tree;
        }
        if folded.is_empty() && w.parents.first() == Some(&parent) {
            return Ok(SyncOutcome { main, up_to_date: true, rebased, folded, conflicts: w.conflicts.clone() });
        }
        let label = format!("yours ({})", &w.change_id.short()[..8]);
        let m = merge::merge_trees(&self.store, Some(work_base), parent_tree, w.tree, &Labels { ours: &main_label, theirs: &label })?;
        let mut nw = w.clone();
        nw.tree = m.tree;
        nw.parents = vec![parent];
        nw.generation = self.generation_for(&nw.parents)?;
        nw.committer = self.identity.clone();
        nw.updated_ms = now_ms();
        nw.conflicts = self.carry_conflicts(m.conflicts, &w.conflicts, nw.tree)?;
        let working_was_empty = stack.last().is_some_and(|(_, last)| last.tree == w.tree) && w.meta.intent.is_empty();
        if let (true, Some(first)) = (working_was_empty, folded.first()) {
            // Nothing of the working change's own: keep the folded change's identity.
            nw.change_id = first.change_id;
            nw.author = first.author.clone();
            nw.created_ms = first.created_ms;
        }
        if !folded.is_empty() {
            if nw.meta.intent.is_empty() {
                let intents: Vec<&str> = folded.iter().map(|r| r.meta.intent.as_str()).filter(|i| !i.is_empty()).collect();
                nw.meta.intent = intents.join("; ");
            }
            if nw.meta.task.is_none() {
                nw.meta.task = folded[0].meta.task.clone();
            }
        }
        let nwh = self.store.put(&Object::Revision(nw.clone()))?;
        wc::checkout(&self.root, &self.store, w.tree, nw.tree, self.eol()?)?;
        self.set_entry(
            &self.workspace,
            Some(nwh),
            format!("sync onto main {} ({} rebased, {} folded)", &main_rev.change_id.short()[..8], rebased.len(), folded.len()),
            None,
        )?;
        self.refresh_index()?;
        Ok(SyncOutcome { main, up_to_date: false, rebased, folded, conflicts: nw.conflicts })
    }

    /// New conflicts, plus earlier ones that are still present in `tree`.
    fn carry_conflicts(&self, mut new: Vec<Conflict>, old: &[Conflict], tree: Hash) -> Result<Vec<Conflict>> {
        for c in old {
            if !new.iter().any(|n| n.path == c.path) && merge::still_conflicted(&self.store, tree, c)? {
                new.push(c.clone());
            }
        }
        Ok(new)
    }

    pub(crate) fn refresh_index(&self) -> Result<()> {
        let scan = self.scan_own()?;
        scan.save_index(&self.index_path())
    }

    /// Put this workspace's committed changes onto main: sync, optionally run
    /// `check` against exactly the tree being landed, record the result on the
    /// landed change, and advance main with a compare-and-swap. If another
    /// agent landed in the meantime, sync and check again (up to 3 times).
    /// With `land.queue`, first wait for this land's turn (see `queue`).
    pub fn land(&self, check: Option<&str>) -> Result<LandResult> {
        self.land_with(check, &mut |_| {})
    }

    /// [`Repo::land`], calling `on_wait` whenever the land queue changes
    /// while this land waits its turn.
    pub fn land_with(&self, check: Option<&str>, on_wait: &mut dyn FnMut(&crate::queue::LandQueue)) -> Result<LandResult> {
        let _turn = if self.land_queue_on() {
            // What would refuse the land anyway is found before the wait.
            let (_, w) = self.working()?;
            if !w.conflicts.is_empty() {
                bail!("resolve the {} conflict(s) in your working change first (see `psync status`)", w.conflicts.len());
            }
            if !self.changes_of(&w)?.is_empty() {
                bail!("your working change has uncommitted edits: `psync commit` them (or `psync undo`) before landing");
            }
            let changes = match w.parents.first() {
                Some(tip) => self.unlanded(*tip, self.main_tip()?)?,
                None => vec![],
            };
            if changes.is_empty() {
                bail!("nothing to land: commit your work first (`psync commit -m ...`)");
            }
            for (h, r) in &changes {
                self.vet(crate::msgcheck::Stage::Land, Some(h), r, Some(&self.identity))?;
            }
            let place = crate::queue::Place::join(&self.dot, &self.workspace, &self.identity, check)?;
            place.wait_for_main(on_wait)?;
            Some(place)
        } else {
            None
        };
        for _ in 0..3 {
            let (_, w) = self.working()?;
            if !w.conflicts.is_empty() {
                bail!("resolve the {} conflict(s) in your working change first (see `psync status`)", w.conflicts.len());
            }
            if !self.changes_of(&w)?.is_empty() {
                bail!("your working change has uncommitted edits: `psync commit` them (or `psync undo`) before landing");
            }
            let main = self.main_tip()?;
            let mut synced = false;
            if let Some(m) = main {
                let s = self.rebase_onto(m, &w)?;
                if !s.conflicts.is_empty() {
                    return Ok(LandResult::Conflicts(s));
                }
                synced = !s.up_to_date;
            }
            let (wh, w) = self.working()?;
            let tip = *w.parents.first().context("nothing to land")?;
            let changes = self.unlanded(tip, main)?;
            if changes.is_empty() {
                bail!("nothing to land: commit your work first (`psync commit -m ...`)");
            }
            for (h, r) in &changes {
                self.vet(crate::msgcheck::Stage::Land, Some(h), r, Some(&self.identity))?;
            }
            let tip_rev = self.store.get_revision(&tip)?;
            let run = match check {
                Some(cmd) => Some(self.run_check(cmd, tip_rev.tree)?),
                None => None,
            };
            if let Some(r) = run.as_ref().filter(|r| !r.passed) {
                return Ok(LandResult::CheckFailed { check: r.clone(), changes });
            }
            let (final_tip, final_w, changes) = match &run {
                Some(r) => {
                    let mut t = tip_rev.clone();
                    t.meta.checks.push(Check {
                        command: r.command.clone(),
                        passed: true,
                        exit_code: r.exit_code,
                        duration_ms: r.duration_ms,
                        tree: t.tree,
                        time_ms: now_ms(),
                        by: self.identity.clone(),
                    });
                    let th = self.store.put(&Object::Revision(t.clone()))?;
                    let mut nw = w.clone();
                    nw.parents = vec![th];
                    nw.generation = self.generation_for(&nw.parents)?;
                    let nwh = self.store.put(&Object::Revision(nw))?;
                    let mut changes = changes;
                    if let Some(last) = changes.last_mut() {
                        *last = (th, t);
                    }
                    (th, nwh, changes)
                }
                None => (tip, wh, changes),
            };
            let ws = self.workspace.clone();
            let desc = format!(
                "land {} change(s) onto main: {}",
                changes.len(),
                first_line(&changes.last().expect("non-empty").1.meta.intent)
            );
            let task_ids: BTreeSet<String> = changes.iter().filter_map(|(_, r)| r.meta.task.clone()).collect();
            let mut shared = vec![MAIN.to_string(), "claims".to_string()];
            shared.extend(task_ids.iter().map(|t| format!("task:{t}")));
            let applied = self.update_view(Some(&ws), desc, None, shared, |view| {
                if view.bookmarks.get(MAIN).copied() != main {
                    return Ok(false);
                }
                view.bookmarks.insert(MAIN.into(), final_tip);
                view.workspaces.insert(ws.clone(), final_w);
                self.record_landed(view, &changes, std::slice::from_ref(&ws))?;
                Ok(true)
            })?;
            if applied.is_some() {
                return Ok(LandResult::Landed { main: final_tip, changes, check: run, synced });
            }
        }
        bail!("main kept moving while landing (3 attempts); run `psync land` again")
    }

    /// In a land's view update: record progress on the tasks the landed
    /// changes belong to, and release the claims of `workspaces` (their work
    /// is on main, so the claims have served their purpose).
    pub(crate) fn record_landed(&self, view: &mut View, changes: &[(Hash, Revision)], workspaces: &[String]) -> Result<()> {
        let task_ids: BTreeSet<String> = changes.iter().filter_map(|(_, r)| r.meta.task.clone()).collect();
        for id in &task_ids {
            if let Some(th) = view.tasks.get(id).copied() {
                let mut t = self.store.get_task(&th)?;
                for (_, r) in changes.iter().filter(|(_, r)| r.meta.task.as_deref() == Some(id)) {
                    if !t.landed.contains(&r.change_id) {
                        t.landed.push(r.change_id);
                    }
                }
                if t.status == crate::object::TaskStatus::Open {
                    t.status = crate::object::TaskStatus::InProgress;
                }
                t.previous = Some(th);
                t.updated_ms = now_ms();
                view.tasks.insert(id.clone(), self.store.put(&Object::Task(t))?);
            }
        }
        let before = view.claims.len();
        view.claims.retain(|c| !workspaces.iter().any(|w| c.is_of(w)));
        if view.claims.len() != before {
            crate::coord::touch_claims(view);
        }
        Ok(())
    }

    pub(crate) fn run_check(&self, cmd: &str, expected_tree: Hash) -> Result<CheckRun> {
        let start = Instant::now();
        let out = crate::platform::shell(cmd)
            .current_dir(&self.root)
            .env("PSYNC_WORKSPACE", &self.workspace)
            .output()
            .with_context(|| format!("running check `{cmd}`"))?;
        let duration_ms = start.elapsed().as_millis() as u64;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let lines: Vec<&str> = text.lines().collect();
        let output_tail = lines[lines.len().saturating_sub(40)..].join("\n");
        // What was tested must be exactly what lands.
        let scan = self.scan_own()?;
        scan.save_index(&self.index_path())?;
        if scan.tree != expected_tree {
            bail!("the check `{cmd}` modified tracked files, so landing was aborted; review with `psync status`, then commit or undo those edits");
        }
        Ok(CheckRun { command: cmd.into(), passed: out.status.success(), exit_code: out.status.code(), duration_ms, output_tail })
    }

    /// Merge another change (e.g. `claude-1@`) into the working change, which
    /// gains it as an extra parent. Conflicts are recorded, not fatal.
    pub fn merge(&self, spec: &str) -> Result<MergeOutcome> {
        let other = self.resolve(spec)?;
        let (wh, w) = self.working()?;
        if other == wh || self.is_ancestor(other, wh)? {
            bail!("'{spec}' is already part of your history");
        }
        let orev = self.store.get_revision(&other)?;
        let base = self.merge_base(wh, other)?;
        let base_tree = match base {
            Some(b) => self.store.get_revision(&b)?.tree,
            None => self.empty_tree()?,
        };
        let ours = format!("yours ({})", &w.change_id.short()[..8]);
        let theirs = format!("{spec} ({})", &orev.change_id.short()[..8]);
        let m = merge::merge_trees(&self.store, Some(base_tree), w.tree, orev.tree, &Labels { ours: &ours, theirs: &theirs })?;
        let files = diff::diff_trees(&self.store, Some(w.tree), Some(m.tree))?.len();
        let mut nw = w.clone();
        nw.tree = m.tree;
        nw.parents.push(other);
        nw.generation = self.generation_for(&nw.parents)?;
        nw.committer = self.identity.clone();
        nw.updated_ms = now_ms();
        nw.conflicts = self.carry_conflicts(m.conflicts, &w.conflicts, nw.tree)?;
        let nwh = self.store.put(&Object::Revision(nw.clone()))?;
        wc::checkout(&self.root, &self.store, w.tree, nw.tree, self.eol()?)?;
        self.set_entry(
            &self.workspace,
            Some(nwh),
            format!("merge {spec} ({}) into {}", &orev.change_id.short()[..8], w.change_id.short()),
            None,
        )?;
        self.refresh_index()?;
        Ok(MergeOutcome { merged: other, base, files, conflicts: nw.conflicts })
    }

    /// Undo the changes of `spec` (typically a landed change) in the working
    /// change, which must have no changes of its own: a three-way merge from
    /// the change's tree back to its parent's, applied on top of everything
    /// since. Later edits to the same lines become recorded conflicts. The
    /// working change gets the intent `Revert "..."` (unless it has one) and
    /// `revert_of` metadata; commit and land it like any other change.
    /// Make `paths` (files, directories or globs) in the working change
    /// exactly as they are in revision `spec`: their files are written back
    /// byte for byte, and files under them that `spec` does not have are
    /// removed. Everything else in the working change is untouched. One
    /// operation: `undo` reverts it (edits it overwrote were snapshotted).
    pub fn restore(&self, spec: &str, paths: &[String]) -> Result<RestoreOutcome> {
        if paths.is_empty() {
            bail!("name the files or directories to restore (for a whole earlier state, use `psync undo` or `psync op restore`)");
        }
        let source = self.resolve(spec)?;
        let src = self.store.get_revision(&source)?;
        let (_, w) = self.working()?;
        let specs: Vec<PathSpec> = paths.iter().map(|p| PathSpec::new(p)).collect::<Result<_>>()?;
        let theirs = diff::flatten(&self.store, src.tree)?;
        let ours = diff::flatten(&self.store, w.tree)?;
        for sp in &specs {
            if !theirs.iter().chain(&ours).any(|(p, _)| sp.matches(p)) {
                bail!("'{}' matches no file in {spec} or in the working change", sp.raw);
            }
        }
        let changes: Vec<FileChange> = diff::diff_trees(&self.store, Some(w.tree), Some(src.tree))?
            .into_iter()
            .filter(|c| specs.iter().any(|sp| sp.matches(&c.path)))
            .collect();
        let mut files = Vec::with_capacity(changes.len());
        for c in &changes {
            let size = match c.new {
                Some((_, h)) => Some(self.store.file_size(&h)?),
                None => None,
            };
            files.push(RestoredFile { path: c.path.clone(), leaf: c.new, size });
        }
        if changes.is_empty() {
            return Ok(RestoreOutcome { source, files });
        }
        let mut nw = w.clone();
        nw.tree = wc::apply_changes(&self.store, w.tree, &changes)?;
        nw.conflicts.retain(|c| !specs.iter().any(|sp| sp.matches(&c.path)));
        nw.committer = self.identity.clone();
        nw.updated_ms = now_ms();
        let nwh = self.store.put(&Object::Revision(nw.clone()))?;
        wc::checkout(&self.root, &self.store, w.tree, nw.tree, self.eol()?)?;
        let short = &src.change_id.short()[..8];
        self.set_entry(
            &self.workspace,
            Some(nwh),
            format!("restore {} from {short}: {}", plural_files(files.len()), paths.join(" ")),
            None,
        )?;
        self.refresh_index()?;
        Ok(RestoreOutcome { source, files })
    }

    /// The last commit of this workspace and which files differ from it
    /// (only under `paths`, when given), like `git rev-parse HEAD` plus
    /// `git status --porcelain`. It scans the working copy (through the
    /// watcher if one runs) but records nothing and needs no workspace lock,
    /// so it is cheap to run on every build.
    pub fn identify(&self, paths: &[String]) -> Result<Identification> {
        let (_, w) = self.working()?;
        let Some(&revision) = w.parents.first() else { bail!("this workspace has no commit yet") };
        let commit = self.store.get_revision(&revision)?;
        let specs: Vec<PathSpec> = paths.iter().map(|p| PathSpec::new(p)).collect::<Result<_>>()?;
        let tree = self.scan_own()?.tree;
        let changed = diff::diff_trees(&self.store, Some(commit.tree), Some(tree))?
            .into_iter()
            .map(|c| c.path)
            .filter(|p| specs.is_empty() || specs.iter().any(|sp| sp.matches(p)))
            .collect();
        let on_main = match self.main_tip()? {
            Some(m) => Some(m == revision || self.is_ancestor(revision, m)?),
            None => None,
        };
        Ok(Identification { revision, commit, working: w.change_id, changed, on_main })
    }

    /// Refuse files above `commit.max_file_mb` that `rev` adds or changes
    /// (no staging area: this keeps model weights and datasets out).
    fn check_file_sizes(&self, rev: &Revision) -> Result<()> {
        let Some((limit, _)) = config::get(Some(&self.dot), "commit.max_file_mb") else { return Ok(()) };
        let mb: f64 = limit.trim().parse().map_err(|_| crate::error::Error::msg(format!("commit.max_file_mb is not a number: {limit}")))?;
        if mb <= 0.0 {
            return Ok(());
        }
        let max = (mb * 1024.0 * 1024.0) as u64;
        let mut large = Vec::new();
        for c in diff::diff_trees(&self.store, Some(self.parent_tree(rev)?), Some(rev.tree))? {
            if let Some((EntryKind::File | EntryKind::Exec, h)) = c.new {
                let size = self.store.file_size(&h)?;
                if size > max {
                    large.push(format!("{} ({:.1} MB)", c.path, size as f64 / 1048576.0));
                }
            }
        }
        if !large.is_empty() {
            bail!(
                "{} above commit.max_file_mb ({limit} MB): {}. To keep {} out, add {} to .psyncignore or commit other paths only (`psync commit <paths>`); to commit anyway, pass --allow-large",
                if large.len() == 1 { "1 file is".to_string() } else { format!("{} files are", large.len()) },
                large.join(", "),
                if large.len() == 1 { "it" } else { "them" },
                if large.len() == 1 { "it" } else { "them" },
            );
        }
        Ok(())
    }

    /// Write revision `spec`'s files (only under `paths`, when given) as a
    /// tar to `w`, each under `prefix`. See [`crate::archive`].
    pub fn archive(
        &self,
        spec: &str,
        paths: &[String],
        prefix: &str,
        w: &mut dyn std::io::Write,
    ) -> Result<(Hash, Revision, crate::archive::ArchiveStats)> {
        let h = self.resolve(spec)?;
        let rev = self.store.get_revision(&h)?;
        let specs: Vec<PathSpec> = paths.iter().map(|p| PathSpec::new(p)).collect::<Result<_>>()?;
        let mtime = u64::try_from(rev.updated_ms / 1000).unwrap_or(0);
        let stats = crate::archive::write_tar(&self.store, rev.tree, h, mtime, prefix, &specs, w)?;
        Ok((h, rev, stats))
    }

    /// The file at `path` in revision `spec`: (revision, kind, content hash).
    pub fn file_at(&self, spec: &str, path: &str) -> Result<(Hash, EntryKind, Hash)> {
        let rev = self.resolve(spec)?;
        let tree = self.store.get_revision(&rev)?.tree;
        let path = path.trim_matches('/');
        match diff::lookup(&self.store, tree, path)? {
            Some((EntryKind::Tree, _)) => bail!("'{path}' is a directory in {spec}"),
            Some((kind, h)) => Ok((rev, kind, h)),
            None => bail!("no file '{path}' in {spec}"),
        }
    }

    pub fn revert(&self, spec: &str) -> Result<RevertOutcome> {
        let target = self.resolve(spec)?;
        let (wh, w) = self.working()?;
        if target == wh {
            bail!("cannot revert the working change itself; use `psync undo` or edit the files");
        }
        if !self.is_ancestor(target, wh)? {
            bail!("'{spec}' is not part of your history (if it landed on main since, run `psync sync` first)");
        }
        let parent_tree = match w.parents.first() {
            Some(p) => self.store.get_revision(p)?.tree,
            None => self.empty_tree()?,
        };
        if w.tree != parent_tree || !w.conflicts.is_empty() {
            bail!("your working change has changes of its own; commit or undo them first, so the revert is a change by itself");
        }
        let rev = self.store.get_revision(&target)?;
        let before = match rev.parents.first() {
            Some(p) => self.store.get_revision(p)?.tree,
            None => self.empty_tree()?,
        };
        let short = &rev.change_id.short()[..8];
        let theirs = format!("revert of {short}");
        let m = merge::merge_trees(&self.store, Some(rev.tree), w.tree, before, &Labels { ours: "current", theirs: &theirs })?;
        if m.tree == w.tree && m.conflicts.is_empty() {
            bail!("nothing to revert: the changes of {short} are already undone");
        }
        let files = diff::diff_trees(&self.store, Some(w.tree), Some(m.tree))?.len();
        let mut nw = w.clone();
        nw.tree = m.tree;
        if nw.meta.intent.is_empty() {
            nw.meta.intent = format!("Revert \"{}\"", first_line(&rev.meta.intent));
        }
        nw.meta.extra.insert("revert_of".into(), rev.change_id.short());
        nw.author = self.identity.clone();
        nw.committer = self.identity.clone();
        nw.updated_ms = now_ms();
        nw.conflicts = m.conflicts;
        let nwh = self.store.put(&Object::Revision(nw.clone()))?;
        wc::checkout(&self.root, &self.store, w.tree, nw.tree, self.eol()?)?;
        self.set_entry(&self.workspace, Some(nwh), format!("revert {short} in {}", w.change_id.short()), None)?;
        self.refresh_index()?;
        Ok(RevertOutcome { reverted: target, files, conflicts: nw.conflicts })
    }

    /// Mark a conflict resolved. Refuses while the file still has conflict
    /// markers unless `force`.
    pub fn resolve_conflict(&self, path: &str, force: bool) -> Result<Vec<Conflict>> {
        let (_, mut w) = self.working()?;
        let i = w
            .conflicts
            .iter()
            .position(|c| c.path == path)
            .with_context(|| format!("no conflict recorded for '{path}'"))?;
        if !force {
            if let Some((kind, h)) = diff::lookup(&self.store, w.tree, path)? {
                if kind != crate::object::EntryKind::Tree && self.store.get_small_file(&h)?.is_some_and(|d| merge::has_markers(&d)) {
                    bail!("'{path}' still contains conflict markers: edit it, or pass --force to accept it as is");
                }
            }
        }
        w.conflicts.remove(i);
        w.committer = self.identity.clone();
        w.updated_ms = now_ms();
        let h = self.store.put(&Object::Revision(w.clone()))?;
        self.set_entry(&self.workspace, Some(h), format!("resolve conflict in {path}"), None)?;
        Ok(w.conflicts)
    }

    // ---- workspaces ----------------------------------------------------

    pub fn workspace_meta(&self, ws: &str) -> Option<WorkspaceMeta> {
        fs::read(self.ws_dir(ws).join("meta.json")).ok().and_then(|b| crate::json::from_slice(&b).ok())
    }

    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let view = self.view()?;
        view.workspaces
            .iter()
            .map(|(name, rev)| {
                Ok(WorkspaceInfo {
                    name: name.clone(),
                    path: self.workspace_meta(name).map(|m| m.path),
                    rev: *rev,
                    revision: self.store.get_revision(rev)?,
                })
            })
            .collect()
    }

    /// Create a workspace: a new directory with its own working change on
    /// top of `base` (default: the parent of the current working change).
    pub fn workspace_add(&self, name: &str, path: Option<&Path>, base: Option<&str>) -> Result<AddedWorkspace> {
        validate_workspace_name(name)?;
        if self.view()?.workspaces.contains_key(name) || self.ws_dir(name).exists() {
            bail!("workspace '{name}' already exists");
        }
        let path = match path {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => std::env::current_dir()?.join(p),
            None => {
                let main = self.workspace_meta(DEFAULT_WORKSPACE).map(|m| m.path).unwrap_or(self.root.clone());
                let repo_name = main.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
                main.parent().unwrap_or(&main).join(format!("{repo_name}-{name}"))
            }
        };
        if path.exists() && fs::read_dir(&path)?.next().is_some() {
            bail!("{} already exists and is not empty", path.display());
        }
        let base_hash = match base {
            Some(spec) => self.resolve(spec)?,
            None => match self.main_tip()? {
                Some(m) => m,
                None => {
                    let (_, working) = self.working()?;
                    *working.parents.first().context(
                        "the working change has no parent to branch from: commit it first, or pass --rev @",
                    )?
                }
            },
        };
        let base_rev = self.store.get_revision(&base_hash)?;

        fs::create_dir_all(&path)?;
        let path = crate::platform::canonicalize(&path)?;
        let ws_dir = self.ws_dir(name);
        fs::create_dir_all(&ws_dir)?;
        let meta = WorkspaceMeta { path: path.clone(), created_ms: now_ms(), created_by: self.identity.clone() };
        atomic_write(&ws_dir.join("meta.json"), &crate::json::to_vec_pretty(&meta)?)?;
        let link = WorkspaceLink { repo: self.dot.clone(), workspace: name.to_string() };
        atomic_write(&path.join(DOT_DIR), &crate::json::to_vec_pretty(&link)?)?;

        let source_index = Index::load(&self.index_path());
        let source = CloneSource { root: &self.root, index: &source_index };
        let m = wc::materialize(&path, &self.store, base_rev.tree, Some(source), self.eol()?)?;
        m.index.save(&ws_dir.join("index"))?;

        let rev = self.start_on(base_hash)?;
        let rev_hash = self.store.put(&Object::Revision(rev.clone()))?;
        self.set_entry(
            name,
            Some(rev_hash),
            format!("add workspace {name} on {} at {}", base_rev.change_id.short(), path.display()),
            None,
        )?;
        Ok(AddedWorkspace {
            info: WorkspaceInfo { name: name.to_string(), path: Some(path), rev: rev_hash, revision: rev },
            files: m.files,
            cloned: m.cloned,
        })
    }

    /// Remove a workspace and delete its directory. Its last state is
    /// snapshotted first, so its work stays in history.
    pub fn workspace_remove(&self, name: &str) -> Result<Hash> {
        if name == DEFAULT_WORKSPACE {
            bail!("the default workspace cannot be removed");
        }
        if name == self.workspace {
            bail!("cannot remove the workspace you are in; run this from another workspace");
        }
        self.working_of(name)?;
        let ws_dir = self.ws_dir(name);
        let lock = open_lock(&ws_dir.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!("workspace '{name}' is busy (a command is running in it)"),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let path = self.workspace_meta(name).map(|m| m.path);
        if let Some(p) = &path {
            if p.is_dir() {
                snapshot_workspace(self, name, p, &ws_dir, "")?;
            }
        }
        watch::stop(&ws_dir);
        let (last, _) = self.working_of(name)?;
        self.set_entry(name, None, format!("remove workspace {name} (last revision {})", last.short()), None)?;
        self.release_where(format!("release claims of removed workspace {name}"), |c| c.workspace == name)?;
        if let Some(p) = path {
            // Only delete a directory that is really this workspace.
            let link: Option<WorkspaceLink> =
                fs::read(p.join(DOT_DIR)).ok().and_then(|b| crate::json::from_slice(&b).ok());
            if link.is_some_and(|l| l.workspace == name && l.repo == self.dot) {
                fs::remove_dir_all(&p).with_context(|| format!("deleting {}", p.display()))?;
            }
        }
        drop(lock);
        fs::remove_dir_all(&ws_dir)?;
        Ok(last)
    }

    // ---- context ---------------------------------------------------------

    /// Briefing for an agent: its working change, recent history, and what
    /// every other workspace has changed since diverging — with overlaps.
    pub fn context(&self, recent: usize) -> Result<Briefing> {
        let working = self.working()?;
        let files = self.changes_of(&working.1)?.into_iter().map(|c| c.path).collect();
        let recent = self.log(working.0, recent + 1)?.into_iter().skip(1).collect();
        let mut peers = Vec::new();
        let mut all: Vec<(WorkspaceInfo, Option<String>)> =
            self.workspaces()?.into_iter().filter(|i| i.name != self.workspace).map(|i| (i, None)).collect();
        for (name, h) in self.remote_heads()? {
            let revision = self.store.get_revision(&h.rev)?;
            all.push((WorkspaceInfo { name, path: None, rev: h.rev, revision }, Some(h.home.place)));
        }
        for (info, place) in all {
            let base = match self.merge_base(working.0, info.rev)? {
                Some(b) => self.store.get_revision(&b)?.tree,
                None => self.empty_tree()?,
            };
            let theirs: Vec<String> =
                diff::diff_trees(&self.store, Some(base), Some(info.revision.tree))?.into_iter().map(|c| c.path).collect();
            let mine: HashSet<String> =
                diff::diff_trees(&self.store, Some(base), Some(working.1.tree))?.into_iter().map(|c| c.path).collect();
            if place.is_some() && theirs.is_empty() && info.revision.meta.intent.is_empty() {
                continue; // another repository's idle workspace: noise here
            }
            let overlap = theirs.iter().filter(|p| mine.contains(*p)).cloned().collect();
            peers.push(PeerSummary { info, files: theirs, overlap, place });
        }
        let (main, ahead, behind) = match self.ahead_behind()? {
            Some((m, a, b)) => (Some((m, self.store.get_revision(&m)?)), a, b),
            None => (None, 0, 0),
        };
        Ok(Briefing {
            workspace: self.workspace.clone(),
            root: self.root.clone(),
            identity: self.identity.clone(),
            working,
            files,
            main,
            ahead,
            behind,
            recent,
            peers,
        })
    }
}

fn plural_files(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

/// Same accountable human (name and email), whatever agent acts for them.
fn same_owner(a: &Identity, b: &Identity) -> bool {
    a.name == b.name && a.email == b.email
}

/// Snapshot workspace `ws` (rooted at `root`, metadata in `ws_dir`) into its
/// working change. The caller must hold that workspace's lock.
fn snapshot_workspace(repo: &Repo, ws: &str, root: &Path, ws_dir: &Path, note: &str) -> Result<SnapshotOutcome> {
    let index_path = ws_dir.join("index");
    let scan = watch::scan(root, &repo.store, ws_dir)?;
    let (_, mut rev) = repo.working_of(ws)?;
    let changed = scan.tree != rev.tree;
    if changed {
        repo.claim_if_unclaimed(&mut rev, true)?;
        let change = rev.change_id.short();
        let mut conflicts = Vec::new();
        for c in &rev.conflicts {
            if merge::still_conflicted(&repo.store, scan.tree, c)? {
                conflicts.push(c.clone());
            }
        }
        let new = Revision { tree: scan.tree, conflicts, committer: repo.identity.clone(), updated_ms: now_ms(), ..rev };
        let h = repo.store.put(&Object::Revision(new))?;
        repo.set_entry(ws, Some(h), format!("snapshot working copy into {change}{note}"), None)?;
    }
    scan.save_index(&index_path)?;
    Ok(SnapshotOutcome {
        changed,
        files: scan.index.entries.len(),
        hashed: scan.hashed,
        cached: scan.cached,
        watched: scan.incremental,
    })
}

/// Group paths by the workspaces touching them (used for overlap reports).
pub fn overlap_map(peers: &[PeerSummary]) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in peers {
        for f in &p.overlap {
            m.entry(f.clone()).or_default().push(p.info.name.clone());
        }
    }
    m
}

pub(crate) fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// Incremental ancestor set of one revision, expanded in decreasing
/// generation order only as far as queries need. Generations strictly
/// decrease from child to parent, so once every node of generation >= g has
/// been expanded, membership is exact for any revision of generation g.
/// (Revisions written before generations existed have generation 0 and
/// force a full walk, which stays correct.)
struct Ancestry<'a> {
    repo: &'a Repo,
    heap: BinaryHeap<(u64, Hash)>,
    expanded: HashSet<Hash>,
    /// Parents of revisions seen so far (each revision is read once).
    parents: HashMap<Hash, Vec<Hash>>,
}

impl<'a> Ancestry<'a> {
    fn new(repo: &'a Repo, start: Hash) -> Result<Self> {
        let mut a = Ancestry { repo, heap: BinaryHeap::new(), expanded: HashSet::new(), parents: HashMap::new() };
        let g = a.read(start)?;
        a.heap.push((g, start));
        Ok(a)
    }

    /// Read a revision: remember its parents, return its generation.
    fn read(&mut self, h: Hash) -> Result<u64> {
        let rev = self.repo.store.get_revision(&h)?;
        self.parents.insert(h, rev.parents);
        Ok(rev.generation)
    }

    fn contains(&mut self, h: Hash, generation: u64) -> Result<bool> {
        while let Some(&(g, x)) = self.heap.peek() {
            if g < generation {
                break;
            }
            self.heap.pop();
            if !self.expanded.insert(x) {
                continue;
            }
            let parents = self.parents.get(&x).cloned().unwrap_or_default();
            for p in parents {
                if self.parents.contains_key(&p) {
                    continue; // already queued
                }
                let pg = self.read(p)?;
                self.heap.push((pg, p));
            }
        }
        Ok(self.expanded.contains(&h))
    }
}
