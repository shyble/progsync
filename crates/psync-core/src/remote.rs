//! Remotes: syncing `main`, tasks and claims between repositories.
//!
//! A remote is another psync repository (for now: a path on this machine;
//! the transfer only needs "list what you have, copy these objects", so a
//! network transport can be added without changing the model). What is
//! shared between repositories is exactly the shared state of a repository:
//! the `main` bookmark, the tasks and the claims. Workspaces and working
//! changes stay local, and each repository keeps its own operation log; fetches and
//! pushes are recorded as operations on the side they change.
//!
//! - `fetch` copies missing objects reachable from the remote's main and
//!   tasks, records the remote's main as the bookmark `<remote>/main`, and
//!   merges tasks.
//! - `pull` = fetch, then moves local main: fast-forward when possible; if
//!   both sides landed different changes, main is set to the remote's and the
//!   local-only landed changes go back into your workspace as unlanded
//!   commits (rebased by `sync`), to be landed again, with the check, on top.
//! - `push` copies objects to the remote and advances its main, only as a
//!   fast-forward (otherwise: pull first), merging tasks there.
//! - Claims travel with fetch and push (T-26): each repository sends its own
//!   active claims and those it has from others, each marked with the
//!   repository it was made in (`Claim::home`), and when each repository's
//!   claims last changed (`View::claim_stamps`, set only by that
//!   repository). The receiver replaces what it has from a repository with
//!   a newer set, so claims and releases reach repositories that only talk
//!   through a shared remote. A repository's own claims are never replaced;
//!   other repositories' claims count like other workspaces' claims.
//! - Tasks merge per id: if one side's version descends from the other's,
//!   the newer wins; if both changed, the version updated last wins and a
//!   merge version records the other (`Task::merged_from`). The merge is
//!   deterministic, so both repositories converge on the same object.

use crate::config;
use crate::hash::{ChangeId, Hash};
use crate::object::{Claim, ClaimHome, Identity, Object, RemoteHead, Revision, Task, View};
use crate::repo::{Repo, SyncOutcome, DEFAULT_WORKSPACE, MAIN};
use crate::transport::{self, local_dot, Address};
use crate::store::Store;
use crate::util::{atomic_write, now_ms, random_hex};
use crate::wc;
use crate::bail;
use crate::error::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_REMOTE: &str = "origin";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// Absolute path of the remote repository's root.
    pub url: String,
}
crate::json_struct!(RemoteInfo { name, url });

pub struct FetchOutcome {
    pub remote: String,
    /// The remote's main (now also `<remote>/main` here).
    pub main: Option<Hash>,
    /// Objects copied.
    pub objects: usize,
    /// Tasks whose local version changed.
    pub tasks: Vec<String>,
    /// Tasks that changed on both sides, with how they were merged.
    pub notes: Vec<String>,
    /// If claims from other repositories changed: how many are active now.
    pub claims: Option<usize>,
    /// If other repositories' workspaces changed: how many are known now.
    pub workspaces: Option<usize>,
    /// Revs (named revisions) new here.
    pub revs: Vec<String>,
}

/// What a repository shares about its own and other repositories' work:
/// claims and workspace heads, each with its `home`, and when each home's
/// claims or workspaces last changed. (Travels as `claims` on the wire.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shared {
    pub claims: Vec<Claim>,
    pub heads: Vec<RemoteHead>,
    pub stamps: BTreeMap<String, i64>,
}
crate::json_struct!(Shared { claims #default, heads #default ?Vec::is_empty, stamps #default });

impl Shared {
    /// Revisions of other repositories' workspaces (not `me`'s own).
    fn revisions(&self, me: &str) -> impl Iterator<Item = Hash> + '_ {
        let me = me.to_string();
        self.heads.iter().filter(move |h| h.home.id != me).map(|h| h.rev)
    }
}

/// Take the claims and workspace heads of every repository in `incoming`
/// that are newer than what `view` has (never this repository's own, `me`;
/// heads only if their revision is here). Returns whether anything changed.
fn merge_shared(view: &mut View, me: &str, incoming: &Shared, has: &dyn Fn(&Hash) -> bool) -> bool {
    let now = now_ms();
    let mut changed = false;
    for (id, &stamp) in &incoming.stamps {
        if id.is_empty() || id == me || view.claim_stamps.get(id).is_some_and(|&have| have >= stamp) {
            continue;
        }
        let from = |c: &&Claim| c.home.as_ref().is_some_and(|h| &h.id == id);
        view.claims.retain(|c| !from(&c) && c.expires_ms > now);
        view.claims.extend(incoming.claims.iter().filter(from).filter(|c| c.expires_ms > now).cloned());
        view.remote_heads.retain(|h| &h.home.id != id);
        view.remote_heads.extend(incoming.heads.iter().filter(|h| &h.home.id == id && has(&h.rev)).cloned());
        view.claim_stamps.insert(id.clone(), stamp);
        changed = true;
    }
    changed
}

/// Add the revs of `incoming` that are new here (and whose revisions are);
/// a name that is another revision here is kept, with a note. Returns the
/// new names.
fn merge_revs(view: &mut View, incoming: &BTreeMap<String, Hash>, has: &dyn Fn(&Hash) -> bool, notes: &mut Vec<String>) -> Vec<String> {
    let mut added = Vec::new();
    for (name, h) in incoming {
        match view.revs.get(name) {
            Some(mine) if mine == h => {}
            Some(mine) => notes.push(format!(
                "rev {name} is {} here but {} in the other repository; kept {}",
                mine.short(),
                h.short(),
                mine.short()
            )),
            None if has(h) => {
                view.revs.insert(name.clone(), *h);
                added.push(name.clone());
            }
            None => {}
        }
    }
    added
}

fn foreign_claims(view: &View) -> usize {
    let now = now_ms();
    view.claims.iter().filter(|c| c.home.is_some() && c.expires_ms > now).count()
}

pub enum PullMain {
    /// The remote has no main yet.
    NoRemoteMain,
    UpToDate,
    /// Local main was behind (or absent) and now equals the remote's.
    FastForward { from: Option<Hash>, to: Hash },
    /// Local main has changes the remote lacks: push them.
    Ahead,
    /// Both sides landed different changes: main is now the remote's, and the
    /// local-only changes were moved back into this workspace.
    Diverged { moved_back: Vec<ChangeId>, sync: SyncOutcome },
}

pub struct PullOutcome {
    pub fetch: FetchOutcome,
    pub main: PullMain,
}

pub struct PushOutcome {
    pub remote: String,
    pub main: Hash,
    pub previous: Option<Hash>,
    pub objects: usize,
    pub notes: Vec<String>,
}

fn remotes_path(dot: &Path) -> PathBuf {
    dot.join("remotes.json")
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !name.starts_with(['.', '-']);
    if !ok {
        bail!("invalid remote name '{name}': use letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

/// Whether task version `b` is `a` or one of its predecessors.
fn task_descends(store: &Store, a: Hash, b: Hash) -> Result<bool> {
    let mut stack = vec![a];
    let mut seen = HashSet::new();
    while let Some(h) = stack.pop() {
        if h == b {
            return Ok(true);
        }
        if !seen.insert(h) {
            continue;
        }
        let t = store.get_task(&h)?;
        stack.extend(t.previous);
        stack.extend(t.merged_from);
    }
    Ok(false)
}

/// Merge two task maps (both sides' objects must be in `store`).
fn merge_tasks(
    store: &Store,
    ours: &BTreeMap<String, Hash>,
    theirs: &BTreeMap<String, Hash>,
    notes: &mut Vec<String>,
) -> Result<BTreeMap<String, Hash>> {
    let mut out = ours.clone();
    for (id, t) in theirs {
        let Some(o) = ours.get(id) else {
            out.insert(id.clone(), *t);
            continue;
        };
        if o == t || task_descends(store, *o, *t)? {
            continue;
        }
        if task_descends(store, *t, *o)? {
            out.insert(id.clone(), *t);
            continue;
        }
        // Changed on both sides: the version updated last wins; the merge
        // version is the same object whichever side computes it.
        let (ot, tt) = (store.get_task(o)?, store.get_task(t)?);
        let ((wh, w), (lh, l)) =
            if (ot.updated_ms, *o) >= (tt.updated_ms, *t) { ((*o, ot), (*t, tt)) } else { ((*t, tt), (*o, ot)) };
        let merged = Task { previous: Some(wh), merged_from: vec![lh], updated_ms: w.updated_ms.max(l.updated_ms), ..w.clone() };
        let mh = store.put(&Object::Task(merged))?;
        notes.push(format!(
            "task {id} changed in both repositories; kept the version updated last (\"{}\", {}); the other (\"{}\", {}) is in its history",
            w.title,
            w.status.name(),
            l.title,
            l.status.name()
        ));
        out.insert(id.clone(), mh);
    }
    Ok(out)
}


impl Repo {
    /// This repository's random id (created on first use).
    pub fn repo_id(&self) -> Result<String> {
        let p = self.dot.join("id");
        let read = || fs::read_to_string(&p).map(|s| s.trim().to_string());
        if let Ok(id) = read() {
            if !id.is_empty() {
                return Ok(id);
            }
        }
        // Written aside and linked into place, so that concurrent first uses
        // agree on one id.
        let tmp = self.dot.join(format!("id.{}", random_hex(4)));
        fs::write(&tmp, format!("{}\n", random_hex(16)))?;
        let linked = fs::hard_link(&tmp, &p);
        let _ = fs::remove_file(&tmp);
        match linked {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
            _ => {}
        }
        Ok(read()?)
    }

    /// This repository as the home of its claims.
    pub fn claim_home(&self) -> Result<ClaimHome> {
        let root = self.dot.parent().unwrap_or(&self.dot);
        Ok(ClaimHome { id: self.repo_id()?, place: format!("{}:{}", crate::platform::hostname(), root.display()) })
    }

    /// The claims this repository passes on to another (see module docs).
    pub fn shared_state(&self) -> Result<Shared> {
        let home = self.claim_home()?;
        let view = self.view()?;
        let now = now_ms();
        let mut stamps = view.claim_stamps;
        let own = stamps.remove("").unwrap_or(0);
        stamps.insert(home.id.clone(), own);
        let mut heads: Vec<RemoteHead> = view
            .workspaces
            .iter()
            .map(|(ws, rev)| RemoteHead { workspace: ws.clone(), rev: *rev, home: home.clone() })
            .collect();
        heads.extend(view.remote_heads);
        let claims = view
            .claims
            .into_iter()
            .filter(|c| c.expires_ms > now)
            .map(|mut c| {
                c.home.get_or_insert_with(|| home.clone());
                c
            })
            .collect();
        Ok(Shared { claims, heads, stamps })
    }

    pub fn remotes(&self) -> Result<Vec<RemoteInfo>> {
        let p = remotes_path(&self.dot);
        if !p.exists() {
            return Ok(vec![]);
        }
        crate::json::from_slice(&fs::read(&p)?).with_context(|| format!("corrupt {}", p.display()))
    }

    pub fn remote(&self, name: &str) -> Result<RemoteInfo> {
        self.remotes()?.into_iter().find(|r| r.name == name).with_context(|| {
            if name == DEFAULT_REMOTE {
                "no remote named 'origin'; add one with `psync remote add origin <path>`".to_string()
            } else {
                format!("no remote named '{name}'")
            }
        })
    }

    pub fn remote_add(&self, name: &str, url: &str) -> Result<RemoteInfo> {
        validate_name(name)?;
        let mut all = self.remotes()?;
        if all.iter().any(|r| r.name == name) {
            bail!("remote '{name}' already exists");
        }
        let url = match Address::parse(url)? {
            Address::Local(p) => {
                let dot = local_dot(&p)?;
                if dot == crate::platform::canonicalize(&self.dot)? {
                    bail!("{url} is this repository");
                }
                let fmt = fs::read_to_string(dot.join("format")).unwrap_or_default();
                if fmt.trim() != crate::repo::REPO_FORMAT {
                    bail!("{url} uses repository format {:?}, this one {:?}", fmt.trim(), crate::repo::REPO_FORMAT);
                }
                dot.parent().expect("has parent").display().to_string()
            }
            Address::Ssh { .. } => {
                // Check now that the remote is reachable and is a repository.
                transport::connect(url, &self.identity)?;
                url.to_string()
            }
        };
        let info = RemoteInfo { name: name.into(), url };
        all.push(info.clone());
        atomic_write(&remotes_path(&self.dot), &crate::json::to_vec_pretty(&all)?)?;
        Ok(info)
    }

    pub fn remote_remove(&self, name: &str) -> Result<()> {
        let mut all = self.remotes()?;
        let before = all.len();
        all.retain(|r| r.name != name);
        if all.len() == before {
            bail!("no remote named '{name}'");
        }
        atomic_write(&remotes_path(&self.dot), &crate::json::to_vec_pretty(&all)?)?;
        let tracking = format!("{name}/{MAIN}");
        self.update_view(None, format!("remove remote {name}"), None, vec![tracking.clone()], |v| {
            Ok(v.bookmarks.remove(&tracking).is_some())
        })?;
        Ok(())
    }

    /// Copy the remote's main and shared state here (see module docs).
    pub fn fetch(&self, name: &str) -> Result<FetchOutcome> {
        let info = self.remote(name)?;
        let mut t = transport::connect(&info.url, &self.identity)?;
        let rview = t.view()?;
        let rmain = rview.main;
        let me = self.repo_id()?;
        let roots: Vec<Hash> = rmain
            .into_iter()
            .chain(rview.tasks.values().copied())
            .chain(rview.revs.values().copied())
            .chain(rview.claims.revisions(&me))
            .collect();
        let objects = transport::fetch_objects(t.as_mut(), &self.store, &roots, &self.spine_heads(name)?)?;
        let tracking = format!("{name}/{MAIN}");
        let mut notes = Vec::new();
        let mut tasks = Vec::new();
        let (mut claims, mut workspaces, mut revs) = (None, None, Vec::new());
        let has = |h: &Hash| self.store.has(h);
        let shared = vec![tracking.clone(), "tasks".into(), "claims".into(), "revs".into()];
        self.update_view(None, format!("fetch from {name}"), None, shared, |v| {
            let mut changed = match rmain {
                Some(m) => v.bookmarks.insert(tracking.clone(), m) != Some(m),
                None => v.bookmarks.remove(&tracking).is_some(),
            };
            let merged = merge_tasks(&self.store, &v.tasks, &rview.tasks, &mut notes)?;
            tasks = merged.iter().filter(|(id, h)| v.tasks.get(*id) != Some(*h)).map(|(id, _)| id.clone()).collect();
            changed |= !tasks.is_empty();
            v.tasks = merged;
            if merge_shared(v, &me, &rview.claims, &has) {
                claims = Some(foreign_claims(v));
                workspaces = Some(v.remote_heads.len());
                changed = true;
            }
            revs = merge_revs(v, &rview.revs, &has, &mut notes);
            changed |= !revs.is_empty();
            Ok(changed)
        })?;
        Ok(FetchOutcome { remote: name.into(), main: rmain, objects, tasks, notes, claims, workspaces, revs })
    }

    /// Fetch, then bring local main up to the remote's (see module docs).
    pub fn pull(&self, name: &str) -> Result<PullOutcome> {
        let fetch = self.fetch(name)?;
        let Some(rm) = fetch.main else { return Ok(PullOutcome { fetch, main: PullMain::NoRemoteMain }) };
        let lm = self.main_tip()?;
        let main = match lm {
            Some(l) if l == rm => PullMain::UpToDate,
            Some(l) if self.is_ancestor(rm, l)? => PullMain::Ahead,
            None => {
                self.move_main(None, rm, format!("pull from {name}: main fast-forwarded"))?;
                PullMain::FastForward { from: None, to: rm }
            }
            Some(l) if self.is_ancestor(l, rm)? => {
                self.move_main(Some(l), rm, format!("pull from {name}: main fast-forwarded"))?;
                PullMain::FastForward { from: Some(l), to: rm }
            }
            Some(l) => {
                let (wh, _) = self.working()?;
                if !self.is_ancestor(l, wh)? {
                    bail!(
                        "main and {name}/main have both changed, and your workspace does not contain main's latest changes; run `psync sync`, then pull again"
                    );
                }
                let moved_back = self.main_changes_not_in(l, rm)?;
                self.move_main(
                    Some(l),
                    rm,
                    format!("pull from {name}: main diverged; {} local change(s) moved back to {}", moved_back.len(), self.workspace),
                )?;
                let sync = self.sync()?;
                PullMain::Diverged { moved_back, sync }
            }
        };
        Ok(PullOutcome { fetch, main })
    }

    /// Change ids on main's first-parent line from `tip` that are not in `other`'s history.
    fn main_changes_not_in(&self, tip: Hash, other: Hash) -> Result<Vec<ChangeId>> {
        let mut ids = Vec::new();
        let mut next = Some(tip);
        while let Some(h) = next {
            if self.is_ancestor(h, other)? {
                break;
            }
            let rev: Revision = self.store.get_revision(&h)?;
            ids.push(rev.change_id);
            next = rev.parents.first().copied();
        }
        ids.reverse();
        Ok(ids)
    }

    /// Set main to `to` if it is still `expect`.
    fn move_main(&self, expect: Option<Hash>, to: Hash, description: String) -> Result<()> {
        self.update_view(None, description, None, vec![MAIN.into()], |v| {
            if v.bookmarks.get(MAIN).copied() != expect {
                bail!("main moved while pulling; pull again");
            }
            v.bookmarks.insert(MAIN.into(), to);
            Ok(true)
        })?;
        Ok(())
    }

    /// Spine objects this repository already has that a remote may share:
    /// where a fetch can stop.
    fn spine_heads(&self, name: &str) -> Result<Vec<Hash>> {
        let v = self.view()?;
        let tracking = v.bookmarks.get(&format!("{name}/{MAIN}")).copied();
        Ok(v.bookmarks
            .get(MAIN)
            .copied()
            .into_iter()
            .chain(tracking)
            .chain(v.tasks.values().copied())
            .chain(v.revs.values().copied())
            .chain(v.workspaces.values().copied())
            .chain(v.remote_heads.iter().map(|h| h.rev))
            .collect())
    }

    /// Send local main and shared state to the remote (main only as a fast-forward).
    pub fn push(&self, name: &str) -> Result<PushOutcome> {
        let info = self.remote(name)?;
        let lm = self.main_tip()?.context("nothing to push: there is no main yet (land a change first)")?;
        let mut t = transport::connect(&info.url, &self.identity)?;
        let rview = t.view()?;
        let rm = rview.main;
        if let Some(r) = rm {
            if r != lm && !(self.store.has(&r) && self.is_ancestor(r, lm)?) {
                bail!("{name} has changes on main that you don't have; run `psync pull {name}` first, then push");
            }
        }
        let local = self.view()?;
        let me = self.repo_id()?;
        let shared = self.shared_state()?;
        let roots: Vec<Hash> = std::iter::once(lm)
            .chain(local.tasks.values().copied())
            .chain(local.revs.values().copied())
            .chain(shared.heads.iter().map(|h| h.rev))
            .collect();
        let objects = transport::push_objects(&self.store, t.as_mut(), &roots, &remote_known(&rview))?;
        let from = self.root.display().to_string();
        let notes = t.advance(rm, lm, &local.tasks, &local.revs, &shared, &self.identity, &from)?;
        let tracking = format!("{name}/{MAIN}");
        let has = |h: &Hash| self.store.has(h);
        let shared = vec![tracking.clone(), "claims".into(), "revs".into()];
        self.update_view(None, format!("push to {name}"), None, shared, |v| {
            let moved = v.bookmarks.insert(tracking.clone(), lm) != Some(lm);
            // What the remote knows of others comes back the same way (its
            // objects are only here if fetched before).
            let mut ignored = Vec::new();
            let revs = merge_revs(v, &rview.revs, &has, &mut ignored);
            Ok(merge_shared(v, &me, &rview.claims, &has) | moved | !revs.is_empty())
        })?;
        Ok(PushOutcome { remote: name.into(), main: lm, previous: rm, objects, notes })
    }

    /// Create a repository at `dest` from the one at `url` (as remote
    /// `origin`), with the default workspace on top of its main.
    pub fn clone_from(url: &str, dest: &Path, identity: Identity) -> Result<Repo> {
        let address = Address::parse(url)?;
        if let Address::Local(p) = &address {
            local_dot(p)?;
        }
        if dest.exists() && fs::read_dir(dest)?.next().is_some() {
            bail!("{} already exists and is not empty", dest.display());
        }
        fs::create_dir_all(dest)?;
        let repo = Repo::init(dest, identity)?;
        repo.remote_add(DEFAULT_REMOTE, url)?;
        // Carry over the shared project settings that make main meaningful.
        let remote_dot = match &address {
            Address::Local(p) => Some(local_dot(p)?),
            Address::Ssh { .. } => None, // settings are not transferred over ssh yet
        };
        for key in ["land.check", crate::msgcheck::KEY] {
            if let Some((check, config::Source::Repo)) = remote_dot.as_deref().and_then(|d| config::get(Some(d), key)) {
                config::set(&config::repo_path(&repo.dot), key, Some(&check))?;
            }
        }
        repo.pull(DEFAULT_REMOTE)?;
        if let Some(m) = repo.main_tip()? {
            let (_, w) = repo.working()?;
            let main_tree = repo.store.get_revision(&m)?.tree;
            let now = now_ms();
            let rev = Revision {
                change_id: ChangeId::random(),
                tree: main_tree,
                parents: vec![m],
                meta: Default::default(),
                author: repo.identity.clone(),
                committer: repo.identity.clone(),
                conflicts: vec![],
                generation: repo.generation_for(&[m])?,
                created_ms: now,
                updated_ms: now,
            };
            let h = repo.store.put(&Object::Revision(rev))?;
            wc::checkout(&repo.root, &repo.store, w.tree, main_tree, repo.eol()?)?;
            repo.set_entry(DEFAULT_WORKSPACE, Some(h), format!("check out main from {url}"), None)?;
            repo.refresh_index()?;
        }
        Ok(repo)
    }
}

/// Objects a remote has, from its view: where sending can stop.
fn remote_known(rview: &transport::RemoteView) -> Vec<Hash> {
    rview
        .main
        .into_iter()
        .chain(rview.tasks.values().copied())
        .chain(rview.revs.values().copied())
        .chain(rview.claims.heads.iter().map(|h| h.rev))
        .collect()
}

impl Repo {
    /// Name revision `spec` `name` (see [`Repo::rev_add`]); with `remote`,
    /// also create it there directly, sending its objects (main is not
    /// touched). Returns notes from the remote.
    pub fn rev_add_remote(&self, name: &str, spec: &str, remote: &str) -> Result<(Hash, Vec<String>)> {
        let h = self.rev_add(name, spec)?;
        let info = self.remote(remote)?;
        let mut t = transport::connect(&info.url, &self.identity)?;
        let rview = t.view()?;
        if let Some(other) = rview.revs.get(name).filter(|o| **o != h) {
            bail!("rev {name} is already {} in {remote}", other.short());
        }
        transport::push_objects(&self.store, t.as_mut(), &[h], &remote_known(&rview))?;
        let revs = BTreeMap::from([(name.to_string(), h)]);
        let from = self.root.display().to_string();
        let notes = t
            .share(&revs, &self.shared_state()?, &self.identity, &from)
            .with_context(|| format!("rev {name} is named here, but not in {remote}"))?;
        Ok((h, notes))
    }
}

/// The receiving side of `rev add --remote`: add the revs and take the
/// sender's shared state, as one operation by the sender.
pub(crate) fn receive_share(repo: &Repo, revs: &BTreeMap<String, Hash>, shared: &Shared, from: &str) -> Result<Vec<String>> {
    if revs.values().any(|h| !repo.store.has(h)) {
        bail!("missing objects; send again");
    }
    let mut notes = Vec::new();
    let me = repo.repo_id()?;
    let has = |h: &Hash| repo.store.has(h);
    repo.update_view(None, format!("revs from {from}"), None, vec!["revs".into(), "claims".into()], |v| {
        let added = merge_revs(v, revs, &has, &mut notes);
        Ok(merge_shared(v, &me, shared, &has) | !added.is_empty())
    })?;
    Ok(notes)
}

/// The receiving side of a push (local or `remote-serve`): check that `main`
/// is present and a fast-forward of `expect`, then advance main (if still
/// `expect`) and merge tasks and claims, as one operation by the pusher.
pub(crate) fn receive_push(
    repo: &Repo,
    expect: Option<Hash>,
    main: Hash,
    tasks: &BTreeMap<String, Hash>,
    revs: &BTreeMap<String, Hash>,
    claims: &Shared,
    from: &str,
) -> Result<Vec<String>> {
    if !repo.store.has(&main) || tasks.values().any(|t| !repo.store.has(t)) {
        bail!("push is missing objects; push again");
    }
    if let Some(e) = expect {
        if e != main && !repo.is_ancestor(e, main)? {
            bail!("not a fast-forward of main; pull first");
        }
    }
    let mut notes = Vec::new();
    let me = repo.repo_id()?;
    let has = |h: &Hash| repo.store.has(h);
    let shared = vec![MAIN.into(), "tasks".into(), "claims".into(), "revs".into()];
    repo.update_view(None, format!("push from {from}"), None, shared, |v| {
        if v.bookmarks.get(MAIN).copied() != expect {
            bail!("main changed while pushing; pull and push again");
        }
        let mut changed = v.bookmarks.insert(MAIN.into(), main) != Some(main);
        let merged = merge_tasks(&repo.store, &v.tasks, tasks, &mut notes)?;
        changed |= merged != v.tasks;
        v.tasks = merged;
        changed |= merge_shared(v, &me, claims, &has);
        changed |= !merge_revs(v, revs, &has, &mut notes).is_empty();
        Ok(changed)
    })?;
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(pattern: &str, home: &str, expires_ms: i64) -> Claim {
        Claim {
            pattern: pattern.into(),
            workspace: "default".into(),
            by: Identity::default(),
            task: None,
            created_ms: 0,
            expires_ms,
            home: (!home.is_empty()).then(|| ClaimHome { id: home.into(), place: format!("h:/{home}") }),
        }
    }

    #[test]
    fn newer_claim_sets_replace_older_ones_and_own_claims_stay() {
        let later = now_ms() + 3_600_000;
        let mut v = View { claims: vec![claim("mine", "", later)], ..Default::default() };
        let set = |stamp: i64, claims: Vec<Claim>| Shared {
            heads: vec![],
            stamps: [("b".to_string(), stamp), ("me".to_string(), 99)].into(),
            claims: claims.into_iter().chain([claim("echo", "me", later)]).collect(),
        };
        let m = |v: &mut View, s: &Shared| merge_shared(v, "me", s, &|_| true);
        assert!(m(&mut v, &set(5, vec![claim("b1", "b", later), claim("old", "b", 1)])));
        let names = |v: &View| v.claims.iter().map(|c| c.pattern.clone()).collect::<Vec<_>>();
        assert_eq!(names(&v), ["mine", "b1"], "own claims echoed back and expired ones are dropped");
        assert!(!m(&mut v, &set(5, vec![])), "same set");
        assert!(!m(&mut v, &set(4, vec![claim("stale", "b", later)])), "an older set, relayed late");
        assert!(m(&mut v, &set(6, vec![])));
        assert_eq!(names(&v), ["mine"], "b released everything");
        assert_eq!(v.claim_stamps.keys().collect::<Vec<_>>(), ["b"]);
    }
}
