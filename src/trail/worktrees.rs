//! `trail worktrees`: every worktree of the repository, present or removed.
//!
//! Git knows the worktrees that exist (`.git/worktrees/<id>` plus the main
//! one). trail's store under the common dir knows every worktree that was
//! ever recorded, so a worktree that was removed with `git worktree remove`
//! still appears, with what its sessions remembered about it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Result, TrailError};
use crate::git::history;
use crate::git::repository::Repo;
use crate::git::worktree::WorktreeKind;
use crate::recorder::checkpoint::build_checkpoints;
use crate::recorder::store::{self, SessionFile};

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeEntry {
    /// `main`, or the directory name under `.git/worktrees/`.
    pub id: String,
    pub path: PathBuf,
    pub kind: WorktreeKind,
    /// False when the directory is gone (`git worktree remove`).
    pub exists: bool,
    /// The worktree trail was invoked from.
    pub current: bool,
    pub branch: Option<String>,
    /// HEAD of the worktree; for a removed one, its last recorded position.
    pub head: Option<String>,
    /// Commits ahead of the base branch, when the tip is still known.
    pub commits: Option<usize>,
    pub sessions: usize,
    pub checkpoints: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreesReport {
    pub repository: String,
    pub worktrees: Vec<WorktreeEntry>,
}

/// What the recorded sessions of one worktree remember about it.
struct Recorded {
    path: PathBuf,
    branch: Option<String>,
    sessions: usize,
    checkpoints: usize,
    /// Last known HEAD: the last commit boundary of the latest session, or
    /// the HEAD the latest session started at.
    tip: Option<gix::ObjectId>,
}

fn recorded_by_worktree(repo: &Repo) -> Result<BTreeMap<String, Recorded>> {
    let mut out: BTreeMap<String, Recorded> = BTreeMap::new();
    // Oldest first, so the latest session ends up describing the worktree.
    for session in store::list_sessions(&repo.worktree.common_dir)? {
        let checkpoints = build_checkpoints(&session.header.session_id, &session.records).len();
        let tip = last_head(&session);
        let entry = out
            .entry(session.header.worktree_id.clone())
            .or_insert_with(|| Recorded {
                path: session.header.worktree_path.clone(),
                branch: session.header.branch.clone(),
                sessions: 0,
                checkpoints: 0,
                tip: None,
            });
        entry.path = session.header.worktree_path.clone();
        entry.branch = session.header.branch.clone();
        entry.sessions += 1;
        entry.checkpoints += checkpoints;
        if tip.is_some() {
            entry.tip = tip;
        }
    }
    Ok(out)
}

fn last_head(session: &SessionFile) -> Option<gix::ObjectId> {
    let hex = session
        .records
        .iter()
        .rev()
        .find_map(|r| match r {
            store::Record::Commit { to_head, .. } => Some(to_head.as_str()),
            _ => None,
        })
        .or(session.header.start_head.as_deref())?;
    gix::ObjectId::from_hex(hex.as_bytes()).ok()
}

/// Worktrees git currently has: the main one and every linked one, as
/// `(id, path)`.
fn existing_worktrees(repo: &Repo) -> Result<Vec<(String, PathBuf, WorktreeKind)>> {
    let mut out = Vec::new();
    let main_path = match repo.worktree.kind {
        WorktreeKind::Main => repo.workdir().to_path_buf(),
        WorktreeKind::Linked => repo
            .gix()
            .main_repo()
            .ok()
            .and_then(|m| m.workdir().map(Path::to_path_buf))
            .unwrap_or_else(|| {
                repo.worktree
                    .common_dir
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default()
            }),
    };
    out.push(("main".to_string(), main_path, WorktreeKind::Main));
    let proxies = repo
        .gix()
        .worktrees()
        .map_err(|e| TrailError::WorktreeUnavailable(format!("cannot list worktrees: {e}")))?;
    for proxy in proxies {
        let id = proxy.id().to_string();
        let path = match proxy.base() {
            Ok(p) => p,
            Err(_) => continue, // gitdir file unreadable: treat as removed
        };
        out.push((id, path, WorktreeKind::Linked));
    }
    Ok(out)
}

/// Commits ahead of the base branch for a worktree whose HEAD is `head`.
fn commits_ahead(repo: &Repo, base: Option<&str>) -> Option<usize> {
    let base = repo.resolve_base(base).ok()?;
    history::commits_between(repo.gix(), repo.head_id, base.merge_base)
        .ok()
        .map(|c| c.len())
}

/// Every worktree, existing ones first (in git's order), then removed ones.
pub fn build_worktrees(repo: &Repo, base: Option<&str>) -> Result<WorktreesReport> {
    let current_id = repo.worktree_id();
    let mut recorded = recorded_by_worktree(repo)?;
    let mut worktrees = Vec::new();
    for (id, path, kind) in existing_worktrees(repo)? {
        let rec = recorded.remove(&id);
        let (sessions, checkpoints) = rec
            .as_ref()
            .map(|r| (r.sessions, r.checkpoints))
            .unwrap_or((0, 0));
        // The worktree exists in git; its directory may still be missing
        // (moved, or on an unmounted volume).
        let exists = path.is_dir();
        let target = if id == current_id {
            None
        } else if exists {
            Repo::discover(&path).ok()
        } else {
            None
        };
        let target = target
            .as_ref()
            .or(if id == current_id { Some(repo) } else { None });
        let (branch, head, commits) = match target {
            Some(t) => (
                t.head.branch_name().map(str::to_string),
                Some(t.head_id.to_string()),
                commits_ahead(t, base),
            ),
            None => (
                rec.as_ref().and_then(|r| r.branch.clone()),
                rec.as_ref().and_then(|r| r.tip.map(|t| t.to_string())),
                None,
            ),
        };
        worktrees.push(WorktreeEntry {
            current: id == current_id,
            id,
            path,
            kind,
            exists,
            branch,
            head,
            commits,
            sessions,
            checkpoints,
        });
    }
    for (id, rec) in recorded {
        let removed = removed_worktree(repo, &id, &rec).ok();
        worktrees.push(WorktreeEntry {
            id,
            path: rec.path.clone(),
            kind: WorktreeKind::Linked,
            exists: false,
            current: false,
            branch: rec.branch.clone(),
            head: removed.as_ref().map(|r| r.head_id.to_string()),
            commits: removed.as_ref().and_then(|r| commits_ahead(r, base)),
            sessions: rec.sessions,
            checkpoints: rec.checkpoints,
        });
    }
    Ok(WorktreesReport {
        repository: repo.name.clone(),
        worktrees,
    })
}

/// Reconstruct a removed worktree from what its sessions recorded. The tip
/// is the branch ref when the recorded HEAD is still part of it (commits
/// made after the recorder stopped are then included), otherwise the
/// recorded HEAD itself.
fn removed_worktree(repo: &Repo, id: &str, rec: &Recorded) -> Result<Repo> {
    let recorded_tip = rec.tip.filter(|t| repo.gix().find_commit(*t).is_ok());
    let branch_tip = rec.branch.as_deref().and_then(|name| {
        repo.gix()
            .try_find_reference(&format!("refs/heads/{name}"))
            .ok()
            .flatten()
            .and_then(|r| r.into_fully_peeled_id().ok())
            .map(|id| id.detach())
    });
    let head_id = match (recorded_tip, branch_tip) {
        (Some(rec_tip), Some(br)) if repo.is_ancestor(rec_tip, br) => br,
        (Some(rec_tip), _) => rec_tip,
        (None, Some(br)) => br,
        (None, None) => {
            return Err(TrailError::WorktreeUnavailable(format!(
            "worktree {id} was removed and its last recorded commit is no longer in the repository"
        )))
        }
    };
    Ok(repo.for_removed_worktree(id, &rec.path, rec.branch.as_deref(), head_id))
}

/// Resolve `--worktree <id|branch>` to a repository handle: the current
/// one, another existing worktree, or a removed worktree rebuilt from its
/// recorded history. Ambiguous selectors are an error, never a guess.
pub fn resolve_worktree(repo: &Repo, selector: &str) -> Result<Repo> {
    let selector = selector.trim();
    let recorded = recorded_by_worktree(repo)?;
    let existing = existing_worktrees(repo)?;

    // Candidates are (id, is_existing, path, recorded branch).
    let mut matches: Vec<(String, Option<PathBuf>)> = Vec::new();
    for (id, path, _) in &existing {
        let branch = if path.is_dir() {
            Repo::discover(path)
                .ok()
                .and_then(|r| r.head.branch_name().map(str::to_string))
        } else {
            None
        };
        if *id == selector || branch.as_deref() == Some(selector) {
            matches.push((id.clone(), Some(path.clone())));
        }
    }
    for (id, rec) in &recorded {
        if existing.iter().any(|(e, _, _)| e == id) {
            continue;
        }
        if id == selector || rec.branch.as_deref() == Some(selector) {
            matches.push((id.clone(), None));
        }
    }
    match matches.len() {
        0 => Err(TrailError::InvalidSelection(format!(
            "no worktree with id or branch '{selector}'\n  hint: `trail worktrees` lists them"
        ))),
        1 => {
            let (id, path) = matches.remove(0);
            if id == repo.worktree_id() {
                return Ok(repo.clone_handle());
            }
            match path.filter(|p| p.is_dir()) {
                Some(path) => Repo::discover(&path),
                None => {
                    let rec = recorded.get(&id).ok_or_else(|| {
                        TrailError::WorktreeUnavailable(format!(
                            "worktree {id} is missing from disk and has no recorded history"
                        ))
                    })?;
                    removed_worktree(repo, &id, rec)
                }
            }
        }
        _ => {
            let ids: Vec<&str> = matches.iter().map(|(id, _)| id.as_str()).collect();
            Err(TrailError::InvalidSelection(format!(
                "'{selector}' matches several worktrees: {}\n  hint: pass the worktree id instead of the branch",
                ids.join(", ")
            )))
        }
    }
}
