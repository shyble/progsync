//! Immutable, content-addressed objects.
//!
//! Encoding: `[FORMAT_VERSION, TAG, payload...]`; the object's [`Hash`] is
//! BLAKE3 over the full encoding.
//! - Blobs: raw bytes.
//! - Chunked files (files larger than [`crate::chunk::LARGE`]): a list of
//!   chunks, each a blob ([`crate::bin`] encoding).
//! - Trees: psync's binary encoding ([`crate::bin`]; compact, fast; trees are
//!   hot and their shape is fixed).
//! - Revisions, operations and tasks: JSON ([`crate::json`]; fields in
//!   declaration order, so deterministic; extensible: new fields are added
//!   as `#default` and older readers ignore fields they do not know).

use crate::hash::{ChangeId, Hash};
use crate::bail;
use crate::error::Result;
use std::collections::BTreeMap;

pub const FORMAT_VERSION: u8 = 2;
pub const TAG_BLOB: u8 = b'b';
pub const TAG_TREE: u8 = b't';
pub const TAG_REVISION: u8 = b'r';
pub const TAG_OPERATION: u8 = b'o';
pub const TAG_TASK: u8 = b'k';
pub const TAG_CHUNKED: u8 = b'c';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Exec,
    Symlink,
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub hash: Hash,
}

/// A directory listing. Entries are sorted by name (byte order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl EntryKind {
    fn code(self) -> u64 {
        match self {
            EntryKind::File => 0,
            EntryKind::Exec => 1,
            EntryKind::Symlink => 2,
            EntryKind::Tree => 3,
        }
    }

    fn from_code(c: u64) -> Result<EntryKind> {
        Ok(match c {
            0 => EntryKind::File,
            1 => EntryKind::Exec,
            2 => EntryKind::Symlink,
            3 => EntryKind::Tree,
            _ => bail!("corrupt entry kind {c}"),
        })
    }

    pub(crate) fn write(self, w: &mut crate::bin::Writer) {
        w.u64(self.code());
    }

    pub(crate) fn read(r: &mut crate::bin::Reader) -> Result<EntryKind> {
        EntryKind::from_code(r.u64()?)
    }
}

impl Tree {
    /// Encoding: entry count, then per entry its name, kind and hash.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = crate::bin::Writer::default();
        w.len(self.entries.len());
        for e in &self.entries {
            w.str(&e.name);
            e.kind.write(&mut w);
            w.hash(&e.hash);
        }
        w.buf
    }

    pub fn decode(data: &[u8]) -> Result<Tree> {
        let mut r = crate::bin::Reader::new(data);
        // Each entry is at least 1 (name length) + 1 (kind) + 32 (hash) bytes.
        let n = r.len(34)?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            entries.push(TreeEntry { name: r.str()?, kind: EntryKind::read(&mut r)?, hash: r.hash()? });
        }
        r.finish()?;
        Ok(Tree { entries })
    }
}

/// Who did something: the accountable human owner, and the agent acting for
/// them (`None` when the human acted directly).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
    pub agent: Option<String>,
}

impl Identity {
    /// The owner, e.g. `Ada <ada@example.com>`.
    pub fn owner(&self) -> String {
        match (self.name.is_empty(), self.email.is_empty()) {
            (false, false) => format!("{} <{}>", self.name, self.email),
            (false, true) => self.name.clone(),
            (true, false) => self.email.clone(),
            (true, true) => "unknown".into(),
        }
    }

    /// Short label: the agent if any, else the owner's name.
    pub fn short(&self) -> String {
        match &self.agent {
            Some(a) => a.clone(),
            None if !self.name.is_empty() => self.name.clone(),
            None => self.owner(),
        }
    }

    /// Full label, e.g. `claude-1 for Ada <ada@example.com>`.
    pub fn full(&self) -> String {
        match &self.agent {
            Some(a) => format!("{a} for {}", self.owner()),
            None => self.owner(),
        }
    }
}

/// Structured metadata attached to a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeMeta {
    /// What the change is meant to accomplish (the "commit message").
    pub intent: String,
    /// Task this change contributes to, if any.
    pub task: Option<String>,
    /// Free-form key/value metadata: model, session, tool, etc.
    pub extra: BTreeMap<String, String>,
    /// Verification evidence: checks that ran against this change's tree.
    pub checks: Vec<Check>,
}

/// A check (e.g. `cargo test`) that ran against an exact tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// The tree the check ran against.
    pub tree: Hash,
    pub time_ms: i64,
    pub by: Identity,
}

pub type Leaf = (EntryKind, Hash);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides edited the same lines; the file contains conflict markers.
    Content,
    /// Both sides changed a binary file (or symlink); ours is kept on disk.
    Binary,
    /// One side deleted the file, the other modified it; the modified
    /// version is kept on disk.
    DeleteModify,
    /// A file on one side, a directory on the other; ours is kept.
    FileDirectory,
}

/// An unresolved merge conflict recorded on a working change. Conflicts never
/// block other agents; they must be resolved before committing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub kind: ConflictKind,
    pub base: Option<Leaf>,
    pub ours: Option<Leaf>,
    pub theirs: Option<Leaf>,
    /// Blob written to the working copy for this path when the conflict arose.
    pub written: Option<Hash>,
}

/// One immutable state of a change. Amending a change creates a new revision
/// with the same `change_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub change_id: ChangeId,
    pub tree: Hash,
    pub parents: Vec<Hash>,
    pub meta: ChangeMeta,
    /// Who created the change.
    pub author: Identity,
    /// Who produced this revision of it (last snapshot, describe or commit).
    pub committer: Identity,
    /// Unresolved conflicts present in this revision's tree.
    pub conflicts: Vec<Conflict>,
    /// 1 + the largest parent generation (0 = unknown, for revisions written
    /// before generations existed). Ancestry queries stop early with it.
    pub generation: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// The repository state recorded by an operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// Workspace name -> revision checked out there (its working change).
    pub workspaces: BTreeMap<String, Hash>,
    /// Shared named pointers, e.g. `main`.
    pub bookmarks: BTreeMap<String, Hash>,
    /// Task id -> current version of the task.
    pub tasks: BTreeMap<String, Hash>,
    /// Active (possibly expired, pruned lazily) claims on paths: this
    /// repository's, and those fetched from other repositories (`home`).
    pub claims: Vec<Claim>,
    /// When each repository's claims or workspaces last changed, by
    /// repository id (`""`: this one). A newer set of a repository's claims
    /// and workspace heads replaces an older one, so releases travel too
    /// (see `remote`).
    pub claim_stamps: BTreeMap<String, i64>,
    /// Named revisions (`psync rev`): name -> revision; a name never moves.
    pub revs: BTreeMap<String, Hash>,
    /// Workspaces of other repositories, as last heard through remotes.
    pub remote_heads: Vec<RemoteHead>,
    /// Workspace -> the working change it left with `switch` (`switch -`
    /// returns to it).
    pub parked: BTreeMap<String, Hash>,
}

/// Another repository's workspace and its working change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteHead {
    pub workspace: String,
    pub rev: Hash,
    pub home: ClaimHome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskStatus {
    Open,
    InProgress,
    Done,
    /// No longer needed; counts as finished for dependents.
    Cancelled,
}

impl TaskStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Cancelled)
    }

    pub fn name(self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskOwner {
    pub workspace: String,
    pub by: Identity,
    pub since_ms: i64,
}

/// A unit of work. Immutable: every update creates a new version that links
/// to the previous one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Acceptance check: a command that must pass on main for `task done`.
    pub check: Option<String>,
    /// Tasks that must be done before this one can start.
    pub after: Vec<String>,
    pub status: TaskStatus,
    pub owner: Option<TaskOwner>,
    /// Changes landed on main for this task.
    pub landed: Vec<ChangeId>,
    /// Evidence recorded when the task was completed.
    pub completion: Option<Check>,
    pub created_by: Identity,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub previous: Option<Hash>,
    /// Versions merged into this one when the task changed in two
    /// repositories (see `remote`).
    pub merged_from: Vec<Hash>,
}

/// A soft lock: a workspace announces it is working on matching paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// A path, a directory (`src/auth/`), or a glob (`src/**/*.rs`).
    pub pattern: String,
    pub workspace: String,
    pub by: Identity,
    pub task: Option<String>,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// The repository the claim was made in, if not this one.
    pub home: Option<ClaimHome>,
}

/// A repository claims come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimHome {
    /// Random id of the repository (`.psync/id`).
    pub id: String,
    /// Where it is, for people: `host:/path`.
    pub place: String,
}

impl Claim {
    /// Whether this is workspace `ws`'s claim in this repository.
    pub fn is_of(&self, ws: &str) -> bool {
        self.home.is_none() && self.workspace == ws
    }

    /// `workspace W`, or `workspace W at host:/path` for another repository's.
    pub fn location(&self) -> String {
        match &self.home {
            Some(h) => format!("workspace {} at {}", self.workspace, h.place),
            None => format!("workspace {}", self.workspace),
        }
    }
}

/// An entry in the operation log. Every state change of the repository is an
/// operation, so every state change can be undone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub parent: Option<Hash>,
    pub view: View,
    pub time_ms: i64,
    pub actor: Identity,
    /// The workspace whose state this operation changed.
    pub workspace: Option<String>,
    pub description: String,
    /// Set when this operation undoes another operation.
    pub undo_of: Option<Hash>,
    /// Shared state this operation changed, e.g. `main`, `task:T-3`, `claims`.
    /// Such changes are never reverted by `undo`.
    pub shared: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

crate::json_enum!(EntryKind { File = "File", Exec = "Exec", Symlink = "Symlink", Tree = "Tree" });
crate::json_enum!(ConflictKind {
    Content = "content",
    Binary = "binary",
    DeleteModify = "delete_modify",
    FileDirectory = "file_directory",
});
crate::json_enum!(TaskStatus { Open = "open", InProgress = "in_progress", Done = "done", Cancelled = "cancelled" });
crate::json_struct!(Identity { name #default, email #default, agent #default ?Option::is_none });
crate::json_struct!(ChangeMeta { intent #default, task #default, extra #default, checks #default ?Vec::is_empty });
crate::json_struct!(Check { command, passed, exit_code, duration_ms, tree, time_ms, by });
crate::json_struct!(Conflict { path, kind, base, ours, theirs, written });
crate::json_struct!(Revision {
    change_id,
    tree,
    parents #default,
    meta #default,
    author #default,
    committer #default,
    conflicts #default ?Vec::is_empty,
    generation #default ?is_zero,
    created_ms,
    updated_ms,
});
crate::json_struct!(View {
    workspaces #default,
    bookmarks #default ?BTreeMap::is_empty,
    tasks #default ?BTreeMap::is_empty,
    claims #default ?Vec::is_empty,
    claim_stamps #default ?BTreeMap::is_empty,
    revs #default ?BTreeMap::is_empty,
    remote_heads #default ?Vec::is_empty,
    parked #default ?BTreeMap::is_empty,
});
crate::json_struct!(RemoteHead { workspace, rev, home });
crate::json_struct!(TaskOwner { workspace, by, since_ms });
crate::json_struct!(Task {
    id,
    title,
    description #default,
    check #default,
    after #default,
    status,
    owner #default,
    landed #default,
    completion #default,
    created_by,
    created_ms,
    updated_ms,
    previous #default,
    merged_from #default ?Vec::is_empty,
});
crate::json_struct!(Claim { pattern, workspace, by, task #default, created_ms, expires_ms, home #default ?Option::is_none });
crate::json_struct!(ClaimHome { id, place });
crate::json_struct!(Operation {
    parent,
    view,
    time_ms,
    actor #default,
    workspace #default,
    description,
    undo_of #default,
    shared #default @"bookmarks" ?Vec::is_empty,
});

/// A large file's contents: its chunks (blobs) in order. A tree entry for
/// a file refers to either a blob or one of these.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chunked {
    /// The file's size (the sum of the chunks' lengths).
    pub size: u64,
    pub chunks: Vec<(Hash, u64)>,
}

impl Chunked {
    /// Encoding: size, chunk count, then per chunk its hash and length.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = crate::bin::Writer::default();
        w.u64(self.size);
        w.len(self.chunks.len());
        for (h, len) in &self.chunks {
            w.hash(h);
            w.u64(*len);
        }
        w.buf
    }

    pub fn decode(data: &[u8]) -> Result<Chunked> {
        let mut r = crate::bin::Reader::new(data);
        let size = r.u64()?;
        let n = r.len(33)?;
        let mut chunks = Vec::with_capacity(n);
        for _ in 0..n {
            chunks.push((r.hash()?, r.u64()?));
        }
        r.finish()?;
        if chunks.iter().try_fold(0u64, |t, (_, l)| t.checked_add(*l)) != Some(size) {
            bail!("corrupt chunk list: lengths do not add up to the size");
        }
        Ok(Chunked { size, chunks })
    }
}

pub enum Object {
    Blob(Vec<u8>),
    Chunked(Chunked),
    Tree(Tree),
    Revision(Revision),
    Operation(Operation),
    Task(Task),
}

impl Object {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let (tag, body) = match self {
            Object::Blob(b) => {
                let mut v = Vec::with_capacity(b.len() + 2);
                v.extend_from_slice(&[FORMAT_VERSION, TAG_BLOB]);
                v.extend_from_slice(b);
                return Ok(v);
            }
            Object::Chunked(x) => (TAG_CHUNKED, x.encode()),
            Object::Tree(x) => (TAG_TREE, x.encode()),
            Object::Revision(x) => (TAG_REVISION, crate::json::to_vec(x)?),
            Object::Operation(x) => (TAG_OPERATION, crate::json::to_vec(x)?),
            Object::Task(x) => (TAG_TASK, crate::json::to_vec(x)?),
        };
        let mut v = Vec::with_capacity(body.len() + 2);
        v.extend_from_slice(&[FORMAT_VERSION, tag]);
        v.extend_from_slice(&body);
        Ok(v)
    }

    pub fn decode(bytes: &[u8]) -> Result<Object> {
        if bytes.len() < 2 {
            bail!("corrupt object: too short");
        }
        if bytes[0] != FORMAT_VERSION {
            bail!("unsupported object format version {} (expected {FORMAT_VERSION})", bytes[0]);
        }
        let body = &bytes[2..];
        Ok(match bytes[1] {
            TAG_BLOB => Object::Blob(body.to_vec()),
            TAG_CHUNKED => Object::Chunked(Chunked::decode(body)?),
            TAG_TREE => Object::Tree(Tree::decode(body)?),
            TAG_REVISION => Object::Revision(crate::json::from_slice(body)?),
            TAG_OPERATION => Object::Operation(crate::json::from_slice(body)?),
            TAG_TASK => Object::Task(crate::json::from_slice(body)?),
            t => bail!("corrupt object: unknown tag {t}"),
        })
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Object::Blob(_) => "blob",
            Object::Chunked(_) => "chunked file",
            Object::Tree(_) => "tree",
            Object::Revision(_) => "revision",
            Object::Operation(_) => "operation",
            Object::Task(_) => "task",
        }
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    /// Tree bytes are part of every tree's hash: this layout must not change
    /// without a new FORMAT_VERSION.
    #[test]
    fn tree_layout_is_fixed() {
        let t = Tree {
            entries: vec![
                TreeEntry { name: "a".into(), kind: EntryKind::File, hash: Hash([1; 32]) },
                TreeEntry { name: "é".into(), kind: EntryKind::Tree, hash: Hash([2; 32]) },
            ],
        };
        let mut want = vec![2, 1, b'a', 0];
        want.extend([1; 32]);
        want.extend([2, 0xc3, 0xa9, 3]);
        want.extend([2; 32]);
        assert_eq!(t.encode(), want);
        assert_eq!(Tree::decode(&want).unwrap(), t);
        // Truncated, trailing or unknown-kind data is rejected.
        assert!(Tree::decode(&want[..want.len() - 1]).is_err());
        assert!(Tree::decode(&[want.as_slice(), &[0]].concat()).is_err());
        let mut bad = want.clone();
        bad[3] = 9;
        assert!(Tree::decode(&bad).is_err());
    }
}
