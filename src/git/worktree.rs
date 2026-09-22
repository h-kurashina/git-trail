//! Worktree awareness.
//!
//! A linked worktree has a `.git` *file* pointing at `<common>/.git/worktrees/<id>`.
//! `gix` resolves that for us; this module only records what we learned so the
//! rest of the program never has to care whether it runs in the main worktree
//! or a linked one.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Result, TrailError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeKind {
    /// The worktree that owns the `.git` directory.
    Main,
    /// A worktree created with `git worktree add`.
    Linked,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeInfo {
    /// Root of the working tree we are operating on.
    pub root: PathBuf,
    /// Private git dir of this worktree (`.git` or `.git/worktrees/<id>`).
    pub git_dir: PathBuf,
    /// Shared git dir (`.git` of the main worktree).
    pub common_dir: PathBuf,
    pub kind: WorktreeKind,
    /// Identifier of a linked worktree (the directory name under `.git/worktrees`).
    pub id: Option<String>,
    /// Number of *other* worktrees attached to the same repository.
    pub sibling_count: usize,
}

impl WorktreeInfo {
    pub fn from_repo(repo: &gix::Repository) -> Result<Self> {
        let root = repo
            .workdir()
            .ok_or_else(|| TrailError::BareRepository(repo.git_dir().to_path_buf()))?
            .to_path_buf();
        let worktree = repo
            .worktree()
            .ok_or_else(|| TrailError::WorktreeUnavailable("gix reported no worktree".into()))?;
        let kind = if worktree.is_main() {
            WorktreeKind::Main
        } else {
            WorktreeKind::Linked
        };
        let id = worktree.id().map(|id| id.to_string());
        let git_dir = repo.git_dir().to_path_buf();
        let common_dir = normalize(repo.common_dir());

        let sibling_count = match repo.worktrees() {
            Ok(list) => {
                list.into_iter()
                    .filter(|proxy| Some(proxy.id().to_string()) != id)
                    .count()
                    + usize::from(kind == WorktreeKind::Linked)
            } // the main worktree itself
            Err(err) => {
                return Err(TrailError::WorktreeUnavailable(format!(
                    "cannot list worktrees: {err}"
                )))
            }
        };

        Ok(WorktreeInfo {
            root,
            git_dir,
            common_dir,
            kind,
            id,
            sibling_count,
        })
    }
}

/// Collapse `..` segments produced by gix when it resolves `commondir` files.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
