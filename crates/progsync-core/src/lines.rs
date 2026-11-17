//! Lines of work beyond your own workspace (T-29): named revisions (revs),
//! other repositories' workspaces, and switching between them.
//!
//! - A **rev** names one revision, for good: `psync rev add try-auth claude-1@`.
//!   Names travel with fetch, pull and push, and `rev add --remote` creates
//!   one directly in another repository (sending the revision's objects,
//!   without touching main). A name is never moved: one that already names
//!   another revision is refused here, and kept (with a note) when two
//!   repositories disagree.
//! - **Other repositories' workspaces** travel like their claims (see
//!   `remote`): `claude-1@laptop` is workspace `claude-1` of the repository on
//!   host `laptop` (or of the one whose id starts with `laptop`), and
//!   `claude-1@` is enough when no local workspace and no other repository's
//!   workspace has that name.
//! - **switch** points this workspace at another line: a new working change
//!   on top of the target, whose files are checked out. The working change
//!   you leave is kept (`View::parked`), and `switch -` returns to it. On
//!   unfinished work (another workspace's working change, or a snapshot of
//!   one), the new change starts as a copy of it with a change id of its
//!   own, so the two can both land later without one replacing the other.

use crate::bail;
use crate::error::{Context, Result};
use crate::hash::{ChangeId, Hash};
use crate::object::{Object, RemoteHead, Revision, View};
use crate::repo::{Repo, MAIN};
use crate::util::now_ms;
use crate::wc;

/// A rev name: like a workspace name, and never readable as a change id or
/// a revision hash (so names and ids cannot be confused).
pub fn validate_rev_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !name.starts_with(['.', '-'])
        && !name.ends_with('-')
        && name != MAIN;
    if !ok {
        bail!("invalid rev name '{name}': use letters, digits, '.', '_' or '-' (not leading, and not a trailing '-'), and not 'main'");
    }
    if ChangeId::is_change_id_text(name) || name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("rev name '{name}' reads as a change id or a revision hash; add a digit, a '-' or another letter (e.g. '{name}-1')");
    }
    Ok(())
}

fn host(h: &RemoteHead) -> &str {
    h.home.place.split(':').next().unwrap_or("")
}

/// How to name each of `heads` in a revision: `workspace@host`, or
/// `workspace@<id prefix>` where two share both.
pub fn head_labels(heads: &[RemoteHead]) -> Vec<String> {
    heads
        .iter()
        .map(|h| {
            let twins = heads.iter().filter(|o| o.workspace == h.workspace && host(o) == host(h)).count();
            if twins > 1 { format!("{}@{}", h.workspace, &h.home.id[..8.min(h.home.id.len())]) } else { format!("{}@{}", h.workspace, host(h)) }
        })
        .collect()
}

pub struct SwitchOutcome {
    /// The working change left (now parked).
    pub from: Hash,
    /// The new working change.
    pub to: Hash,
    pub revision: Revision,
    /// Whether this returned to the parked working change (`switch -`).
    pub back: bool,
}

impl Repo {
    /// Name revision `spec` `name`. Naming the same revision again is fine;
    /// a name already given to another revision is refused.
    pub fn rev_add(&self, name: &str, spec: &str) -> Result<Hash> {
        validate_rev_name(name)?;
        let h = self.resolve(spec)?;
        self.update_view(None, format!("rev {name} = {}", h.short()), None, vec![format!("rev:{name}")], |v| {
            match v.revs.get(name) {
                Some(old) if *old == h => Ok(false),
                Some(old) => bail!("rev {name} already names {} (a rev never moves; pick another name)", old.short()),
                None => {
                    v.revs.insert(name.to_string(), h);
                    Ok(true)
                }
            }
        })?;
        Ok(h)
    }

    /// Forget a rev here (other repositories keep theirs, and a fetch from
    /// one that has it brings it back).
    pub fn rev_remove(&self, name: &str) -> Result<Hash> {
        let mut removed = None;
        self.update_view(None, format!("remove rev {name}"), None, vec![format!("rev:{name}")], |v| {
            removed = v.revs.remove(name);
            Ok(removed.is_some())
        })?;
        removed.with_context(|| format!("no rev named '{name}'"))
    }

    /// Revs with their revisions, by name.
    pub fn revs(&self) -> Result<Vec<(String, Hash, Revision)>> {
        self.view()?.revs.into_iter().map(|(n, h)| Ok((n, h, self.store.get_revision(&h)?))).collect()
    }

    /// The working change this workspace left with `switch`.
    pub fn parked(&self) -> Result<Option<(Hash, Revision)>> {
        match self.view()?.parked.get(&self.workspace) {
            Some(h) => Ok(Some((*h, self.store.get_revision(h)?))),
            None => Ok(None),
        }
    }

    /// Other repositories' workspaces, with the names to use for them.
    pub fn remote_heads(&self) -> Result<Vec<(String, RemoteHead)>> {
        let heads = self.view()?.remote_heads;
        Ok(head_labels(&heads).into_iter().zip(heads).collect())
    }

    /// `<ws>@` or `<ws>@<where>` (see module docs).
    pub(crate) fn resolve_workspace(&self, view: &View, ws: &str, at: &str) -> Result<Hash> {
        if at.is_empty() {
            if let Some(h) = view.workspaces.get(ws) {
                return Ok(*h);
            }
        }
        let labels = head_labels(&view.remote_heads);
        let found: Vec<(&RemoteHead, &String)> = view
            .remote_heads
            .iter()
            .zip(&labels)
            .filter(|(h, _)| h.workspace == ws && (at.is_empty() || host(h) == at || h.home.id.starts_with(at)))
            .collect();
        match found.as_slice() {
            [(h, _)] => Ok(h.rev),
            [] if at.is_empty() => bail!("no workspace named '{ws}' (here or in other repositories, as of the last fetch)"),
            [] => bail!("no workspace '{ws}' at '{at}' (as of the last fetch; `psync context` lists the known ones)"),
            many => bail!(
                "'{ws}@{at}' is ambiguous: {}",
                many.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// A new working change that continues revision `h`. On top of it if it
    /// is committed (on main, or below some workspace's working change);
    /// otherwise it is a snapshot of unfinished work, whose change may still
    /// grow and land later: then a copy of it with a change id of its own,
    /// so that landing one never makes `sync` drop the other.
    pub(crate) fn start_on(&self, h: Hash) -> Result<Revision> {
        let r = self.store.get_revision(&h)?;
        let view = self.view()?;
        let mut committed = match view.bookmarks.get(MAIN) {
            Some(m) => self.is_ancestor(h, *m)?,
            None => false,
        };
        let heads: Vec<Hash> = view.workspaces.values().copied().chain(view.remote_heads.iter().map(|x| x.rev)).collect();
        for head in heads.into_iter().filter(|x| *x != h) {
            if committed {
                break;
            }
            committed = self.is_ancestor(h, head)?;
        }
        if committed {
            return self.new_revision(r.tree, h, Default::default());
        }
        let now = now_ms();
        Ok(Revision {
            change_id: ChangeId::random(),
            generation: self.generation_for(&r.parents)?,
            author: self.identity.clone(),
            committer: self.identity.clone(),
            created_ms: now,
            updated_ms: now,
            ..r
        })
    }

    /// Point this workspace at `spec` (`-`: back to the parked working
    /// change); see module docs.
    pub fn switch(&self, spec: &str) -> Result<SwitchOutcome> {
        let (from, w) = self.working()?;
        let ws = self.workspace.clone();
        let back = spec == "-";
        let (to, revision) = if back {
            let p = self.view()?.parked.get(&ws).copied().context("nothing to switch back to (use `psync switch <rev>` first)")?;
            (p, self.store.get_revision(&p)?)
        } else {
            let target = self.resolve(spec)?;
            if target == from {
                bail!("{spec} is already your working change");
            }
            let rev = self.start_on(target)?;
            (self.store.put(&Object::Revision(rev.clone()))?, rev)
        };
        wc::checkout(&self.root, &self.store, w.tree, revision.tree, self.eol()?)?;
        let desc = if back { "switch back".to_string() } else { format!("switch to {spec}") };
        self.update_view(Some(&ws), desc, None, vec![], |v| {
            v.workspaces.insert(ws.clone(), to);
            v.parked.insert(ws.clone(), from);
            Ok(true)
        })?;
        self.refresh_index()?;
        Ok(SwitchOutcome { from, to, revision, back })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ClaimHome;

    #[test]
    fn rev_names_cannot_read_as_ids() {
        for ok in ["v1.2", "try-auth", "release_3", "x1"] {
            validate_rev_name(ok).unwrap();
        }
        for bad in ["", "main", "-x", "x-", "a/b", "a@b", "kxz", "beef", "123"] {
            assert!(validate_rev_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn labels_use_the_host_unless_two_share_it() {
        let h = |ws: &str, id: &str, place: &str| RemoteHead {
            workspace: ws.into(),
            rev: Hash::of(b"x"),
            home: ClaimHome { id: id.into(), place: place.into() },
        };
        let heads = [h("default", "1111aaaa99", "laptop:/a"), h("default", "2222bbbb99", "laptop:/b"), h("claude-1", "3333", "box:/c")];
        assert_eq!(head_labels(&heads), ["default@1111aaaa", "default@2222bbbb", "claude-1@box"]);
    }
}
