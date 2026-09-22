//! Domain model of a Development Trail.
//!
//! Everything here is plain data with `serde::Serialize`, so `--json` and the
//! terminal renderer consume exactly the same structures. Future sources
//! (coding agent sessions, file watcher, SQLite cache) only need to produce
//! `TrailEvent`s; nothing in `display/` has to change.

pub mod builder;
pub mod event;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::git::diff::{ChangeKind, FileStat, LineStats, StatusEntry};
use crate::git::history::FileCommit;
use crate::git::repository::HeadState;
use crate::git::worktree::WorktreeInfo;
use event::TrailEvent;

/// Facts about where the trail was recorded.
#[derive(Debug, Clone, Serialize)]
pub struct RepositoryContext {
    pub name: String,
    pub head: HeadState,
    pub base: String,
    pub merge_base: String,
    pub worktree: WorktreeInfo,
    pub shallow: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TrailSummary {
    /// Number of change events (commits and working tree changes).
    pub changes: usize,
    /// Distinct files that differ between the merge base and the working tree.
    pub files_changed: usize,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trail {
    pub repository: RepositoryContext,
    /// Chronological, oldest first. Events without a timestamp come last.
    pub events: Vec<TrailEvent>,
    pub summary: TrailSummary,
}

impl Trail {
    pub fn truncate_to_latest(&mut self, n: usize) {
        if self.events.len() > n {
            let drop = self.events.len() - n;
            self.events.drain(..drop);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusCounts {
    pub modified: usize,
    pub added: usize,
    pub deleted: usize,
    pub renamed: usize,
    pub type_changed: usize,
    pub conflicted: usize,
    pub untracked: usize,
    pub staged: usize,
    pub unstaged: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub repository: RepositoryContext,
    pub counts: StatusCounts,
    /// Working tree vs HEAD (staged + unstaged + untracked).
    pub stats: LineStats,
    /// Commits on this branch that are not on the base.
    pub commits_ahead: usize,
    pub entries: Vec<StatusEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffGroup {
    /// Directory the files live in ("." for the repository root).
    pub directory: String,
    pub files: Vec<FileStat>,
    pub stats: LineStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub repository: RepositoryContext,
    pub groups: Vec<DiffGroup>,
    pub files_changed: usize,
    pub stats: LineStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectReport {
    pub repository: RepositoryContext,
    pub path: PathBuf,
    pub tracked: bool,
    pub exists: bool,
    pub status: Option<StatusEntry>,
    /// Working tree vs HEAD.
    pub worktree_stat: Option<FileStat>,
    /// Working tree vs merge base with the base branch.
    pub base_stat: Option<FileStat>,
    pub last_modified: Option<DateTime<Utc>>,
    pub commits: Vec<InspectCommit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectCommit {
    #[serde(flatten)]
    pub commit: FileCommit,
    /// True when the commit is on this branch but not on the base.
    pub on_branch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub worktree_id: String,
    pub worktree_path: PathBuf,
    /// False when the worktree directory no longer exists.
    pub worktree_exists: bool,
    pub branch: Option<String>,
    pub started_at: DateTime<Utc>,
    /// `None` while the session is still open (or the recorder crashed).
    pub ended_at: Option<DateTime<Utc>>,
    pub last_activity: DateTime<Utc>,
    pub events: usize,
    pub checkpoints: usize,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionsReport {
    pub repository: String,
    pub sessions: Vec<SessionSummary>,
}

/// Helper used by several reports.
pub fn kind_counts(entries: &[StatusEntry]) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for entry in entries {
        if entry.untracked {
            counts.untracked += 1;
            continue;
        }
        if entry.staged.is_some() {
            counts.staged += 1;
        }
        if entry.unstaged.is_some() {
            counts.unstaged += 1;
        }
        match entry.effective_kind() {
            ChangeKind::Added | ChangeKind::Copied => counts.added += 1,
            ChangeKind::Modified => counts.modified += 1,
            ChangeKind::Deleted => counts.deleted += 1,
            ChangeKind::Renamed => counts.renamed += 1,
            ChangeKind::TypeChanged => counts.type_changed += 1,
            ChangeKind::Conflicted => counts.conflicted += 1,
        }
    }
    counts
}
