# psync design

psync ("progress sync"; the project is named progsync) is a standalone
version control system built for AI agents (and the humans working with
them) contributing to the same project in parallel. It manages progress,
not only files: tasks, claims and checked landings decide what reaches
`main`, and every change records who made it and why. It is
agent-agnostic: any agent that can run a CLI and read JSON can use it.
This document is about how psync works; [USAGE.md](USAGE.md) is about how
to use it.

## Principles

1. **No ceremony.** No staging area. The working directory *is* the working
   change (`@`) and is snapshotted automatically by every command.
2. **Everything is undoable.** Every state change is an operation in an
   append-only log. `undo` and `op restore` can reach any past state.
3. **Changes carry meaning.** A change records its intent, task, the agent
   that made it, and free-form metadata (model, session, ...), not only a diff.
4. **Machine-first interface.** Git-like commands; every command supports
   `--json` with stable fields and `{"ok": false, "error": ...}` on failure.
5. **Fast by construction.** Content addressing, a stat cache, parallel
   scanning and hashing, and subtree skipping in diffs.
6. **Local-first, distributed.** Immutable content-addressed objects and an
   append-only op log per repository; repositories exchange only shared
   state (main, tasks, claims, revs, workspace heads; see Remotes).

## Data model

All objects are immutable and stored under `.psync/objects/` by the BLAKE3 hash
of their encoding (`[format, tag, payload]`, zstd-compressed on disk): new
ones as one file each (*loose*), older ones in pack files (see Packs).

| Object      | Contents |
|-------------|----------|
| `Blob`      | file bytes (or symlink target) |
| `Chunked`   | a file larger than 4 MiB: its size and its chunks `(blob hash, length)` in order (binary; see Large files) |
| `Tree`      | sorted entries `(name, kind: File/Exec/Symlink/Tree, hash)` (binary, see below) |
| `Revision`  | `change_id`, `tree`, `parents`, `meta {intent, task, extra}`, `author`, `committer`, timestamps (JSON) |
| `Operation` | `parent` op, `view` (workspace -> working revision), `actor`, `workspace`, description, `undo_of` (JSON) |

Revisions, operations and tasks are JSON (`json.rs`, psync's own reader and
writer) so the schema can grow: new fields are declared `#default` and older
readers ignore unknown fields. Fields are written in declaration order and
strings are escaped one fixed way, so the bytes (and hashes) are
deterministic. Trees use a
compact binary encoding (`bin.rs`) because they are hot and their shape is
fixed: an entry count, then per entry the name (length + UTF-8), the kind
(0 file, 1 exec, 2 symlink, 3 tree) and the 32-byte hash, with LEB128
varints for counts and lengths. The stat-cache index uses the same
encoding. (The layout matches the `postcard` crate used before, so trees
written then keep their hashes.)

### Identity

Every revision and operation records an `Identity { name, email, agent }`:
the **owner** (accountable human, from `user.name`/`user.email` config,
imported from git on first `init`) and the **agent** acting for them
(`PSYNC_AGENT` / `--agent`; absent when the human acts directly).
`author` is who created a change, `committer` who produced its latest
revision. An empty change created by someone else (e.g. by `workspace add`)
is credited to whoever first puts work into it, except that a snapshot by a
command run without an agent never takes a change from an agent of the same
owner (it cannot tell who made the edits). A change with no intent yet is
credited at commit to whoever commits it, when they are the same owner.

### Ids, operations, index

- **Change id vs revision.** A change keeps a stable id (letters `k`-`z`)
  across amends; each amend creates a new revision (hex hash). The two
  alphabets never collide, so a prefix is never ambiguous between them.
- **Op head.** `.psync/op_head` points to the latest operation. The repository
  lock (`.psync/lock`) is held only while appending an operation.
- **Index.** `.psync/workspaces/<name>/index` is a per-workspace stat cache (size, mtime, inode -> hash), plus every directory's tree hash and the file watcher's token. It is
  only an optimization; deleting it costs one full re-hash. Files modified
  within 2s of a scan are re-hashed on the next one ("racy" entries).
- **Undo.** `undo` finds this workspace's most recent operation (skipping other
  workspaces' operations and already-undone ones) and restores this workspace's
  entry to its value before it. Repeated `undo` walks further back.
  `op restore <op>` sets this workspace to its state at any operation.

## Durability, check, gc (T-21)

- **Crash safety.** Every file psync changes is replaced by rename, and new
  objects reach the disk before `op_head` moves to an operation that refers
  to them, so after a crash or power cut `op_head` always names a complete
  state (the newest operation may be lost, never half-written). Per
  platform (`platform::barrier`): Linux issues one `syncfs` before the
  `op_head` rename; macOS `fsync`s each new object and directory, then one
  `F_BARRIERFSYNC` (an ordering barrier; `F_FULLFSYNC` would cost ~10 ms per
  command); Windows flushes each new object. Cost on macOS: ~3 ms per
  command, none measurable for a 20k-file import. The stat cache (`index`) is
  not flushed: an unreadable one is rebuilt.
- **`psync check`** walks everything the kept operations refer to (views,
  revisions and their parents, trees, files, conflict sides, check trees,
  task versions), reads each object whole and verifies its hash and kind.
  It runs before the snapshot, so it works on a damaged repository; it
  exits 1 if it finds problems and reports unreachable objects.
- **`psync gc`** deletes unreachable objects and leftover temp files.
  Concurrent agents stay safe: objects less than a day old (`GRACE_MS`) are
  kept, along with everything they and the workspaces' stat caches refer
  to; a writer that reuses an existing object instead of writing it
  *freshens* its modification time (when older than half the grace period);
  and objects are deleted referrers first, so even an interrupted `gc`
  leaves every remaining object complete (fetching relies on that). `gc`
  refuses to delete anything if objects are missing.
- **`gc --keep AGE`** forgets operations older than AGE by writing
  `.psync/op_horizon` (the oldest kept operation; its parent counts as
  absent). `op log`, `undo` and `op restore` stop there. All change history
  stays; what becomes collectable is what only old operations referred to,
  mostly intermediate snapshots of working changes. Without `--keep`, the
  operation history is kept in full.

## Packs (T-23)

One file per object costs a disk block, an inode and a directory entry
each: the 20k-file benchmark stored 1.7 MB of data in 20,565 files taking
80 MB, and copying the store took 6 s. Packs (`pack`) fix that.

- **Format.** `.psync/objects/pack/<name>.pack` holds objects' stored bytes
  back to back (the same bytes as a loose file); `<name>.idx` is a sorted
  table (hash, offset, length) with a 256-entry fan-out, the pack's length
  and a BLAKE3 checksum. The name derives from the contents. A pack becomes
  visible when its index is renamed into place, after the pack.
- **Reading.** A loose file first, then the packs (indexes are read into
  memory, 48 bytes per object; lookup is a fan-out step and a binary
  search). On a miss the pack directory is read again if it changed, since
  another process may have just packed the object; long-lived handles also
  look every second so they let go of removed packs.
- **Writing** is always loose: cheap, atomic, and safe with concurrent
  agents. An object found only in a pack is relied on by freshening the
  pack (a packed object's age is its pack's).
- **Automatic packing.** After a command, if one fan-out directory suggests
  more than 4096 loose objects (or there are more than 16 packs), psync
  starts a detached `psync gc --auto` (no snapshot, no workspace lock, log in
  `.psync/gc.log`): it writes all loose objects (merging the packs, if many)
  into one pack, flushes it, then deletes the loose files. Nothing is
  deleted that is not in a durable pack.
- **`gc`** writes one pack of all reachable objects. Kept but unreachable
  objects (young ones, and what they refer to) leave the packs as loose
  files carrying their pack's time, so their grace period still runs out.
  Then, in this order: flush; delete doomed loose objects, referrers first
  (one freshened meanwhile is spared with everything it refers to; so are
  the doomed objects of an old pack freshened during the gc); take spared
  objects out of the old packs; flush; delete loose copies of packed objects;
  delete the old packs.
- **Results** (20k files): store 80 MB in 20,565 files → 2.7 MB in 2 files;
  copying it 6.1 s → 76 ms; `check` 332 → 110 ms; status and snapshot
  unchanged; checking out 20k files +6 %.

## Large files (T-24)

Files used to be read into memory whole and stored as one blob: hashing a
1 GB file took 1 GB of memory (several large files at once, several GB),
and every edit stored another full compressed copy.

- **Chunking.** A file larger than 4 MiB (`chunk::LARGE`) is split by
  content-defined chunking (`chunk`, FastCDC-style: a gear hash over the
  last 64 bytes, normalized cut masks; chunks 256 KiB to 4 MiB, about 1 MiB
  on average). Each chunk is an ordinary blob; a `Chunked` object lists
  them, and the tree entry (still `File`/`Exec`) points to the list. Cut
  points depend only on nearby bytes, so an edit changes one or two chunks
  and the rest are shared with the previous version. Chunking is
  deterministic (fixed parameters and gear table), so the same contents
  always get the same hash. Files up to 4 MiB are stored exactly as before.
- **Streaming.** A snapshot reads a file once, splitting it as it goes;
  worker threads compress and write the chunks meanwhile (large files
  stored at the same time share the processors). Checkout writes a file a
  chunk at a time (`Store::write_file_to`).
- **Diff and merge** treat chunked files as binary without reading them
  (`Store::get_small_file`); two edits of one large file are a binary
  conflict. Git export assembles the file.
- **Old repositories.** A large file stored whole by an earlier version
  keeps that hash while its contents are unchanged (the snapshot also
  computes the whole-blob hash and compares it with the index), so
  upgrading does not show untouched large files as modified. Versions
  before T-24 cannot read chunked files (`unknown tag 99`), and refuse a
  push that contains them.
- **gc, check, transfer** follow a chunk list to its chunks. For a file
  entry they read only the object's tag (its first bytes) to see whether it
  is a list; `check` still reads and verifies everything.
- **Results** (1 GB random file; time, peak memory):

  | | before | after |
  |---|---|---|
  | `init` | 2.6 s, 1034 MB | 1.5 s, 40 MB |
  | edit 7 bytes, then `status` | 2.5 s, 2058 MB; store +1 GB | 2.0 s, 13 MB; store +3 MB |
  | `clone` (local path) | 6.2 s, 2201 MB | 5.0 s, 55 MB |
  | `init`, eight 200 MB files | 1.1 s, 1634 MB | 0.9 s, 150 MB |
  | `clone` over ssh | | 4.3 s, 69 MB |
  | `push` of an 8-byte edit | the whole file | 4 objects (a chunk, the list, a tree, a revision) |

  The 20k-file benchmark is unchanged (init 1.85 s, status 250 ms), except
  `gc`'s reachability walk: 98 → 125 ms, for reading each file's tag.

## Workspaces (M1)

A workspace is a directory with its own working change. The default
workspace is the directory containing `.psync/`; others contain a `.psync`
*file* linking to the repository: `{"repo": "/abs/.psync", "workspace": "name"}`.
Local metadata lives in `.psync/workspaces/<name>/{meta.json, index, lock}`.

- `workspace add <name>` creates `../<repo>-<name>` (or `--path`) with a new
  change on top of main (else `@-`), or continuing `--rev` (as `switch`
  does; see Revs). Files whose content matches the source
  workspace are created with `fs::copy`, which clones copy-on-write on
  APFS/btrfs/XFS. 20k files / 78 MB: 1.5 s, no measurable extra disk
  (`git worktree add`: 1.9 s, 78 MB).
- **Ownership rule:** only workspace `W` changes `view.workspaces[W]`, and
  every operation records the workspace it changed. Undo/restore are scoped
  to the current workspace, so agents cannot roll back each other's work.
- **Concurrency:** each command locks its own workspace; the repository lock is
  taken only to append an operation (read view -> set one entry -> write).
  Agents in different workspaces scan, hash and commit in parallel.
- Directories containing a `.psync` entry are never scanned by an enclosing
  workspace, so workspaces may be nested inside the main checkout.
- `workspace remove` snapshots the workspace, records its removal, and deletes
  the directory only if its link file points to this repository.

## Context briefing

`psync context` gives an agent, in one call: its identity and workspace, its
working change and files, recent history, and for every other workspace the
files changed since the two histories diverged (nearest common ancestor) and
the **overlap** with its own changes. Overlaps flag likely conflicts before
any merge happens. Peers' state is as of their last psync command, or live
when they run the file watcher (it snapshots when edits settle).

## Main, sync, land, merge (M3)

`main` is a shared bookmark in the view (`view.bookmarks["main"]`): the line
where finished work from all workspaces comes together. It stays linear.

- **`land [--check CMD]`** puts the workspace's committed, unlanded changes
  onto main. It requires a clean working change (what is tested is what
  lands), syncs onto main, runs the check in the workspace (default from
  config `land.check`), verifies the check did not modify tracked files,
  records a `Check {command, passed, duration, tree, by}` on the landed tip,
  and advances main with a compare-and-swap under the repository lock. If
  another agent landed meanwhile, it syncs and checks again (3 attempts).
  The first land creates main. Land operations cannot be undone (main is
  shared); operations record which bookmarks they moved.
- **Land queue (T-32, `queue`).** With `land.queue`, a land joins a queue
  (`.psync/land/wait/<ticket>`, tickets ordered by time joined) and waits
  until main is free and its ticket is the first live one; then it takes the
  lease (`.psync/land/holder.json`) for the sync, check and compare-and-swap,
  and gives it up whatever the outcome. The queue files change only under
  `.psync/land/lock`. A thread of the owning process renews its entry every
  5 s, so a lease lasts as long as the land however long the check takes;
  an entry not renewed for 60 s (its process died) is ignored and removed.
  What would refuse the land anyway (uncommitted edits, conflicts, nothing
  to land, the message check) is checked before waiting. The
  compare-and-swap and its retries stay, for main moved by other means
  (`pull`, `git import`, a land with the queue off).
- **Batch land (T-33, `batch`).** `land <ws>@...` stacks the committed,
  unlanded changes of this workspace and then of each named one (local, or
  another repository's head) on main, in order, rebasing each onto the one
  before (three-way merge; changes already on main or in the batch under
  the same change id are skipped). Any conflict refuses the whole batch
  before anything is written to disk. The combined tree is checked out in
  this workspace (clean, so its files are its committed tip), the check
  runs once, and one compare-and-swap moves main to the top (check recorded
  there) and this workspace onto it; tasks progress and the named local
  workspaces' claims are released as in a land. Other workspaces' view
  entries are never touched: their next `sync` drops the landed changes by
  change id. On a failed check or error, this workspace's files are
  checked back out. It takes the land queue like any land.
- **`sync`** rebases unlanded committed changes and the working change onto
  main's tip with a three-way merge per change (stable change ids are kept).
  It never aborts on conflicts: if a committed change conflicts, it and
  everything after it are folded into the working change (keeping the folded
  change's identity when the working change had nothing of its own), so all
  conflicts live in one place.
- **`merge <rev>`** three-way merges another change (e.g. `claude-1@`) into
  the working change, which gains it as an extra parent. Landing rebases onto
  main with first parents only, so main stays linear.
- **Conflicts** are recorded on the working change (`Revision.conflicts`:
  path, kind, base/ours/theirs, blob written) and written to disk with
  diff3-style markers (`<<<<<<< main` / `||||||| base` / `=======` /
  `>>>>>>> yours`). They never block other agents. `commit` and `land`
  refuse while any remain. Editing the file to remove the markers resolves a
  content conflict at the next snapshot; `resolve <path> [--force]` marks one
  resolved explicitly. Kinds: `content`, `binary`, `delete_modify`,
  `file_directory`.
- **`restore <rev> -- <paths>`** sets the given paths of the working change
  to their state in `<rev>` (`wc::apply_changes` of the tree diff restricted
  to the paths, then `checkout`, so `text.eol` and chunked files are written
  as by any checkout); conflicts recorded on those paths are dropped. It is
  one workspace operation, so `undo` reverts it. `cat` streams a file's stored
  bytes (`Store::write_file_to`).
- **`revert <rev>`** undoes a change that is in your history (typically one
  already on main, since a land cannot be undone): a three-way merge from the
  change's tree back to its first parent's tree, applied to a clean working
  change, so later work is kept and later edits to the same lines become
  recorded conflicts. The working change gets the intent `Revert "<intent>"`
  and `revert_of` metadata; it is committed and landed like any change, so
  main stays linear and the revert is itself revertible.
- **Text merge** combines base->ours and base->theirs hunks: non-overlapping
  hunks merge cleanly (adjacent lines included), identical edits merge
  cleanly, anything else is a conflict block. Tree merges skip identical
  subtrees by hash.
- **Structural merge** (`semantic`). When the line merge of a Rust, Python,
  JavaScript, TypeScript/TSX or Go file conflicts, all three versions are
  parsed with tree-sitter and split into top-level declarations (with their
  leading comments and attributes), keyed by kind and name. Declarations
  changed on one side take that side; changed on both sides, they are
  line-merged on their own and must merge cleanly; added on both sides, all
  are kept (two agents appending a function at the same place is the most
  common conflict between agents); deleted on one side and unchanged on the
  other, deleted. Order follows "ours", and declarations new in "theirs" go
  after the one they follow there. Anything doubtful (delete vs modify, the
  same new name with different bodies, a remaining conflict, a parse error in
  any version or the result) keeps the line merge and its conflict markers.
- **Merge base** is the highest-generation ancestor of one side that is also
  an ancestor of the other (see Performance).

## Tasks and claims (M2)

Coordination state lives in the view next to workspaces and bookmarks, so it
is versioned by the operation log like everything else:

- `view.tasks`: task id -> current `Task` object (immutable; each update
  links to the previous version). A task has a title, description, optional
  acceptance `check`, dependencies (`after`; edits that would create a cycle
  are refused), `status` (open, in_progress, done, cancelled; *blocked* is
  derived from unfinished dependencies, and cancelled dependencies count as
  finished), an `owner`
  (workspace + identity), the change ids `landed` for it, and `completion`
  evidence.
  - `task start` takes ownership (refusing blocked tasks and tasks owned by
    another live workspace unless forced) and sets the working change's task.
  - `land` appends landed change ids to their tasks (open -> in_progress) in
    the same compare-and-swap that advances main.
  - `task done` runs the acceptance check in a workspace that is exactly main
    (clean, parent == main), so the recorded `Check.tree` is main's tree.
- `view.claims`: `Claim {pattern, workspace, by, task, expires}`. A pattern
  is a path, a directory or a glob (`*` does not cross `/`). Two patterns
  overlap if either matches the other's text or both match a known file (from
  main and the claimant's tree), so claims on not-yet-created files work.
  A claim can also name one top-level declaration of a source file,
  `src/auth.rs::login` (by name, or `kind:name`; it must exist if the file
  does). Symbol claims on the same file overlap only if they name the same
  declaration, or the other claim covers the whole file; editing under
  someone's symbol claim is reported only when your changes add, remove or
  modify that declaration (or the file cannot be parsed).
  Claims are soft: overlapping claims are refused unless forced, expired
  claims are ignored and pruned, edits under another workspace's claim are
  reported (`claimed_by_others`) but never blocked. `land`, `task done` and
  `workspace remove` release the relevant claims.
- Task and claim operations are not attached to a workspace, and operations
  record the shared state they change (`shared`: `main`, `task:T-1`,
  `claims`). `undo` never reverts shared state: it skips task and claim
  operations and refuses to undo a land.
- **Message check (T-31, `msgcheck`).** `commit.check` names a command
  that vets a change's whole record (intent, task, metadata, author,
  committer, checks, and at land the lander), given as one JSON object on
  stdin; a non-zero exit refuses. It runs at `commit` and `describe` on the
  revision about to be written, at `land` on every change about to go onto
  main (after the sync, before the land check, so a refusal costs no suite
  run), and at `git export` on every revision about to be written (all
  before anything is written). psync's own metadata updates (`task start`
  setting the task) are not vetted. A refused change waiting to land is
  fixed with `describe -r`: the change gets the new metadata, and the
  changes after it (and the working change) are re-recorded on top with the
  same trees and change ids. It is a project rule, not a security
  boundary: whoever can run psync can change the setting (or set
  `PSYNC_COMMIT_CHECK`).
- **Partial commit**: `commit <paths>` builds the committed tree from the
  parent tree plus only the selected file changes; the working change keeps
  the full tree (and any conflicts outside the selection).

### Ignore rules

`ignores.rs`, with globs from `glob.rs`. Each directory's `.gitignore` and
`.psyncignore` apply to it and below, in git's syntax: `#` comments, `!` to
re-include, a trailing `/` for directories only, a `/` at the start or in the
middle anchors a pattern to the file's directory (else it matches a name at
any depth), `*` `?` `[..]` `**`, `\` escapes. The deepest directory with a
matching rule decides; within a directory the last match wins and
`.psyncignore` rules count after `.gitignore` ones. Lowest of all is the
user's global file, `~/.config/psync/ignore` (`$XDG_CONFIG_HOME/psync/ignore`).
As in git, an ignored directory is not entered, so nothing inside it can be
re-included. `.psync`, `.git` and nested repositories are always skipped,
symlinks are recorded, not followed. Git's own global settings
(`core.excludesFile`, `~/.config/git/ignore`, `.git/info/exclude`) are not
read: psync does not depend on Git's configuration.

### Line endings (T-27)

`eol.rs`. Off by default: files are stored byte for byte as they are on
disk. Conversion is opt-in, per path:

- The `text.eol` setting (`lf`, `crlf`, `native`; repository or global, or
  `PSYNC_TEXT_EOL`) treats every file as `text=auto` and checks text files
  out with that ending. A Windows user sets `native` and gets CRLF files
  while the repository holds LF; Mac and Linux users need nothing.
- `.gitattributes` and `.psyncattributes` at the workspace root (the
  second wins): `text`, `-text`, `binary`, `text=auto`, `eol=lf`,
  `eol=crlf` (implies `text`), `crlf`/`-crlf`; last matching line wins per
  attribute; a pattern with `/` is anchored at the root, otherwise it
  matches the file name. Attributes beat the setting, so `*.sh eol=lf` and
  `*.bat eol=crlf` hold for everyone. Directory, negated and quoted
  patterns, and attribute files below the root, are not read.
- A snapshot stores a converted file with CRLF → LF; a checkout
  (`checkout`, `materialize`) writes LF → CRLF where the ending is CRLF,
  keeping CRLFs already stored. Snapshots read the attribute files on
  disk; checkouts the ones in the tree being written, so a fresh clone
  uses the project's rules. `text=auto` skips files with a NUL in the
  first 8000 bytes; symlinks and files over 4 MiB (chunked) are never
  converted.
- As in git, changing the rules does not rewrite unchanged files: the stat
  cache still vouches for them. A file is converted when it next changes
  or is checked out. Turning conversion on normalizes CRLF files as they
  are edited (the first edit shows every line changed once).
- When off (no setting, no rules about text), the only cost is two failed
  `open`s per scan.

## Performance

- **Stat cache + parallel scan.** Unchanged files (same size, mtime, inode,
  kind, not modified within 2s of the last scan) are not read; changed files
  are hashed in parallel; the directory walk is parallel.
- **Tree cache.** The index also stores every directory's tree hash. Only
  directories containing a changed, added or removed file (and their
  ancestors) are re-encoded; all others reuse the cached hash. Property
  tested against full rebuilds.
- **Index saved only when it changed.**
- **Generation numbers.** Each revision stores `1 + max(parent generations)`.
  Ancestry queries (`is_ancestor`, `merge_base`, unlanded changes,
  ahead/behind) expand ancestors in decreasing generation order and stop at
  the generation they need, instead of walking all history. Revisions from
  before generations existed have generation 0, which forces a (correct)
  full walk through them.

- **Folder-only tree rebuild.** Only dirty directories are re-encoded, deepest
  first; each one's children are read from the sorted index by seeking past
  subdirectories, so cost follows the number of changed directories, not the
  size of the tree.

## File watcher (T-1)

`psync daemon start` runs one background process per workspace
(`watch::run`) that records changed paths from FSEvents/inotify, and
optionally snapshots the workspace ~300 ms after edits settle (skipping, and
retrying later, while a command holds the workspace lock). Watcher snapshots
are ordinary snapshot operations, marked `(watcher)`.

- **Protocol.** Newline-delimited JSON over a Unix socket (path in
  `.psync/workspaces/<ws>/daemon.json`; mode 0600). A client sends `since`,
  loads its index while the daemon syncs, then sends its token and gets back
  the changed paths and a new token, stored in the index.
- **No missed edits.** To answer, the daemon writes a cookie file into a
  watched directory and waits for that file's own event; events arrive in
  order, so everything before the request has been recorded (git fsmonitor
  and watchman do the same). The answer is `full` (client does a full scan)
  for a token from another daemon instance, after dropped events or an
  overflow (>100k paths), or if the cookie does not arrive within 2 s.
- **Incremental scan** (`wc::scan_changed`). A changed path that is a known
  file is re-stat'd directly; a known directory is rescanned recursively (it
  may have been replaced by a rename); anything else re-lists its nearest
  known directory, so new files and directories are found and ignore rules
  apply exactly as in a full walk (the walk still starts at the root but only
  enters the needed directories). Changes to ignore files or nested
  workspaces force a full scan. Property-tested against full scans. (The
  global ignore file lives outside the workspace: after editing it, run
  `psync daemon stop`, or the watcher keeps the old rules until it restarts.)
- `watch.auto = true` starts the watcher on the first command in a workspace.
  `workspace remove` stops the workspace's watcher.
- **Ignored directories (T-25).** Events where a scan would not look
  (ignored paths, nested workspaces, `.git`) are dropped, by the walk's own
  rules (`watch::Filter`, which remembers each directory's decision and
  forgets it when an ignore file or `.psync` appears or changes, or a
  directory is created or moved in). So a build writing into `target/`
  neither fills the changed-path list nor triggers snapshots. Read-only
  events are dropped too: inotify reports opens and reads, which made every
  scan (it reads ignore files) look like an ignore-file change and so a full
  scan, and made the watcher's own snapshots trigger more snapshots.
- **Watching directory by directory** (`watch::PER_DIRECTORY`, Linux and
  other non-macOS Unix). inotify needs one watch per directory, so watching
  the tree recursively would also watch every directory in `target/` and
  `node_modules/`, and could exhaust `fs.inotify.max_user_watches`. The
  daemon watches each directory a scan enters, one by one. A new directory
  is watched (with everything below it) by a separate thread (the event
  callback cannot: the watcher may be waiting for it) and then recorded as
  changed again, so a later scan sees whatever was written before its watch
  existed. A changed ignore file re-decides every watch. A watch that cannot
  be added (the limit) is reported in `daemon status` and every answer
  says `full`, so no edit goes unseen. FSEvents and Windows watch a tree for
  free, and are left watching the whole tree.
  Measured in Docker (Linux, aarch64): a tree of 11,056 directories, of
  which about 11,000 are under ignored `target/` and `node_modules/`: 53 inotify
  watches (52 directories and the cookie directory), not one per directory;
  writing 2,000 files under `target/` delivers no events at all.

Measured (Apple Silicon, release build, median of 15 runs including ~2.5 ms of
process spawn): `status` with 20k files and no changes 47 ms without the
watcher, 13 ms with it (most of that is the OS delivering the sync cookie,
5-11 ms); `status` on a 1,000-change history 11 ms.

## Remotes (T-7)

Repositories share exactly their shared state: `main`, the tasks, the
claims, the revs, and (read-only) where each workspace is. Workspaces are
worked on only in their own repository, and each repository keeps its own
operation log; fetches and pushes are operations on the side they change
(`shared`: `main`, `<remote>/main`, `tasks`, `claims`, `revs`; never
undone).

- **Remotes** are listed in `.psync/remotes.json` (name -> URL): a local path,
  `ssh://[user@]host[:port]/path` or `[user@]host:path`. Over ssh, psync runs
  `psync remote-serve <path>` on the other machine (`PSYNC_SSH`,
  `PSYNC_REMOTE_PSYNC` override the commands) and speaks newline-delimited
  JSON with binary object payloads on its stdin/stdout.
- **Transport** (`transport`): `view`, `spine`, `has`, `get`, `put`,
  `advance`, `share`, implemented for a local repository (direct file access) and
  over ssh; fetch and push use the same algorithm for both. The *spine*
  (revisions and task versions reachable from the roots, stopping at ones the
  receiver has) is listed in one request; trees and blobs are then requested
  level by level, so round trips grow with directory depth, not history
  length. Objects travel compressed, as stored.
- **Integrity:** every received object is decompressed and its hash checked
  before it is stored (both directions), and objects are written after
  everything they reference, so an object's presence implies its closure and
  an interrupted transfer leaves no dangling references. The receiving side
  of a push checks itself that main is a fast-forward and that all objects
  arrived before moving main.
- **Bounded memory (T-22):** a `get` returns at most `max_bytes` (16 MB;
  at least one object); older clients, which do not send it, get everything
  as before. Received files are stored at once (they refer to nothing);
  trees, revisions and tasks wait in temp files and are stored referrers
  last. A push keeps only hashes and references and reads each object from
  disk as it sends it, in batches of at most 16 MB. Cloning 400 MB: client
  470 → 63 MB, ssh server 415 → 40 MB; pushing 400 MB: 510 → 53 MB. A
  single object is at most a 4 MiB chunk (see Large files).
- Measured (20k files): clone over ssh to localhost 11.1 s, from a local path
  7.9 s (mostly writing the files); a push with nothing new sends nothing.
- **fetch** copies the remote's objects, sets the bookmark `<remote>/main`,
  and merges tasks, claims, workspace heads and revs. **pull** then fast-forwards main, or, if both sides
  landed different changes, sets main to the remote's and syncs the current
  workspace (which must contain local main): the local-only landed changes
  become unlanded commits there and are landed again with the check. Other
  workspaces that still have the old revisions skip them on their next sync,
  because `sync` drops changes whose change id is already on main.
- **push** requires the remote's main to be an ancestor of local main
  (otherwise: pull first), copies objects, and advances the remote's main
  with a compare-and-swap under the remote's repository lock.
- **Tasks** merge per id: if one version descends from the other (via
  `previous` and `merged_from`), the newer wins; if both changed, the version
  updated last wins and a merge version (`merged_from` = the other) is
  written. The merge is deterministic, so both sides converge on one object.
- **Claims (T-26)** travel with every fetch (in `view`) and push (in
  `advance`), so agents in separate clones of one hub see each other's
  claims. Each repository has a random id (`.psync/id`); a claim made
  elsewhere carries its `home` (id + `host:/path`). A repository sends its
  own active claims and those it has from others, plus `claim_stamps`:
  for each home, when that home's claims last changed (set only by the home,
  on claim, release and land, so clock differences between machines do not
  matter). The receiver replaces everything it has from a home with a set
  that has a newer stamp, so releases propagate, a set relayed late cannot
  undo a newer one, and a repository's own claims are never replaced by
  echoes. A push also takes the remote's claims back. Claims from other
  repositories count like other workspaces' claims (overlapping claims are
  refused, edits under them are warned about in `status`, `context` and
  `land`), and only their home releases them. Older psync versions ignore
  the extra fields; copying a `.psync` directory copies its id.
- **Workspace heads (T-29)** travel with the claims, the same way: each
  repository sends its workspaces' working changes (and those it has from
  others) in the same per-home set, stamped when a workspace or a claim
  changes, as `View::remote_heads` on the receiving side (`Shared::heads` on
  the wire). Their revisions are fetched and pushed with main, so a hub
  relays every clone's work to every other. They are roots for gc.
- **Revs (T-29)** (`view.revs`) travel in `view` and `advance` too; a rev
  new to the receiver is added, one naming another revision there is kept
  (with a note), so both repositories keep their own. `rev add --remote`
  sends the revision's objects and a `share` request (revs + shared set)
  that does not touch main; older psync versions answer it with an error,
  and ignore the extra fields otherwise.
- **clone** creates a repository, adds `origin`, copies `land.check`, pulls,
  and puts the default workspace on top of main.

## Git bridge (T-8)

Uses the `git` command's plumbing only; psync never needs Git otherwise.

- **Import** reads the branch's first-parent history with one
  `git log --raw -z --diff-merges=first-parent` and every needed blob with
  one `git cat-file --batch`. Each commit becomes a revision (message as
  intent, author/committer and dates kept, `git_commit` metadata) whose change
  id is derived from the commit hash, so imports are repeatable. A later
  import continues from main's `git_commit` if Git still contains it and
  main has nothing psync-only on top. Submodules (mode 160000) are skipped.
  An untouched working change is synced onto the new main.
- **Export** walks main back to the newest revision Git already has (its
  `git_commit`, or `.psync/git-export.json` from earlier exports), then
  streams the rest to `git fast-import` as commits with only the changed
  files. The branch is only fast-forwarded; a branch with foreign commits or
  unrelated history is refused. The Git working tree is not touched.
- Measured: importing a 20k-file repository (including writing the files
  into the workspace) 5.9 s; exporting it 1.1 s.

## MCP server (T-2)

`psync mcp` speaks MCP (JSON-RPC 2.0, newline-delimited, over stdio;
protocol versions 2025-06-18, 2025-03-26, 2024-11-05). Tools are a table
mapping arguments to command-line arguments (positional values go after
`--`, so a path like `-x` is never read as an option); a call parses them
with the CLI's own parser and runs the command in-process with `--json`.
Results are returned as text (JSON) and, from 2025-06-18, as
`structuredContent`; `{"ok": false}` results and errors become tool errors
(`isError`), not protocol errors, so the agent sees the message. Read-only
tools carry `readOnlyHint`. Without a configured agent, work is attributed to
the client's `clientInfo.name`. Not exposed: `init`, `config`, `daemon`,
`workspace remove`, `op restore`.

## HTTP API (T-3)

`psync daemon start --http [ADDR]` makes the watcher daemon also serve the
MCP tool table over HTTP/1.1 (std `TcpListener`, a thread per connection,
`Connection: close`). A call runs exactly what an MCP call runs (the CLI
command in-process with `--json`), so all three interfaces agree.

- `GET /v1/health` (no token), `GET /v1/tools`, `POST /v1/tools/<name>`
  with a JSON object body, and `GET /v1/tools/<name>?k=v` for read-only
  tools (query values are converted to the argument's schema type).
- Status 200 on success, 422 when the command reports `"ok": false`,
  400/401/404/405/413 for malformed, unauthenticated, unknown, wrong-method
  or oversized (> 8 MB) requests.
- **Auth:** a random 256-bit bearer token, compared in constant time,
  published with the URL in `.psync/workspaces/<ws>/http.json` (mode 0600)
  and removed when the daemon exits. The default bind address is loopback.
- `X-Psync-Agent` sets the agent for one request (default: the daemon's).

## Platforms (T-9)

OS differences live in `platform`:

| | Unix | Windows |
|---|---|---|
| change detection | size, mtime (ns), inode | size, mtime (100 ns); no inode |
| executable bit | recorded and restored | none: files keep their recorded kind (git `core.filemode = false`) |
| symlinks | real | real when permitted (developer mode/admin), else a file holding the target (git `core.symlinks = false`) |
| daemon connection | Unix socket, mode 0600 | loopback TCP port |
| owner-only files (`daemon.json`, `http.json`) | mode 0600 | user profile ACLs |
| checks (`land --check`, task acceptance) | `sh -c` | `cmd /C` |
| background daemon | new process group | `DETACHED_PROCESS` |
| global config | `$XDG_CONFIG_HOME` or `~/.config` | same, else `%APPDATA%` |

Clients of the daemon always send its random key (from the owner-only
`daemon.json`) first, so on Windows, where the port is reachable by other
local users, only the owner can talk to it. Canonical paths drop Windows'
`\\?\` prefix; event paths and user patterns use `/`.

## Revision specs

The syntax is in [USAGE.md](USAGE.md#revisions). Resolution order: trailing
`-`s are stripped and followed to first parents at the end; then `@`, a
bookmark (`main`, `<remote>/main`), a rev, `<workspace>@[<where>]` (a local
workspace first when `<where>` is empty, then other repositories' heads by
host or id prefix), and finally a change-id or hash prefix, searched in the
current workspace's history first, then in all workspaces.

## Revs, other repositories' workspaces, switch (T-29)

`lines.rs`. psync has no branches or tags: workspaces are the lines of work,
and a **rev** is a name for one revision, `psync rev add try-auth claude-1@`.
A rev never moves, so it means the same thing in every repository; a rev
name cannot read as a change id or a revision hash.

**switch** points the current workspace at another line: a new working
change continuing the target, its files checked out, and the working change
left behind kept in `View::parked` (`switch -` returns to it; `undo` undoes
the switch). Continuing a revision depends on whether it is committed:

- committed (an ancestor of main, or below some workspace's working
  change): the new change goes on top of it;
- otherwise it is a snapshot of unfinished work (another workspace's
  working change, or a rev made from one), whose change may still grow and
  land later. The new change is a copy of it (same tree, parents, intent,
  task and conflicts) with a new change id. Building on top instead would
  put the owner's change id on main first, and `sync` drops changes whose
  id is already on main: the owner's later edits would be lost. With a
  copy, the worst case is the same edit twice in history, which merges
  cleanly (or, if both sides changed it further, as a conflict to settle).

`workspace add -r <rev>` starts a workspace the same way. Other
repositories' workspaces appear in `context` (with overlaps), `log --all`,
and as revisions (`claude-1@laptop`) for `diff`, `merge`, `switch`.

## Roadmap

| Milestone | Scope | Status |
|-----------|-------|--------|
| M0 | Object store, auto-snapshot, changes + metadata, op log, undo, JSON CLI | done |
| M1 | Workspaces (CoW), workspace-scoped undo, owner + agent identity, `config`, `context` with overlaps, `log --all` | done |
| M3 | `main`, `sync`, `land --check` (compare-and-swap, retried), `merge`, three-way merge, recorded conflicts, check evidence | done |
| M2 | Tasks (acceptance checks, dependencies, owners, progress from landed changes), claims (paths/globs with expiry), partial commit | done |
| — | Speed: tree cache, index saved only when changed, generation numbers for ancestry; task edit/cancel | done |
| T-1 | File watcher daemon: incremental scans, live snapshots | done |
| T-2 | MCP server (`psync mcp`) | done |
| T-3 | HTTP API in the daemon (`daemon start --http`) | done |
| T-4 | `revert` | done |
| T-5 | Structural merge of source files (tree-sitter) | done |
| T-6 | Claims on declarations (`file::name`) | done |
| T-7 | Remotes: `clone`, `fetch`, `pull`, `push` of main and tasks (local paths) | done |
| T-8 | Git bridge: `git import`, `git export` | done |
| T-9 | Windows port (`platform`); compiles and builds, not yet run on Windows | done |
| T-10 | Remotes over ssh (`transport`, `remote-serve`) | done |
| T-21 | Crash safety (flush before `op_head` moves), `check`, `gc [--keep AGE]` | done |
| T-22 | Bounded-memory transfer (`get` capped by bytes, staged receive, streaming push) | done |
| T-23 | Pack files: automatic background packing, `gc` repacks reachable objects | done |
| T-24 | Large files: content-defined chunks, streamed snapshot and checkout | done |
| T-25 | Watcher: drop events in ignored paths, per-directory watches on Linux | done |
| T-26 | Claims shared through remotes (per-repository sets, newest wins) | done |
| T-27 | Line-ending conversion: `text.eol`, `.gitattributes` text/eol attributes | done |
| T-28 | Quiet exit on a closed output pipe | done |
| T-29 | Revs (named revisions, also created directly on a remote), other repositories' workspaces, `switch` | done |
| T-31 | Message check (`commit.check`) on commit, describe, land and git export; `describe -r` | done |
| T-32 | Land queue (`land.queue`): lands take turns under a renewed lease, each check runs once | done |
| T-33 | Batch land (`land <ws>@...`): several workspaces' changes under one check, each keeping its change id | done |
| T-34 | Author attribution: agentless snapshots never take a change from an agent; an unnamed change is the committer's | done |
| T-35 | `restore <rev> -- <paths>` (byte-exact, undoable) and `cat <rev>:<path>` | done |
| T-36 | `id`: last commit and dirty state, no operation, no lock | done |
| T-37 | `status --ignored`, `check-ignore` (rule file:line), `commit.max_file_mb` guard | done |
| T-38 | `archive`: reproducible ustar/pax tar of a revision, revision hash as pax comment | done |

The rest of the roadmap is tracked as psync tasks in psync's own repository
(`psync task list`).

## Known limits

- Tested on macOS. Windows: `cargo check`, clippy and a release build of
  `psync.exe` succeed (cross-compiled with mingw-w64), but it has not been
  run on Windows yet. Linux: the test suite passes in Docker (aarch64). Non-UTF-8 paths are
  rejected. Line endings are converted only when asked (`text.eol`,
  attributes; see Line endings).
- Over ssh, `clone` does not copy the remote's `land.check` and
  `commit.check` settings.
- Claims and workspace heads reach another clone only through `push` and
  `pull`/`fetch`; they are not sent on their own.
- Landing rebases (linear main); merge parents are not preserved on main.
- Revisions from before generation numbers make ancestry walks visit all of
  that older history.
- Library callers must call `Repo::snapshot` before other methods to pick up
  edits on disk (the CLI does this automatically).
- Diff and merge of files larger than 4 MiB are binary only.
- Tree payloads are not self-describing: changing the tree shape requires
  bumping `FORMAT_VERSION`.
