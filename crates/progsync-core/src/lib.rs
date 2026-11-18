//! psync core engine.
//!
//! Layers (bottom-up):
//! - [`hash`]: content addresses (BLAKE3) and stable change ids
//! - [`object`]: immutable objects — blobs, trees, revisions, operations
//! - [`chunk`]: content-defined chunking of large files
//! - [`eol`]: opt-in line-ending conversion
//! - [`lines`]: revs, other repositories' workspaces, `switch`
//! - [`store`]: content-addressed, zstd-compressed object store ([`pack`]: pack files)
//! - [`wc`]: working-copy scanning (stat-cached, parallel) and checkout
//! - [`diff`]: tree and content diffs ([`linediff`]: Myers line diff)
//! - [`merge`]: three-way merge of text and trees
//! - [`semantic`]: structure-aware merge of source files (tree-sitter)
//! - [`repo`]: the repository — workspaces, changes, operation log, undo
//! - [`gc`]: integrity check and garbage collection
//! - [`coord`]: coordination — tasks and claims
//! - [`remote`]: syncing main, tasks, claims, revs and workspace heads with other repositories
//! - [`transport`]: talking to other repositories (local paths, ssh)
//! - [`paths`]: path patterns (paths, directories, globs)
//! - [`platform`]: Unix/Windows differences
//! - [`config`]: settings and owner identity
//! - [`json`]: JSON values, parser, writer, conversions
//! - [`gitbridge`]: import/export of Git history
//! - [`watch`]: file watcher daemon (changed paths, live snapshots)

pub mod archive;
pub mod batch;
pub mod bin;
pub mod chunk;
pub mod eol;
pub mod lines;
pub mod localtime;
pub mod msgcheck;
pub mod queue;
pub mod config;
pub mod coord;
pub mod diff;
pub mod error;
pub mod gc;
pub mod gitbridge;
pub mod glob;
pub mod hash;
pub mod ignores;
pub mod json;
pub mod linediff;
pub mod merge;
pub mod object;
pub mod pack;
pub mod paths;
pub mod platform;
pub mod remote;
pub mod repo;
pub mod semantic;
pub mod store;
pub mod transport;
pub mod util;
pub mod wc;
pub mod watch;

pub use hash::{ChangeId, Hash};
pub use object::{
    ChangeMeta, Check, Claim, Conflict, ConflictKind, EntryKind, Identity, Operation, Revision, Task, TaskStatus, Tree,
    TreeEntry, View,
};
pub use repo::{MetaUpdate, Repo};
