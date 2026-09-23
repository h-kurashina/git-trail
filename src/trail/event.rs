//! `TrailEvent`: the unit of a Development Trail.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::git::diff::LineStats;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrailEventType {
    FileAdded,
    FileModified,
    FileDeleted,
    FileRenamed {
        from: PathBuf,
    },
    Commit {
        id: String,
        short_id: String,
        summary: String,
        author: String,
        /// Merge commits are shown but their file list is relative to the first parent.
        is_merge: bool,
    },
    /// A HEAD movement recorded in the reflog that is not a plain commit
    /// (checkout, rebase, reset, amend, merge, cherry-pick, ...).
    RefUpdate {
        action: String,
        message: String,
    },
    /// A group of changes observed by `trail start`, with any human edits
    /// from the metadata overlay applied.
    Checkpoint {
        id: String,
        session_id: String,
        title: Option<String>,
        annotation: Option<String>,
        bulk: bool,
        hidden: bool,
        /// The HEAD movement the recorder saw after this checkpoint, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        boundary: Option<crate::recorder::checkpoint::CommitBoundary>,
        /// The reviewed commit this checkpoint was work towards; `None` means
        /// it is still uncommitted (working tree). See `trail::attach`.
        commit: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        attachment: Option<Attachment>,
        changes: Vec<crate::recorder::checkpoint::FileChange>,
    },
    // Extension points (not implemented yet):
    // AgentSession { tool: String, ... }  -- Claude Code / Codex session events
    // LogicalGroup { title: String, ... } -- `trail why`
}

/// How a checkpoint was tied to its commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attachment {
    /// The recorder watched the commit being made.
    Recorded,
    /// Matched by author time or by order in time.
    Inferred,
}

/// Where the event was reconstructed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    GitCommit,
    GitReflog,
    WorkingTree,
    Filesystem,
    /// Observed live by `trail start`.
    TrailRecorder,
    // Future: ClaudeCodeSession, CodexSession
}

/// How much to trust the *timestamp and ordering* of an event.
///
/// The change itself is always real (it was read from git or the disk); what
/// may be inferred is *when* it happened. Commit and reflog times are exact.
/// A working tree change only has the file's mtime, which is a lower bound at
/// best: an editor, a checkout or a formatter can rewrite it at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    Inferred,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrailEvent {
    /// `None` when nothing on disk records a time (e.g. a deleted file).
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub event_type: TrailEventType,
    pub files: Vec<PathBuf>,
    pub source: EventSource,
    pub confidence: Confidence,
    pub stats: Option<LineStats>,
    /// Staging state for working tree events: true = staged, false = unstaged,
    /// None = not applicable (commits, untracked files, reflog).
    pub staged: Option<bool>,
}

impl TrailEvent {
    /// Whether the event counts as a "change" in summaries.
    pub fn is_change(&self) -> bool {
        !matches!(self.event_type, TrailEventType::RefUpdate { .. })
    }
}
