//! How repositories talk to each other: a small interface implemented for a
//! repository on this machine (direct file access) and over ssh (a
//! `psync remote-serve <path>` process on the other machine, speaking
//! newline-delimited JSON with binary object payloads on stdin/stdout).
//!
//! Both use the same transfer algorithm, which needs few round trips:
//!
//! 1. the *spine* (revisions and task versions reachable from the roots,
//!    stopping at ones the receiver already has) is listed in one request;
//! 2. objects are then requested level by level (revisions, their trees,
//!    subtrees, blobs), so round trips grow with directory depth, not with
//!    history length.
//!
//! Every received object is decompressed and its hash checked before it is
//! stored, and objects are written after everything they reference, so an
//! interrupted transfer never leaves a store with dangling references.
//!
//! Memory stays bounded by [`BATCH_BYTES`] whatever the transfer's size:
//! requests return at most that many bytes (at least one object), received
//! files are stored at once (they refer to nothing), other objects wait in
//! temp files until what they refer to is stored, and a push reads each
//! object from disk as it sends it. Only hashes and references are kept in
//! memory.

use crate::hash::Hash;
use crate::object::{EntryKind, Identity, Object};
use crate::remote::Shared;
use crate::repo::{Repo, DEFAULT_WORKSPACE, DOT_DIR, MAIN};
use crate::store::Store;
use crate::bail;
use crate::error::{Context, Result};
use crate::json;
use crate::json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Objects per request.
const BATCH: usize = 1024;

/// Bytes per request (an object larger than this travels alone).
pub const BATCH_BYTES: usize = 16 << 20;

/// A remote's shared state.
pub struct RemoteView {
    pub main: Option<Hash>,
    pub tasks: BTreeMap<String, Hash>,
    /// Empty from older psync versions.
    pub claims: Shared,
    /// Named revisions; empty from older psync versions.
    pub revs: BTreeMap<String, Hash>,
}

pub trait Transport {
    fn view(&mut self) -> Result<RemoteView>;
    /// Revisions and task versions reachable from `roots`, not descending
    /// into (or listing) anything in `known`.
    fn spine(&mut self, roots: &[Hash], known: &[Hash]) -> Result<Vec<Hash>>;
    fn has(&mut self, hashes: &[Hash]) -> Result<Vec<bool>>;
    /// Stored (compressed) bytes of the first objects of `hashes`: as many
    /// as fit in `max_bytes`, and at least one.
    fn get(&mut self, hashes: &[Hash], max_bytes: usize) -> Result<Vec<Vec<u8>>>;
    /// Store objects; each object's references must already be there.
    fn put(&mut self, objects: &[(Hash, Vec<u8>)]) -> Result<()>;
    /// Advance the remote's main from `expect` to `main` (a fast-forward the
    /// caller has checked) and merge `tasks` and `claims` into its own.
    /// Returns notes about tasks that changed on both sides.
    #[allow(clippy::too_many_arguments)]
    fn advance(
        &mut self,
        expect: Option<Hash>,
        main: Hash,
        tasks: &BTreeMap<String, Hash>,
        revs: &BTreeMap<String, Hash>,
        claims: &Shared,
        actor: &Identity,
        from: &str,
    ) -> Result<Vec<String>>;
    /// Add `revs` (their objects already sent) and merge `claims`, without
    /// touching main. Returns notes about revs that name other revisions.
    fn share(&mut self, revs: &BTreeMap<String, Hash>, claims: &Shared, actor: &Identity, from: &str) -> Result<Vec<String>>;
}

// ---- addresses ----------------------------------------------------------------

/// Where a remote lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Local(PathBuf),
    /// `ssh://[user@]host[:port]/path` or `[user@]host:path`.
    Ssh { host: String, port: Option<u16>, path: String },
}

impl Address {
    pub fn parse(url: &str) -> Result<Address> {
        if let Some(rest) = url.strip_prefix("ssh://") {
            let (authority, path) = rest.split_once('/').context("ssh URL needs a path: ssh://host/path/to/repo")?;
            let (host, port) = match authority.rsplit_once(':') {
                Some((h, p)) if !h.contains(']') || h.ends_with(']') => {
                    (h.to_string(), Some(p.parse().with_context(|| format!("bad port in {url}"))?))
                }
                _ => (authority.to_string(), None),
            };
            if host.is_empty() {
                bail!("ssh URL needs a host: {url}");
            }
            // ssh://host/~/repo is relative to the remote home directory.
            let path = if let Some(home) = path.strip_prefix('~') { format!("~{home}") } else { format!("/{path}") };
            return Ok(Address::Ssh { host, port, path });
        }
        // scp-like host:path (a colon before any slash; not a Windows drive).
        if let Some((host, path)) = url.split_once(':') {
            if !host.is_empty() && !host.contains('/') && host.len() > 1 && !Path::new(url).exists() {
                if path.is_empty() {
                    bail!("missing path after '{host}:'");
                }
                return Ok(Address::Ssh { host: host.into(), port: None, path: path.into() });
            }
        }
        Ok(Address::Local(PathBuf::from(url)))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Address::Local(_))
    }
}

/// The `.psync` directory of the local repository at `url` (its root or its `.psync`).
pub fn local_dot(url: &Path) -> Result<PathBuf> {
    let p = crate::platform::canonicalize(url).with_context(|| format!("no such repository: {}", url.display()))?;
    if p.join(DOT_DIR).is_dir() {
        return Ok(p.join(DOT_DIR));
    }
    if p.file_name().is_some_and(|n| n == DOT_DIR) && p.is_dir() {
        return Ok(p);
    }
    if p.join(DOT_DIR).is_file() {
        bail!("{} is a workspace of another repository; use the repository's main directory", url.display());
    }
    bail!("not a psync repository: {}", url.display())
}

/// Connect to the remote at `url`.
pub fn connect(url: &str, identity: &Identity) -> Result<Box<dyn Transport>> {
    Ok(match Address::parse(url)? {
        Address::Local(p) => {
            let dot = local_dot(&p)?;
            let root = dot.parent().expect(".psync has a parent").to_path_buf();
            Box::new(LocalTransport { repo: Repo::open_handle(dot, DEFAULT_WORKSPACE.into(), root, identity.clone())? })
        }
        Address::Ssh { host, port, path } => Box::new(SshTransport::connect(&host, port, &path)?),
    })
}

// ---- shared helpers ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Blob,
    Other,
}

/// What an object refers to, and whether each reference is a spine object
/// (revision or task version, listed separately).
fn refs(obj: &Object) -> Vec<(Hash, Kind, bool)> {
    let mut out = Vec::new();
    let leaf = |k: EntryKind| if k == EntryKind::Tree { Kind::Other } else { Kind::Blob };
    match obj {
        Object::Tree(t) => out.extend(t.entries.iter().map(|e| (e.hash, leaf(e.kind), false))),
        Object::Chunked(c) => out.extend(c.chunks.iter().map(|(h, _)| (*h, Kind::Blob, false))),
        Object::Revision(r) => {
            out.push((r.tree, Kind::Other, false));
            out.extend(r.parents.iter().map(|p| (*p, Kind::Other, true)));
            out.extend(r.meta.checks.iter().map(|c| (c.tree, Kind::Other, false)));
            for c in &r.conflicts {
                for (k, h) in [c.base, c.ours, c.theirs].into_iter().flatten() {
                    out.push((h, leaf(k), false));
                }
                out.extend(c.written.map(|h| (h, Kind::Blob, false)));
            }
        }
        Object::Task(t) => {
            out.extend(t.previous.map(|h| (h, Kind::Other, true)));
            out.extend(t.merged_from.iter().map(|h| (*h, Kind::Other, true)));
            out.extend(t.completion.as_ref().map(|c| (c.tree, Kind::Other, false)));
        }
        _ => {}
    }
    out
}

/// Spine of a local store: revisions and task versions from `roots`.
pub fn local_spine(store: &Store, roots: &[Hash], known: &[Hash]) -> Result<Vec<Hash>> {
    let known: HashSet<Hash> = known.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut stack: Vec<Hash> = roots.to_vec();
    let mut out = Vec::new();
    while let Some(h) = stack.pop() {
        if known.contains(&h) || !seen.insert(h) || !store.has(&h) {
            continue;
        }
        out.push(h);
        for (r, _, spine) in refs(&store.get(&h)?) {
            if spine {
                stack.push(r);
            }
        }
    }
    Ok(out)
}

/// Visit the objects of `graph` (object -> what it refers to) so that each
/// comes after everything it refers to (references outside `graph` are
/// already stored).
fn post_order(graph: &HashMap<Hash, Vec<Hash>>, mut visit: impl FnMut(Hash) -> Result<()>) -> Result<()> {
    let mut done: HashSet<Hash> = HashSet::new();
    let mut keys: Vec<&Hash> = graph.keys().collect();
    keys.sort(); // deterministic
    for root in keys {
        let mut stack = vec![(*root, false)];
        while let Some((h, children_done)) = stack.pop() {
            if done.contains(&h) {
                continue;
            }
            let Some(children) = graph.get(&h) else { continue };
            if children_done {
                visit(h)?;
                done.insert(h);
            } else {
                stack.push((h, true));
                stack.extend(children.iter().filter(|c| !done.contains(c)).map(|c| (*c, false)));
            }
        }
    }
    Ok(())
}

/// Received objects waiting in temp files; any left are removed on drop
/// (when the transfer fails).
struct Staged(HashMap<Hash, PathBuf>);

impl Drop for Staged {
    fn drop(&mut self) {
        for p in self.0.values() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Copy what `local` lacks of the remote's closure of `roots`. `known` are
/// spine objects `local` has (the walk stops there). Returns objects copied.
pub fn fetch_objects(t: &mut dyn Transport, local: &Store, roots: &[Hash], known: &[Hash]) -> Result<usize> {
    let roots: Vec<Hash> = roots.iter().copied().filter(|h| !local.has(h)).collect();
    if roots.is_empty() {
        return Ok(0);
    }
    let spine = t.spine(&roots, known)?;
    let mut queued: HashSet<Hash> = HashSet::new();
    let mut frontier: Vec<Hash> = Vec::new();
    for h in spine.into_iter().filter(|h| !local.has(h)) {
        if queued.insert(h) {
            frontier.push(h);
        }
    }
    let mut graph: HashMap<Hash, Vec<Hash>> = HashMap::new();
    let mut staged = Staged(HashMap::new());
    let mut files = 0;
    while !frontier.is_empty() {
        let mut next = Vec::new();
        let mut i = 0;
        while i < frontier.len() {
            let hashes = &frontier[i..(i + BATCH).min(frontier.len())];
            let raws = t.get(hashes, BATCH_BYTES)?;
            if raws.is_empty() || raws.len() > hashes.len() {
                bail!("remote sent {} objects for {} requested", raws.len(), hashes.len());
            }
            for (h, raw) in hashes.iter().zip(&raws) {
                let obj = Store::verify_raw(h, raw)?;
                if matches!(obj, Object::Blob(_)) {
                    // Refers to nothing: complete as soon as it is stored.
                    local.put_raw(h, raw)?;
                    files += 1;
                    continue;
                }
                let mut children = Vec::new();
                for (c, _, _) in refs(&obj) {
                    children.push(c);
                    if !local.has(&c) && queued.insert(c) {
                        next.push(c);
                    }
                }
                graph.insert(*h, children);
                staged.0.insert(*h, local.stage(raw)?);
            }
            i += raws.len();
        }
        frontier = next;
    }
    let n = files + graph.len();
    post_order(&graph, |h| {
        let tmp = staged.0.remove(&h).expect("every graph object is staged");
        local.put_staged(&h, &tmp)
    })?;
    Ok(n)
}

/// Copy what the remote lacks of `local`'s closure of `roots`. `known` are
/// spine objects the remote has. Returns objects sent.
pub fn push_objects(local: &Store, t: &mut dyn Transport, roots: &[Hash], known: &[Hash]) -> Result<usize> {
    let spine = local_spine(local, roots, known)?;
    // What the remote lacks, with what each refers to (the bytes are read
    // again when sending).
    let mut missing: HashMap<Hash, Vec<Hash>> = HashMap::new();
    let mut asked: HashSet<Hash> = spine.iter().copied().collect();
    let mut frontier: Vec<(Hash, Kind)> = spine.into_iter().map(|h| (h, Kind::Other)).collect();
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for chunk in frontier.chunks(BATCH) {
            let hashes: Vec<Hash> = chunk.iter().map(|(h, _)| *h).collect();
            let have = t.has(&hashes)?;
            for ((h, kind), present) in chunk.iter().zip(have) {
                if present {
                    continue;
                }
                let mut children = Vec::new();
                // Files refer to nothing (except a large file's chunk list
                // to its chunks): no need to read them now.
                if *kind == Kind::Other || local.tag_of(h)? == crate::object::TAG_CHUNKED {
                    for (c, k, _) in refs(&local.get(h)?) {
                        children.push(c);
                        if asked.insert(c) {
                            next.push((c, k));
                        }
                    }
                }
                missing.insert(*h, children);
            }
        }
        frontier = next;
    }
    let n = missing.len();
    let mut batch: Vec<(Hash, Vec<u8>)> = Vec::new();
    let mut bytes = 0;
    post_order(&missing, |h| {
        let raw = local.read_raw(&h)?;
        if !batch.is_empty() && (bytes + raw.len() > BATCH_BYTES || batch.len() >= BATCH) {
            t.put(&std::mem::take(&mut batch))?;
            bytes = 0;
        }
        bytes += raw.len();
        batch.push((h, raw));
        Ok(())
    })?;
    if !batch.is_empty() {
        t.put(&batch)?;
    }
    Ok(n)
}

// ---- local ------------------------------------------------------------------

/// A repository on this machine, accessed directly.
pub struct LocalTransport {
    repo: Repo,
}

impl Transport for LocalTransport {
    fn view(&mut self) -> Result<RemoteView> {
        let v = self.repo.view()?;
        Ok(RemoteView { main: v.bookmarks.get(MAIN).copied(), tasks: v.tasks, claims: self.repo.shared_state()?, revs: v.revs })
    }

    fn spine(&mut self, roots: &[Hash], known: &[Hash]) -> Result<Vec<Hash>> {
        local_spine(&self.repo.store, roots, known)
    }

    fn has(&mut self, hashes: &[Hash]) -> Result<Vec<bool>> {
        Ok(hashes.iter().map(|h| self.repo.store.has(h)).collect())
    }

    fn get(&mut self, hashes: &[Hash], max_bytes: usize) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut bytes = 0;
        for h in hashes {
            let raw = self.repo.store.read_raw(h)?;
            if !out.is_empty() && bytes + raw.len() > max_bytes {
                break;
            }
            bytes += raw.len();
            out.push(raw);
        }
        Ok(out)
    }

    fn put(&mut self, objects: &[(Hash, Vec<u8>)]) -> Result<()> {
        for (h, raw) in objects {
            Store::verify_raw(h, raw)?;
            self.repo.store.put_raw(h, raw)?;
        }
        Ok(())
    }

    fn advance(
        &mut self,
        expect: Option<Hash>,
        main: Hash,
        tasks: &BTreeMap<String, Hash>,
        revs: &BTreeMap<String, Hash>,
        claims: &Shared,
        actor: &Identity,
        from: &str,
    ) -> Result<Vec<String>> {
        let handle = self.handle(actor)?;
        crate::remote::receive_push(&handle, expect, main, tasks, revs, claims, from)
    }

    fn share(&mut self, revs: &BTreeMap<String, Hash>, claims: &Shared, actor: &Identity, from: &str) -> Result<Vec<String>> {
        crate::remote::receive_share(&self.handle(actor)?, revs, claims, from)
    }
}

impl LocalTransport {
    /// The repository, acting for `actor`.
    fn handle(&self, actor: &Identity) -> Result<Repo> {
        Repo::open_handle(self.repo.dot.clone(), DEFAULT_WORKSPACE.into(), self.repo.root.clone(), actor.clone())
    }
}

// ---- ssh ------------------------------------------------------------------

/// A repository on another machine, through `ssh host psync remote-serve path`.
pub struct SshTransport {
    child: Child,
    to: BufWriter<ChildStdin>,
    from: BufReader<ChildStdout>,
}

fn shell_quote(s: &str) -> String {
    // `~/x` stays expandable by the remote shell; everything else is quoted.
    if let Some(rest) = s.strip_prefix("~/") {
        return format!("~/{}", shell_quote(rest));
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl SshTransport {
    fn connect(host: &str, port: Option<u16>, path: &str) -> Result<SshTransport> {
        // PSYNC_SSH: the ssh command (may include options), like GIT_SSH_COMMAND.
        let ssh = std::env::var("PSYNC_SSH").unwrap_or_else(|_| "ssh".into());
        let mut words = ssh.split_whitespace();
        let mut cmd = Command::new(words.next().context("PSYNC_SSH is empty")?);
        cmd.args(words);
        if let Some(p) = port {
            cmd.arg("-p").arg(p.to_string());
        }
        // PSYNC_REMOTE_PSYNC: how to run psync on the remote machine.
        let remote_psync = std::env::var("PSYNC_REMOTE_PSYNC").unwrap_or_else(|_| "psync".into());
        cmd.arg(host).arg(format!("{remote_psync} remote-serve {}", shell_quote(path)));
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("running {ssh} {host}"))?;
        let to = BufWriter::new(child.stdin.take().expect("piped"));
        let from = BufReader::new(child.stdout.take().expect("piped"));
        let mut t = SshTransport { child, to, from };
        let hello = t
            .request(json!({ "op": "hello", "format": crate::repo::REPO_FORMAT }), &[])
            .with_context(|| format!("connecting to psync on {host} (is psync installed there and is {path} a repository?)"))?;
        if hello["format"] != json!(crate::repo::REPO_FORMAT) {
            bail!("{host}:{path} uses repository format {}, this psync {}", hello["format"], crate::repo::REPO_FORMAT);
        }
        Ok(t)
    }

    /// Send a request (with optional binary payload) and read the reply line.
    fn request(&mut self, req: Value, payload: &[&[u8]]) -> Result<Value> {
        writeln!(self.to, "{req}")?;
        for p in payload {
            self.to.write_all(p)?;
        }
        self.to.flush()?;
        let mut line = String::new();
        if self.from.read_line(&mut line)? == 0 {
            bail!("the remote psync closed the connection");
        }
        let v: Value = crate::json::from_str(&line).context("unexpected reply from the remote psync")?;
        if v["ok"] == json!(false) {
            bail!("remote: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        Ok(v)
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        let _ = writeln!(self.to, "{}", json!({ "op": "bye" }));
        let _ = self.to.flush();
        let _ = self.child.wait();
    }
}

fn hashes_of(v: &Value, key: &str) -> Result<Vec<Hash>> {
    crate::json::from_value(v[key].clone()).with_context(|| format!("bad '{key}' in reply"))
}

impl Transport for SshTransport {
    fn view(&mut self) -> Result<RemoteView> {
        let v = self.request(json!({ "op": "view" }), &[])?;
        let claims = match v.get("claims") {
            Some(c) => crate::json::from_value(c.clone())?,
            None => Shared::default(),
        };
        let revs = match v.get("revs") {
            Some(r) => crate::json::from_value(r.clone())?,
            None => BTreeMap::new(),
        };
        Ok(RemoteView { main: crate::json::from_value(v["main"].clone())?, tasks: crate::json::from_value(v["tasks"].clone())?, claims, revs })
    }

    fn spine(&mut self, roots: &[Hash], known: &[Hash]) -> Result<Vec<Hash>> {
        let v = self.request(json!({ "op": "spine", "roots": roots, "known": known }), &[])?;
        hashes_of(&v, "hashes")
    }

    fn has(&mut self, hashes: &[Hash]) -> Result<Vec<bool>> {
        let v = self.request(json!({ "op": "has", "hashes": hashes }), &[])?;
        crate::json::from_value(v["have"].clone())
    }

    fn get(&mut self, hashes: &[Hash], max_bytes: usize) -> Result<Vec<Vec<u8>>> {
        let v = self.request(json!({ "op": "get", "hashes": hashes, "max_bytes": max_bytes }), &[])?;
        let sizes: Vec<usize> = crate::json::from_value(v["sizes"].clone())?;
        let mut out = Vec::with_capacity(sizes.len());
        for n in sizes {
            let mut buf = vec![0u8; n];
            self.from.read_exact(&mut buf)?;
            out.push(buf);
        }
        Ok(out)
    }

    fn put(&mut self, objects: &[(Hash, Vec<u8>)]) -> Result<()> {
        let hashes: Vec<Hash> = objects.iter().map(|(h, _)| *h).collect();
        let sizes: Vec<usize> = objects.iter().map(|(_, r)| r.len()).collect();
        let payload: Vec<&[u8]> = objects.iter().map(|(_, r)| r.as_slice()).collect();
        self.request(json!({ "op": "put", "hashes": hashes, "sizes": sizes }), &payload)?;
        Ok(())
    }

    fn advance(
        &mut self,
        expect: Option<Hash>,
        main: Hash,
        tasks: &BTreeMap<String, Hash>,
        revs: &BTreeMap<String, Hash>,
        claims: &Shared,
        actor: &Identity,
        from: &str,
    ) -> Result<Vec<String>> {
        let v = self.request(
            json!({ "op": "advance", "expect": expect, "main": main, "tasks": tasks, "revs": revs, "claims": claims, "actor": actor, "from": from }),
            &[],
        )?;
        crate::json::from_value(v["notes"].clone())
    }

    fn share(&mut self, revs: &BTreeMap<String, Hash>, claims: &Shared, actor: &Identity, from: &str) -> Result<Vec<String>> {
        let v = self
            .request(json!({ "op": "share", "revs": revs, "claims": claims, "actor": actor, "from": from }), &[])
            .map_err(|e| match e.to_string().contains("unknown variant `share`") {
                true => crate::err!("the remote's psync is too old to receive revs directly; update it there, or land and push"),
                false => e,
            })?;
        crate::json::from_value(v["notes"].clone())
    }
}

// ---- server ------------------------------------------------------------------

enum Req {
    Hello {
        #[allow(dead_code)]
        format: String,
    },
    View,
    Spine {
        roots: Vec<Hash>,
        known: Vec<Hash>,
    },
    Has {
        hashes: Vec<Hash>,
    },
    Get {
        hashes: Vec<Hash>,
        /// Absent from older clients, which expect every object.
        max_bytes: Option<usize>,
    },
    Put {
        hashes: Vec<Hash>,
        sizes: Vec<usize>,
    },
    Advance {
        expect: Option<Hash>,
        main: Hash,
        tasks: BTreeMap<String, Hash>,
        /// Absent from older clients.
        revs: Option<BTreeMap<String, Hash>>,
        /// Absent from older clients.
        claims: Option<Shared>,
        actor: Identity,
        from: String,
    },
    Share {
        revs: BTreeMap<String, Hash>,
        claims: Shared,
        actor: Identity,
        from: String,
    },
    Bye,
}

struct ErrorReply {
    ok: bool,
    error: String,
}
crate::json_struct!(ErrorReply { ok, error });

impl crate::json::FromJson for Req {
    /// `{"op": "<name>", ...fields}`.
    fn from_json(v: &crate::json::Value) -> Result<Self> {
        fn field<T: crate::json::FromJson>(v: &crate::json::Value, k: &str) -> Result<T> {
            match v.get(k) {
                Some(x) => T::from_json(x).with_context(|| format!("field `{k}`")),
                None => T::missing().with_context(|| format!("missing field `{k}`")),
            }
        }
        Ok(match v["op"].as_str() {
            Some("hello") => Req::Hello { format: field(v, "format")? },
            Some("view") => Req::View,
            Some("spine") => Req::Spine { roots: field(v, "roots")?, known: field(v, "known")? },
            Some("has") => Req::Has { hashes: field(v, "hashes")? },
            Some("get") => Req::Get { hashes: field(v, "hashes")?, max_bytes: field(v, "max_bytes")? },
            Some("put") => Req::Put { hashes: field(v, "hashes")?, sizes: field(v, "sizes")? },
            Some("advance") => Req::Advance {
                expect: field(v, "expect")?,
                main: field(v, "main")?,
                tasks: field(v, "tasks")?,
                revs: field(v, "revs")?,
                claims: field(v, "claims")?,
                actor: field(v, "actor")?,
                from: field(v, "from")?,
            },
            Some("share") => Req::Share {
                revs: field(v, "revs")?,
                claims: field(v, "claims")?,
                actor: field(v, "actor")?,
                from: field(v, "from")?,
            },
            Some("bye") => Req::Bye,
            Some(other) => crate::bail!("unknown variant `{other}`"),
            None => crate::bail!("missing field `op`"),
        })
    }
}

/// Serve the repository at `path` on `input`/`output` (`psync remote-serve`).
pub fn serve(path: &Path, input: impl Read, output: impl Write) -> Result<()> {
    let mut input = BufReader::new(input);
    let mut output = BufWriter::new(output);
    let mut opened = local_dot(path).and_then(|dot| {
        let root = dot.parent().expect(".psync has a parent").to_path_buf();
        let anyone = Identity { name: String::new(), email: String::new(), agent: None };
        let repo = Repo::open_handle(dot, DEFAULT_WORKSPACE.into(), root, anyone)?;
        Ok(LocalTransport { repo })
    });
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let req: Req = match crate::json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                writeln!(output, "{}", crate::json::to_string(&ErrorReply { ok: false, error: format!("bad request: {e}") })?)?;
                output.flush()?;
                continue;
            }
        };
        if matches!(req, Req::Bye) {
            return Ok(());
        }
        // A payload must be read even if the request fails.
        let mut payload: Vec<Vec<u8>> = Vec::new();
        if let Req::Put { sizes, .. } = &req {
            for n in sizes {
                let mut buf = vec![0u8; *n];
                input.read_exact(&mut buf)?;
                payload.push(buf);
            }
        }
        let result: Result<(Value, Vec<Vec<u8>>)> = match opened.as_mut() {
            Err(e) => Err(crate::err!("{e:#}")),
            Ok(t) => handle(t, req, payload),
        };
        match result {
            Ok((v, binary)) => {
                writeln!(output, "{v}")?;
                for b in binary {
                    output.write_all(&b)?;
                }
            }
            Err(e) => writeln!(output, "{}", crate::json::to_string(&ErrorReply { ok: false, error: format!("{e:#}") })?)?,
        }
        output.flush()?;
    }
}

fn handle(t: &mut LocalTransport, req: Req, payload: Vec<Vec<u8>>) -> Result<(Value, Vec<Vec<u8>>)> {
    Ok(match req {
        Req::Hello { .. } => {
            (json!({ "ok": true, "format": crate::repo::REPO_FORMAT, "version": env!("CARGO_PKG_VERSION") }), vec![])
        }
        Req::View => {
            let v = t.view()?;
            (json!({ "ok": true, "main": v.main, "tasks": v.tasks, "claims": v.claims, "revs": v.revs }), vec![])
        }
        Req::Spine { roots, known } => (json!({ "ok": true, "hashes": t.spine(&roots, &known)? }), vec![]),
        Req::Has { hashes } => (json!({ "ok": true, "have": t.has(&hashes)? }), vec![]),
        Req::Get { hashes, max_bytes } => {
            let raws = t.get(&hashes, max_bytes.unwrap_or(usize::MAX))?;
            let sizes: Vec<usize> = raws.iter().map(Vec::len).collect();
            (json!({ "ok": true, "sizes": sizes }), raws)
        }
        Req::Put { hashes, sizes: _ } => {
            if hashes.len() != payload.len() {
                bail!("put: {} hashes but {} objects", hashes.len(), payload.len());
            }
            let objects: Vec<(Hash, Vec<u8>)> = hashes.into_iter().zip(payload).collect();
            t.put(&objects)?;
            (json!({ "ok": true }), vec![])
        }
        Req::Advance { expect, main, tasks, revs, claims, actor, from } => {
            let (revs, claims) = (revs.unwrap_or_default(), claims.unwrap_or_default());
            (json!({ "ok": true, "notes": t.advance(expect, main, &tasks, &revs, &claims, &actor, &from)? }), vec![])
        }
        Req::Share { revs, claims, actor, from } => {
            (json!({ "ok": true, "notes": t.share(&revs, &claims, &actor, &from)? }), vec![])
        }
        Req::Bye => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        let ssh = |h: &str, p: Option<u16>, path: &str| Address::Ssh { host: h.into(), port: p, path: path.into() };
        assert_eq!(Address::parse("ssh://me@box:2222/srv/app").unwrap(), ssh("me@box", Some(2222), "/srv/app"));
        assert_eq!(Address::parse("ssh://box/~/app").unwrap(), ssh("box", None, "~/app"));
        assert_eq!(Address::parse("box:repos/app").unwrap(), ssh("box", None, "repos/app"));
        assert_eq!(Address::parse("../app").unwrap(), Address::Local("../app".into()));
        assert_eq!(Address::parse("/abs/app").unwrap(), Address::Local("/abs/app".into()));
        assert_eq!(Address::parse("C:/work/app").unwrap(), Address::Local("C:/work/app".into()), "Windows drive");
        assert!(Address::parse("ssh://box").is_err());
        assert!(Address::parse("ssh://:22/x").is_err());
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(shell_quote("~/x y"), "~/'x y'");
    }

    #[test]
    fn server_rejects_tampered_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let id = Identity { name: "A".into(), email: "a@x".into(), agent: None };
        Repo::init(tmp.path(), id).unwrap();
        let real = Store::open(&tmp.path().join(DOT_DIR));
        let good = real.put_blob(b"hello").unwrap();
        let raw = real.read_raw(&good).unwrap();
        let fake = Hash::of(b"something else");
        let mut input = Vec::new();
        for h in [fake, good] {
            input.extend(format!("{}\n", json!({ "op": "put", "hashes": [h], "sizes": [raw.len()] })).as_bytes());
            input.extend(&raw);
        }
        let mut out = Vec::new();
        serve(tmp.path(), input.as_slice(), &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| crate::json::from_str(l).unwrap()).collect();
        assert_eq!(replies[0]["ok"], json!(false));
        assert!(replies[0]["error"].as_str().unwrap().contains("does not match"), "{}", replies[0]);
        assert_eq!(replies[1]["ok"], json!(true));
        assert!(!real.has(&fake));
    }

    /// A transport that answers at most one object per `get` and can fail
    /// after a number of them.
    struct Flaky {
        inner: LocalTransport,
        gets_left: usize,
    }

    impl Transport for Flaky {
        fn view(&mut self) -> Result<RemoteView> {
            self.inner.view()
        }
        fn spine(&mut self, roots: &[Hash], known: &[Hash]) -> Result<Vec<Hash>> {
            self.inner.spine(roots, known)
        }
        fn has(&mut self, hashes: &[Hash]) -> Result<Vec<bool>> {
            self.inner.has(hashes)
        }
        fn get(&mut self, hashes: &[Hash], _max_bytes: usize) -> Result<Vec<Vec<u8>>> {
            if self.gets_left == 0 {
                bail!("connection lost");
            }
            self.gets_left -= 1;
            self.inner.get(&hashes[..1], 0)
        }
        fn put(&mut self, objects: &[(Hash, Vec<u8>)]) -> Result<()> {
            self.inner.put(objects)
        }
        fn share(&mut self, _: &BTreeMap<String, Hash>, _: &Shared, _: &Identity, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn advance(&mut self, _: Option<Hash>, _: Hash, _: &BTreeMap<String, Hash>, _: &BTreeMap<String, Hash>, _: &Shared, _: &Identity, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
    }

    fn source_repo(dir: &Path) -> (Repo, Hash) {
        for (rel, text) in [("a.txt", "a\n"), ("src/b.rs", "b\n"), ("src/deep/c.rs", "c\n"), ("docs/d.md", "d\n")] {
            std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
            std::fs::write(dir.join(rel), text).unwrap();
        }
        let id = Identity { name: "A".into(), email: "a@x".into(), agent: None };
        let repo = Repo::init(dir, id).unwrap();
        std::fs::write(dir.join("src/b.rs"), "b2\n").unwrap();
        repo.snapshot().unwrap();
        let head = repo.working().unwrap().0;
        (repo, head)
    }

    /// Every object in `store` has everything it refers to.
    fn assert_complete(store: &Store) {
        for h in store.list().unwrap() {
            for (c, _, _) in refs(&store.get(&h).unwrap()) {
                assert!(store.has(&c), "{} refers to missing {}", h.short(), c.short());
            }
        }
    }

    #[test]
    fn fetch_handles_partial_replies_and_interruptions() {
        let src = tempfile::tempdir().unwrap();
        let (repo, head) = source_repo(src.path());
        assert!(repo.store.list().unwrap().len() > 6, "more objects than the interrupted fetch gets");
        let dot = repo.dot.clone();
        let root = repo.root.clone();
        drop(repo);
        let open = || LocalTransport { repo: Repo::open_handle(dot.clone(), DEFAULT_WORKSPACE.into(), root.clone(), Identity::default()).unwrap() };

        // Interrupted after a few objects: what was stored is complete, and
        // no temp files are left.
        let dst = tempfile::tempdir().unwrap();
        let local = Store::open(dst.path());
        local.create_dirs().unwrap();
        let mut t = Flaky { inner: open(), gets_left: 5 };
        assert!(fetch_objects(&mut t, &local, &[head], &[]).is_err());
        assert_complete(&local);
        assert!(!local.has(&head));
        assert_eq!(std::fs::read_dir(dst.path().join("tmp")).unwrap().count(), 0);

        // One object per reply: the client asks again for the rest.
        let mut t = Flaky { inner: open(), gets_left: usize::MAX };
        let n = fetch_objects(&mut t, &local, &[head], &[]).unwrap();
        assert!(n > 0);
        assert!(local.has(&head));
        assert_complete(&local);
        let mut t = Flaky { inner: open(), gets_left: usize::MAX };
        assert_eq!(fetch_objects(&mut t, &local, &[head], &[]).unwrap(), 0, "nothing left to fetch");
    }

    #[test]
    fn server_get_honours_max_bytes_and_serves_old_clients_everything() {
        let src = tempfile::tempdir().unwrap();
        let (repo, _) = source_repo(src.path());
        let hashes: Vec<Hash> = repo.store.list().unwrap();
        let ask = |max: Option<usize>| {
            let mut req = json!({ "op": "get", "hashes": hashes });
            if let Some(m) = max {
                req["max_bytes"] = json!(m);
            }
            let mut out = Vec::new();
            serve(src.path(), format!("{req}\n").as_bytes(), &mut out).unwrap();
            let line = out.split(|b| *b == b'\n').next().unwrap().to_vec();
            let v: Value = crate::json::from_slice(&line).unwrap();
            v["sizes"].as_array().unwrap().len()
        };
        assert_eq!(ask(None), hashes.len());
        assert_eq!(ask(Some(1)), 1, "at least one object, even if larger");
        assert_eq!(ask(Some(1 << 30)), hashes.len());
    }
}
