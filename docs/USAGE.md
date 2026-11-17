# Using psync

How to work with psync, command by command. The [README](../README.md) has
the short version; [DESIGN.md](DESIGN.md) explains how it works inside.

- [Basics](#basics): changes, the working change, undo
- [Identity](#identity)
- [Several agents](#several-agents): workspaces, `context`, main, land, sync
- [Conflicts](#conflicts)
- [Tasks](#tasks) and [claims](#claims)
- [Message check](#message-check): a project's rules for change records
- [Pinning results](#pinning-results): naming the exact code a result came from
- [Remotes](#remotes): clone, pull, push, over ssh
- [Other agents' work](#other-agents-work): other clones' workspaces, revs, switch
- [Files](#files): ignore rules, line endings, large files
- [Git bridge](#git-bridge)
- [MCP server](#mcp-server) and [HTTP API](#http-api)
- [File watcher](#file-watcher)
- [Maintenance](#maintenance): check, gc
- [Revisions](#revisions): how to name a change
- [Commands](#commands)
- [For agents](#for-agents)

## Basics

```sh
psync init                                  # current files become the first change
psync commit -m "Project skeleton"
# ...edit files; no `add` needed...
psync status                                # working change (@) and changed files
psync commit -m "Add rate limiting" --task T-14 --meta model=opus-5.5
psync log
psync show @-                               # metadata + diff of the previous change
psync undo                                  # undo the last operation (repeatable)
```

- **No staging area.** The working directory *is* the working change, `@`,
  and every command snapshots it first. `commit` finishes `@` and starts a
  new empty one on top; `describe` changes `@`'s metadata without finishing it.
- **Partial commit.** `psync commit -m "..." <paths>` commits only those
  paths (files, directories, globs); the rest stays in your working change.
- **Changes carry meaning.** `-m` is the change's *intent*, `--task` links it
  to a task, `--meta key=value` adds anything else (model, session, test
  results; `key=` clears one).
- **Change ids.** A change keeps its id (letters `k`-`z`, e.g. `wvxsyk`)
  when it is amended or rebased; each version of it is a revision (a hex hash).
- **Everything is undoable.** `undo` reverses this workspace's last operation,
  including edits captured by a snapshot; repeat it to go further back.
  `psync op log` lists operations and `psync op restore <op>` puts this
  workspace back to any of them. Undo never reverts shared state (a land,
  tasks, claims), so one agent cannot roll back another's work.
- **`revert <rev>`** undoes a change that is already in history (for example,
  a landed one) in a new working change; then commit and land it.
- **`restore <rev> -- <paths...>`** brings files or directories back exactly
  as they are in a revision (byte for byte, large and binary files included),
  without touching the rest of your working change: e.g. files a cleanup
  deleted. Files under those paths that the revision lacks are removed.
  `undo` reverts it. **`cat <rev>:<path>`** prints one file's exact bytes at a
  revision, without touching the working copy.
- **`archive <rev> [paths] [--prefix dir/] -o app.tar`** writes exactly the
  files of a revision (no untracked or uncommitted files) as a tar, to build
  or deploy it elsewhere. Executable bits and symlinks are kept, and the same
  revision always gives the same bytes. The revision hash is stored in the tar
  (pax `comment`, as `git archive` does) and reported (`--json`). Without
  `-o` the tar goes to stdout (e.g. `| ssh gpu tar -x -C /srv/app`).
- **`id`** tells a build script which revision it is building: the last
  commit's hash, with `-dirty` if files differ from it (`--json` for details;
  paths limit what counts). It records nothing and never waits for a running
  command, so it can run on every build:
  `psync --json id src/ Cargo.toml` -> `{"revision": "...", "dirty": false, ...}`.

## Identity

Every change and operation records the **owner** (the accountable human) and
the **agent** acting for them:

```sh
psync config --global user.name "Ada Lovelace"   # imported from git on first init
psync config --global user.email ada@example.com
export PSYNC_AGENT=claude-1                      # or --agent; omit when you act yourself
```

## Several agents

```sh
psync workspace add claude-1                # ../<repo>-claude-1, files cloned copy-on-write
cd ../myrepo-claude-1
export PSYNC_AGENT=claude-1
psync context                               # your work, main, other workspaces, overlaps
psync commit -m "..." --task T-14
psync land --check "cargo test"             # sync onto main, run the check, put your work on main
psync sync                                  # bring others' landed work into your workspace
psync log --all                             # history across all workspaces
psync diff codex-1@                         # look at another agent's working change
psync merge codex-1@                        # pull another agent's work in directly
```

- **A workspace per agent.** Each is a directory with its own working change.
  `workspace add` starts it on `main` (or `-r <rev>`); files are copied
  copy-on-write where the file system can (APFS, btrfs, XFS).
  `workspace list`, `workspace remove <name>` (the work stays in history).
- **`context`** is the briefing for an agent: its workspace, working change
  and task, the ready tasks, how far ahead or behind main it is, recent
  history, what every other workspace changed, claims, and the files where
  its changes **overlap** with someone else's (likely conflicts, before any
  merge happens). Others' state is as of their last psync command, or live
  when they run the [file watcher](#file-watcher).
- **`main`** is the shared line of finished work, and stays linear.
  `land` puts your committed changes onto it only when your check passes
  against exactly the code being landed, and records the result on the
  change (`psync show main`). It needs a clean working change; if another
  agent landed meanwhile, it syncs and checks again. Set a default check once:
  `psync config land.check "cargo test"` (`--no-check` skips it). A land
  cannot be undone (main is shared): `revert` the change instead.
- **Long checks: the land queue.** If another agent lands while your check
  runs, `land` syncs and runs the check again (up to 3 times). With
  `psync config land.queue true`, lands take turns instead: one land at a
  time holds main while it syncs, runs its check and lands, and the others
  wait in line (`Waiting to land: claude-2 holds main ...`), so each check
  runs once. `context` shows who holds main and who waits. The hold lasts
  as long as the land runs, however long its check takes; if that process
  dies, the next land goes ahead within a minute. The queue orders the
  workspaces of one repository; other clones land in their own and meet at
  `push`/`pull`.
- **Batch land: several workspaces under one check.** To merge a batch of
  work and run the full suite once:

  ```sh
  psync land claude-1@ claude-2@ codex-1@laptop --check "<suite>"
  ```

  Your committed changes (if any) and each named workspace's committed
  changes are stacked on main in that order, checked once, and land
  together. Each change keeps its own change id, author, intent and task,
  so main still shows who did what; the named workspaces' claims are
  released, and each owner's next `sync` finds its work on main. Only
  committed changes land: uncommitted work stays with its owner. If a
  change conflicts with main or with the changes before it, nothing lands
  and the error names it: land without that workspace, or have its owner
  `sync` and resolve first. If the check fails, nothing lands and your
  files are put back. Another clone's workspaces can be named too (after a
  `pull`); `push` then sends the result, and their next `pull` and `sync`
  drop what landed.
- **`sync`** rebases your work onto the latest main. **`merge <rev>`** brings
  another change into your working change directly.

## Conflicts

`sync`, `land` and `merge` never get stuck on conflicts. Conflicting files get
markers (`<<<<<<< main` / `||||||| base` / `=======` / `>>>>>>> yours`), the
conflict is recorded on your working change, and you keep working. Edit the
file to remove the markers and the conflict is resolved; then `commit` and
`land`. `psync resolve` lists conflicts; `psync resolve <path> --force`
accepts a file as is. `commit` and `land` refuse while conflicts remain.

In Rust, Python, JavaScript, TypeScript and Go files, conflicts the line
merge cannot resolve are retried declaration by declaration (tree-sitter):
two agents adding different functions at the same place, or editing
different functions next to each other, merge cleanly.

## Tasks

```sh
psync task add "Rate-limit logins" --check "cargo test rate_limit"   # T-1, with an acceptance check
psync task add "Document rate limits" --after T-1                     # T-2, blocked until T-1 is done
psync task list                                                       # status, owners, progress (--all: done ones too)
psync task start T-1          # take it: you own it, and your working change belongs to it
# ...work, commit, land: landing records progress on T-1...
psync task done T-1           # runs the acceptance check against main, records the result
```

- A task is **ready** when it is open, has no unfinished dependencies and no
  owner; `psync context` lists the ready ones, so an agent can just pick one.
- `task show`, `task drop` (give it back), `task edit <id> [--title]
  [-d] [--check] [--after]`, `task cancel <id>` (a cancelled dependency
  counts as finished).

## Claims

```sh
psync claim src/auth/ --ttl 1h        # tell others you are working there
psync claim src/auth.rs::login        # or on one declaration of a source file
psync claim                           # list active claims
psync release                         # release yours (or name paths)
```

**Claims are soft locks.** A claim is a path, a directory, a glob (`*` does
not cross `/`) or a declaration (`file::name`). Overlapping claims are
refused (unless `--force`), claims expire (`--ttl`, default 30m), and editing
a file someone else claimed is reported in `status`, `context`, `land` and
`--json` output, not blocked. `land`, `task done` and `workspace remove`
release the relevant claims.

## Message check

A project can have rules about what a change may record: no attribution
trailers, no private details, no tool or brand names, a message format. Put
them in a script and point `commit.check` at it:

```sh
psync config commit.check "scripts/msg-check"   # the repository's config; `clone` copies it
```

psync runs the script for each change, with the change's whole record as
JSON on stdin; a non-zero exit refuses the change, and the script's output
is shown as the reason:

```json
{"stage": "commit", "revision": null, "change_id": "wvxsykqm...",
 "intent": "Add rate limiting", "task": "T-14", "meta": {"session": "abc"},
 "author": {"name": "Ada", "email": "ada@example.com", "agent": "claude-1"},
 "committer": {"name": "Ada", "email": "ada@example.com", "agent": "claude-1"},
 "checks": [], "landed_by": null}
```

It runs at four points (`stage`, also in `$PSYNC_STAGE`):

| Stage | When | What to do if refused |
|-------|------|-----------------------|
| `commit` | before a change is committed | change `-m` / `--meta` and commit again |
| `describe` | `psync describe` (the intent may still be empty here) | describe again |
| `land` | for every change about to go onto main, before the land check runs; `landed_by` is who lands it | `psync describe -r <change> -m ...` (or `--meta key=` to drop a field), then land again |
| `export` | for every revision `git export` is about to write; nothing is written if one is refused | the change is on main already; `git export --no-commit-check` exports anyway |

`land` catches changes recorded before the check was set, or brought in by
`merge` or `pull`. `git export` writes only the intent, author and committer
(name and email) into Git: never the agent, the task or `--meta` fields. A
minimal script:

```sh
#!/bin/sh
# scripts/msg-check: refuse brand names anywhere in the record
if grep -qiE 'acme|widgetco'; then echo "no brand names in change records"; exit 1; fi
```

## Pinning results

To tie a result (a benchmark, a training run, a report) to the exact code it
came from:

- **From main:** record the revision hash (`psync log --json`). Main only
  moves forward, so a revision on it keeps its hash and is never collected.
- **From work not yet landed:** `psync rev add run-42 @` names the exact
  state first, and the result records `run-42`. A rev never moves and is
  never collected; `psync switch run-42` (or `workspace add -r run-42`)
  brings that code back. Run-launch scripts can do this every time:
  `psync --json rev add "$RUN_ID" @`.
- One exception for main: when a `pull` finds both repositories landed
  different changes, your local-only landed changes land again on top, with
  new hashes. Pin those with a rev.

## Remotes

Repositories exchange `main`, the tasks, the claims, the revs, and where
each workspace is (read-only). A remote is another psync repository: a local
path, or a machine reachable over ssh (with psync installed there).

```sh
psync clone ../app app-laptop         # new repository, origin = ../app
psync clone ssh://me@server/srv/app   # or server:srv/app; runs `psync remote-serve` there
psync pull                            # fetch origin, fast-forward main
psync push                            # send main, tasks, claims, revs (fast-forward only)
psync remote add backup /mnt/backup/app
psync fetch backup                    # see backup/main without moving main
```

- If both repositories landed different changes, `push` refuses and `pull`
  makes main the remote's, moving your local-only landed changes back into
  your workspace: `psync land` re-lands them on top (running the check),
  then `psync push`.
- Tasks changed on both sides merge per task (the version updated last
  wins; the other stays in its history).
- Claims made in another clone show up after a `pull` (`push` sends yours),
  marked with where they were made, and count like any other workspace's
  claims; only the clone that made a claim releases it.
- `PSYNC_SSH` sets the ssh command (e.g. `ssh -i key`) and
  `PSYNC_REMOTE_PSYNC` the psync path on the remote machine.

## Other agents' work

psync has no branches or tags. Every workspace is a line of work, and after
a `pull` you also see the workspaces of other clones (pushed to the same
remote), named `<workspace>@<host>`:

```sh
psync context                       # lists claude-1@laptop with its files and overlaps
psync diff claude-1@laptop          # or claude-1@ when the name is unique
psync switch claude-1@laptop        # test it, or continue it, right here
psync switch -                      # back to your own working change (it was parked)
psync rev add try-auth              # name your current state for good (a rev never moves)
psync rev add try-auth --remote origin   # ...also directly on the remote, main untouched
psync switch try-auth               # in another clone, after a pull
psync rev list                      # rev remove <name> forgets one here
```

- **switch** checks out the target and gives you a new working change to
  continue in. The one you leave is parked (`status` reminds you), and
  `switch -` returns to it. For a separate directory instead:
  `psync workspace add mine -r claude-1@laptop`.
- Switching onto someone's **unfinished** work gives you a copy of it with
  its own change id, so you can both land without one replacing the other;
  committed work (on main, or below someone's working change) is continued
  directly.
- A **rev** names one revision for good. Rev names cannot look like a change
  id or a revision hash. If two repositories give one name to different
  revisions, each keeps its own and the fetch prints a note.
- `claude-1@laptop` is workspace `claude-1` of the clone on host `laptop`;
  when two clones on one host have it, use the repository id prefix that
  `context` shows. Local workspaces win over other clones' for `claude-1@`.
  Idle ones (nothing changed) are left out of `context`.

## Files

- **Ignore rules.** `.psyncignore` and `.gitignore` in any directory, and the
  global `~/.config/psync/ignore` (`$XDG_CONFIG_HOME/psync/ignore`), in git's
  syntax. `.psyncignore` rules count after `.gitignore` ones. `.psync`, `.git`
  and nested repositories are always skipped; symlinks are recorded, not
  followed. Git's own global settings are not read.
- **What a commit takes.** There is no staging area, so every file that is
  not ignored goes in. `psync status --ignored` lists what is left out (an
  ignored directory once, not its contents); `psync check-ignore <paths>`
  names the rule that decides each path (`.psyncignore:2  weights/`), and
  works for paths that do not exist yet. To guard against model weights or
  datasets slipping in, `psync config commit.max_file_mb 50`: `commit`
  then refuses files above 50 MB unless `--allow-large`, naming them.
- **Line endings** are stored as they are unless asked:
  - `psync config --global text.eol native` (or `lf`, `crlf`; env
    `PSYNC_TEXT_EOL`): text files are stored with LF and checked out with
    that ending (Windows: CRLF files, LF in the repository).
  - `text`, `-text`, `binary`, `text=auto`, `eol=lf|crlf` attributes in a
    root `.gitattributes` or `.psyncattributes` hold for everyone, e.g.
    `*.sh eol=lf`, `*.bat eol=crlf`.
  - Changing the rules does not rewrite unchanged files; a file is converted
    when it next changes or is checked out.
- **Large files** (over 4 MiB) are stored in chunks, so an edit stores only
  the changed parts and memory use stays small. Diff and merge treat them as
  binary.

## Git bridge

```sh
psync init && psync git import ../project-git        # Git history -> main (re-run to continue)
psync git export ../project-git --branch psync       # main -> Git commits (fast-forward only)
```

Import keeps each commit's message, author and dates (first-parent history;
submodules are skipped) and gives the same change ids on every import.
Export writes commits with `git fast-import` and continues from the last
revision Git already has; the Git working tree is not touched.

## MCP server

`psync mcp` serves psync as [Model Context Protocol](https://modelcontextprotocol.io)
tools over stdio, so agents call psync directly instead of through a shell:
`context`, `status`, `log`, `show`, `diff`, `commit`, `describe`, `sync`,
`land`, `merge`, `switch`, `revert`, `restore`, `cat`, `id`, `archive`, `check_ignore`, `resolve`, `undo`, `pull`, `push`,
`task_*`, `claim`, `release`, `rev_add`, `rev_list`, `workspace_list`,
`workspace_add`, `op_log`. Each tool runs the matching command with
`--json`, so results and errors are the same as on the CLI.

```sh
claude mcp add psync -- psync mcp                  # Claude Code, in the workspace directory
claude mcp add psync -- psync -C ../app-claude-1 mcp --agent claude-1
```

Any MCP client works the same way (`{"command": "psync", "args": ["mcp"]}`).
Work is attributed to `--agent`/`PSYNC_AGENT`, or else to the client's name
from the MCP handshake.

## HTTP API

`psync daemon start --http [ADDR]` also serves the MCP tools over HTTP
(default `127.0.0.1`, any free port), for tools and dashboards that don't
speak MCP:

```sh
psync daemon start --http
I=.psync/workspaces/default/http.json        # url + token, mode 0600
curl -H "Authorization: Bearer $(jq -r .token $I)" "$(jq -r .url $I)/v1/tools/context"
curl -X POST -H "Authorization: Bearer ..." -d '{"title": "Fix login"}' "$URL/v1/tools/task_add"
```

`GET /v1/tools` lists the tools; `POST /v1/tools/<name>` runs one with a
JSON object of arguments; read-only tools also take `GET` with query
arguments. Results are the commands' `--json` output (status 422 when
`"ok": false`). `X-Psync-Agent` attributes a request to an agent.

## File watcher

A per-workspace background watcher makes commands look only at the files that
changed (20k files: `status` 47 ms -> 13 ms) and snapshots your edits once they
settle, so other agents see your work live in `psync context`:

```sh
psync daemon start              # or: psync config watch.auto true (start on first command)
psync daemon status
psync daemon stop
```

It never misses an edit: each command first syncs with the OS event stream,
and falls back to a full scan whenever the watcher cannot vouch for the result.
`--no-snapshot` keeps the speed-up without automatic snapshots. After editing
the global ignore file, restart it (`psync daemon stop`).

## Maintenance

- `psync check` verifies every object the repository refers to; it exits 1
  if something is missing or damaged.
- `psync gc` packs objects and removes unreachable ones (objects less than a
  day old are kept, so it is safe while agents work); `--dry-run` shows what
  it would remove. `gc --keep 30d` also forgets operations older than 30 days
  (`undo` and `op restore` stop there; change history stays). Packing also
  happens automatically in the background.

## Revisions

Anywhere a command takes a revision:

| Spec | Means |
|------|-------|
| `@` | your working change |
| `@-`, `@--`, `claude-1@-` | parent, grandparent (append `-` to any spec) |
| `claude-1@` | workspace `claude-1`'s working change (here, or another clone's if unique) |
| `claude-1@laptop` | workspace `claude-1` of the clone on host `laptop`, as of the last fetch |
| `main`, `origin/main` | the shared line, and a remote's as of the last fetch |
| `try-auth` | a rev |
| `wvxsyk` | a change-id prefix (letters `k`-`z`) |
| `b3fdbdd4` | a revision-hash prefix |

## Commands

| Command | Purpose |
|---------|---------|
| `init [path]` | create a repository |
| `status [--ignored]` / `st` | working change and its files (and what is ignored) |
| `check-ignore <paths...>` | whether each path is ignored, and by which rule (file:line pattern) |
| `commit -m <intent> [--task T] [--meta k=v] [--allow-large] [paths]` | finish `@` (or only `paths`), start a new working change |
| `describe -m/--task/--meta [-r REV]` | edit `@`'s metadata without finishing it (or a committed change not yet on main) |
| `log [-n N] [--all]` | history (this workspace, or all) |
| `show [rev] [--stat]` | one change with diff |
| `diff [rev] [--stat] [--name-only]` | changes introduced by a change |
| `context [-n N]` | briefing: your work, main (ahead/behind), other workspaces, overlaps |
| `land [workspaces...] [--check CMD] [--no-check]` | put committed changes onto main after the check passes (with workspaces: theirs too, under one check) |
| `sync` | rebase your work onto the latest main |
| `merge <rev>` | merge another change into your working change |
| `switch <rev>`, `switch -` | point your workspace at another line of work (yours is parked), or back |
| `rev add <name> [rev] [--remote R]`, `rev list`, `rev remove` | name a revision for good, here or also on a remote |
| `revert <rev>` | undo a change (e.g. a landed one) in a new change; then commit and land |
| `restore <rev> [--] <paths...>` | make files/directories exactly as in a revision; the rest of `@` untouched; undoable |
| `cat <rev>:<path>` (or `cat <rev> <path>`) | print a file's exact bytes at a revision (`--json`: content, hash, size) |
| `archive <rev> [paths...] [--prefix P] [-o FILE]` | a revision's files as a reproducible tar, revision hash inside |
| `id [paths...]` | the last commit's hash, `-dirty` if files differ (`--json`: revision, change_id, dirty, changed, on_main); records nothing, takes no lock |
| `resolve [path] [--force]` | list conflicts, or mark one resolved |
| `task add/list/show/start/drop/done` | plan work, take it, finish it with an acceptance check |
| `task edit <id> [--title] [--check] [--after]`, `task cancel <id>` | change or cancel a task |
| `claim [paths] [--ttl 30m] [--force]` | claim paths, or declarations as `file::name` (no args: list claims) |
| `release [paths]` | release your claims (no args: all) |
| `workspace add <name> [--path P] [-r REV]` | new workspace (default base: `main`, else `@-`) |
| `workspace list` / `workspace remove <name>` | manage workspaces |
| `undo` | undo this workspace's last operation |
| `op log`, `op restore <op>` | operation log; restore this workspace to any state |
| `config [--global] [--unset] [-l] [key] [value]` | `user.name`, `user.email`, `land.check`, `land.queue`, `commit.check`, `commit.max_file_mb`, `watch.auto`, `text.eol` |
| `daemon start/stop/status [--no-snapshot] [--http [ADDR]]` | file watcher for this workspace, optionally serving the HTTP API |
| `mcp` | serve psync as MCP tools over stdio |
| `clone <src> [dest]`, `remote add/list/remove` | repositories and remotes |
| `fetch`, `pull`, `push [remote]` | exchange main, tasks, claims, revs and workspaces with a remote (default `origin`) |
| `git import/export <git repo> [-b branch]` | bring Git history in, or write main out as Git commits (export: `--no-commit-check`) |
| `snapshot` | record the directory now (normally automatic) |
| `check` | verify every object the repository refers to |
| `gc [--keep 30d] [--dry-run]` | pack objects and remove unreachable ones; `--keep` also forgets older operations |

Global flags: `--json`, `--agent <id>` (or `PSYNC_AGENT`), `-C <path>`.
`psync <command> --help` describes every option.

## For agents

- Start with `psync context --json`: it tells you your workspace, your working
  change, your task, the tasks ready to pick up, how far ahead/behind main you
  are, recent history, what other agents changed, claims, and where you
  overlap. If you are behind, `psync sync`.
- Pick a ready task with `psync task start <id>`, claim the paths you will
  touch, and finish with `psync task done <id>` after landing.
- Finish work with `psync commit -m ...` then `psync land`. If land reports
  conflicts or a failed check (`"ok": false`, exit 1), fix, commit, land again.
- Always pass `--json`. Success: `{"ok": true, ...}`. Failure:
  `{"ok": false, "error": "..."}` with exit code 1.
- Set `PSYNC_AGENT` so your changes and operations are attributed to you.
- Write the *intent* of a change in `-m`, attach the task with `--task`, and
  put anything else (model, session, test results) in `--meta key=value`.
- `commit` includes every change in your workspace (no staging), except
  ignored files.
- Experiment freely: `psync undo` reverses your last operation, including
  edits captured by the automatic snapshot (but not a land: main is shared).
