# psync

Version control built for AI agents. Git-like commands, no staging area,
a workspace per agent, a shared `main` that work lands on after checks pass,
non-blocking conflicts, everything undoable, structured change metadata with
owner + agent identity, and `--json` on every command.

```sh
cargo build --release          # binary: target/release/psync
```

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

Platforms: tested on macOS, and on Linux (the test suite, in Docker). Windows
builds (cross-compiled) but is untested on real Windows.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in psync by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
