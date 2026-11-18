mod args;
mod http;
mod mcp;

use progsync_core::bail;
use progsync_core::error::Result;
use progsync_core::config;
use progsync_core::coord::{parse_ttl, TaskDone, TaskEdit, TaskInfo};
use progsync_core::diff::{self, FileChange};
use progsync_core::repo::{overlap_map, LandResult, Status, SyncOutcome};
use progsync_core::util::{relative, rfc3339};
use progsync_core::watch::{self, DaemonOptions, DaemonStatus};
use progsync_core::{Claim, Conflict, Hash, Identity, MetaUpdate, Operation, Repo, Revision};
use progsync_core::json;
use progsync_core::json::Value;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Version control built for AI agents.
///
/// No staging area: the working directory is always the working change (@)
/// and is snapshotted automatically by every command. Each agent works in its
/// own workspace; `psync context` shows what everyone else is doing.
struct Cli {
    /// Machine-readable JSON output
    json: bool,

    /// Agent acting for the owner (omit when a human acts directly)
    agent: Option<String>,

    /// Path inside the repository [default: current directory]
    dir: Option<PathBuf>,

    cmd: Cmd,
}

enum Cmd {
    /// Create a repository (current contents become the first change)
    Init { path: Option<PathBuf> },
    /// Show the working change and its changed files
    Status { ignored: bool },
    /// Which ignore rule (if any) applies to each path
    CheckIgnore { paths: Vec<String> },
    /// Finish the working change and start a new one on top
    Commit(CommitArgs),
    /// Set intent/task/metadata of the working change (or, with -r, of a committed change not yet on main)
    Describe { meta: MetaArgs, rev: String },
    /// Show change history
    Log {
        limit: usize,
        /// Include every workspace's history
        all: bool,
        /// One block per change: author, date, task, checks, files, full intent
        long: bool,
    },
    /// Show a change: metadata and diff
    Show {
        rev: String,
        /// Only list files with line counts
        stat: bool,
    },
    /// Show the changes introduced by a change (default: working change)
    Diff {
        rev: String,
        stat: bool,
        name_only: bool,
    },
    /// Rebase your work onto the latest main (brings in others' landed work)
    Sync,
    /// Put your committed changes onto main, after an optional check (with workspaces: theirs too, under one check)
    Land {
        /// Command that must pass, run against exactly what lands [default: config land.check]
        check: Option<String>,
        /// Skip the configured land.check
        no_check: bool,
        /// Other workspaces whose committed changes land with yours, e.g. claude-1@ codex-2@laptop
        workspaces: Vec<String>,
    },
    /// Merge another change into your working change, e.g. `psync merge claude-1@`
    Merge { rev: String },
    /// Undo a change (e.g. a landed one) with a new change; then commit and land it
    Revert { rev: String },
    /// Make paths in the working change as they are in a revision
    Restore { rev: String, paths: Vec<String> },
    /// Print a file's exact contents at a revision
    Cat { spec: String, path: Option<String> },
    /// A revision's files as a tar
    Archive { rev: String, paths: Vec<String>, prefix: Option<String>, output: Option<PathBuf> },
    /// Point this workspace at another line of work (`-`: back)
    Switch { rev: String },
    /// Named revisions
    Rev(RevCmd),
    /// List conflicts in the working change, or mark one resolved
    Resolve {
        path: Option<String>,
        /// Accept the file even if it still has conflict markers
        force: bool,
    },
    /// Tasks: plan work, take it, finish it
    Task(TaskCmd),
    /// Claim paths you are working on (no arguments: list active claims)
    Claim {
        /// Paths, directories (src/auth/), globs (src/**/*.rs), or one declaration of a source file (src/auth.rs::login)
        patterns: Vec<String>,
        /// How long the claim lasts, e.g. 30m, 2h, 1d
        ttl: String,
        /// Claim even if another workspace claims an overlapping path
        force: bool,
    },
    /// Release your claims (no arguments: all of them)
    Release { patterns: Vec<String> },
    /// Briefing for agents: your work, other workspaces, overlapping files
    Context {
        /// Number of recent changes to include
        recent: usize,
    },
    /// Manage workspaces (one per agent)
    Workspace(WsCmd),
    /// Undo this workspace's last operation (repeat to go further back)
    Undo,
    /// Record the working directory now (normally automatic)
    Snapshot,
    /// Operation log: every repository state, restorable
    Op(OpCmd),
    /// Verify the repository: every object it refers to is present and intact
    Check,
    /// The last commit and whether files differ from it (for build scripts)
    Id { paths: Vec<String> },
    /// Remove unreachable objects and leftover temp files
    Gc {
        /// Also forget operations older than this (e.g. 30d)
        keep: Option<String>,
        dry_run: bool,
        /// Only pack loose objects, if there are many (run in the background)
        auto: bool,
    },
    /// Get or set configuration (user.name, user.email, land.check, land.queue, commit.check, commit.max_file_mb, watch.auto, text.eol)
    Config(ConfigArgs),
    /// File watcher for this workspace: faster commands, live snapshots
    Daemon(DaemonCmd),
    /// Serve psync as MCP tools over stdio (for agent clients)
    Mcp,
    /// Manage remotes (other psync repositories to share main, tasks, claims, revs and workspaces with)
    Remote(RemoteCmd),
    /// Copy a remote's main (as <remote>/main), tasks, claims, revs and workspaces, without moving main
    Fetch {
        remote: String,
    },
    /// Fetch, then bring main up to the remote's main
    Pull {
        remote: String,
    },
    /// Send main, tasks, claims, revs and workspaces to a remote (main only as a fast-forward)
    Push {
        remote: String,
    },
    /// Serve a repository to a remote psync over stdin/stdout (run by ssh)
    RemoteServe { path: PathBuf },
    /// Bridge to Git: import a Git history, export main as Git commits
    Git(GitCmd),
    /// Create a repository from another one, with origin set to it
    Clone {
        /// Repository to clone: a path, ssh://[user@]host/path or host:path
        source: String,
        /// Directory to create [default: the source's name, here]
        dest: Option<PathBuf>,
    },
}

enum GitCmd {
    /// Import a Git branch's history into main (continues a previous import)
    Import {
        /// Path of the Git repository
        path: PathBuf,
        branch: String,
    },
    /// Export main as commits on a Git branch (fast-forward only)
    Export {
        /// Path of the Git repository (created with `git init` if needed)
        path: PathBuf,
        branch: String,
        /// Skip the commit.check message check
        no_commit_check: bool,
    },
}

enum RevCmd {
    /// Name a revision for good (with --remote: also create it there)
    Add { name: String, rev: String, remote: Option<String> },
    /// List revs
    List,
    /// Forget a rev here
    Remove { name: String },
}

enum RemoteCmd {
    /// Add a remote: a path to another psync repository
    Add { name: String, url: String },
    /// List remotes
    List,
    /// Remove a remote
    Remove { name: String },
}

enum DaemonCmd {
    /// Start the watcher in the background
    Start {
        /// Only track changed paths; do not snapshot when edits settle
        no_snapshot: bool,
        /// Also serve the HTTP API [default address: 127.0.0.1, any free port]
        http: Option<String>,
    },
    /// Stop the watcher
    Stop,
    /// Show whether a watcher is running
    Status,
    /// Run the watcher in the foreground
    Run {
        no_snapshot: bool,
        http: Option<String>,
    },
}

enum WsCmd {
    /// Create a workspace with its own working change
    Add {
        name: String,
        /// Directory to create [default: ../<repo>-<name>]
        path: Option<PathBuf>,
        /// Revision to start from [default: main, else @-]
        rev: Option<String>,
    },
    /// List workspaces
    List,
    /// Remove a workspace and delete its directory (its work stays in history)
    Remove { name: String },
}

enum TaskCmd {
    /// Create a task
    Add {
        title: String,
        /// Task id [default: next T-<n>]
        id: Option<String>,
        description: String,
        /// Acceptance check: must pass on main for `task done`
        check: Option<String>,
        /// Tasks that must be done first (comma-separated)
        after: Vec<String>,
    },
    /// List tasks (not done ones, unless --all)
    List {
        all: bool,
    },
    /// Show one task
    Show { id: String },
    /// Take a task in this workspace and point your working change at it
    Start {
        id: String,
        force: bool,
    },
    /// Give a task back (it becomes open again)
    Drop {
        id: String,
        force: bool,
    },
    /// Finish a task (runs its acceptance check against main)
    Done { id: String },
    /// Change a task's title, description, acceptance check or dependencies
    Edit {
        id: String,
        title: Option<String>,
        description: Option<String>,
        /// New acceptance check ("" removes it)
        check: Option<String>,
        /// New dependency list, comma-separated ("" clears it)
        after: Option<String>,
    },
    /// Mark a task as no longer needed (unblocks tasks that wait for it)
    Cancel { id: String },
}

enum OpCmd {
    /// List operations, newest first
    Log {
        limit: usize,
    },
    /// Restore this workspace to its state at an operation
    Restore { op: String },
}

struct ConfigArgs {
    /// Use the global config instead of the repository's
    global: bool,
    /// Remove the key
    unset: bool,
    /// List effective settings
    list: bool,
    key: Option<String>,
    value: Option<String>,
}

struct CommitArgs {
    meta: MetaArgs,
    /// Only commit changes under these paths/globs (the rest stays in @)
    paths: Vec<String>,
    /// Skip the commit.max_file_mb guard
    allow_large: bool,
}

struct MetaArgs {
    /// What the change does (its intent)
    message: Option<String>,
    /// Task the change contributes to ("" clears)
    task: Option<String>,
    /// Extra metadata, e.g. --meta model=opus --meta session=abc ("k=" clears)
    meta: Vec<(String, String)>,
}

impl MetaArgs {
    fn update(&self) -> MetaUpdate {
        MetaUpdate { intent: self.message.clone(), task: self.task.clone(), extra: self.meta.clone() }
    }
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected KEY=VALUE, got {s:?}")),
    }
}

// ---- command line ---------------------------------------------------------------

/// The command-line interface, for parsing and help (see `args`).
fn spec() -> args::Cmd {
    use args::{Arg, Cmd as C};
    let meta_args = |c: C| {
        c.arg(Arg::opt("message").short('m').alias(&["intent"]).help("What the change does (its intent)"))
            .arg(Arg::opt("task").short('t').help("Task the change contributes to (\"\" clears)"))
            .arg(Arg::multi("meta").value_name("KEY=VALUE").help("Extra metadata, e.g. --meta model=opus --meta session=abc (\"k=\" clears)"))
    };
    let http = || {
        Arg::opt("http")
            .value_name("ADDR")
            .missing("127.0.0.1:0")
            .help("Also serve the HTTP API [default address: 127.0.0.1, any free port]")
    };
    let remote = |name: &'static str, about: &'static str| C::new(name, about).arg(Arg::pos("remote").default("origin"));
    C::new("psync", "Version control built for AI agents.")
        .long_about(
            "No staging area: the working directory is always the working change (@) and is snapshotted automatically by every command. Each agent works in its own workspace; `psync context` shows what everyone else is doing.",
        )
        .arg(Arg::flag("json").global().help("Machine-readable JSON output"))
        .arg(Arg::opt("agent").global().env("PSYNC_AGENT").help("Agent acting for the owner (omit when a human acts directly)"))
        .arg(Arg::opt("dir").short_only('C').global().value_name("PATH").help("Path inside the repository [default: current directory]"))
        .sub(C::new("init", "Create a repository (current contents become the first change)").arg(Arg::pos_opt("path")))
        .sub(C::new("status", "Show the working change and its changed files").alias(&["st"])
            .arg(Arg::flag("ignored").help("Also list ignored files and directories (what a commit leaves out)")))
        .sub(C::new("check-ignore", "Show whether paths are ignored, and which rule (file and line) decides")
            .arg(Arg::pos_multi("paths").missing("name at least one path")))
        .sub(meta_args(C::new("commit", "Finish the working change and start a new one on top"))
            .arg(Arg::pos_multi("paths").help("Only commit changes under these paths/globs (the rest stays in @)"))
            .arg(Arg::flag("allow_large").help("Commit files above commit.max_file_mb anyway")))
        .sub(meta_args(C::new("describe", "Set intent/task/metadata of the working change (or, with -r, of a committed change not yet on main)"))
            .arg(Arg::opt("rev").short('r').default("@").help("The change to describe: @, or one of your committed changes not yet on main")))
        .sub(C::new("log", "Show change history")
            .arg(Arg::opt("limit").short('n').default("20"))
            .arg(Arg::flag("all").help("Include every workspace's history"))
            .arg(Arg::flag("long").short('l').help("Expanded view: author, date, task, checks, files and the full intent of each change")))
        .sub(C::new("show", "Show a change: metadata and diff")
            .arg(Arg::pos("rev").default("@"))
            .arg(Arg::flag("stat").help("Only list files with line counts")))
        .sub(C::new("diff", "Show the changes introduced by a change (default: working change)")
            .arg(Arg::pos("rev").default("@"))
            .arg(Arg::flag("stat"))
            .arg(Arg::flag("name_only")))
        .sub(C::new("sync", "Rebase your work onto the latest main (brings in others' landed work)"))
        .sub(C::new("land", "Put your committed changes onto main, after an optional check (with workspaces: theirs too, under one check)")
            .arg(Arg::pos_multi("workspaces").help("Other workspaces whose committed changes land with yours, under one check, e.g. claude-1@ codex-2@laptop"))
            .arg(Arg::opt("check").help("Command that must pass, run against exactly what lands [default: config land.check]"))
            .arg(Arg::flag("no_check").conflicts("check").help("Skip the configured land.check")))
        .sub(C::new("merge", "Merge another change into your working change, e.g. `psync merge claude-1@`").arg(Arg::pos("rev")))
        .sub(C::new("revert", "Undo a change (e.g. a landed one) with a new change; then commit and land it").arg(Arg::pos("rev")))
        .sub(C::new("restore", "Bring files or directories back as they are in a revision, e.g. `psync restore main -- src/a.rs`")
            .long_about("Make the given paths (files, directories or globs) in your working change exactly as they are in <rev>: files are written back byte for byte, and files under those paths that <rev> does not have are removed. The rest of your working change is untouched. Edits it overwrites were snapshotted first, so `psync undo` brings them back.")
            .arg(Arg::pos("rev"))
            .arg(Arg::pos_multi("paths").help("Files, directories or globs to restore")))
        .sub(C::new("archive", "Write a revision's files as a tar, e.g. `psync archive main -o app.tar` (to build or deploy exactly that revision)")
            .long_about("Write exactly the files of <rev> (no untracked or uncommitted files) as a tar, to -o FILE or to stdout. Executable bits and symlinks are kept; the order and timestamps are fixed, so the same revision always gives the same bytes. The revision hash is recorded as the tar's pax `comment` (as git archive does) and reported; `--json` needs -o.")
            .arg(Arg::pos("rev"))
            .arg(Arg::pos_multi("paths").help("Only these files, directories or globs"))
            .arg(Arg::opt("prefix").help("Put every path under this directory, e.g. app-1.2/"))
            .arg(Arg::opt("output").short('o').value_name("FILE").help("Write the tar here [default: stdout]")))
        .sub(C::new("cat", "Print a file's exact contents at a revision: `psync cat <rev>:<path>` (or `<rev> <path>`)")
            .arg(Arg::pos("spec").value_name("REV:PATH"))
            .arg(Arg::pos_opt("path")))
        .sub(C::new("switch", "Point this workspace at another line of work, e.g. `psync switch claude-1@laptop` (`-`: back)")
            .long_about("Point this workspace at another line of work: another workspace (`claude-1@`, `claude-1@laptop` for another repository's, as of the last fetch), main, a rev, or any revision. You get a new working change on top of it (on a copy, for unfinished work) and its files. The working change you leave is kept: `psync switch -` returns to it.")
            .arg(Arg::pos("rev")))
        .sub(C::new("rev", "Named revisions: name one for good, to test or continue it anywhere")
            .sub(C::new("add", "Name a revision (a rev never moves)")
                .arg(Arg::pos("name"))
                .arg(Arg::pos("rev").default("@").help("Revision to name, e.g. claude-1@, @-, main"))
                .arg(Arg::opt("remote").help("Also create it in this remote, sending the revision (main is not touched)")))
            .sub(C::new("list", "List revs").alias(&["ls"]))
            .sub(C::new("remove", "Forget a rev here (remotes keep theirs)").alias(&["rm"]).arg(Arg::pos("name"))))
        .sub(C::new("resolve", "List conflicts in the working change, or mark one resolved")
            .arg(Arg::pos_opt("path"))
            .arg(Arg::flag("force").help("Accept the file even if it still has conflict markers")))
        .sub(C::new("task", "Tasks: plan work, take it, finish it")
            .sub(C::new("add", "Create a task")
                .arg(Arg::pos("title"))
                .arg(Arg::opt("id").help("Task id [default: next T-<n>]"))
                .arg(Arg::opt("description").short('d').default(""))
                .arg(Arg::opt("check").help("Acceptance check: must pass on main for `task done`"))
                .arg(Arg::multi("after").delimiter(',').help("Tasks that must be done first (comma-separated)")))
            .sub(C::new("list", "List tasks (not done ones, unless --all)").alias(&["ls"]).arg(Arg::flag("all")))
            .sub(C::new("show", "Show one task").arg(Arg::pos("id")))
            .sub(C::new("start", "Take a task in this workspace and point your working change at it").arg(Arg::pos("id")).arg(Arg::flag("force")))
            .sub(C::new("drop", "Give a task back (it becomes open again)").arg(Arg::pos("id")).arg(Arg::flag("force")))
            .sub(C::new("done", "Finish a task (runs its acceptance check against main)").arg(Arg::pos("id")))
            .sub(C::new("edit", "Change a task's title, description, acceptance check or dependencies")
                .arg(Arg::pos("id"))
                .arg(Arg::opt("title"))
                .arg(Arg::opt("description").short('d'))
                .arg(Arg::opt("check").help("New acceptance check (\"\" removes it)"))
                .arg(Arg::opt("after").help("New dependency list, comma-separated (\"\" clears it)")))
            .sub(C::new("cancel", "Mark a task as no longer needed (unblocks tasks that wait for it)").arg(Arg::pos("id"))))
        .sub(C::new("claim", "Claim paths you are working on (no arguments: list active claims)")
            .arg(Arg::pos_multi("patterns").help("Paths, directories (src/auth/), globs (src/**/*.rs), or one declaration of a source file (src/auth.rs::login)"))
            .arg(Arg::opt("ttl").default("30m").help("How long the claim lasts, e.g. 30m, 2h, 1d"))
            .arg(Arg::flag("force").help("Claim even if another workspace claims an overlapping path")))
        .sub(C::new("release", "Release your claims (no arguments: all of them)").arg(Arg::pos_multi("patterns")))
        .sub(C::new("context", "Briefing for agents: your work, other workspaces, overlapping files")
            .arg(Arg::opt("recent").short('n').default("5").help("Number of recent changes to include")))
        .sub(C::new("workspace", "Manage workspaces (one per agent)")
            .alias(&["ws"])
            .sub(C::new("add", "Create a workspace with its own working change")
                .arg(Arg::pos("name"))
                .arg(Arg::opt("path").help("Directory to create [default: ../<repo>-<name>]"))
                .arg(Arg::opt("rev").short('r').help("Revision to start from [default: main, else @-]")))
            .sub(C::new("list", "List workspaces").alias(&["ls"]))
            .sub(C::new("remove", "Remove a workspace and delete its directory (its work stays in history)").alias(&["rm"]).arg(Arg::pos("name"))))
        .sub(C::new("undo", "Undo this workspace's last operation (repeat to go further back)"))
        .sub(C::new("snapshot", "Record the working directory now (normally automatic)"))
        .sub(C::new("op", "Operation log: every repository state, restorable")
            .sub(C::new("log", "List operations, newest first").arg(Arg::opt("limit").short('n').default("20")))
            .sub(C::new("restore", "Restore this workspace to its state at an operation").arg(Arg::pos("op"))))
        .sub(C::new("check", "Verify the repository: every object it refers to is present and intact"))
        .sub(C::new("id", "Which revision this tree is: the last commit, and whether files differ from it (`-dirty`)")
            .long_about("Print the last commit of this workspace (the working change's parent), with `-dirty` if files differ from it; with paths, only changes under them count. `--json` gives revision, change_id, intent, dirty, changed and on_main. Records nothing and takes no lock, so build scripts can run it on every build.")
            .arg(Arg::pos_multi("paths").help("Only changes under these paths count")))
        .sub(C::new("gc", "Remove unreachable objects and leftover temp files")
            .long_about("Remove unreachable objects and leftover temp files.\n\nOperation history is kept in full unless --keep is given; then operations older than that are forgotten (undo and op restore cannot go back further), along with what only they refer to, such as intermediate snapshots. The current state and all change history stay. Unreachable objects less than a day old are always kept: a running command may still need them.")
            .arg(Arg::opt("keep").value_name("AGE").help("Forget operations older than AGE, e.g. 30d, 2w, 12h"))
            .arg(Arg::flag("dry_run").help("Show what would be removed, without removing anything"))
            .arg(Arg::flag("auto").help("Only move loose objects into packs, if there are many (commands start this in the background)")))
        .sub(C::new("config", "Get or set configuration (user.name, user.email, land.check, land.queue, commit.check, commit.max_file_mb, watch.auto, text.eol)")
            .arg(Arg::flag("global").help("Use the global config instead of the repository's"))
            .arg(Arg::flag("unset").help("Remove the key"))
            .arg(Arg::flag("list").short('l').help("List effective settings"))
            .arg(Arg::pos_opt("key"))
            .arg(Arg::pos_opt("value")))
        .sub(C::new("daemon", "File watcher for this workspace: faster commands, live snapshots")
            .sub(C::new("start", "Start the watcher in the background")
                .arg(Arg::flag("no_snapshot").help("Only track changed paths; do not snapshot when edits settle"))
                .arg(http()))
            .sub(C::new("stop", "Stop the watcher"))
            .sub(C::new("status", "Show whether a watcher is running"))
            .sub(C::new("run", "Run the watcher in the foreground").arg(Arg::flag("no_snapshot")).arg(http())))
        .sub(C::new("mcp", "Serve psync as MCP tools over stdio (for agent clients)"))
        .sub(C::new("remote", "Manage remotes (other psync repositories to share main, tasks, claims, revs and workspaces with)")
            .sub(C::new("add", "Add a remote: a path to another psync repository").arg(Arg::pos("name")).arg(Arg::pos("url")))
            .sub(C::new("list", "List remotes").alias(&["ls"]))
            .sub(C::new("remove", "Remove a remote").alias(&["rm"]).arg(Arg::pos("name"))))
        .sub(remote("fetch", "Copy a remote's main (as <remote>/main), tasks, claims, revs and workspaces, without moving main"))
        .sub(remote("pull", "Fetch, then bring main up to the remote's main"))
        .sub(remote("push", "Send main, tasks, claims, revs and workspaces to a remote (main only as a fast-forward)"))
        .sub(C::new("remote-serve", "Serve a repository to a remote psync over stdin/stdout (run by ssh)").hidden().arg(Arg::pos("path")))
        .sub(C::new("git", "Bridge to Git: import a Git history, export main as Git commits")
            .sub(C::new("import", "Import a Git branch's history into main (continues a previous import)")
                .arg(Arg::pos("path").help("Path of the Git repository"))
                .arg(Arg::opt("branch").short('b').default("main")))
            .sub(C::new("export", "Export main as commits on a Git branch (fast-forward only)")
                .arg(Arg::pos("path").help("Path of the Git repository (created with `git init` if needed)"))
                .arg(Arg::opt("branch").short('b').default("main"))
                .arg(Arg::flag("no_commit_check").help("Export even changes the commit.check message check refuses"))))
        .sub(C::new("clone", "Create a repository from another one, with origin set to it")
            .arg(Arg::pos("source").help("Repository to clone: a path, ssh://[user@]host/path or host:path"))
            .arg(Arg::pos_opt("dest").help("Directory to create [default: the source's name, here]")))
}

impl Cli {
    fn parse() -> Cli {
        Cli::try_parse_from(std::env::args_os().map(|a| a.to_string_lossy().into_owned())).unwrap_or_else(|e| e.exit())
    }

    fn try_parse_from<I: IntoIterator<Item = T>, T: Into<String>>(argv: I) -> Result<Cli, args::Error> {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        let m = args::parse(&spec(), env!("CARGO_PKG_VERSION"), &argv)?;
        Ok(Cli { json: m.flag("json"), agent: m.get("agent"), dir: m.get("dir").map(PathBuf::from), cmd: build(&m)? })
    }
}

/// The command `m` selects, with its arguments.
fn build(m: &args::Matches) -> Result<Cmd, args::Error> {
    let num = |id: &str| -> Result<usize, args::Error> {
        Ok(m.parse(id, |v| v.parse::<usize>().map_err(|e| e.to_string()))?.expect("has a default"))
    };
    let path = |id: &str| m.get(id).map(PathBuf::from);
    let meta = || -> Result<MetaArgs, args::Error> {
        Ok(MetaArgs { message: m.get("message"), task: m.get("task"), meta: m.parse_all("meta", parse_kv)? })
    };
    let path_str: Vec<&str> = m.path.to_vec();
    Ok(match path_str.as_slice() {
        ["init"] => Cmd::Init { path: path("path") },
        ["status"] => Cmd::Status { ignored: m.flag("ignored") },
        ["check-ignore"] => Cmd::CheckIgnore { paths: m.all("paths") },
        ["commit"] => Cmd::Commit(CommitArgs { meta: meta()?, paths: m.all("paths"), allow_large: m.flag("allow_large") }),
        ["describe"] => Cmd::Describe { meta: meta()?, rev: m.req("rev") },
        ["log"] => Cmd::Log { limit: num("limit")?, all: m.flag("all"), long: m.flag("long") },
        ["show"] => Cmd::Show { rev: m.req("rev"), stat: m.flag("stat") },
        ["diff"] => Cmd::Diff { rev: m.req("rev"), stat: m.flag("stat"), name_only: m.flag("name_only") },
        ["sync"] => Cmd::Sync,
        ["land"] => Cmd::Land { check: m.get("check"), no_check: m.flag("no_check"), workspaces: m.all("workspaces") },
        ["merge"] => Cmd::Merge { rev: m.req("rev") },
        ["revert"] => Cmd::Revert { rev: m.req("rev") },
        ["restore"] => Cmd::Restore { rev: m.req("rev"), paths: m.all("paths") },
        ["cat"] => Cmd::Cat { spec: m.req("spec"), path: m.get("path") },
        ["archive"] => Cmd::Archive { rev: m.req("rev"), paths: m.all("paths"), prefix: m.get("prefix"), output: path("output") },
        ["switch"] => Cmd::Switch { rev: m.req("rev") },
        ["rev", sub] => Cmd::Rev(match *sub {
            "add" => RevCmd::Add { name: m.req("name"), rev: m.req("rev"), remote: m.get("remote") },
            "list" => RevCmd::List,
            "remove" => RevCmd::Remove { name: m.req("name") },
            other => unreachable!("rev {other}"),
        }),
        ["resolve"] => Cmd::Resolve { path: m.get("path"), force: m.flag("force") },
        ["task", sub] => Cmd::Task(match *sub {
            "add" => TaskCmd::Add {
                title: m.req("title"),
                id: m.get("id"),
                description: m.req("description"),
                check: m.get("check"),
                after: m.all("after"),
            },
            "list" => TaskCmd::List { all: m.flag("all") },
            "show" => TaskCmd::Show { id: m.req("id") },
            "start" => TaskCmd::Start { id: m.req("id"), force: m.flag("force") },
            "drop" => TaskCmd::Drop { id: m.req("id"), force: m.flag("force") },
            "done" => TaskCmd::Done { id: m.req("id") },
            "edit" => TaskCmd::Edit {
                id: m.req("id"),
                title: m.get("title"),
                description: m.get("description"),
                check: m.get("check"),
                after: m.get("after"),
            },
            "cancel" => TaskCmd::Cancel { id: m.req("id") },
            other => unreachable!("task {other}"),
        }),
        ["claim"] => Cmd::Claim { patterns: m.all("patterns"), ttl: m.req("ttl"), force: m.flag("force") },
        ["release"] => Cmd::Release { patterns: m.all("patterns") },
        ["context"] => Cmd::Context { recent: num("recent")? },
        ["workspace", sub] => Cmd::Workspace(match *sub {
            "add" => WsCmd::Add { name: m.req("name"), path: path("path"), rev: m.get("rev") },
            "list" => WsCmd::List,
            "remove" => WsCmd::Remove { name: m.req("name") },
            other => unreachable!("workspace {other}"),
        }),
        ["undo"] => Cmd::Undo,
        ["snapshot"] => Cmd::Snapshot,
        ["op", "log"] => Cmd::Op(OpCmd::Log { limit: num("limit")? }),
        ["op", "restore"] => Cmd::Op(OpCmd::Restore { op: m.req("op") }),
        ["check"] => Cmd::Check,
        ["id"] => Cmd::Id { paths: m.all("paths") },
        ["gc"] => Cmd::Gc { keep: m.get("keep"), dry_run: m.flag("dry_run"), auto: m.flag("auto") },
        ["config"] => Cmd::Config(ConfigArgs {
            global: m.flag("global"),
            unset: m.flag("unset"),
            list: m.flag("list"),
            key: m.get("key"),
            value: m.get("value"),
        }),
        ["daemon", sub] => Cmd::Daemon(match *sub {
            "start" => DaemonCmd::Start { no_snapshot: m.flag("no_snapshot"), http: m.get("http") },
            "stop" => DaemonCmd::Stop,
            "status" => DaemonCmd::Status,
            "run" => DaemonCmd::Run { no_snapshot: m.flag("no_snapshot"), http: m.get("http") },
            other => unreachable!("daemon {other}"),
        }),
        ["mcp"] => Cmd::Mcp,
        ["remote", sub] => Cmd::Remote(match *sub {
            "add" => RemoteCmd::Add { name: m.req("name"), url: m.req("url") },
            "list" => RemoteCmd::List,
            "remove" => RemoteCmd::Remove { name: m.req("name") },
            other => unreachable!("remote {other}"),
        }),
        ["fetch"] => Cmd::Fetch { remote: m.req("remote") },
        ["pull"] => Cmd::Pull { remote: m.req("remote") },
        ["push"] => Cmd::Push { remote: m.req("remote") },
        ["remote-serve"] => Cmd::RemoteServe { path: PathBuf::from(m.req("path")) },
        ["git", sub] => {
            let (p, branch) = (PathBuf::from(m.req("path")), m.req("branch"));
            Cmd::Git(if *sub == "import" { GitCmd::Import { path: p, branch } } else { GitCmd::Export { path: p, branch, no_commit_check: m.flag("no_commit_check") } })
        }
        ["clone"] => Cmd::Clone { source: m.req("source"), dest: path("dest") },
        other => unreachable!("command {other:?} is in spec() but not built"),
    })
}

// ---- styling ------------------------------------------------------------

static COLOR: OnceLock<bool> = OnceLock::new();
/// `--json`: progress notes (e.g. waiting in the land queue) stay quiet.
static JSON_MODE: OnceLock<bool> = OnceLock::new();

fn paint(code: &str, s: &str) -> String {
    if *COLOR.get().unwrap_or(&false) {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}
fn c_id(s: &str) -> String {
    paint("1;35", s)
}
fn c_hash(s: &str) -> String {
    paint("34", s)
}
fn c_dim(s: &str) -> String {
    paint("2", s)
}
fn c_warn(s: &str) -> String {
    paint("1;33", s)
}
fn c_status(f: &FileChange) -> String {
    let code = match f.status() {
        diff::FileStatus::Added => "32",
        diff::FileStatus::Modified => "33",
        diff::FileStatus::Deleted => "31",
    };
    paint(code, &f.status().letter().to_string())
}
fn short_id(rev: &Revision) -> String {
    c_id(&rev.change_id.short()[..8])
}
fn check_cmd(repo: &Repo) -> Result<Out> {
    let r = progsync_core::gc::check(repo)?;
    let mut text = format!(
        "Checked {} ({}) in {}: {}\n",
        plural(r.objects, "object"),
        size(r.bytes),
        plural(r.operations, "operation"),
        if r.problems.is_empty() { "no problems".to_string() } else { plural(r.problems.len(), "problem") }
    );
    for p in &r.problems {
        text.push_str(&format!("  {p}\n"));
    }
    text.push_str(&format!("Stored in {} and {}\n", plural(r.packs, "pack"), plural(r.loose, "loose object")));
    if r.unreachable > 0 {
        text.push_str(&format!("{} unreachable (`psync gc` removes them once a day old)\n", plural(r.unreachable, "object")));
    }
    Ok(Out {
        json: json!({
            "ok": r.problems.is_empty(),
            "objects": r.objects,
            "bytes": r.bytes,
            "operations": r.operations,
            "unreachable": r.unreachable,
            "packs": r.packs,
            "loose": r.loose,
            "problems": r.problems,
        }),
        text,
    })
}

fn gc_cmd(repo: &Repo, keep: Option<&str>, dry_run: bool) -> Result<Out> {
    let keep_ms = keep.map(progsync_core::coord::parse_ttl).transpose()?;
    let r = progsync_core::gc::gc(repo, keep_ms, dry_run)?;
    let verb = if dry_run { "Would remove" } else { "Removed" };
    let mut text = format!("{verb} {} ({})", plural(r.removed, "unreachable object"), size(r.removed_bytes));
    if r.temp_files > 0 {
        text.push_str(&format!(" and {}", plural(r.temp_files, "temp file")));
    }
    text.push_str(&format!("; {} kept\n", plural(r.kept, "object")));
    if !dry_run {
        text.push_str(&format!(
            "Packed {} into {} (were: {} and {})\n",
            plural(r.packed, "reachable object"),
            plural(r.packs_after, "pack"),
            plural(r.packs_before, "pack"),
            plural(r.loose_before, "loose object")
        ));
    }
    if r.young > 0 {
        text.push_str(&format!("Kept {} that may still be in use (less than a day old)\n", plural(r.young, "unreachable object")));
    }
    if r.forgotten_ops > 0 {
        let what = if dry_run { "Would forget" } else { "Forgot" };
        text.push_str(&format!("{what} {} older than {}\n", plural(r.forgotten_ops, "operation"), keep.unwrap_or("")));
    }
    match r.horizon {
        Some((h, t)) => text.push_str(&format!("Operation history starts at {} ({})\n", c_hash(&h.short()[..12]), progsync_core::util::relative(t))),
        None => text.push_str("Operation history is kept in full (--keep 30d would forget older operations)\n"),
    }
    Ok(Out {
        json: json!({
            "dry_run": dry_run,
            "removed": r.removed,
            "removed_bytes": r.removed_bytes,
            "kept": r.kept,
            "young": r.young,
            "temp_files": r.temp_files,
            "forgotten_ops": r.forgotten_ops,
            "packed": r.packed,
            "packs_before": r.packs_before,
            "packs_after": r.packs_after,
            "loose_before": r.loose_before,
            "horizon": r.horizon.map(|(h, t)| json!({ "op": h.short(), "time": progsync_core::util::rfc3339(t) })),
        }),
        text,
    })
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

// ---- output -------------------------------------------------------------

struct Out {
    json: Value,
    text: String,
}

/// Write `text` to stdout. If the reader has gone (`psync log | head`),
/// stop quietly with exit code `code`, the one the command has anyway.
pub(crate) fn emit(text: &str, code: i32) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(code);
        }
        eprintln!("error: writing output: {e}");
        std::process::exit(1);
    }
}

fn main() {
    let cli = Cli::parse();
    let _ = COLOR.set(!cli.json && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    let json_mode = cli.json;
    let _ = JSON_MODE.set(json_mode);
    match run(cli) {
        Ok(out) => {
            let failed = out.json.get("ok") == Some(&Value::Bool(false));
            let code = if failed { 1 } else { 0 };
            if json_mode {
                let mut v = out.json;
                if let Value::Object(m) = &mut v {
                    m.entry("ok".to_string()).or_insert(Value::Bool(true));
                }
                emit(&format!("{v}\n"), code);
            } else if !out.text.is_empty() {
                let nl = if out.text.ends_with('\n') { "" } else { "\n" };
                emit(&format!("{}{nl}", out.text), code);
            }
            if failed {
                std::process::exit(1);
            }
        }
        Err(e) => {
            if json_mode {
                emit(&format!("{}\n", json!({ "ok": false, "error": format!("{e:#}") })), 1);
            } else {
                eprintln!("error: {e:#}");
            }
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<Out> {
    let agent = cli.agent.clone().filter(|a| !a.is_empty());
    let dir = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };

    match &cli.cmd {
        Cmd::Init { path } => return init(&dir, path.as_ref(), agent),
        Cmd::Clone { source, dest } => return clone_cmd(&dir, source, dest.as_ref(), agent),
        Cmd::RemoteServe { path } => {
            let path = if path.is_absolute() { path.clone() } else { dir.join(path) };
            progsync_core::transport::serve(&path, std::io::stdin().lock(), std::io::stdout().lock())?;
            std::process::exit(0);
        }
        Cmd::Config(args) => {
            // Config never snapshots, and works outside a repository.
            let repo = if args.global { None } else { Repo::discover(&dir, agent).ok() };
            return config_cmd(repo.as_ref(), args);
        }
        // The watcher takes no workspace lock and does not snapshot.
        Cmd::Daemon(cmd) => return daemon_cmd(&dir, agent, cmd),
        Cmd::Mcp => {
            mcp::Server::new(cli.dir.clone(), agent).serve()?;
            return Ok(Out { json: json!({}), text: String::new() });
        }
        // No snapshot: checking must work on a damaged repository too.
        Cmd::Check => return check_cmd(&Repo::discover(&dir, agent)?),
        // No snapshot and no lock: cheap, and never waits for a running command.
        Cmd::Id { paths } => return id_cmd(&Repo::open_unlocked(&dir)?, paths),
        // No snapshot and no workspace lock: runs beside the user's commands.
        Cmd::Gc { auto: true, .. } => {
            let packed = progsync_core::gc::auto(&Repo::open_unlocked(&dir)?)?;
            return Ok(Out { json: json!({ "packed": packed }), text: if packed { "Packed loose objects\n".into() } else { String::new() } });
        }
        _ => {}
    }

    let trace = std::env::var_os("PSYNC_TRACE").is_some();
    let t = std::time::Instant::now();
    let repo = Repo::discover(&dir, agent.clone())?;
    if config::get(Some(repo.dot()), "watch.auto").is_some_and(|(v, _)| v == "true")
        && !watch::is_running(&repo.workspace_dir())
    {
        // Best effort: this command scans normally; later ones use the watcher.
        let _ = spawn_daemon(&repo.root, &repo.workspace_dir(), agent.as_deref(), false, None);
    }
    let t_open = t.elapsed();
    let snap = repo.snapshot()?;
    if trace {
        eprintln!("trace open {:?}, snapshot total {:?}", t_open, t.elapsed() - t_open);
    }

    let out = match cli.cmd {
        Cmd::Init { .. } => unreachable!(),
        Cmd::Config(args) => config_cmd(Some(&repo), &args),
        Cmd::Daemon(_) | Cmd::Mcp | Cmd::Clone { .. } | Cmd::RemoteServe { .. } | Cmd::Id { .. } => unreachable!(),
        Cmd::Remote(cmd) => remote_cmd(&repo, cmd),
        Cmd::Fetch { remote } => {
            let f = repo.fetch(&remote)?;
            Ok(Out { json: fetch_json(&f), text: fetch_text(&f) })
        }
        Cmd::Pull { remote } => pull_cmd(&repo, &remote),
        Cmd::Git(GitCmd::Import { path, branch }) => {
            let path = if path.is_absolute() { path } else { dir.join(path) };
            let o = repo.git_import(&path, &branch)?;
            let mut text = if o.commits == 0 {
                format!("Already up to date with {branch} ({})\n", &o.git_head[..12])
            } else {
                format!(
                    "Imported {} from {} ({branch}): main is now {}\n",
                    plural(o.commits, "commit"),
                    path.display(),
                    o.main.map_or("(none)".into(), |h| h.short())
                )
            };
            if o.skipped_submodules > 0 {
                text.push_str(&format!("  skipped {} (submodules are not supported)\n", plural(o.skipped_submodules, "submodule entry")));
            }
            if o.commits > 0 && !o.synced {
                text.push_str("Run `psync sync` to bring main into your workspace.\n");
            }
            Ok(Out {
                json: json!({
                    "commits": o.commits,
                    "main": o.main.map(|h| h.short()),
                    "git_head": o.git_head,
                    "skipped_submodules": o.skipped_submodules,
                    "synced": o.synced,
                }),
                text,
            })
        }
        Cmd::Git(GitCmd::Export { path, branch, no_commit_check }) => {
            let path = if path.is_absolute() { path } else { dir.join(path) };
            if !path.exists() {
                std::fs::create_dir_all(&path)?;
            }
            if !path.join(".git").exists() && std::process::Command::new("git").arg("-C").arg(&path).args(["rev-parse", "--git-dir"]).output().map_or(true, |o| !o.status.success()) {
                let st = std::process::Command::new("git").arg("-C").arg(&path).args(["init", "-q", "-b", &branch]).status()?;
                if !st.success() {
                    bail!("git init failed in {}", path.display());
                }
            }
            let o = repo.git_export(&path, &branch, !no_commit_check)?;
            let text = if o.commits == 0 {
                format!("{branch} in {} is up to date\n", path.display())
            } else {
                format!(
                    "Exported {} to {branch} in {} (now {}). The Git working tree is not updated; check it out there if needed.\n",
                    plural(o.commits, "commit"),
                    path.display(),
                    o.git_head.as_deref().map_or("?", |h| &h[..12])
                )
            };
            Ok(Out { json: json!({ "commits": o.commits, "branch": o.branch, "git_head": o.git_head }), text })
        }
        Cmd::Push { remote } => {
            let p = repo.push(&remote)?;
            let mut text = match p.previous {
                Some(prev) if prev == p.main => format!("{remote} is up to date (main {})\n", p.main.short()),
                _ => format!(
                    "Pushed main to {remote}: {} -> {} ({})\n",
                    p.previous.map_or("(none)".to_string(), |h| h.short()),
                    p.main.short(),
                    plural(p.objects, "object")
                ),
            };
            for n in &p.notes {
                text.push_str(&format!("  note: {n}\n"));
            }
            Ok(Out {
                json: json!({
                    "remote": remote,
                    "main": p.main.short(),
                    "previous": p.previous.map(|h| h.short()),
                    "objects": p.objects,
                    "notes": p.notes,
                }),
                text,
            })
        }
        Cmd::Status { ignored } => {
            let mut out = status(&repo, &repo.status()?)?;
            if ignored {
                let list = progsync_core::wc::ignored_paths(&repo.root)?;
                out.text.push_str(if list.is_empty() { "\nIgnored (not committed): none\n" } else { "\nIgnored (not committed):\n" });
                for p in &list {
                    match (p.is_dir, p.size) {
                        (true, _) => out.text.push_str(&format!("  {}/\n", p.path)),
                        (false, Some(n)) => out.text.push_str(&format!("  {}  {}\n", p.path, human_size(n))),
                        (false, None) => out.text.push_str(&format!("  {}\n", p.path)),
                    }
                }
                out.json["ignored"] = Value::Array(
                    list.iter().map(|p| json!({ "path": p.path, "directory": p.is_dir, "size": p.size })).collect(),
                );
            }
            Ok(out)
        }
        Cmd::CheckIgnore { paths } => {
            let mut text = String::new();
            let mut results = Vec::new();
            for p in &paths {
                let rel = repo_relative(&repo, &dir, p);
                let v = progsync_core::wc::check_ignore(&repo.root, &rel)?;
                let rule = v.rule.as_ref().map(|r| format!("{}:{}  {}", r.source, r.line, r.pattern));
                let line = match (&v.reason, &v.inside, &rule, v.ignored) {
                    (Some(why), _, _, _) => format!("{rel}  ignored: {why}"),
                    (None, Some(d), Some(r), _) => format!("{rel}  ignored: inside {d}/ ({r})"),
                    (None, None, Some(r), true) => format!("{rel}  ignored by {r}"),
                    (None, None, Some(r), false) => format!("{rel}  tracked: re-included by {r}"),
                    _ => format!("{rel}  tracked: no rule matches"),
                };
                text.push_str(&line);
                text.push('\n');
                results.push(json!({
                    "path": rel,
                    "ignored": v.ignored,
                    "source": v.rule.as_ref().map(|r| r.source.clone()),
                    "line": v.rule.as_ref().map(|r| r.line),
                    "pattern": v.rule.as_ref().map(|r| r.pattern.clone()),
                    "inside": v.inside,
                    "reason": v.reason,
                }));
            }
            Ok(Out { json: json!({ "paths": results }), text })
        }
        Cmd::Snapshot => Ok(Out {
            json: json!({ "changed": snap.changed, "files": snap.files, "hashed": snap.hashed, "cached": snap.cached, "watched": snap.watched }),
            text: format!(
                "{} ({}: {} hashed, {} cached)\n",
                if snap.changed { "Working change updated" } else { "No changes" },
                plural(snap.files, "file"),
                snap.hashed,
                snap.cached
            ),
        }),
        Cmd::Describe { meta, rev: spec } => {
            let update = meta.update();
            if update.is_empty() {
                bail!("nothing to describe: pass -m, --task or --meta");
            }
            if spec == "@" {
                let (h, rev) = repo.describe(&update)?;
                return Ok(Out {
                    json: json!({ "working": rev_json(&h, &rev) }),
                    text: format!("Working change {} {}\n", short_id(&rev), intent_text(&rev)),
                });
            }
            let (h, rev) = repo.describe_rev(&spec, &update)?;
            Ok(Out {
                json: json!({ "described": rev_json(&h, &rev) }),
                text: format!("Described {} {}\n", short_id(&rev), intent_text(&rev)),
            })
        }
        Cmd::Commit(args) => {
            let ((dh, done), (nh, next)) = repo.commit_with(&args.meta.update(), &args.paths, args.allow_large)?;
            let files = repo.changes_of(&done)?.len();
            Ok(Out {
                json: json!({ "committed": rev_json(&dh, &done), "files": files, "working": rev_json(&nh, &next) }),
                text: format!(
                    "Committed {} {} ({})\nNew working change {}\n",
                    short_id(&done),
                    intent_text(&done),
                    plural(files, "file"),
                    short_id(&next)
                ),
            })
        }
        Cmd::Log { limit, all, long } => log(&repo, limit, all, long),
        Cmd::Show { rev, stat } => {
            let h = repo.resolve(&rev)?;
            let r = repo.store.get_revision(&h)?;
            let changes = repo.changes_of(&r)?;
            let (files_json, files_text) = files_out(&repo, &changes, !stat)?;
            let mut text = format!(
                "Change    {}\nRevision  {}\nAuthor    {}\n",
                c_id(&r.change_id.to_string()),
                c_hash(&h.to_hex()),
                r.author.full()
            );
            if r.committer != r.author {
                text.push_str(&format!("Committer {}\n", r.committer.full()));
            }
            text.push_str(&format!("Updated   {} ({})\n", rfc3339(r.updated_ms), relative(r.updated_ms)));
            if let Some(t) = &r.meta.task {
                text.push_str(&format!("Task      {t}\n"));
            }
            for (k, v) in &r.meta.extra {
                text.push_str(&format!("{k:<9} {v}\n"));
            }
            for c in &r.meta.checks {
                text.push_str(&format!(
                    "Check     {} `{}` in {:.1}s on tree {} by {}{}\n",
                    if c.passed { paint("32", "passed") } else { paint("31", "failed") },
                    c.command,
                    c.duration_ms as f64 / 1000.0,
                    c_hash(&c.tree.short()[..8]),
                    c.by.short(),
                    if c.tree == r.tree { String::new() } else { c_warn(" (tree has changed since)") }
                ));
            }
            text.push_str(&format!("\n    {}\n\n{files_text}", intent_text(&r)));
            Ok(Out { json: json!({ "revision": rev_json(&h, &r), "files": files_json }), text })
        }
        Cmd::Diff { rev, stat, name_only } => {
            let h = repo.resolve(&rev)?;
            let r = repo.store.get_revision(&h)?;
            let changes = repo.changes_of(&r)?;
            if name_only {
                let names: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
                return Ok(Out { json: json!({ "files": names }), text: names.join("\n") });
            }
            let (files_json, files_text) = files_out(&repo, &changes, !stat)?;
            Ok(Out { json: json!({ "files": files_json }), text: files_text })
        }
        Cmd::Context { recent } => context(&repo, recent),
        Cmd::Task(cmd) => task(&repo, cmd),
        Cmd::Claim { patterns, ttl, force } => {
            if patterns.is_empty() {
                let claims = repo.active_claims()?;
                let text = if claims.is_empty() { "No active claims.\n".into() } else { claims_text(&claims, repo.workspace()) };
                return Ok(Out { json: json!({ "claims": claims.iter().map(claim_json).collect::<Vec<_>>() }), text });
            }
            let created = repo.claim(&patterns, parse_ttl(&ttl)?, force)?;
            Ok(Out {
                json: json!({ "claimed": created.iter().map(claim_json).collect::<Vec<_>>() }),
                text: format!("Claimed for {}:\n{}", relative_future(created[0].expires_ms), claims_text(&created, repo.workspace())),
            })
        }
        Cmd::Release { patterns } => {
            let n = repo.release(&patterns)?;
            Ok(Out { json: json!({ "released": n }), text: format!("Released {}\n", plural(n, "claim")) })
        }
        Cmd::Sync => Ok(sync_out(&repo.sync()?)),
        Cmd::Land { check, no_check, workspaces } => {
            let check = if no_check { None } else { check.or_else(|| config::get(Some(repo.dot()), "land.check").map(|(v, _)| v)) };
            land(&repo, check.as_deref(), &workspaces)
        }
        Cmd::Merge { rev } => {
            let m = repo.merge(&rev)?;
            let mut text = format!("Merged {rev} into your working change ({} changed)\n", plural(m.files, "file"));
            text.push_str(&conflicts_text(&m.conflicts));
            Ok(Out {
                json: json!({
                    "merged": m.merged.short(),
                    "base": m.base.map(|b| b.short()),
                    "files": m.files,
                    "conflicts": m.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
                }),
                text,
            })
        }
        Cmd::Restore { rev, paths } => {
            let r = repo.restore(&rev, &paths)?;
            let src = repo.store.get_revision(&r.source)?;
            let short = &src.change_id.short()[..8];
            let mut text = if r.files.is_empty() {
                format!("Nothing to restore: {} already as in {rev} ({short}).\n", paths.join(" "))
            } else {
                format!("Restored {} from {rev} ({short}):\n", plural(r.files.len(), "file"))
            };
            for f in &r.files {
                match (f.leaf, f.size) {
                    (Some(_), Some(size)) => text.push_str(&format!("  {}  ({size} bytes)\n", f.path)),
                    _ => text.push_str(&format!("  {}  (removed: not in {rev})\n", f.path)),
                }
            }
            if !r.files.is_empty() {
                text.push_str("`psync undo` puts back what was there before.\n");
            }
            let files: Vec<Value> = r
                .files
                .iter()
                .map(|f| match f.leaf {
                    Some((kind, h)) => json!({ "path": f.path, "hash": h.to_string(), "size": f.size, "kind": kind_name(kind) }),
                    None => json!({ "path": f.path, "removed": true }),
                })
                .collect();
            Ok(Out { json: json!({ "source": r.source.to_string(), "change_id": src.change_id.short(), "files": files }), text })
        }
        Cmd::Archive { rev, paths, prefix, output } => {
            let prefix = prefix.unwrap_or_default();
            let json_mode = *JSON_MODE.get().unwrap_or(&false);
            let Some(output) = output else {
                if json_mode {
                    bail!("with --json, write the archive to a file: -o FILE");
                }
                if std::io::stdout().is_terminal() {
                    bail!("not writing a tar to the terminal: use -o FILE or redirect, e.g. `psync archive {rev} > app.tar`");
                }
                let mut out = std::io::BufWriter::new(QuietPipe(std::io::stdout().lock()));
                let (h, _, stats) = repo.archive(&rev, &paths, &prefix, &mut out)?;
                drop(out);
                eprintln!("archive of {h} ({}, {})", plural(stats.files, "file"), human_size(stats.bytes));
                return Ok(Out { json: json!({}), text: String::new() });
            };
            let output = if output.is_absolute() { output } else { dir.join(output) };
            // Written beside the target, then renamed: no half-written tar.
            let tmp = output.with_file_name(format!(
                ".{}.tmp-{}",
                output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                std::process::id()
            ));
            let result = (|| -> Result<_> {
                let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
                let r = repo.archive(&rev, &paths, &prefix, &mut f)?;
                std::io::Write::flush(&mut f)?;
                f.into_inner().map_err(|e| e.into_error())?.sync_all()?;
                std::fs::rename(&tmp, &output)?;
                Ok(r)
            })();
            let (h, r, stats) = result.inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })?;
            Ok(Out {
                json: json!({
                    "revision": h.to_string(),
                    "change_id": r.change_id.short(),
                    "intent": r.meta.intent.lines().next().unwrap_or_default(),
                    "output": output,
                    "files": stats.files,
                    "bytes": stats.bytes,
                }),
                text: format!(
                    "Wrote {}: {} ({}) of {rev} {} ({h})\n",
                    output.display(),
                    plural(stats.files, "file"),
                    human_size(stats.bytes),
                    &r.change_id.short()[..8]
                ),
            })
        }
        Cmd::Cat { spec, path } => {
            let (rev, path) = match path {
                Some(p) => (spec, p),
                None => match spec.split_once(':') {
                    Some((r, p)) if !p.is_empty() => (if r.is_empty() { "@".to_string() } else { r.to_string() }, p.to_string()),
                    _ => bail!("expected <rev>:<path> (or <rev> <path>), e.g. `psync cat main:src/lib.rs`"),
                },
            };
            let (h, kind, file) = repo.file_at(&rev, &path)?;
            let size = repo.store.file_size(&file)?;
            if *JSON_MODE.get().unwrap_or(&false) {
                let data = repo.store.get_blob(&file)?;
                let content = String::from_utf8(data).ok();
                return Ok(Out {
                    json: json!({
                        "revision": h.to_string(), "path": path, "kind": kind_name(kind), "hash": file.to_string(),
                        "size": size, "binary": content.is_none(), "content": content,
                    }),
                    text: String::new(),
                });
            }
            let mut out = QuietPipe(std::io::stdout().lock());
            repo.store.write_file_to(&file, &mut out)?;
            std::io::Write::flush(&mut out)?;
            Ok(Out { json: json!({}), text: String::new() })
        }
        Cmd::Revert { rev } => {
            let r = repo.revert(&rev)?;
            let (_, w) = repo.working()?;
            let mut text = format!(
                "Reverted {rev} in your working change ({}): {}\n",
                plural(r.files, "file"),
                w.meta.intent
            );
            if r.conflicts.is_empty() {
                text.push_str("Next: `psync commit` then `psync land`.\n");
            } else {
                text.push_str(&conflicts_text(&r.conflicts));
            }
            Ok(Out {
                json: json!({
                    "reverted": r.reverted.short(),
                    "intent": w.meta.intent,
                    "files": r.files,
                    "conflicts": r.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
                }),
                text,
            })
        }
        Cmd::Resolve { path, force } => match path {
            None => {
                let (_, w) = repo.working()?;
                let text = if w.conflicts.is_empty() { "No conflicts.\n".to_string() } else { conflicts_text(&w.conflicts) };
                Ok(Out { json: json!({ "conflicts": w.conflicts.iter().map(conflict_json).collect::<Vec<_>>() }), text })
            }
            Some(p) => {
                let left = repo.resolve_conflict(&p, force)?;
                Ok(Out {
                    json: json!({ "resolved": p, "conflicts": left.iter().map(conflict_json).collect::<Vec<_>>() }),
                    text: format!("Resolved {p}; {} remaining\n", plural(left.len(), "conflict")),
                })
            }
        },
        Cmd::Workspace(cmd) => workspace(&repo, cmd),
        Cmd::Switch { rev } => {
            let o = repo.switch(&rev)?;
            let r = &o.revision;
            let mut text = match o.back {
                true => format!("Switched back to {} {}\n", short_id(r), intent_text(r)),
                false => format!("Switched to {rev}: working change {} {}\n", short_id(r), intent_text(r)),
            };
            text.push_str("The working change you left is parked; `psync switch -` returns to it.\n");
            Ok(Out { json: json!({ "working": rev_json(&o.to, r), "parked": o.from.short(), "back": o.back }), text })
        }
        Cmd::Rev(cmd) => rev_cmd(&repo, cmd),
        Cmd::Gc { keep, dry_run, .. } => gc_cmd(&repo, keep.as_deref(), dry_run),
        Cmd::Check => unreachable!("handled before the snapshot"),
        Cmd::Undo => {
            let u = repo.undo()?;
            Ok(Out {
                json: json!({ "op": u.op.short(), "undone": { "op": u.undone.short(), "description": u.undone_description } }),
                text: format!("Undid operation {}: {}\n", c_hash(&u.undone.short()[..12]), u.undone_description),
            })
        }
        Cmd::Op(OpCmd::Log { limit }) => {
            let ops = repo.op_log(limit)?;
            let text: String = ops
                .iter()
                .map(|(h, op)| {
                    format!(
                        "{} {:<10} {:<14} {:>7}  {}\n",
                        c_hash(&h.short()[..12]),
                        op.workspace.as_deref().unwrap_or("-"),
                        op.actor.short(),
                        c_dim(&relative(op.time_ms)),
                        op.description
                    )
                })
                .collect();
            let list: Vec<Value> = ops.iter().map(|(h, op)| op_json(h, op)).collect();
            Ok(Out { json: json!({ "operations": list }), text })
        }
        Cmd::Op(OpCmd::Restore { op }) => {
            let target = repo.resolve_op(&op)?;
            let new = repo.op_restore(target)?;
            Ok(Out {
                json: json!({ "op": new.short(), "restored": target.short() }),
                text: format!("Restored workspace {} to operation {}\n", repo.workspace(), c_hash(&target.short()[..12])),
            })
        }
    };
    pack_in_background(&repo);
    out
}

fn init(dir: &std::path::Path, path: Option<&PathBuf>, agent: Option<String>) -> Result<Out> {
    let target = match path {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => dir.join(p),
        None => dir.to_path_buf(),
    };
    std::fs::create_dir_all(&target)?;
    let note = config::ensure_owner_identity();
    let identity = config::identity(None, agent);
    let repo = Repo::init(&target, identity)?;
    pack_in_background(&repo);
    let st = repo.status()?;
    let (h, rev) = &st.working;
    let mut text = String::new();
    if let Some(n) = &note {
        text.push_str(&format!("{n}\n"));
    }
    text.push_str(&format!(
        "Initialized psync repository in {}\nOwner: {}\nTracking {} in working change {}\n",
        repo.root.display(),
        repo.identity().owner(),
        plural(st.changes.len(), "file"),
        short_id(rev)
    ));
    Ok(Out {
        json: json!({
            "root": repo.root,
            "owner": identity_json(repo.identity()),
            "files": st.changes.len(),
            "working": rev_json(h, rev),
            "note": note,
        }),
        text,
    })
}

fn config_cmd(repo: Option<&Repo>, args: &ConfigArgs) -> Result<Out> {
    let dot = repo.map(|r| r.dot());
    if args.list || args.key.is_none() {
        let items = config::list(dot);
        let text = items.iter().map(|(k, v, s)| format!("{k}={v}  {}\n", c_dim(&format!("({})", s.name())))).collect();
        let list: Vec<Value> = items.iter().map(|(k, v, s)| json!({ "key": k, "value": v, "source": s.name() })).collect();
        return Ok(Out { json: json!({ "settings": list }), text });
    }
    let key = args.key.as_deref().expect("checked");
    let path = if args.global {
        config::global_path().ok_or_else(|| progsync_core::err!("cannot locate global config (HOME not set)"))?
    } else {
        config::repo_path(dot.ok_or_else(|| progsync_core::err!("not in a repository: use --global"))?)
    };
    if args.unset {
        config::set(&path, key, None)?;
        return Ok(Out { json: json!({ "key": key, "unset": true }), text: format!("Unset {key}\n") });
    }
    match &args.value {
        Some(v) => {
            config::set(&path, key, Some(v))?;
            Ok(Out { json: json!({ "key": key, "value": v }), text: format!("{key}={v}  ({})\n", path.display()) })
        }
        None => match config::get(dot, key) {
            Some((v, s)) => Ok(Out { json: json!({ "key": key, "value": v, "source": s.name() }), text: format!("{v}\n") }),
            None => bail!("{key} is not set"),
        },
    }
}

fn status(repo: &Repo, st: &Status) -> Result<Out> {
    let (h, rev) = &st.working;
    let mut text = format!(
        "Workspace      {} {}\nWorking change {} {}\n",
        repo.workspace(),
        c_dim(&format!("({})", repo.identity().full())),
        short_id(rev),
        intent_text(rev)
    );
    let task = repo.current_task()?;
    match (&task, &rev.meta.task) {
        (Some(t), _) => text.push_str(&format!("Task           {} {}  {}\n", t.task.id, t.task.title, c_dim(&format!("({})", t.display_status())))),
        (None, Some(id)) => text.push_str(&format!("Task           {id}\n")),
        _ => {}
    }
    if let Some((_, p)) = &st.parent {
        text.push_str(&format!("Parent         {} {}\n", short_id(p), intent_text(p)));
    }
    let parked = repo.parked()?;
    if let Some((_, p)) = &parked {
        text.push_str(&format!("Parked         {} {}  {}\n", short_id(p), intent_text(p), c_dim("(`psync switch -` returns to it)")));
    }
    let ab = repo.ahead_behind()?;
    if let Some((m, ahead, behind)) = ab {
        let mr = repo.store.get_revision(&m)?;
        text.push_str(&format!(
            "Main           {} {}  {}\n",
            short_id(&mr),
            intent_text(&mr),
            c_dim(&format!("(you: {ahead} ahead, {behind} behind)"))
        ));
    }
    if !rev.conflicts.is_empty() {
        text.push('\n');
        text.push_str(&conflicts_text(&rev.conflicts));
    }
    let (files_json, files_text) = files_out(repo, &st.changes, false)?;
    if st.changes.is_empty() {
        text.push_str("\nNo changes.\n");
    } else {
        text.push_str(&format!("\nChanges:\n{files_text}"));
    }
    let warnings = repo.claimed_by_others(&repo.my_changed_paths()?)?;
    text.push_str(&claim_warnings_text(&warnings));
    Ok(Out {
        json: json!({
            "workspace": repo.workspace(),
            "working": rev_json(h, rev),
            "parent": st.parent.as_ref().map(|(ph, p)| rev_json(ph, p)),
            "parked": parked.as_ref().map(|(ph, p)| rev_json(ph, p)),
            "main": ab.map(|(m, ahead, behind)| json!({ "revision": m.short(), "ahead": ahead, "behind": behind })),
            "conflicts": rev.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
            "task": task.as_ref().map(task_json),
            "claimed_by_others": warnings_json(&warnings),
            "files": files_json,
        }),
        text,
    })
}

fn log(repo: &Repo, limit: usize, all: bool, long: bool) -> Result<Out> {
    let heads: std::collections::HashMap<Hash, Vec<String>> = {
        let mut m: std::collections::HashMap<Hash, Vec<String>> = std::collections::HashMap::new();
        let view = repo.view()?;
        for (name, h) in view.workspaces {
            m.entry(h).or_default().push(format!("{name}@"));
        }
        for (name, h) in view.bookmarks {
            m.entry(h).or_default().push(name);
        }
        m
    };
    let revs = if all { repo.log_all(limit)? } else { repo.log(repo.working()?.0, limit)? };
    let mut text = String::new();
    let mut list = Vec::new();
    for (i, (h, rev)) in revs.iter().enumerate() {
        let ws = heads.get(h).cloned().unwrap_or_default();
        let mine = ws.iter().any(|w| *w == format!("{}@", repo.workspace()));
        let marker = if mine { "@" } else if ws.iter().any(|w| w.ends_with('@')) { "◎" } else { "○" };
        let ws_label = if ws.is_empty() { String::new() } else { format!(" {}", paint("36", &format!("[{}]", ws.join(", ")))) };
        if long {
            let last = i + 1 == revs.len();
            let (block, files) = log_block(repo, h, rev, marker, &ws_label, last)?;
            text.push_str(&block);
            let mut v = rev_json(h, rev);
            v["labels"] = json!(ws);
            v["files"] = json!(files);
            list.push(v);
            continue;
        }
        let task = rev.meta.task.as_ref().map(|t| format!(" [{t}]")).unwrap_or_default();
        text.push_str(&format!(
            "{marker} {} {} {} {}  {}{task}{ws_label}\n",
            short_id(rev),
            c_hash(&h.short()[..8]),
            rev.author.short(),
            c_dim(&relative(rev.updated_ms)),
            intent_text(rev),
        ));
        let mut v = rev_json(h, rev);
        v["labels"] = json!(ws);
        list.push(v);
    }
    Ok(Out { json: json!({ "revisions": list }), text })
}

/// Files listed per change in `log --long`.
const LOG_FILES: usize = 10;

/// One change of `log --long`: a header line, then its details and full
/// intent behind a rail (`│`) that leads to the next change.
fn log_block(repo: &Repo, h: &Hash, rev: &Revision, marker: &str, labels: &str, last: bool) -> Result<(String, Vec<Value>)> {
    let rail = if last { "  ".to_string() } else { format!("{} ", c_dim("│")) };
    let mut out = format!("{marker} {} {}{labels}\n", short_id(rev), c_hash(&h.short()[..8]));
    let mut field = |name: &str, value: &str| out.push_str(&format!("{rail}{}  {value}\n", c_dim(&format!("{name:<9}"))));
    field("Author", &rev.author.full());
    if rev.committer != rev.author {
        field("Committer", &rev.committer.full());
    }
    field("Date", &format!("{} UTC ({})", rfc3339(rev.updated_ms)[..16].replacen('T', " ", 1), relative(rev.updated_ms)));
    if let Some(t) = &rev.meta.task {
        let title = repo.task(t).map(|i| format!(" {}", i.task.title)).unwrap_or_default();
        field("Task", &format!("{}{title}", paint("1", t)));
    }
    for (k, v) in &rev.meta.extra {
        field(k, v);
    }
    for c in &rev.meta.checks {
        let stale = if c.tree == rev.tree { String::new() } else { c_warn(" (tree has changed since)") };
        let verdict = if c.passed { paint("32", "passed") } else { paint("31", "failed") };
        field("Check", &format!("{verdict} `{}` in {:.1}s by {}{stale}", c.command, c.duration_ms as f64 / 1000.0, c.by.short()));
    }
    let changes = repo.changes_of(rev)?;
    let (files, files_text) = files_out(repo, &changes, false)?;
    if !files.is_empty() {
        let sum = |k: &str| files.iter().filter_map(|f| f[k].as_u64()).sum::<u64>();
        let totals = format!("{}{}", paint("32", &format!("+{}", sum("added"))), paint("31", &format!(" -{}", sum("removed"))));
        field("Files", &format!("{}  {totals}", plural(files.len(), "file")));
        for l in files_text.lines().take(LOG_FILES) {
            out.push_str(&format!("{rail}         {l}\n"));
        }
        if files.len() > LOG_FILES {
            out.push_str(&format!("{rail}           {}\n", c_dim(&format!("… {} more", files.len() - LOG_FILES))));
        }
    }
    let blank = format!("{}\n", rail.trim_end());
    out.push_str(&blank);
    if rev.meta.intent.is_empty() {
        out.push_str(&format!("{rail}    {}\n", c_dim("(no intent)")));
    } else {
        for l in rev.meta.intent.lines() {
            if l.trim().is_empty() {
                out.push_str(&blank);
                continue;
            }
            for w in wrap(l, INTENT_WIDTH) {
                out.push_str(&format!("{rail}    {}\n", paint("1", &w)));
            }
        }
    }
    if !last {
        out.push_str(&blank);
    }
    Ok((out, files))
}

/// Intent lines in `log --long` wrap at this many characters.
const INTENT_WIDTH: usize = 72;

/// Word-wrap `line` at `width` characters (a longer word gets its own line),
/// keeping its leading indentation on the first piece.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let indent = &line[..line.len() - line.trim_start().len()];
    let mut cur = indent.to_string();
    for word in line.split_whitespace() {
        let n = cur.chars().count();
        if n > indent.len() && n + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() && !cur.ends_with(' ') {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    out.push(cur);
    out
}

const MAX_LISTED: usize = 50;

fn capped(files: &[String]) -> Value {
    json!(files.iter().take(MAX_LISTED).collect::<Vec<_>>())
}

fn context(repo: &Repo, recent: usize) -> Result<Out> {
    let b = repo.context(recent)?;
    let (wh, wrev) = &b.working;
    let mut text = format!(
        "You        {} in workspace {} ({})\nWorking    {} {}{}  — {} changed\n",
        b.identity.full(),
        paint("1", &b.workspace),
        b.root.display(),
        short_id(wrev),
        intent_text(wrev),
        wrev.meta.task.as_ref().map(|t| format!(" [{t}]")).unwrap_or_default(),
        plural(b.files.len(), "file"),
    );
    for f in b.files.iter().take(10) {
        text.push_str(&format!("             {f}\n"));
    }
    if b.files.len() > 10 {
        text.push_str(&format!("             … {} more\n", b.files.len() - 10));
    }
    if let Some((_, mr)) = &b.main {
        text.push_str(&format!(
            "Main       {} {}  — you are {} ahead, {} behind{}\n",
            short_id(mr),
            intent_text(mr),
            b.ahead,
            b.behind,
            if b.behind > 0 { c_warn("  → run `psync sync`") } else { String::new() }
        ));
    }
    let queue = repo.land_queue();
    if !queue.is_empty() {
        text.push_str(&format!("Landing    {}\n", queue_text(&queue, &b.workspace)));
    }
    if !wrev.conflicts.is_empty() {
        text.push('\n');
        text.push_str(&conflicts_text(&wrev.conflicts));
    }
    if !b.recent.is_empty() {
        text.push_str("\nRecent\n");
        for (_, r) in &b.recent {
            text.push_str(&format!("  {} {}  {}\n", short_id(r), r.author.short(), intent_text(r)));
        }
    }
    if b.peers.is_empty() {
        text.push_str("\nNo other workspaces.\n");
    } else {
        text.push_str("\nOther workspaces\n");
        for p in &b.peers {
            let r = &p.info.revision;
            text.push_str(&format!(
                "  {:<12} {} {}  {} {}  — {} changed since you diverged\n",
                p.info.name,
                short_id(r),
                intent_text(r),
                r.committer.short(),
                c_dim(&relative(r.updated_ms)),
                plural(p.files.len(), "file"),
            ));
            if !p.overlap.is_empty() {
                text.push_str(&format!("    {} {}\n", c_warn("overlap:"), p.overlap.join(", ")));
            }
            if !r.conflicts.is_empty() {
                text.push_str(&format!("    {} {}\n", c_warn("conflicts:"), plural(r.conflicts.len(), "file")));
            }
        }
    }
    let overlaps = overlap_map(&b.peers);
    if !overlaps.is_empty() {
        text.push_str(&format!(
            "\n{} {} also changed in other workspaces — coordinate before editing further.\n",
            c_warn("!"),
            plural(overlaps.len(), "file")
        ));
    }
    let tasks = repo.tasks()?;
    let my_task = repo.current_task()?;
    let ready: Vec<&TaskInfo> = tasks.iter().filter(|t| t.is_ready()).take(5).collect();
    let counts = repo.task_counts()?;
    if !tasks.is_empty() {
        text.push_str(&format!(
            "\nTasks      {} open, {} in progress, {} blocked, {} done\n",
            counts.open, counts.in_progress, counts.blocked, counts.done
        ));
        if let Some(t) = &my_task {
            text.push_str(&format!("  yours    {} {}{}\n", t.task.id, t.task.title, t.task.check.as_ref().map(|c| c_dim(&format!("  (accept: {c})"))).unwrap_or_default()));
        }
        for t in &ready {
            text.push_str(&format!("  ready    {} {}\n", t.task.id, t.task.title));
        }
    }
    let claims = repo.active_claims()?;
    if !claims.is_empty() {
        text.push_str(&format!("\nClaims\n{}", claims_text(&claims, &b.workspace)));
    }
    let warnings = repo.claimed_by_others(&repo.my_changed_paths()?)?;
    text.push_str(&claim_warnings_text(&warnings));
    let peers: Vec<Value> = b
        .peers
        .iter()
        .map(|p| {
            json!({
                "name": p.info.name,
                "path": p.info.path,
                "repository": p.place,
                "working": rev_json(&p.info.rev, &p.info.revision),
                "files_changed": p.files.len(),
                "files": capped(&p.files),
                "overlap": p.overlap,
                "conflicts": p.info.revision.conflicts.len(),
            })
        })
        .collect();
    let mut working = rev_json(wh, wrev);
    working["files_changed"] = json!(b.files.len());
    working["files"] = capped(&b.files);
    Ok(Out {
        json: json!({
            "workspace": b.workspace,
            "root": b.root,
            "identity": identity_json(&b.identity),
            "working": working,
            "main": b.main.as_ref().map(|(h, r)| rev_json(h, r)),
            "ahead": b.ahead,
            "behind": b.behind,
            "land_queue": queue_json(&queue),
            "conflicts": wrev.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
            "recent": b.recent.iter().map(|(h, r)| rev_json(h, r)).collect::<Vec<_>>(),
            "workspaces": peers,
            "overlaps": overlaps,
            "task": my_task.as_ref().map(task_json),
            "ready_tasks": ready.iter().map(|t| task_json(t)).collect::<Vec<_>>(),
            "task_counts": { "open": counts.open, "in_progress": counts.in_progress, "blocked": counts.blocked, "done": counts.done },
            "claims": claims.iter().map(claim_json).collect::<Vec<_>>(),
            "claimed_by_others": warnings_json(&warnings),
        }),
        text,
    })
}

fn workspace(repo: &Repo, cmd: WsCmd) -> Result<Out> {
    match cmd {
        WsCmd::Add { name, path, rev } => {
            let a = repo.workspace_add(&name, path.as_deref(), rev.as_deref())?;
            let path = a.info.path.clone().unwrap_or_default();
            let parent = a.info.revision.parents.first().map(|p| p.short()).unwrap_or_default();
            Ok(Out {
                json: json!({
                    "name": a.info.name,
                    "path": path,
                    "files": a.files,
                    "cloned": a.cloned,
                    "working": rev_json(&a.info.rev, &a.info.revision),
                }),
                text: format!(
                    "Created workspace {} at {}\n{} ({} cloned copy-on-write)\nWorking change {} on top of {}\n\nStart an agent there:\n  cd {} && PSYNC_AGENT={} psync context\n",
                    paint("1", &a.info.name),
                    path.display(),
                    plural(a.files, "file"),
                    a.cloned,
                    short_id(&a.info.revision),
                    c_hash(&parent[..8]),
                    path.display(),
                    a.info.name,
                ),
            })
        }
        WsCmd::List => {
            let mut text = String::new();
            let mut list = Vec::new();
            for w in repo.workspaces()? {
                let files = repo.changes_of(&w.revision)?.len();
                let current = w.name == repo.workspace();
                text.push_str(&format!(
                    "{} {:<12} {} {}  {}  {}\n",
                    if current { "*" } else { " " },
                    w.name,
                    short_id(&w.revision),
                    intent_text(&w.revision),
                    c_dim(&format!("{} · {}", plural(files, "file"), w.revision.committer.short())),
                    c_dim(&w.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
                ));
                list.push(json!({
                    "name": w.name,
                    "path": w.path,
                    "current": current,
                    "files_changed": files,
                    "working": rev_json(&w.rev, &w.revision),
                }));
            }
            Ok(Out { json: json!({ "workspaces": list }), text })
        }
        WsCmd::Remove { name } => {
            let last = repo.workspace_remove(&name)?;
            Ok(Out {
                json: json!({ "removed": name, "last_revision": last.short() }),
                text: format!("Removed workspace {name}; its last revision {} stays in history\n", c_hash(&last.short())),
            })
        }
    }
}

/// Per-file JSON and text; `patch` includes unified diffs.
fn files_out(repo: &Repo, changes: &[FileChange], patch: bool) -> Result<(Vec<Value>, String)> {
    let mut list = Vec::with_capacity(changes.len());
    let mut text = String::new();
    let mut patches = String::new();
    for c in changes {
        let st = diff::line_stat(&repo.store, c)?;
        let mut v = json!({ "path": c.path, "status": c.status(), "added": st.added, "removed": st.removed });
        if st.binary {
            v["binary"] = Value::Bool(true);
        }
        let counts = if st.binary {
            "binary".to_string()
        } else {
            format!("{}{}", paint("32", &format!("+{}", st.added)), paint("31", &format!(" -{}", st.removed)))
        };
        text.push_str(&format!("  {} {}  {}\n", c_status(c), c.path, counts));
        if patch {
            let p = diff::unified(&repo.store, c)?;
            patches.push_str(&colorize_patch(&p));
            v["patch"] = Value::String(p);
        }
        list.push(v);
    }
    if patch && !patches.is_empty() {
        text.push('\n');
        text.push_str(&patches);
    }
    Ok((list, text))
}

fn colorize_patch(p: &str) -> String {
    if !*COLOR.get().unwrap_or(&false) {
        return p.to_string();
    }
    p.lines()
        .map(|l| {
            let s = if l.starts_with("+++") || l.starts_with("---") {
                paint("1", l)
            } else if l.starts_with('+') {
                paint("32", l)
            } else if l.starts_with('-') {
                paint("31", l)
            } else if l.starts_with("@@") {
                paint("36", l)
            } else {
                l.to_string()
            };
            s + "\n"
        })
        .collect()
}

fn intent_text(rev: &Revision) -> String {
    if rev.meta.intent.is_empty() {
        c_dim("(no intent)")
    } else {
        rev.meta.intent.lines().next().unwrap_or_default().to_string()
    }
}

fn task(repo: &Repo, cmd: TaskCmd) -> Result<Out> {
    let one = |t: TaskInfo, verb: &str| Out {
        text: format!("{verb} {} {}  {}\n", paint("1", &t.task.id), t.task.title, c_dim(&format!("({})", t.display_status()))),
        json: json!({ "task": task_json(&t) }),
    };
    Ok(match cmd {
        TaskCmd::Add { title, id, description, check, after } => {
            let t = repo.task_add(&title, id.as_deref(), &description, check, after)?;
            one(repo.task(&t.id)?, "Added")
        }
        TaskCmd::List { all } => {
            let tasks: Vec<TaskInfo> =
                repo.tasks()?.into_iter().filter(|t| all || !t.task.status.is_finished()).collect();
            let mut text = String::new();
            for t in &tasks {
                let owner = t.task.owner.as_ref().map(|o| format!("{} ({})", o.by.short(), o.workspace)).unwrap_or_default();
                let extra = if !t.blocked_by.is_empty() {
                    format!("waits for {}", t.blocked_by.join(", "))
                } else if !t.task.landed.is_empty() {
                    format!("{} landed", plural(t.task.landed.len(), "change"))
                } else {
                    String::new()
                };
                let status = t.display_status();
                let colored = match status {
                    "done" => paint("32", status),
                    "cancelled" => c_dim(status),
                    "blocked" => paint("31", status),
                    "in_progress" => paint("33", status),
                    _ => status.to_string(),
                };
                text.push_str(&format!("{:<8} {:<20} {}  {} {}\n", t.task.id, colored, t.task.title, c_dim(&owner), c_dim(&extra)));
            }
            if tasks.is_empty() {
                text = if all { "No tasks.\n".into() } else { "No open tasks.\n".into() };
            }
            let c = repo.task_counts()?;
            text.push_str(&c_dim(&format!("{} open, {} in progress, {} blocked, {} done\n", c.open, c.in_progress, c.blocked, c.done)));
            Out { json: json!({ "tasks": tasks.iter().map(task_json).collect::<Vec<_>>() }), text }
        }
        TaskCmd::Show { id } => {
            let t = repo.task(&id)?;
            let mut text = format!("Task      {} {}\nStatus    {}\n", paint("1", &t.task.id), t.task.title, t.display_status());
            if let Some(o) = &t.task.owner {
                text.push_str(&format!("Owner     {} in workspace {}\n", o.by.full(), o.workspace));
            }
            if !t.task.after.is_empty() {
                text.push_str(&format!("After     {}\n", t.task.after.join(", ")));
            }
            if let Some(c) = &t.task.check {
                text.push_str(&format!("Accept    `{c}`\n"));
            }
            if !t.task.landed.is_empty() {
                let ids: Vec<String> = t.task.landed.iter().map(|c| c.short()[..8].to_string()).collect();
                text.push_str(&format!("Landed    {}\n", ids.join(", ")));
            }
            if let Some(c) = &t.task.completion {
                text.push_str(&format!("Completed `{}` passed on tree {} by {}\n", c.command, c_hash(&c.tree.short()[..8]), c.by.short()));
            }
            text.push_str(&format!("Created   by {} {}\n", t.task.created_by.full(), c_dim(&relative(t.task.created_ms))));
            if !t.task.description.is_empty() {
                text.push_str(&format!("\n{}\n", t.task.description));
            }
            Out { json: json!({ "task": task_json(&t) }), text }
        }
        TaskCmd::Start { id, force } => {
            repo.task_start(&id, force)?;
            let mut out = one(repo.task(&id)?, "Started");
            out.text.push_str("Your working change now belongs to this task. Consider `psync claim <paths>` for the files you will touch.\n");
            out
        }
        TaskCmd::Drop { id, force } => {
            repo.task_drop(&id, force)?;
            one(repo.task(&id)?, "Dropped")
        }
        TaskCmd::Edit { id, title, description, check, after } => {
            let after = after.map(|a| a.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
            repo.task_edit(&id, TaskEdit { title, description, check, after })?;
            one(repo.task(&id)?, "Edited")
        }
        TaskCmd::Cancel { id } => {
            repo.task_cancel(&id)?;
            one(repo.task(&id)?, "Cancelled")
        }
        TaskCmd::Done { id } => match repo.task_done(&id)? {
            TaskDone::Done(_) => one(repo.task(&id)?, "Done"),
            TaskDone::CheckFailed(check) => Out {
                json: json!({ "ok": false, "error": format!("acceptance check `{}` failed", check.command), "check": check_json(&check) }),
                text: format!(
                    "Not done: acceptance check `{}` failed (exit {})\n--- output (last lines) ---\n{}\n",
                    check.command,
                    check.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                    check.output_tail
                ),
            },
        },
    })
}

fn task_json(t: &TaskInfo) -> Value {
    json!({
        "id": t.task.id,
        "title": t.task.title,
        "description": t.task.description,
        "status": t.display_status(),
        "check": t.task.check,
        "after": t.task.after,
        "blocked_by": t.blocked_by,
        "owner": t.task.owner.as_ref().map(|o| json!({ "workspace": o.workspace, "by": identity_json(&o.by) })),
        "landed": t.task.landed.iter().map(|c| c.short()).collect::<Vec<_>>(),
        "completion": t.task.completion.as_ref().map(|c| json!({ "command": c.command, "tree": c.tree.short(), "by": identity_json(&c.by), "time": rfc3339(c.time_ms) })),
        "created_by": identity_json(&t.task.created_by),
        "updated": rfc3339(t.task.updated_ms),
    })
}

fn claim_json(c: &Claim) -> Value {
    json!({
        "pattern": c.pattern,
        "workspace": c.workspace,
        "by": identity_json(&c.by),
        "task": c.task,
        "expires": rfc3339(c.expires_ms),
        "repository": c.home.as_ref().map(|h| h.place.as_str()),
    })
}

fn relative_future(ms: i64) -> String {
    let s = ((ms - progsync_core::util::now_ms()).max(0)) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h{}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86_400),
    }
}

fn claims_text(claims: &[Claim], me: &str) -> String {
    claims
        .iter()
        .map(|c| {
            let who = match &c.home {
                None if c.workspace == me => paint("1", "you"),
                None => format!("{} ({})", c.by.short(), c.workspace),
                Some(h) => format!("{} ({} at {})", c.by.short(), c.workspace, h.place),
            };
            let task = c.task.as_ref().map(|t| format!(" [{t}]")).unwrap_or_default();
            format!("  {:<28} {}{}  {}\n", c.pattern, who, task, c_dim(&format!("expires in {}", relative_future(c.expires_ms))))
        })
        .collect()
}

fn claim_warnings_text(w: &[(String, Claim)]) -> String {
    if w.is_empty() {
        return String::new();
    }
    let mut t = format!("\n{} you changed files claimed by other workspaces:\n", c_warn("!"));
    for (p, c) in w {
        t.push_str(&format!("  {p}  claimed by {} in {} as '{}'\n", c.by.short(), c.location(), c.pattern));
    }
    t
}

fn warnings_json(w: &[(String, Claim)]) -> Value {
    json!(w.iter().map(|(p, c)| json!({ "path": p, "claim": claim_json(c) })).collect::<Vec<_>>())
}

fn conflict_json(c: &Conflict) -> Value {
    json!({ "path": c.path, "kind": c.kind })
}

fn conflicts_text(cs: &[Conflict]) -> String {
    if cs.is_empty() {
        return String::new();
    }
    let mut t = format!("{} (edit to remove markers, or `psync resolve <path>`):\n", c_warn(&format!("{} unresolved", plural(cs.len(), "conflict"))));
    for c in cs {
        let kind = progsync_core::json::to_value(c.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        t.push_str(&format!("  {} {}  {}\n", paint("31", "C"), c.path, c_dim(&kind)));
    }
    t
}

fn sync_out(s: &SyncOutcome) -> Out {
    let mut text = if s.up_to_date {
        format!("Already up to date with main {}\n", c_hash(&s.main.short()[..8]))
    } else {
        format!("Synced onto main {}: {} rebased\n", c_hash(&s.main.short()[..8]), plural(s.rebased.len(), "change"))
    };
    if !s.folded.is_empty() {
        let ids: Vec<String> = s.folded.iter().map(|r| r.change_id.short()[..8].to_string()).collect();
        text.push_str(&format!(
            "{} conflicted with main and {} folded into your working change: {}\n",
            plural(s.folded.len(), "committed change"),
            if s.folded.len() == 1 { "was" } else { "were" },
            ids.join(", ")
        ));
    }
    text.push_str(&conflicts_text(&s.conflicts));
    Out {
        json: json!({
            "main": s.main.short(),
            "up_to_date": s.up_to_date,
            "rebased": s.rebased.iter().map(|(h, r)| rev_json(h, r)).collect::<Vec<_>>(),
            "folded": s.folded.iter().map(|r| r.change_id.short()).collect::<Vec<_>>(),
            "conflicts": s.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
        }),
        text,
    }
}

fn check_json(c: &progsync_core::repo::CheckRun) -> Value {
    json!({ "command": c.command, "passed": c.passed, "exit_code": c.exit_code, "duration_ms": c.duration_ms, "output_tail": c.output_tail })
}

fn land(repo: &Repo, check: Option<&str>, workspaces: &[String]) -> Result<Out> {
    // Before landing: afterwards the changes are on main, no longer yours.
    let warnings = repo.claimed_by_others(&repo.my_changed_paths()?)?;
    let quiet = JSON_MODE.get().copied().unwrap_or(false);
    let me = repo.workspace().to_string();
    let mut on_wait = |q: &progsync_core::queue::LandQueue| {
        if !quiet {
            eprintln!("Waiting to land: {}", queue_text(q, &me));
        }
    };
    if !workspaces.is_empty() {
        let b = repo.land_batch(workspaces, check, &mut on_wait)?;
        let mut out = land_out(b.result);
        if out.json.get("landed") == Some(&Value::Bool(true)) {
            // Each change line gets where it came from.
            let mut lines: Vec<String> = out.text.lines().map(String::from).collect();
            for (i, src) in b.sources.iter().enumerate() {
                if let Some(l) = lines.get_mut(i + 1) {
                    l.push_str(&format!("  {}", c_dim(&format!("from {src}"))));
                }
            }
            out.text = lines.join("\n") + "\n";
        }
        out.json["sources"] = json!(b.sources);
        out.text.push_str(&claim_warnings_text(&warnings));
        out.json["claimed_by_others"] = warnings_json(&warnings);
        return Ok(out);
    }
    let mut out = land_out(repo.land_with(check, &mut on_wait)?);
    out.text.push_str(&claim_warnings_text(&warnings));
    out.json["claimed_by_others"] = warnings_json(&warnings);
    Ok(out)
}

/// "claude-2 holds main (12m, `cargo test`); waiting: claude-3, you"
fn queue_text(q: &progsync_core::queue::LandQueue, me: &str) -> String {
    let name = |e: &progsync_core::queue::Entry| if e.workspace == me { "you".to_string() } else { e.workspace.clone() };
    let mut parts = Vec::new();
    match &q.holder {
        Some(h) => parts.push(format!(
            "{} holds main (taken {}{})",
            name(h),
            relative(h.holding_ms.unwrap_or(h.since_ms)),
            h.check.as_ref().map(|c| format!(", running `{c}`")).unwrap_or_default()
        )),
        None => parts.push("main is free".to_string()),
    }
    if !q.waiting.is_empty() {
        parts.push(format!("waiting: {}", q.waiting.iter().map(name).collect::<Vec<_>>().join(", ")));
    }
    parts.join("; ")
}

fn queue_json(q: &progsync_core::queue::LandQueue) -> Value {
    let entry = |e: &progsync_core::queue::Entry| {
        json!({
            "workspace": e.workspace,
            "by": e.by.full(),
            "agent": e.by.agent,
            "check": e.check,
            "since": rfc3339(e.since_ms),
            "holding_since": e.holding_ms.map(rfc3339),
        })
    };
    json!({ "holder": q.holder.as_ref().map(entry), "waiting": q.waiting.iter().map(entry).collect::<Vec<_>>() })
}

fn land_out(result: LandResult) -> Out {
    match result {
        LandResult::Landed { main, changes, check, synced } => {
            let mut text = format!(
                "Landed {} onto main{} — main is now {}\n",
                plural(changes.len(), "change"),
                if synced { " (after syncing)" } else { "" },
                c_hash(&main.short()[..8])
            );
            for (_, r) in &changes {
                text.push_str(&format!("  {} {}\n", short_id(r), intent_text(r)));
            }
            if let Some(c) = &check {
                text.push_str(&format!("Check `{}` passed in {:.1}s\n", c.command, c.duration_ms as f64 / 1000.0));
            }
            Out {
                json: json!({
                    "landed": true,
                    "main": main.short(),
                    "synced": synced,
                    "changes": changes.iter().map(|(h, r)| rev_json(h, r)).collect::<Vec<_>>(),
                    "check": check.as_ref().map(check_json),
                }),
                text,
            }
        }
        LandResult::Conflicts(s) => {
            let mut out = sync_out(&s);
            out.text = format!("Not landed: syncing onto main produced conflicts.\n{}Resolve them, commit, then run `psync land` again.\n", out.text);
            out.json["ok"] = json!(false);
            out.json["landed"] = json!(false);
            out.json["error"] = json!("conflicts with main");
            out
        }
        LandResult::CheckFailed { check, changes } => Out {
            json: json!({
                "ok": false,
                "landed": false,
                "error": format!("check `{}` failed", check.command),
                "check": check_json(&check),
                "changes": changes.iter().map(|(h, r)| rev_json(h, r)).collect::<Vec<_>>(),
            }),
            text: format!(
                "Not landed: check `{}` failed (exit {}) after {:.1}s\n--- output (last lines) ---\n{}\n",
                check.command,
                check.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                check.duration_ms as f64 / 1000.0,
                check.output_tail
            ),
        },
    }
}

fn identity_json(id: &Identity) -> Value {
    json!({ "name": id.name, "email": id.email, "agent": id.agent })
}

fn rev_json(h: &Hash, rev: &Revision) -> Value {
    json!({
        "change_id": rev.change_id.short(),
        "revision": h.short(),
        "intent": rev.meta.intent,
        "task": rev.meta.task,
        "author": identity_json(&rev.author),
        "committer": identity_json(&rev.committer),
        "meta": rev.meta.extra,
        "parents": rev.parents.iter().map(|p| p.short()).collect::<Vec<_>>(),
        "checks": rev.meta.checks.iter().map(|c| json!({
            "command": c.command,
            "passed": c.passed,
            "duration_ms": c.duration_ms,
            "tree": c.tree.short(),
            "matches_tree": c.tree == rev.tree,
            "by": identity_json(&c.by),
            "time": rfc3339(c.time_ms),
        })).collect::<Vec<_>>(),
        "created": rfc3339(rev.created_ms),
        "updated": rfc3339(rev.updated_ms),
    })
}

fn op_json(h: &Hash, op: &Operation) -> Value {
    json!({
        "op": h.short(),
        "workspace": op.workspace,
        "actor": identity_json(&op.actor),
        "time": rfc3339(op.time_ms),
        "description": op.description,
        "undo_of": op.undo_of.map(|u| u.short()),
    })
}

// ---- daemon ---------------------------------------------------------------

/// Once there are many loose objects (or packs), move them into a pack in a
/// detached `psync gc --auto`, so no command waits for it. Best effort.
fn pack_in_background(repo: &Repo) {
    if !progsync_core::gc::auto_needed(repo) {
        return;
    }
    let spawn = || -> Result<()> {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(repo.dot().join("gc.log"))?;
        let mut c = std::process::Command::new(std::env::current_exe()?);
        c.arg("-C").arg(&repo.root).args(["gc", "--auto"]);
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
        progsync_core::platform::detach(&mut c);
        let mut child = c.spawn()?;
        // Reaped if this process lives on (the MCP server); otherwise the
        // packing simply outlives it.
        std::thread::spawn(move || child.wait());
        Ok(())
    };
    if let Err(e) = spawn() {
        eprintln!("warning: could not start background packing: {e:#}");
    }
}

fn spawn_daemon(root: &Path, ws_dir: &Path, agent: Option<&str>, no_snapshot: bool, http: Option<&str>) -> Result<()> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(ws_dir.join("daemon.log"))?;
    let mut c = std::process::Command::new(std::env::current_exe()?);
    c.arg("-C").arg(root);
    if let Some(a) = agent {
        c.args(["--agent", a]);
    }
    c.args(["daemon", "run"]);
    if no_snapshot {
        c.arg("--no-snapshot");
    }
    if let Some(addr) = http {
        c.arg(format!("--http={addr}"));
    }
    // Own process group: not killed with the terminal's foreground job.
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    progsync_core::platform::detach(&mut c);
    c.spawn()?;
    Ok(())
}

fn daemon_cmd(dir: &Path, agent: Option<String>, cmd: &DaemonCmd) -> Result<Out> {
    let (dot, ws, root) = Repo::locate(dir)?;
    let ws_dir = dot.join("workspaces").join(&ws);
    if !ws_dir.is_dir() {
        bail!("workspace '{ws}' no longer exists in {}", dot.display());
    }
    match cmd {
        DaemonCmd::Run { no_snapshot, http } => {
            let identity = config::identity(Some(&dot), agent.clone());
            let on_settled: Option<watch::OnSettled> = if *no_snapshot {
                None
            } else {
                let root = root.clone();
                Some(Box::new(move || Ok(Repo::snapshot_if_idle(&root, identity.clone())?.is_some())))
            };
            if watch::is_running(&ws_dir) {
                bail!("a watcher is already running for workspace {ws}");
            }
            if let Some(addr) = http {
                // Before the watcher starts, so `daemon start` sees the URL once it is up.
                crate::http::start(addr, root.clone(), &ws_dir, agent)?;
            }
            let result = watch::run(DaemonOptions { root, ws_dir: ws_dir.clone(), on_settled, per_directory: watch::PER_DIRECTORY });
            crate::http::stop(&ws_dir);
            result?;
            Ok(Out { json: json!({ "stopped": true }), text: String::new() })
        }
        DaemonCmd::Start { no_snapshot, http } => {
            if let Some(st) = watch::status(&ws_dir) {
                if http.is_some() && crate::http::url(&ws_dir).is_none() {
                    bail!("a watcher without the HTTP API is running; `psync daemon stop` it first");
                }
                return Ok(daemon_out(&ws, &ws_dir, &st, "Watcher already running"));
            }
            spawn_daemon(&root, &ws_dir, agent.as_deref(), *no_snapshot, http.as_deref())?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(st) = watch::status(&ws_dir) {
                    return Ok(daemon_out(&ws, &ws_dir, &st, "Watcher started"));
                }
                if std::time::Instant::now() > deadline {
                    bail!("the watcher did not start; see {}", ws_dir.join("daemon.log").display());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        DaemonCmd::Stop => {
            let stopped = watch::stop(&ws_dir);
            let text = if stopped { "Watcher stopped." } else { "No watcher was running." };
            Ok(Out { json: json!({ "workspace": ws, "stopped": stopped }), text: text.into() })
        }
        DaemonCmd::Status => match watch::status(&ws_dir) {
            Some(st) => Ok(daemon_out(&ws, &ws_dir, &st, "Watcher running")),
            None => Ok(Out {
                json: json!({ "workspace": ws, "running": false }),
                text: format!("No watcher running for workspace {ws}. Start one with `psync daemon start`."),
            }),
        },
    }
}

fn daemon_out(ws: &str, ws_dir: &Path, st: &DaemonStatus, headline: &str) -> Out {
    let http_url = crate::http::url(ws_dir);
    let json = json!({
        "http": http_url,
        "http_info": http_url.as_ref().map(|_| crate::http::info_path(ws_dir)),
        "workspace": ws,
        "running": true,
        "pid": st.info.pid,
        "root": st.info.root,
        "started": rfc3339(st.info.started_ms),
        "snapshots": st.info.snapshots,
        "snapshots_taken": st.snapshots_taken,
        "events": st.events,
        "paths": st.paths,
        "ignored_events": st.ignored,
        "watched_directories": st.directories,
        "last_error": st.last_error,
    });
    let mut text = format!(
        "{headline} for workspace {ws} (pid {}, started {})\n  Root       {}\n  Events     {} ({} paths remembered, {} in ignored paths dropped)\n",
        st.info.pid,
        relative(st.info.started_ms),
        st.info.root.display(),
        st.events,
        st.paths,
        st.ignored
    );
    match st.directories {
        Some(n) => text.push_str(&format!("  Watching   {n} directories one by one (ignored ones skipped)\n")),
        None => text.push_str("  Watching   the whole tree\n"),
    }
    if st.info.snapshots {
        text.push_str(&format!("  Snapshots  when edits settle ({} taken)\n", st.snapshots_taken));
    } else {
        text.push_str("  Snapshots  off (changed paths only)\n");
    }
    if let Some(u) = &http_url {
        text.push_str(&format!("  HTTP API   {u}  (token in {})\n", crate::http::info_path(ws_dir).display()));
    }
    if let Some(e) = &st.last_error {
        text.push_str(&format!("  Last error {e}\n"));
    }
    Out { json, text }
}

// ---- remotes ----------------------------------------------------------------

fn rev_cmd(repo: &Repo, cmd: RevCmd) -> Result<Out> {
    Ok(match cmd {
        RevCmd::Add { name, rev, remote } => {
            let (h, notes) = match &remote {
                Some(r) => repo.rev_add_remote(&name, &rev, r)?,
                None => (repo.rev_add(&name, &rev)?, vec![]),
            };
            let r = repo.store.get_revision(&h)?;
            let mut text = format!("Rev {name} = {} {}{}\n", short_id(&r), intent_text(&r), remote.as_ref().map(|x| format!(" (also in {x})")).unwrap_or_default());
            for n in &notes {
                text.push_str(&format!("  note: {n}\n"));
            }
            Out { json: json!({ "name": name, "revision": rev_json(&h, &r), "remote": remote, "notes": notes }), text }
        }
        RevCmd::List => {
            let revs = repo.revs()?;
            let text = if revs.is_empty() {
                "No revs. Name one with `psync rev add <name> [rev]`.\n".to_string()
            } else {
                revs.iter().map(|(n, _, r)| format!("  {n:<20} {} {}  {}\n", short_id(r), intent_text(r), c_dim(&r.author.short()))).collect()
            };
            let list: Vec<Value> = revs.iter().map(|(n, h, r)| json!({ "name": n, "revision": rev_json(h, r) })).collect();
            Out { json: json!({ "revs": list }), text }
        }
        RevCmd::Remove { name } => {
            let h = repo.rev_remove(&name)?;
            Out { json: json!({ "removed": name, "revision": h.short() }), text: format!("Removed rev {name} (was {})\n", c_hash(&h.short()[..8])) }
        }
    })
}

fn fetch_json(f: &progsync_core::remote::FetchOutcome) -> Value {
    json!({
        "remote": f.remote,
        "main": f.main.map(|h| h.short()),
        "objects": f.objects,
        "tasks": f.tasks,
        "notes": f.notes,
        "claims_from_other_repositories": f.claims,
        "workspaces_of_other_repositories": f.workspaces,
        "revs": f.revs,
    })
}

fn fetch_text(f: &progsync_core::remote::FetchOutcome) -> String {
    let mut text = match f.main {
        Some(m) => format!("Fetched {}: {}/main is {} ({})\n", f.remote, f.remote, m.short(), plural(f.objects, "new object")),
        None => format!("Fetched {}: it has no main yet\n", f.remote),
    };
    if !f.tasks.is_empty() {
        text.push_str(&format!("  tasks updated: {}\n", f.tasks.join(", ")));
    }
    if let (Some(w), Some(c)) = (f.workspaces, f.claims) {
        text.push_str(&format!("  other repositories: {}, {} (`psync context`)\n", plural(w, "workspace"), plural(c, "active claim")));
    }
    if !f.revs.is_empty() {
        text.push_str(&format!("  new revs: {}\n", f.revs.join(", ")));
    }
    for n in &f.notes {
        text.push_str(&format!("  note: {n}\n"));
    }
    text
}

fn pull_cmd(repo: &Repo, remote: &str) -> Result<Out> {
    use progsync_core::remote::PullMain;
    let p = repo.pull(remote)?;
    let mut text = fetch_text(&p.fetch);
    let mut json = fetch_json(&p.fetch);
    let (state, extra) = match &p.main {
        PullMain::NoRemoteMain => ("no_remote_main", String::new()),
        PullMain::UpToDate => ("up_to_date", "main is up to date\n".to_string()),
        PullMain::Ahead => ("ahead", format!("main has changes {remote} lacks: `psync push {remote}`\n")),
        PullMain::FastForward { from, to } => (
            "fast_forward",
            format!(
                "main fast-forwarded {} -> {}; run `psync sync` to bring the changes into your workspace\n",
                from.map_or("(none)".to_string(), |h| h.short()),
                to.short()
            ),
        ),
        PullMain::Diverged { moved_back, sync } => {
            json["moved_back"] = json!(moved_back.iter().map(|c| c.short()).collect::<Vec<_>>());
            json["sync"] = sync_out(sync).json;
            let mut t = format!(
                "main and {remote}/main had both changed: main is now {remote}'s, and your {} landed here but not there were moved back into this workspace.\n",
                plural(moved_back.len(), "change")
            );
            t.push_str(&sync_out(sync).text);
            t.push_str("Land them again (`psync land`), then `psync push`.\n");
            ("diverged", t)
        }
    };
    json["main_update"] = json!(state);
    text.push_str(&extra);
    Ok(Out { json, text })
}

fn remote_cmd(repo: &Repo, cmd: RemoteCmd) -> Result<Out> {
    match cmd {
        RemoteCmd::Add { name, url } => {
            let r = repo.remote_add(&name, &url)?;
            Ok(Out {
                json: json!({ "name": r.name, "url": r.url }),
                text: format!("Added remote {} -> {}\nNext: `psync pull {}` or `psync push {}`\n", r.name, r.url, r.name, r.name),
            })
        }
        RemoteCmd::List => {
            let all = repo.remotes()?;
            let view = repo.view()?;
            let mut text = String::new();
            let mut list = Vec::new();
            for r in &all {
                let tracking = view.bookmarks.get(&format!("{}/main", r.name));
                text.push_str(&format!(
                    "{:<12} {}  (main {})\n",
                    r.name,
                    r.url,
                    tracking.map_or("not fetched".to_string(), |h| h.short())
                ));
                list.push(json!({ "name": r.name, "url": r.url, "main": tracking.map(|h| h.short()) }));
            }
            if all.is_empty() {
                text = "No remotes. Add one with `psync remote add origin <path>`.\n".into();
            }
            Ok(Out { json: json!({ "remotes": list }), text })
        }
        RemoteCmd::Remove { name } => {
            repo.remote_remove(&name)?;
            Ok(Out { json: json!({ "removed": name }), text: format!("Removed remote {name}\n") })
        }
    }
}

fn clone_cmd(dir: &Path, source: &str, dest: Option<&PathBuf>, agent: Option<String>) -> Result<Out> {
    use progsync_core::transport::Address;
    let address = Address::parse(source)?;
    let src: String = match &address {
        Address::Local(p) if p.is_absolute() => p.display().to_string(),
        Address::Local(p) => dir.join(p).display().to_string(),
        Address::Ssh { .. } => source.to_string(),
    };
    let target = match dest {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => dir.join(p),
        None => {
            let name = match &address {
                Address::Local(_) => progsync_core::platform::canonicalize(Path::new(&src))?
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
                Address::Ssh { path, .. } => path.trim_end_matches('/').rsplit('/').next().map(String::from),
            };
            dir.join(name.filter(|n| !n.is_empty() && n != "~").unwrap_or_else(|| "clone".into()))
        }
    };
    let note = config::ensure_owner_identity();
    let identity = config::identity(None, agent);
    let repo = Repo::clone_from(&src, &target, identity)?;
    let main = repo.main_tip()?;
    let tasks = repo.tasks()?.len();
    let mut text = note.map(|n| format!("{n}\n")).unwrap_or_default();
    text.push_str(&format!(
        "Cloned {} into {}\n  main {}, {}; remote origin -> {}\n",
        src,
        repo.root.display(),
        main.map_or("(none)".to_string(), |h| h.short()),
        plural(tasks, "task"),
        src
    ));
    Ok(Out { json: json!({ "root": repo.root, "main": main.map(|h| h.short()), "tasks": tasks }), text })
}

fn kind_name(k: progsync_core::EntryKind) -> &'static str {
    match k {
        progsync_core::EntryKind::File => "file",
        progsync_core::EntryKind::Exec => "executable",
        progsync_core::EntryKind::Symlink => "symlink",
        progsync_core::EntryKind::Tree => "directory",
    }
}

/// Stdout for raw bytes: if the reader has gone (`psync cat x | head`), stop quietly.
struct QuietPipe<W>(W);

impl<W: std::io::Write> std::io::Write for QuietPipe<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf).inspect_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().inspect_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        })
    }
}

fn id_cmd(repo: &Repo, paths: &[String]) -> Result<Out> {
    let id = repo.identify(paths)?;
    let dirty = !id.changed.is_empty();
    Ok(Out {
        json: json!({
            "revision": id.revision.to_string(),
            "change_id": id.commit.change_id.short(),
            "intent": id.commit.meta.intent.lines().next().unwrap_or_default(),
            "working_change": id.working.short(),
            "dirty": dirty,
            "changed": id.changed,
            "on_main": id.on_main,
        }),
        text: format!("{}{}\n", id.revision, if dirty { "-dirty" } else { "" }),
    })
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// A path given on the command line (relative to the current directory, or
/// absolute) as a path relative to the workspace root.
fn repo_relative(repo: &Repo, cwd: &Path, p: &str) -> String {
    let abs = if Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) };
    let root = repo.root.canonicalize().unwrap_or_else(|_| repo.root.clone());
    // Resolve `..` and symlinks of the existing part, keep the rest as typed.
    let mut existing = abs.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(n), Some(parent)) => {
                rest.push(n);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut full = existing.canonicalize().unwrap_or(existing);
    for n in rest.into_iter().rev() {
        full.push(n);
    }
    let rel = full.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| p.to_string());
    if p.ends_with('/') && !rel.ends_with('/') { format!("{rel}/") } else { rel }
}
