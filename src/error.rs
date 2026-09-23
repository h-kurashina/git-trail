//! Error type shared by every layer of `trail`.
//!
//! Every variant maps to a situation a user can understand and act on. Lower
//! level failures (I/O, gix, git CLI) are wrapped so the message stays short.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrailError {
    #[error("{0} is not inside a Git repository\n  hint: run trail from within a repository or worktree, or pass -C <path>")]
    NotARepository(PathBuf),

    #[error("permission denied while reading {0}")]
    PermissionDenied(PathBuf),

    #[error("{0} is a bare repository; trail needs a working tree\n  hint: run trail inside a worktree of this repository")]
    BareRepository(PathBuf),

    #[error(
        "repository has no commits yet\n  hint: create the first commit, then run trail again"
    )]
    EmptyRepository,

    #[error("base branch '{0}' was not found\n  hint: pass --base <branch> (a local branch or origin/<branch>)")]
    BaseBranchNotFound(String),

    #[error("could not detect a base branch (tried main, master and origin/HEAD)\n  hint: pass --base <branch>")]
    NoBaseBranch,

    #[error("no common ancestor between HEAD and '{base}'{shallow}\n  hint: pass --base <branch> that shares history with the current branch", shallow = if *.shallow { " (the repository is a shallow clone, history may be truncated)" } else { "" })]
    NoMergeBase { base: String, shallow: bool },

    #[error("worktree information is unavailable: {0}")]
    WorktreeUnavailable(String),

    #[error("{0} is not tracked and does not exist in the working tree")]
    FileNotFound(PathBuf),

    #[error(
        "no editor configured\n  hint: set $VISUAL or $EDITOR (for example: export EDITOR=nvim)"
    )]
    NoEditor,

    #[error("editor failed: {0}\n  no changes were applied")]
    EditorFailed(String),

    #[error("invalid trail edit: {0}\n  no changes were applied")]
    InvalidEdit(String),

    #[error("cannot resolve baseline: {0}\n  hint: --since accepts push, upstream, base, auto or a revision")]
    NoBaseline(String),

    #[error("{0}")]
    InvalidSelection(String),

    #[error("interactive review needs a terminal\n  hint: run `trail review -i` directly, not through a pipe; `trail review` prints text")]
    NotATerminal,

    #[error("snapshot unavailable: {0}")]
    SnapshotUnavailable(String),

    #[error("git {command} failed: {stderr}")]
    GitCommand { command: String, stderr: String },

    #[error("git executable not found\n  hint: install git and make sure it is on PATH")]
    GitNotInstalled,

    #[error("git error: {0}")]
    Git(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, TrailError>;

impl TrailError {
    /// Map an I/O error to the most specific variant we have.
    pub fn from_io(err: std::io::Error, path: &std::path::Path) -> Self {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            TrailError::PermissionDenied(path.to_path_buf())
        } else {
            TrailError::Io(err)
        }
    }
}
