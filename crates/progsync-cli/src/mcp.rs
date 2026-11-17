//! `psync mcp`: a Model Context Protocol server over stdio, so agents can
//! call psync as tools instead of running shell commands.
//!
//! Each tool maps to a CLI command and runs it in-process with `--json`, so
//! tools behave exactly like the commands (same validation, same output).
//! Messages are newline-delimited JSON-RPC 2.0 on stdin/stdout.

use crate::{run, Cli};
use progsync_core::bail;
use progsync_core::error::Result;
use progsync_core::json;
use progsync_core::json::{Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "psync is version control for agents working in parallel. \
Start with `context`: your workspace, working change, task, ready tasks, main (ahead/behind), \
what other workspaces changed and where you overlap. There is no staging: every call first \
records the working directory into your working change (@). Typical flow: task_start, claim the \
paths you will touch, edit files, commit (message = the change's intent), land (runs the \
project's check and puts your work on main), task_done. If land reports conflicts, edit the \
files to resolve them, commit, land again. `undo` reverses your last operation (not a land).";

/// How a tool argument becomes command-line arguments.
#[derive(Clone, Copy)]
enum Arg {
    /// Positional value.
    Pos,
    /// Positional values (array).
    PosList,
    /// `--flag value`.
    Opt(&'static str),
    /// `--flag` when true.
    Switch(&'static str),
    /// Array joined with commas: `--flag a,b`.
    Comma(&'static str),
    /// Object of strings: `--meta k=v` per entry.
    Meta,
}

#[derive(Clone, Copy)]
struct Prop {
    name: &'static str,
    arg: Arg,
    /// JSON schema type: "string", "integer", "boolean", "array", "object".
    ty: &'static str,
    description: &'static str,
}

struct Tool {
    name: &'static str,
    command: &'static [&'static str],
    description: &'static str,
    read_only: bool,
    props: &'static [Prop],
    required: &'static [&'static str],
}

const fn p(name: &'static str, arg: Arg, ty: &'static str, description: &'static str) -> Prop {
    Prop { name, arg, ty, description }
}

const META: [Prop; 3] = [
    p("message", Arg::Opt("--message"), "string", "The change's intent: what it does and why"),
    p("task", Arg::Opt("--task"), "string", "Task id the change contributes to (\"\" clears)"),
    p("meta", Arg::Meta, "object", "Extra metadata as string values, e.g. {\"model\": \"opus\"} (\"\" clears a key)"),
];

static TOOLS: &[Tool] = &[
    Tool {
        name: "context",
        command: &["context"],
        description: "Briefing: your workspace and working change, your task and ready tasks, main (ahead/behind), recent history, what every other workspace changed, claims, and overlapping files. Call this first.",
        read_only: true,
        props: &[p("recent", Arg::Opt("--recent"), "integer", "Number of recent changes to include (default 5)")],
        required: &[],
    },
    Tool {
        name: "status",
        command: &["status"],
        description: "The working change, its changed files, conflicts, and position relative to main; with ignored, also the ignored files and directories a commit leaves out.",
        read_only: true,
        props: &[p("ignored", Arg::Switch("--ignored"), "boolean", "Also list ignored files and directories")],
        required: &[],
    },
    Tool {
        name: "log",
        command: &["log"],
        description: "Change history of this workspace (or all workspaces).",
        read_only: true,
        props: &[
            p("limit", Arg::Opt("--limit"), "integer", "Maximum number of changes (default 20)"),
            p("all", Arg::Switch("--all"), "boolean", "Include every workspace's history"),
        ],
        required: &[],
    },
    Tool {
        name: "show",
        command: &["show"],
        description: "One change: metadata and diff. Revisions: @, <workspace>@, main, a change-id or hash prefix; append - for the parent (@-).",
        read_only: true,
        props: &[
            p("rev", Arg::Pos, "string", "Revision (default @)"),
            p("stat", Arg::Switch("--stat"), "boolean", "Only list files with line counts"),
        ],
        required: &[],
    },
    Tool {
        name: "diff",
        command: &["diff"],
        description: "The changes introduced by a change (default: the working change).",
        read_only: true,
        props: &[
            p("rev", Arg::Pos, "string", "Revision (default @)"),
            p("stat", Arg::Switch("--stat"), "boolean", "Only list files with line counts"),
            p("name_only", Arg::Switch("--name-only"), "boolean", "Only list file names"),
        ],
        required: &[],
    },
    Tool {
        name: "commit",
        command: &["commit"],
        description: "Finish the working change (all changed files, or only `paths`) and start a new one on top. No staging needed.",
        read_only: false,
        props: &[
            META[0],
            META[1],
            META[2],
            p("paths", Arg::PosList, "array", "Only commit changes under these paths or globs; the rest stays in the working change"),
            p("allow_large", Arg::Switch("--allow-large"), "boolean", "Commit files above commit.max_file_mb anyway"),
        ],
        required: &["message"],
    },
    Tool {
        name: "describe",
        command: &["describe"],
        description: "Set the working change's intent, task or metadata without finishing it; with rev, change one of your committed changes not yet on main instead.",
        read_only: false,
        props: &[META[0], META[1], META[2], p("rev", Arg::Opt("--rev"), "string", "A committed change of yours not yet on main (default: the working change)")],
        required: &[],
    },
    Tool {
        name: "sync",
        command: &["sync"],
        description: "Rebase your work onto the latest main, bringing in others' landed work. Conflicts are recorded in the working change, never fatal.",
        read_only: false,
        props: &[],
        required: &[],
    },
    Tool {
        name: "land",
        command: &["land"],
        description: "Put your committed changes onto main: syncs, runs the check (default: config land.check) against exactly what lands, then advances main. Requires a clean working change. With workspaces, their committed changes land too, stacked after yours, under one check.",
        read_only: false,
        props: &[
            p("check", Arg::Opt("--check"), "string", "Command that must pass instead of the configured land.check"),
            p("no_check", Arg::Switch("--no-check"), "boolean", "Skip the configured land.check"),
            p("workspaces", Arg::PosList, "array", "Other workspaces whose committed changes land with yours under one check, e.g. [\"claude-1@\", \"codex-2@laptop\"]"),
        ],
        required: &[],
    },
    Tool {
        name: "merge",
        command: &["merge"],
        description: "Three-way merge another change (e.g. another agent's \"claude-1@\") into your working change.",
        read_only: false,
        props: &[p("rev", Arg::Pos, "string", "Revision to merge")],
        required: &["rev"],
    },
    Tool {
        name: "switch",
        command: &["switch"],
        description: "Point this workspace at another line of work, e.g. another agent's \"claude-1@\" (\"claude-1@laptop\" in another repository, as of the last fetch), main or a rev, to test or continue it. Your working change is parked; \"-\" switches back.",
        read_only: false,
        props: &[p("rev", Arg::Pos, "string", "Where to switch, or \"-\" to go back")],
        required: &["rev"],
    },
    Tool {
        name: "rev_add",
        command: &["rev", "add"],
        description: "Name a revision for good (a rev never moves), e.g. to let others test or continue it. With remote, also create it there directly (main is not touched).",
        read_only: false,
        props: &[
            p("name", Arg::Pos, "string", "Rev name, e.g. try-auth or v1.2"),
            p("rev", Arg::Pos, "string", "Revision to name (default @)"),
            p("remote", Arg::Opt("--remote"), "string", "Also create it in this remote"),
        ],
        required: &["name"],
    },
    Tool {
        name: "rev_list",
        command: &["rev", "list"],
        description: "Named revisions (revs), with the change each names.",
        read_only: true,
        props: &[],
        required: &[],
    },
    Tool {
        name: "revert",
        command: &["revert"],
        description: "Undo a change (e.g. a landed one) with a new change in your clean working change; then commit and land it. Later edits to the same lines become conflicts.",
        read_only: false,
        props: &[p("rev", Arg::Pos, "string", "Revision to revert, e.g. a change-id prefix")],
        required: &["rev"],
    },
    Tool {
        name: "archive",
        command: &["archive"],
        description: "Write a revision's files (exactly, no untracked or uncommitted files) as a tar file, to build or deploy that revision elsewhere. Reports the revision hash, which is also stored in the tar.",
        read_only: false,
        props: &[
            p("rev", Arg::Pos, "string", "Revision, e.g. main or a change-id prefix"),
            p("output", Arg::Opt("--output"), "string", "Tar file to write"),
            p("paths", Arg::PosList, "array", "Only these files, directories or globs"),
            p("prefix", Arg::Opt("--prefix"), "string", "Put every path under this directory, e.g. app-1.2/"),
        ],
        required: &["rev", "output"],
    },
    Tool {
        name: "check_ignore",
        command: &["check-ignore"],
        description: "Whether paths are ignored (left out of commits), and which rule decides: ignore file, line and pattern, or an ignored directory containing them.",
        read_only: true,
        props: &[p("paths", Arg::PosList, "array", "Paths relative to the repository root")],
        required: &["paths"],
    },
    Tool {
        name: "id",
        command: &["id"],
        description: "Which revision this tree is: the last commit (revision hash, change id, intent), whether files differ from it (dirty, changed) and whether it is on main. Records nothing.",
        read_only: true,
        props: &[p("paths", Arg::PosList, "array", "Only changes under these paths count")],
        required: &[],
    },
    Tool {
        name: "restore",
        command: &["restore"],
        description: "Bring files or directories back exactly as they are in a revision (e.g. ones deleted or broken since), leaving the rest of the working change alone. Files under the paths that the revision lacks are removed. Undoable with undo.",
        read_only: false,
        props: &[
            p("rev", Arg::Pos, "string", "Revision to take the files from, e.g. main, @- or a change-id prefix"),
            p("paths", Arg::PosList, "array", "Files, directories or globs to restore"),
        ],
        required: &["rev", "paths"],
    },
    Tool {
        name: "cat",
        command: &["cat"],
        description: "A file's exact contents at a revision (content is null for a binary file), with its hash and size; does not touch the working copy.",
        read_only: true,
        props: &[
            p("rev", Arg::Pos, "string", "Revision, e.g. main or a change-id prefix"),
            p("path", Arg::Pos, "string", "File path in that revision"),
        ],
        required: &["rev", "path"],
    },
    Tool {
        name: "resolve",
        command: &["resolve"],
        description: "List conflicts in the working change, or mark one resolved. Editing a file to remove its conflict markers also resolves it.",
        read_only: false,
        props: &[
            p("path", Arg::Pos, "string", "File to mark resolved (omit to list conflicts)"),
            p("force", Arg::Switch("--force"), "boolean", "Accept the file even if it still has conflict markers"),
        ],
        required: &[],
    },
    Tool {
        name: "undo",
        command: &["undo"],
        description: "Undo this workspace's last operation, including edits captured by a snapshot (repeat to go further back). Lands cannot be undone.",
        read_only: false,
        props: &[],
        required: &[],
    },
    Tool {
        name: "task_list",
        command: &["task", "list"],
        description: "Tasks with status (open, in_progress, blocked, done, cancelled), owner and dependencies.",
        read_only: true,
        props: &[p("all", Arg::Switch("--all"), "boolean", "Include done and cancelled tasks")],
        required: &[],
    },
    Tool {
        name: "task_show",
        command: &["task", "show"],
        description: "One task: description, acceptance check, owner, landed changes, completion evidence.",
        read_only: true,
        props: &[p("id", Arg::Pos, "string", "Task id, e.g. T-3")],
        required: &["id"],
    },
    Tool {
        name: "task_add",
        command: &["task", "add"],
        description: "Create a task.",
        read_only: false,
        props: &[
            p("title", Arg::Pos, "string", "Short title"),
            p("id", Arg::Opt("--id"), "string", "Task id (default: next T-<n>)"),
            p("description", Arg::Opt("--description"), "string", "What done means, context, pointers"),
            p("check", Arg::Opt("--check"), "string", "Acceptance check command; must pass on main for task_done"),
            p("after", Arg::Comma("--after"), "array", "Task ids that must be finished first"),
        ],
        required: &["title"],
    },
    Tool {
        name: "task_start",
        command: &["task", "start"],
        description: "Take a task in this workspace and attach your working change to it. Refuses blocked tasks and tasks owned elsewhere unless forced.",
        read_only: false,
        props: &[
            p("id", Arg::Pos, "string", "Task id"),
            p("force", Arg::Switch("--force"), "boolean", "Take it even if another workspace owns it"),
        ],
        required: &["id"],
    },
    Tool {
        name: "task_drop",
        command: &["task", "drop"],
        description: "Give a task back; it becomes open again.",
        read_only: false,
        props: &[p("id", Arg::Pos, "string", "Task id"), p("force", Arg::Switch("--force"), "boolean", "Drop it even if another workspace owns it")],
        required: &["id"],
    },
    Tool {
        name: "task_done",
        command: &["task", "done"],
        description: "Finish a task after its work has landed: runs its acceptance check against main and records the evidence.",
        read_only: false,
        props: &[p("id", Arg::Pos, "string", "Task id")],
        required: &["id"],
    },
    Tool {
        name: "task_edit",
        command: &["task", "edit"],
        description: "Change a task's title, description, acceptance check or dependencies.",
        read_only: false,
        props: &[
            p("id", Arg::Pos, "string", "Task id"),
            p("title", Arg::Opt("--title"), "string", "New title"),
            p("description", Arg::Opt("--description"), "string", "New description"),
            p("check", Arg::Opt("--check"), "string", "New acceptance check (\"\" removes it)"),
            p("after", Arg::Comma("--after"), "array", "New dependency list ([] clears it)"),
        ],
        required: &["id"],
    },
    Tool {
        name: "task_cancel",
        command: &["task", "cancel"],
        description: "Mark a task as no longer needed (unblocks tasks waiting for it, releases its claims).",
        read_only: false,
        props: &[p("id", Arg::Pos, "string", "Task id")],
        required: &["id"],
    },
    Tool {
        name: "claim",
        command: &["claim"],
        description: "Claim paths you are about to change so other agents see it (soft lock with expiry). No patterns: list active claims.",
        read_only: false,
        props: &[
            p("patterns", Arg::PosList, "array", "Paths, directories (src/auth/), globs (src/**/*.rs), or one top-level declaration of a Rust/Python/JS/TS/Go file (src/auth.rs::login)"),
            p("ttl", Arg::Opt("--ttl"), "string", "How long the claim lasts, e.g. 30m, 2h, 1d (default 30m)"),
            p("force", Arg::Switch("--force"), "boolean", "Claim even if another workspace claims an overlapping path"),
        ],
        required: &[],
    },
    Tool {
        name: "release",
        command: &["release"],
        description: "Release your claims (no patterns: all of them). Landing releases them too.",
        read_only: false,
        props: &[p("patterns", Arg::PosList, "array", "Claims to release")],
        required: &[],
    },
    Tool {
        name: "pull",
        command: &["pull"],
        description: "Bring in main, tasks, claims, revs and workspaces from another repository (a remote): fast-forwards main, or, if both sides landed different changes, moves your local-only landed changes back into your workspace to land again on top.",
        read_only: false,
        props: &[p("remote", Arg::Pos, "string", "Remote name (default origin)")],
        required: &[],
    },
    Tool {
        name: "push",
        command: &["push"],
        description: "Send main, tasks, claims, revs and workspaces to a remote. Only fast-forwards the remote's main; if it has changes you lack, pull first.",
        read_only: false,
        props: &[p("remote", Arg::Pos, "string", "Remote name (default origin)")],
        required: &[],
    },
    Tool {
        name: "workspace_list",
        command: &["workspace", "list"],
        description: "Workspaces (one per agent) with their working changes.",
        read_only: true,
        props: &[],
        required: &[],
    },
    Tool {
        name: "workspace_add",
        command: &["workspace", "add"],
        description: "Create a workspace (a copy-on-write directory with its own working change) for another agent.",
        read_only: false,
        props: &[
            p("name", Arg::Pos, "string", "Workspace name, e.g. claude-2"),
            p("path", Arg::Opt("--path"), "string", "Directory to create (default ../<repo>-<name>)"),
            p("rev", Arg::Opt("--rev"), "string", "Revision to start from (default main, else @-)"),
        ],
        required: &["name"],
    },
    Tool {
        name: "op_log",
        command: &["op", "log"],
        description: "Operation log, newest first: every repository state, with who did what.",
        read_only: true,
        props: &[p("limit", Arg::Opt("--limit"), "integer", "Maximum number of operations (default 20)")],
        required: &[],
    },
];

fn tool_schema(t: &Tool, protocol: &str) -> Value {
    let mut props = Map::new();
    for p in t.props {
        let mut s = json!({ "type": p.ty, "description": p.description });
        match p.ty {
            "array" => s["items"] = json!({ "type": "string" }),
            "object" => s["additionalProperties"] = json!({ "type": "string" }),
            _ => {}
        }
        props.insert(p.name.into(), s);
    }
    let mut v = json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": { "type": "object", "properties": props, "required": t.required, "additionalProperties": false },
    });
    if protocol != "2024-11-05" {
        v["annotations"] = json!({ "readOnlyHint": t.read_only, "destructiveHint": false, "openWorldHint": false });
    }
    v
}

/// Command-line arguments for a tool call (after the global options).
fn argv(t: &Tool, args: &Map) -> Result<Vec<String>> {
    for k in args.keys() {
        if !t.props.iter().any(|p| p.name == k) {
            bail!("unknown argument '{k}' for tool {}", t.name);
        }
    }
    for r in t.required {
        if args.get(*r).is_none_or(Value::is_null) {
            bail!("missing required argument '{r}'");
        }
    }
    let scalar = |name: &str, v: &Value| -> Result<String> {
        match v {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) if n.is_u64() => Ok(n.to_string()),
            _ => bail!("argument '{name}' must be a string or a non-negative integer"),
        }
    };
    let list = |name: &str, v: &Value| -> Result<Vec<String>> {
        match v {
            Value::Array(items) => items
                .iter()
                .map(|i| i.as_str().map(String::from).ok_or_else(|| progsync_core::err!("'{name}' must be an array of strings")))
                .collect(),
            _ => bail!("argument '{name}' must be an array of strings"),
        }
    };
    let mut out: Vec<String> = t.command.iter().map(|s| s.to_string()).collect();
    let mut positional = Vec::new();
    for p in t.props {
        let Some(v) = args.get(p.name).filter(|v| !v.is_null()) else { continue };
        match p.arg {
            Arg::Pos => positional.push(scalar(p.name, v)?),
            Arg::PosList => positional.extend(list(p.name, v)?),
            Arg::Opt(flag) => {
                out.push(flag.into());
                out.push(scalar(p.name, v)?);
            }
            Arg::Switch(flag) => match v {
                Value::Bool(true) => out.push(flag.into()),
                Value::Bool(false) => {}
                _ => bail!("argument '{}' must be a boolean", p.name),
            },
            Arg::Comma(flag) => {
                out.push(flag.into());
                out.push(list(p.name, v)?.join(","));
            }
            Arg::Meta => {
                let Value::Object(m) = v else { bail!("argument '{}' must be an object of strings", p.name) };
                for (k, val) in m {
                    let Some(s) = val.as_str() else { bail!("metadata value for '{k}' must be a string") };
                    out.push("--meta".into());
                    out.push(format!("{k}={s}"));
                }
            }
        }
    }
    if !positional.is_empty() {
        // Values that start with '-' must not be read as options.
        out.push("--".into());
        out.extend(positional);
    }
    Ok(out)
}

pub struct Server {
    dir: Option<PathBuf>,
    agent: Option<String>,
    protocol: String,
}

impl Server {
    pub fn new(dir: Option<PathBuf>, agent: Option<String>) -> Server {
        Server { dir, agent, protocol: PROTOCOL_VERSIONS[0].into() }
    }

    /// Serve requests from stdin until it closes.
    pub fn serve(mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            for reply in self.handle_line(&line) {
                writeln!(stdout, "{reply}")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one message (or batch); returns the replies to send.
    pub fn handle_line(&mut self, line: &str) -> Vec<Value> {
        match progsync_core::json::from_str::<Value>(line) {
            Ok(Value::Array(batch)) => batch.into_iter().filter_map(|m| self.handle(m)).collect(),
            Ok(msg) => self.handle(msg).into_iter().collect(),
            Err(e) => vec![error_reply(Value::Null, -32700, &format!("parse error: {e}"))],
        }
    }

    fn handle(&mut self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // Notifications (no id) get no reply; responses from the client are ignored.
        let id = id?;
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => {
                Ok(json!({ "tools": TOOLS.iter().map(|t| tool_schema(t, &self.protocol)).collect::<Vec<_>>() }))
            }
            "tools/call" => Ok(self.call(&params)),
            "" => Err((-32600, "invalid request: no method".to_string())),
            m => Err((-32601, format!("method not found: {m}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, m)) => error_reply(id, code, &m),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
        self.protocol = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]).to_string();
        // Without a configured agent, attribute work to the client (e.g. "claude-code").
        if self.agent.is_none() {
            self.agent = params
                .pointer("/clientInfo/name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(String::from);
        }
        json!({
            "protocolVersion": self.protocol,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "psync", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    fn call(&self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let empty = Map::new();
        let args = params.get("arguments").and_then(Value::as_object).unwrap_or(&empty);
        let (value, failed) = call_tool(self.dir.as_deref(), self.agent.as_deref(), name, args)
            .unwrap_or_else(|| (json!({ "ok": false, "error": format!("unknown tool '{name}'") }), true));
        let mut r = json!({ "content": [{ "type": "text", "text": value.to_string() }], "isError": failed });
        if self.protocol.as_str() >= "2025-06-18" {
            r["structuredContent"] = value;
        }
        r
    }

}

/// Run tool `name` (as its CLI command) and return its JSON result and
/// whether it failed; `None` if there is no such tool.
pub fn call_tool(dir: Option<&Path>, agent: Option<&str>, name: &str, args: &Map) -> Option<(Value, bool)> {
    let t = TOOLS.iter().find(|t| t.name == name)?;
    Some(match argv(t, args).and_then(|a| run_cli(dir, agent, a)) {
        Ok(v) => {
            let failed = v.get("ok") == Some(&Value::Bool(false));
            (v, failed)
        }
        Err(e) => (json!({ "ok": false, "error": format!("{e:#}") }), true),
    })
}

/// Every tool's name, description, input schema and annotations.
pub fn tool_list() -> Vec<Value> {
    TOOLS.iter().map(|t| tool_schema(t, PROTOCOL_VERSIONS[0])).collect()
}

pub fn is_read_only(name: &str) -> Option<bool> {
    TOOLS.iter().find(|t| t.name == name).map(|t| t.read_only)
}

/// A query-string value as the JSON type the tool's argument expects.
pub fn query_value(tool: &str, key: &str, raw: &str) -> Value {
    let ty = TOOLS.iter().find(|t| t.name == tool).and_then(|t| t.props.iter().find(|p| p.name == key)).map(|p| p.ty);
    match ty {
        Some("integer") => raw.parse::<u64>().map(Value::from).unwrap_or_else(|_| Value::from(raw)),
        Some("boolean") => match raw {
            "" | "1" | "true" => Value::Bool(true),
            "0" | "false" => Value::Bool(false),
            other => Value::from(other),
        },
        Some("array") => Value::from(raw.split(',').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>()),
        _ => Value::from(raw),
    }
}

fn run_cli(dir: Option<&Path>, agent: Option<&str>, args: Vec<String>) -> Result<Value> {
    let mut full = vec!["psync".to_string(), "--json".into()];
    if let Some(d) = dir {
        full.push("-C".into());
        full.push(d.display().to_string());
    }
    if let Some(a) = agent {
        full.push("--agent".into());
        full.push(a.to_string());
    }
    full.extend(args);
    let cli = Cli::try_parse_from(&full).map_err(|e| progsync_core::err!("{}", e.to_string().trim()))?;
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli)))
        .map_err(|_| progsync_core::err!("internal error (panic) while running the command"))??;
    let mut v = out.json;
    if let Value::Object(m) = &mut v {
        m.entry("ok".to_string()).or_insert(Value::Bool(true));
    }
    Ok(v)
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> &'static Tool {
        TOOLS.iter().find(|t| t.name == name).unwrap()
    }

    fn args(v: Value) -> Map {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn arguments_become_command_lines() {
        let a = argv(
            tool("commit"),
            &args(json!({ "message": "fix", "meta": { "model": "opus" }, "paths": ["-odd.rs", "src/"] })),
        )
        .unwrap();
        assert_eq!(a, ["commit", "--message", "fix", "--meta", "model=opus", "--", "-odd.rs", "src/"]);
        let a = argv(tool("task_add"), &args(json!({ "title": "t", "after": ["T-1", "T-2"] }))).unwrap();
        assert_eq!(a, ["task", "add", "--after", "T-1,T-2", "--", "t"]);
        let a = argv(tool("log"), &args(json!({ "limit": 3, "all": false }))).unwrap();
        assert_eq!(a, ["log", "--limit", "3"]);
        assert!(argv(tool("commit"), &args(json!({}))).is_err(), "message required");
        assert!(argv(tool("status"), &args(json!({ "bogus": 1 }))).is_err());
        assert!(argv(tool("log"), &args(json!({ "all": "yes" }))).is_err());
    }

    #[test]
    fn every_tool_parses_as_a_cli_command() {
        for t in TOOLS {
            let mut a = Map::new();
            for r in t.required {
                let list = t.props.iter().any(|p| p.name == *r && p.ty == "array");
                a.insert((*r).into(), if list { json!(["x"]) } else { json!("x") });
            }
            let mut full = vec!["psync".to_string(), "--json".into()];
            full.extend(argv(t, &a).unwrap());
            Cli::try_parse_from(&full).unwrap_or_else(|e| panic!("tool {}: {e}", t.name));
        }
    }
}
