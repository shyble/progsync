//! Command-line parsing and help, from a declarative description of the
//! commands ([`Cmd`] trees of [`Arg`]s).
//!
//! - `--name value`, `--name=value`, `-n value`, `-nvalue`; flags take no value;
//! - options marked global (`--json`, `--agent`, `-C`) may appear anywhere;
//! - an option may be repeated (`--meta a=1 --meta b=2`), split on a
//!   delimiter (`--after T-1,T-2`), or take an optional value (`--http [ADDR]`);
//! - `--` ends options; `-h`/`--help`, `help [COMMAND...]`, `-V`/`--version`.
//!
//! Usage errors print the message, the command's usage line and a hint, and
//! exit with status 2 (help and version exit with 0).

use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `--name`, no value.
    Flag,
    /// `--name VALUE`, at most once.
    Opt,
    /// `--name VALUE`, any number of times.
    Multi,
    /// Required positional.
    Pos,
    /// Optional positional.
    PosOpt,
    /// Any number of positionals (must come last).
    PosMulti,
}

#[derive(Clone)]
pub struct Arg {
    pub id: &'static str,
    pub kind: Kind,
    pub long: Option<&'static str>,
    pub short: Option<char>,
    /// Other long names.
    pub aliases: &'static [&'static str],
    pub help: &'static str,
    pub value_name: Option<&'static str>,
    pub default: Option<&'static str>,
    /// Value when the option is given without one (makes the value optional).
    pub missing: Option<&'static str>,
    pub delimiter: Option<char>,
    pub env: Option<&'static str>,
    pub global: bool,
    pub conflicts: Option<&'static str>,
}

impl Arg {
    const fn new(id: &'static str, kind: Kind) -> Arg {
        Arg {
            id,
            kind,
            long: None,
            short: None,
            aliases: &[],
            help: "",
            value_name: None,
            default: None,
            missing: None,
            delimiter: None,
            env: None,
            global: false,
            conflicts: None,
        }
    }

    /// `--id` flag (underscores in `id` become dashes).
    pub fn flag(id: &'static str) -> Arg {
        Arg { long: Some(id), ..Arg::new(id, Kind::Flag) }
    }

    /// `--id VALUE` option.
    pub fn opt(id: &'static str) -> Arg {
        Arg { long: Some(id), ..Arg::new(id, Kind::Opt) }
    }

    /// `--id VALUE`, repeatable.
    pub fn multi(id: &'static str) -> Arg {
        Arg { long: Some(id), ..Arg::new(id, Kind::Multi) }
    }

    pub fn pos(id: &'static str) -> Arg {
        Arg::new(id, Kind::Pos)
    }

    pub fn pos_opt(id: &'static str) -> Arg {
        Arg::new(id, Kind::PosOpt)
    }

    pub fn pos_multi(id: &'static str) -> Arg {
        Arg::new(id, Kind::PosMulti)
    }

    pub fn short(mut self, c: char) -> Arg {
        self.short = Some(c);
        self
    }

    /// Only a short name (`-C PATH`).
    pub fn short_only(mut self, c: char) -> Arg {
        self.short = Some(c);
        self.long = None;
        self
    }

    pub fn alias(mut self, a: &'static [&'static str]) -> Arg {
        self.aliases = a;
        self
    }

    pub fn help(mut self, h: &'static str) -> Arg {
        self.help = h;
        self
    }

    pub fn value_name(mut self, v: &'static str) -> Arg {
        self.value_name = Some(v);
        self
    }

    pub fn default(mut self, d: &'static str) -> Arg {
        self.default = Some(d);
        self
    }

    pub fn missing(mut self, m: &'static str) -> Arg {
        self.missing = Some(m);
        self
    }

    pub fn delimiter(mut self, c: char) -> Arg {
        self.delimiter = Some(c);
        self
    }

    pub fn env(mut self, e: &'static str) -> Arg {
        self.env = Some(e);
        self
    }

    pub fn global(mut self) -> Arg {
        self.global = true;
        self
    }

    pub fn conflicts(mut self, other: &'static str) -> Arg {
        self.conflicts = Some(other);
        self
    }

    fn long_name(&self) -> Option<String> {
        self.long.map(|l| l.replace('_', "-"))
    }

    fn takes_value(&self) -> bool {
        matches!(self.kind, Kind::Opt | Kind::Multi)
    }

    fn is_positional(&self) -> bool {
        matches!(self.kind, Kind::Pos | Kind::PosOpt | Kind::PosMulti)
    }

    fn placeholder(&self) -> String {
        self.value_name.map(String::from).unwrap_or_else(|| self.id.to_uppercase())
    }

    /// How the option is shown in errors: `--limit <LIMIT>`, `<REV>`.
    fn display(&self) -> String {
        if self.is_positional() {
            return format!("<{}>", self.placeholder());
        }
        let name = match (self.long_name(), self.short) {
            (Some(l), _) => format!("--{l}"),
            (None, Some(s)) => format!("-{s}"),
            (None, None) => self.id.to_string(),
        };
        if self.takes_value() {
            format!("{name} <{}>", self.placeholder())
        } else {
            name
        }
    }
}

pub struct Cmd {
    pub name: &'static str,
    pub about: &'static str,
    /// Extra paragraphs shown in this command's own help.
    pub long_about: &'static str,
    pub aliases: &'static [&'static str],
    pub hidden: bool,
    pub args: Vec<Arg>,
    pub subs: Vec<Cmd>,
}

impl Cmd {
    pub fn new(name: &'static str, about: &'static str) -> Cmd {
        Cmd { name, about, long_about: "", aliases: &[], hidden: false, args: Vec::new(), subs: Vec::new() }
    }

    pub fn long_about(mut self, t: &'static str) -> Cmd {
        self.long_about = t;
        self
    }

    pub fn alias(mut self, a: &'static [&'static str]) -> Cmd {
        self.aliases = a;
        self
    }

    pub fn hidden(mut self) -> Cmd {
        self.hidden = true;
        self
    }

    pub fn arg(mut self, a: Arg) -> Cmd {
        self.args.push(a);
        self
    }

    pub fn sub(mut self, c: Cmd) -> Cmd {
        self.subs.push(c);
        self
    }

    fn find_sub(&self, name: &str) -> Option<&Cmd> {
        self.subs.iter().find(|c| c.name == name || c.aliases.contains(&name))
    }
}

/// Parsed arguments: the chosen subcommand path and each argument's values.
#[derive(Debug, Default)]
pub struct Matches {
    /// Subcommand names from the root, e.g. `["task", "add"]`.
    pub path: Vec<&'static str>,
    values: BTreeMap<&'static str, Vec<String>>,
    /// The chosen command's usage line, for errors.
    usage: String,
    /// How each argument is written (`--limit <LIMIT>`), for errors.
    shown: BTreeMap<String, String>,
}

impl Matches {
    pub fn flag(&self, id: &str) -> bool {
        self.values.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<String> {
        self.values.get(id).and_then(|v| v.last().cloned())
    }

    /// A value that is always present (a required positional or one with a default).
    pub fn req(&self, id: &str) -> String {
        self.get(id).unwrap_or_else(|| panic!("argument {id} has no value or default"))
    }

    pub fn all(&self, id: &str) -> Vec<String> {
        self.values.get(id).cloned().unwrap_or_default()
    }

    /// Parse a value with `f`; errors name the argument as the user wrote it.
    pub fn parse<T>(&self, id: &str, f: impl Fn(&str) -> Result<T, String>) -> Result<Option<T>, Error> {
        match self.get(id) {
            None => Ok(None),
            Some(v) => f(&v).map(Some).map_err(|e| Error::usage(format!("invalid value '{v}' for '{}': {e}", self.shown(id)), self)),
        }
    }

    pub fn parse_all<T>(&self, id: &str, f: impl Fn(&str) -> Result<T, String>) -> Result<Vec<T>, Error> {
        self.all(id)
            .iter()
            .map(|v| f(v).map_err(|e| Error::usage(format!("invalid value '{v}' for '{}': {e}", self.shown(id)), self)))
            .collect()
    }

    fn shown(&self, id: &str) -> String {
        self.shown.get(id).cloned().unwrap_or_else(|| id.to_string())
    }
}

/// A parse outcome other than success.
#[derive(Debug)]
pub enum Error {
    /// Help or version text, printed to stdout (exit 0).
    Info(String),
    /// A usage error (exit 2).
    Usage { msg: String, usage: String },
}

impl Error {
    fn usage(msg: String, m: &Matches) -> Error {
        Error::Usage { msg, usage: m.usage.clone() }
    }

    /// Print and exit like a command-line tool.
    pub fn exit(&self) -> ! {
        match self {
            Error::Info(t) => {
                crate::emit(t, 0);
                std::process::exit(0)
            }
            Error::Usage { .. } => {
                eprint!("{self}");
                std::process::exit(2)
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Info(t) => f.write_str(t),
            Error::Usage { msg, usage } => write!(f, "error: {msg}\n\nUsage: {usage}\n\nFor more information, try '--help'.\n"),
        }
    }
}

/// Parse `argv` (including the program name) against `root`.
pub fn parse(root: &Cmd, version: &str, argv: &[String]) -> Result<Matches, Error> {
    let mut chain: Vec<&Cmd> = vec![root];
    let mut m = Matches { usage: usage(&[root]), ..Default::default() };
    let globals: Vec<&Arg> = root.args.iter().filter(|a| a.global).collect();
    let mut positional: Vec<String> = Vec::new();
    let mut only_positional = false;
    let mut i = 1;
    let mut want_help = false;

    while i < argv.len() {
        let tok = &argv[i];
        i += 1;
        let cmd = *chain.last().expect("chain starts with root");
        if only_positional || tok == "-" || !tok.starts_with('-') {
            // A subcommand name, while no positional of this command was seen.
            if !only_positional && positional.is_empty() && !cmd.subs.is_empty() {
                if tok == "help" {
                    // `help [COMMAND...]`: the help of that command.
                    let mut target = vec![cmd];
                    for name in &argv[i..] {
                        match target.last().unwrap().find_sub(name) {
                            Some(s) => target.push(s),
                            None => break,
                        }
                    }
                    let mut full = chain.clone();
                    full.extend(target.into_iter().skip(1));
                    return Err(Error::Info(help(&full, &globals)));
                }
                match cmd.find_sub(tok) {
                    Some(s) => {
                        chain.push(s);
                        m.path.push(s.name);
                        m.usage = usage(&chain);
                        continue;
                    }
                    None => return Err(Error::usage(format!("unrecognized subcommand '{tok}'"), &m)),
                }
            }
            positional.push(tok.clone());
            continue;
        }
        if tok == "--" {
            only_positional = true;
            continue;
        }
        if tok == "-h" || tok == "--help" {
            want_help = true;
            continue;
        }
        if (tok == "-V" || tok == "--version") && chain.len() == 1 {
            return Err(Error::Info(format!("{} {version}\n", root.name)));
        }
        // An option of this command, or a global one.
        let candidates: Vec<&Arg> = cmd.args.iter().filter(|a| !a.is_positional()).chain(globals.iter().copied()).collect();
        let (arg, inline): (&Arg, Option<String>) = if let Some(body) = tok.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            match candidates.iter().find(|a| a.long_name().as_deref() == Some(name) || a.aliases.contains(&name)) {
                Some(a) => (*a, inline),
                None => return Err(Error::usage(format!("unexpected argument '{tok}' found"), &m)),
            }
        } else {
            let c = tok[1..].chars().next().expect("tok is longer than '-'");
            let rest = &tok[1 + c.len_utf8()..];
            match candidates.iter().find(|a| a.short == Some(c)) {
                Some(a) => (*a, (!rest.is_empty()).then(|| rest.trim_start_matches('=').to_string())),
                None => return Err(Error::usage(format!("unexpected argument '{tok}' found"), &m)),
            }
        };
        match arg.kind {
            Kind::Flag => {
                if inline.is_some() {
                    return Err(Error::usage(format!("unexpected value for '{}'", arg.display()), &m));
                }
                m.values.insert(arg.id, Vec::new());
            }
            _ => {
                let value = match inline {
                    Some(v) => v,
                    None if arg.missing.is_some() => {
                        // Optional value: take the next token only if it is not an option.
                        match argv.get(i) {
                            Some(n) if !n.starts_with('-') => {
                                i += 1;
                                n.clone()
                            }
                            _ => arg.missing.unwrap().to_string(),
                        }
                    }
                    None => match argv.get(i) {
                        Some(n) => {
                            i += 1;
                            n.clone()
                        }
                        None => {
                            return Err(Error::usage(
                                format!("a value is required for '{}' but none was supplied", arg.display()),
                                &m,
                            ))
                        }
                    },
                };
                let parts: Vec<String> = match arg.delimiter {
                    Some(d) => value.split(d).map(String::from).collect(),
                    None => vec![value],
                };
                let slot = m.values.entry(arg.id).or_default();
                if arg.kind == Kind::Opt && !slot.is_empty() {
                    return Err(Error::usage(format!("the argument '{}' cannot be used multiple times", arg.display()), &m));
                }
                slot.extend(parts);
            }
        }
        m.shown.insert(arg.id.to_string(), arg.display());
    }

    let cmd = *chain.last().expect("chain starts with root");
    if want_help {
        return Err(Error::Info(help(&chain, &globals)));
    }
    if !cmd.subs.is_empty() {
        let names: Vec<&str> = cmd.subs.iter().filter(|c| !c.hidden).flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied())).chain(["help"]).collect();
        return Err(Error::Usage {
            msg: format!("'{}' requires a subcommand but one was not provided\n  [subcommands: {}]", path_name(&chain), names.join(", ")),
            usage: usage(&chain),
        });
    }

    // Positionals, in order.
    let mut pos = positional.into_iter();
    for a in cmd.args.iter().filter(|a| a.is_positional()) {
        m.shown.insert(a.id.to_string(), a.display());
        match a.kind {
            Kind::PosMulti => {
                let rest: Vec<String> = pos.by_ref().collect();
                if !rest.is_empty() {
                    m.values.insert(a.id, rest);
                }
            }
            _ => {
                if let Some(v) = pos.next() {
                    m.values.insert(a.id, vec![v]);
                } else if a.kind == Kind::Pos && a.default.is_none() {
                    return Err(Error::usage(format!("the following required arguments were not provided:\n  {}", a.display()), &m));
                }
            }
        }
    }
    if let Some(extra) = pos.next() {
        return Err(Error::usage(format!("unexpected argument '{extra}' found"), &m));
    }

    // Conflicts, environment, defaults.
    let all: Vec<&Arg> = cmd.args.iter().chain(globals.iter().copied()).collect();
    for a in &all {
        if let Some(other) = a.conflicts {
            if m.values.contains_key(a.id) && m.values.contains_key(other) {
                let o = all.iter().find(|x| x.id == other).map_or(other.to_string(), |x| x.display());
                return Err(Error::usage(format!("the argument '{}' cannot be used with '{o}'", a.display()), &m));
            }
        }
    }
    for a in &all {
        if m.values.contains_key(a.id) {
            continue;
        }
        if let Some(v) = a.env.and_then(|e| std::env::var(e).ok()).filter(|v| !v.is_empty()) {
            m.values.insert(a.id, vec![v]);
        } else if let Some(d) = a.default {
            m.values.insert(a.id, vec![d.to_string()]);
        }
    }
    Ok(m)
}

fn path_name(chain: &[&Cmd]) -> String {
    chain.iter().map(|c| c.name).collect::<Vec<_>>().join(" ")
}

/// `psync task add [OPTIONS] <TITLE>`
fn usage(chain: &[&Cmd]) -> String {
    let cmd = chain.last().unwrap();
    let mut s = path_name(chain);
    if cmd.args.iter().any(|a| !a.is_positional()) || chain.len() == 1 {
        s.push_str(" [OPTIONS]");
    }
    if !cmd.subs.is_empty() {
        s.push_str(" <COMMAND>");
    }
    for a in cmd.args.iter().filter(|a| a.is_positional()) {
        let p = a.placeholder();
        s.push_str(&match a.kind {
            Kind::Pos if a.default.is_none() => format!(" <{p}>"),
            Kind::PosMulti => format!(" [{p}]..."),
            _ => format!(" [{p}]"),
        });
    }
    s
}

/// Help text for the last command in `chain`.
fn help(chain: &[&Cmd], globals: &[&Arg]) -> String {
    let cmd = chain.last().unwrap();
    let mut s = String::new();
    s.push_str(cmd.about);
    s.push('\n');
    if !cmd.long_about.is_empty() {
        s.push('\n');
        s.push_str(cmd.long_about);
        s.push('\n');
    }
    s.push_str(&format!("\nUsage: {}\n", usage(chain)));

    let table = |rows: Vec<(String, String)>| -> String {
        let w = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
        rows.iter()
            .map(|(l, r)| if r.is_empty() { format!("  {l}\n") } else { format!("  {l:<w$}  {r}\n") })
            .collect()
    };

    if !cmd.subs.is_empty() {
        let mut rows: Vec<(String, String)> = cmd
            .subs
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| {
                let alias = match c.aliases {
                    [] => String::new(),
                    [a] => format!(" [alias: {a}]"),
                    many => format!(" [aliases: {}]", many.join(", ")),
                };
                (c.name.to_string(), format!("{}{alias}", c.about))
            })
            .collect();
        rows.push(("help".into(), "Print this message or the help of the given subcommand(s)".into()));
        s.push_str("\nCommands:\n");
        s.push_str(&table(rows));
    }

    let positionals: Vec<&Arg> = cmd.args.iter().filter(|a| a.is_positional()).collect();
    if !positionals.is_empty() {
        let rows = positionals
            .iter()
            .map(|a| {
                let p = a.placeholder();
                let shown = match a.kind {
                    Kind::Pos if a.default.is_none() => format!("<{p}>"),
                    Kind::PosMulti => format!("[{p}]..."),
                    _ => format!("[{p}]"),
                };
                (shown, with_default(a))
            })
            .collect();
        s.push_str("\nArguments:\n");
        s.push_str(&table(rows));
    }

    let mut rows: Vec<(String, String)> = cmd
        .args
        .iter()
        .filter(|a| !a.is_positional())
        .chain(globals.iter().copied().filter(|g| !cmd.args.iter().any(|a| a.id == g.id)))
        .map(|a| {
            let short = a.short.map_or("    ".to_string(), |c| if a.long.is_some() { format!("-{c}, ") } else { format!("-{c}") });
            let long = a.long_name().map_or(String::new(), |l| format!("--{l}"));
            let value = if a.takes_value() {
                if a.missing.is_some() { format!(" [<{}>]", a.placeholder()) } else { format!(" <{}>", a.placeholder()) }
            } else {
                String::new()
            };
            let mut help = with_default(a);
            if let Some(e) = a.env {
                help.push_str(&format!(" [env: {e}={}]", std::env::var(e).unwrap_or_default()));
            }
            if !a.aliases.is_empty() {
                help.push_str(&format!(" [aliases: {}]", a.aliases.iter().map(|x| format!("--{x}")).collect::<Vec<_>>().join(", ")));
            }
            (format!("{short}{long}{value}").trim_end().to_string(), help)
        })
        .collect();
    rows.push(("-h, --help".into(), "Print help".into()));
    if chain.len() == 1 {
        rows.push(("-V, --version".into(), "Print version".into()));
    }
    s.push_str("\nOptions:\n");
    s.push_str(&table(rows));
    s
}

fn with_default(a: &Arg) -> String {
    match a.default {
        Some(d) if !d.is_empty() && !a.help.contains("[default") => format!("{} [default: {d}]", a.help).trim_start().to_string(),
        _ => a.help.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Cmd {
        Cmd::new("tool", "A tool")
            .arg(Arg::flag("json").global().help("JSON"))
            .arg(Arg::opt("agent").global().env("TOOL_TEST_AGENT_UNSET"))
            .arg(Arg::opt("dir").short_only('C').global().value_name("PATH"))
            .sub(
                Cmd::new("log", "Show log")
                    .arg(Arg::opt("limit").short('n').default("20"))
                    .arg(Arg::flag("all")),
            )
            .sub(
                Cmd::new("commit", "Commit")
                    .arg(Arg::opt("message").short('m').alias(&["intent"]))
                    .arg(Arg::multi("meta"))
                    .arg(Arg::pos_multi("paths")),
            )
            .sub(Cmd::new("task", "Tasks").sub(
                Cmd::new("add", "Add").alias(&["new"]).arg(Arg::pos("title")).arg(Arg::multi("after").delimiter(',')),
            ))
            .sub(
                Cmd::new("land", "Land")
                    .arg(Arg::opt("check"))
                    .arg(Arg::flag("no_check").conflicts("check"))
                    .arg(Arg::opt("http").missing("127.0.0.1:0")),
            )
            .sub(Cmd::new("show", "Show").arg(Arg::pos("rev").default("@")))
    }

    fn p(args: &[&str]) -> Result<Matches, Error> {
        let argv: Vec<String> = std::iter::once("tool").chain(args.iter().copied()).map(String::from).collect();
        parse(&root(), "1.0", &argv)
    }

    #[test]
    fn options_positionals_and_globals() {
        let m = p(&["--json", "log", "-n", "5"]).unwrap();
        assert_eq!((m.path.clone(), m.flag("json"), m.get("limit")), (vec!["log"], true, Some("5".into())));
        let m = p(&["log", "-n5", "--all", "--json", "-C", "x"]).unwrap();
        assert_eq!((m.get("limit"), m.flag("all"), m.flag("json"), m.get("dir")), (Some("5".into()), true, true, Some("x".into())));
        assert_eq!(p(&["log"]).unwrap().get("limit"), Some("20".into()), "default");
        let m = p(&["commit", "--intent=fix it", "--meta", "a=1", "--meta", "b=2", "src", "--", "-odd"]).unwrap();
        assert_eq!(m.get("message"), Some("fix it".into()));
        assert_eq!(m.all("meta"), ["a=1", "b=2"]);
        assert_eq!(m.all("paths"), ["src", "-odd"]);
        let m = p(&["task", "new", "Title here", "--after", "T-1,T-2"]).unwrap();
        assert_eq!((m.path.clone(), m.req("title"), m.all("after")), (vec!["task", "add"], "Title here".into(), vec!["T-1".into(), "T-2".into()]));
        assert_eq!(p(&["show"]).unwrap().req("rev"), "@");
        assert_eq!(p(&["land", "--http"]).unwrap().get("http"), Some("127.0.0.1:0".into()));
        assert_eq!(p(&["land", "--http", "1.2.3.4:5"]).unwrap().get("http"), Some("1.2.3.4:5".into()));
        assert_eq!(p(&["land", "--http", "--check", "x"]).unwrap().get("http"), Some("127.0.0.1:0".into()));
    }

    #[test]
    fn errors_and_help() {
        let err = |args: &[&str]| match p(args) {
            Err(Error::Usage { msg, usage }) => (msg, usage),
            other => panic!("expected a usage error, got {other:?}"),
        };
        assert_eq!(err(&["nope"]).0, "unrecognized subcommand 'nope'");
        assert_eq!(err(&["log", "--bogus"]), ("unexpected argument '--bogus' found".into(), "tool log [OPTIONS]".into()));
        assert_eq!(err(&["log", "extra"]).0, "unexpected argument 'extra' found");
        assert!(err(&["task", "add"]).0.contains("<TITLE>"));
        assert!(err(&["land", "--check", "x", "--no-check"]).0.contains("cannot be used with"));
        assert!(err(&["log", "-n"]).0.contains("a value is required for '--limit <LIMIT>'"));
        assert!(err(&["log", "-n", "1", "-n", "2"]).0.contains("cannot be used multiple times"));
        assert!(err(&["task"]).0.contains("requires a subcommand"));
        let help = |args: &[&str]| match p(args) {
            Err(Error::Info(t)) => t,
            other => panic!("expected help, got {other:?}"),
        };
        let h = help(&["--help"]);
        assert!(h.starts_with("A tool\n\nUsage: tool [OPTIONS] <COMMAND>\n") && h.contains("  log ") && h.contains("-V, --version"));
        assert!(help(&["help", "task", "add"]).contains("Usage: tool task add [OPTIONS] <TITLE>"));
        assert!(help(&["task", "add", "-h"]).contains("[alias: new]") || help(&["task", "--help"]).contains("[alias: new]"));
        assert_eq!(help(&["-V"]), "tool 1.0\n");
        let m = p(&["log", "-n", "x"]).unwrap();
        assert!(m.parse("limit", |v| v.parse::<usize>().map_err(|e| e.to_string())).unwrap_err().to_string().contains("invalid value 'x' for '--limit <LIMIT>'"));
    }
}
