# progsync

**psync: progress sync for AI agents.** Several agents work on one project
at once, and psync keeps their progress in step: tasks with acceptance
checks, claims on the files each agent is working on, a shared `main` that
work only lands on after its check passes, and a full, undoable history of
who did what and why.

Underneath it is version control built for agents: Git-like commands, no
staging area, a workspace per agent, non-blocking conflicts, structured
change metadata with owner + agent identity, and `--json` on every command.

The project is called progsync; the command is `psync`.

## Why not Git?

Git was designed for people who coordinate by talking to each other. When
several agents edit one repository at once, the gaps show:

- **Staging and dirty trees.** Agents forget `git add`, or commit someone
  else's half-done edits. In psync your files *are* your working change:
  nothing to stage, nothing lost.
- **One checkout, many writers.** Agents in the same folder overwrite each
  other. psync gives each agent its own workspace (cloned copy-on-write, so
  it is cheap) and a shared `main` that work only reaches after a check
  passes.
- **Conflicts stop the agent.** A Git merge conflict halts the run until
  someone fixes it. psync records conflicts and carries on; source files
  merge declaration by declaration, so two agents adding functions to one
  file don't conflict at all.
- **Who did this, and why?** Every psync change records the owner *and* the
  agent, plus the task and any metadata you attach (model, prompt, run id).
- **Mistakes are cheap.** Every operation goes into a log; `psync undo`
  steps back through it, and never undoes another agent's work.
- **Built to be driven by programs.** `--json` on every command, structured
  errors, an MCP server, and an HTTP API.

psync still speaks Git: `psync git import` brings history in, and
`psync git export` writes it back out.

## Install

psync is a single binary with no runtime dependencies. You need Rust 1.89
or newer:

```sh
cargo install --git https://github.com/shyble/progsync progsync-cli   # installs `psync` into ~/.cargo/bin
psync --version
```

Or from a clone: `cargo install --path crates/progsync-cli`.

## Quick start

```sh
psync init                                  # current files become the first change
# ...edit files; no `add` needed...
psync status                                # working change (@) and changed files
psync commit -m "Add rate limiting" --task T-14 --meta model=opus-5.5
psync log
psync undo                                  # undo the last operation (repeatable)
```

## Several agents

```sh
psync workspace add claude-1                # ../<repo>-claude-1, files cloned copy-on-write
cd ../myrepo-claude-1 && export PSYNC_AGENT=claude-1
psync context                               # your work, main, other workspaces, overlaps
psync commit -m "..." --task T-14
psync land --check "cargo test"             # sync onto main, run the check, put your work on main
psync sync                                  # bring others' landed work into your workspace
```

## What it does

- **Workspaces and main.** Each agent works in its own workspace; `land`
  puts work on the shared `main` only when the check passes (lands can take
  turns, and several workspaces can land under one check, so a long suite
  runs once), and `undo` never touches another
  agent's work.
- **Conflicts don't block.** They are recorded and marked in the files, and
  you keep working. Source files merge declaration by declaration
  (tree-sitter), so two agents adding functions side by side merge cleanly.
- **Tasks and claims.** Tasks with acceptance checks and dependencies, which
  agents pick up from `context`; soft claims on paths or declarations.
- **Project rules.** `commit.check` runs your script on every change's
  record (message, metadata, identities) at commit, land and git export.
- **Remotes.** `clone`, `pull`, `push`, locally or over ssh: main, tasks,
  claims, named revisions (`rev`), and every clone's workspaces, which you
  can `diff`, `merge` or `switch` onto.
- **Interfaces.** `--json` everywhere, an MCP server (`psync mcp`), and an
  HTTP API. A file watcher makes commands faster and shows edits live.
- **Git bridge.** `git import` and `git export`.

## Documentation

- [docs/USAGE.md](docs/USAGE.md): how to use psync: every feature, the
  command table, revision syntax, and notes for agents.
- [docs/DESIGN.md](docs/DESIGN.md): how it works: data model, storage,
  merging, remotes, roadmap, known limits.

## For agents

Start with `psync context --json`, pass `--json` to every command (failures
are `{"ok": false, "error": ...}` with exit 1), and set `PSYNC_AGENT`. Or
connect over MCP: `claude mcp add psync -- psync mcp`. See
[For agents](docs/USAGE.md#for-agents).

## Status

psync is young (version 0.1). It manages its own development and the test
suite covers every command, but the on-disk format may still change before
1.0. Keep a Git export of anything important (`psync git export`).

Platforms: tested on macOS and Linux. Windows builds (cross-compiled) but is
untested on real Windows.

Issues and pull requests are welcome.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in psync by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
