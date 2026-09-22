//! `trail review`: how a worktree came to be.
//!
//! The review of a worktree is a list of sections in reading order: one per
//! commit since the baseline, then the working tree. Each section owns the
//! checkpoints that were work towards it (see `trail::attach`) and its own
//! file list:
//!
//! ```text
//! Worktree
//!   Commit / Working tree      (section)
//!     Checkpoint               (how it was made)
//!       File → checkpoint diff (before checkpoint → after checkpoint)
//!     File   → commit diff     (parent tree → commit tree)
//! ```
//!
//! Sessions are the recorder's storage unit and never appear here.
//! Everything is computed once, in this module; text, JSON and the
//! interactive browser only render it.

pub mod diff;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::{Result, TrailError};
use crate::git::diff::{ChangeKind as GitChangeKind, LineStats};
use crate::git::history::{CommitFile as GitCommitFile, CommitInfo};
use crate::git::repository::Repo;
use crate::recorder::checkpoint::{ChangeKind, FileChange};
use crate::recorder::snapshot;
use crate::trail::event::{Attachment, TrailEventType};
use crate::trail::{RepositoryContext, Trail};

/// JSON schema version of [`WorktreeReview`]. Version 1 was a flat list of
/// checkpoints; 2 adds sections and keeps the flat list for compatibility.
pub const REVIEW_VERSION: u32 = 2;

/// The worktree under review.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewWorktree {
    pub id: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    /// False for a worktree that was removed: git and recorded history only.
    pub exists: bool,
    /// The worktree trail was invoked from.
    pub current: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeReview {
    pub version: u32,
    pub repository: RepositoryContext,
    pub worktree: ReviewWorktree,
    /// Commits oldest first, then the working tree.
    pub sections: Vec<ReviewSection>,
    /// Every visible checkpoint in reading order (version 1 shape).
    pub checkpoints: Vec<ReviewCheckpoint>,
    pub summary: ReviewSummary,
    /// Distinct files across all checkpoints (version 1 field).
    pub files_changed: usize,
    /// Sum of all checkpoint diffs (version 1 field).
    pub stats: LineStats,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewSummary {
    pub commits: usize,
    pub checkpoints: usize,
    pub uncommitted_checkpoints: usize,
    /// Distinct files across commit diffs and the working tree.
    pub files_changed: usize,
    /// Sum of commit diffs and the working tree diff.
    pub stats: LineStats,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSection {
    Commit(CommitReview),
    WorkingTree(WorkingTreeReview),
}

impl ReviewSection {
    pub fn number(&self) -> usize {
        match self {
            ReviewSection::Commit(c) => c.number,
            ReviewSection::WorkingTree(w) => w.number,
        }
    }

    pub fn checkpoints(&self) -> &[ReviewCheckpoint] {
        match self {
            ReviewSection::Commit(c) => &c.checkpoints,
            ReviewSection::WorkingTree(w) => &w.checkpoints,
        }
    }

    pub fn stats(&self) -> LineStats {
        match self {
            ReviewSection::Commit(c) => c.stats,
            ReviewSection::WorkingTree(w) => w.stats,
        }
    }

    /// "a82fbc1 Add auth service" or "Working tree".
    pub fn heading(&self) -> String {
        match self {
            ReviewSection::Commit(c) => format!("{} {}", c.short_id, c.summary),
            ReviewSection::WorkingTree(_) => "Working tree".to_string(),
        }
    }

    /// The section's own files (commit diff or working tree state).
    pub fn file_rows(&self) -> Vec<FileRow> {
        match self {
            ReviewSection::Commit(c) => c.files.iter().map(CommitFile::row).collect(),
            ReviewSection::WorkingTree(w) => w.files.iter().map(WorkingTreeFile::row).collect(),
        }
    }

    pub fn as_commit(&self) -> Option<&CommitReview> {
        match self {
            ReviewSection::Commit(c) => Some(c),
            ReviewSection::WorkingTree(_) => None,
        }
    }
}

/// One commit and the checkpoints that led to it.
#[derive(Debug, Clone, Serialize)]
pub struct CommitReview {
    /// 1-based position among the sections.
    pub number: usize,
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub author: String,
    pub time: DateTime<Utc>,
    pub is_merge: bool,
    /// The parent the diff is computed against: the first parent, also for
    /// merges (MVP: a merge shows what it brought onto this branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_parent: Option<String>,
    /// How it was made.
    pub checkpoints: Vec<ReviewCheckpoint>,
    /// What was confirmed: parent tree → commit tree.
    pub files: Vec<CommitFile>,
    pub stats: LineStats,
}

/// One file of a commit diff.
#[derive(Debug, Clone, Serialize)]
pub struct CommitFile {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<PathBuf>,
    pub kind: GitChangeKind,
    pub before_id: Option<String>,
    pub after_id: Option<String>,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub binary: bool,
}

/// Changes since HEAD: what is not committed yet.
#[derive(Debug, Clone, Serialize)]
pub struct WorkingTreeReview {
    pub number: usize,
    /// False for a removed worktree: no working tree state can be read.
    pub available: bool,
    pub checkpoints: Vec<ReviewCheckpoint>,
    pub files: Vec<WorkingTreeFile>,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub stats: LineStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkingTreeFile {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<PathBuf>,
    pub kind: GitChangeKind,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewFile {
    pub path: PathBuf,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<PathBuf>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    /// `None` when the diff could not be computed (binary or no snapshot).
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub binary: bool,
    /// Both sides of the diff are available in the object database.
    pub snapshot: bool,
    pub edits: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewCheckpoint {
    /// Section the checkpoint belongs to.
    pub section: usize,
    /// 1-based position inside its section.
    pub number: usize,
    /// "section.number", the selector shown to the user.
    pub label: String,
    pub id: String,
    pub title: Option<String>,
    pub annotation: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub bulk: bool,
    /// How it was tied to its commit; `None` for the working tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
    pub files: Vec<ReviewFile>,
    pub stats: LineStats,
}

impl ReviewCheckpoint {
    /// The human title, or "Checkpoint <label>" when none was given.
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| format!("Checkpoint {}", self.label))
    }

    pub fn file_rows(&self) -> Vec<FileRow> {
        self.files.iter().map(ReviewFile::row).collect()
    }
}

/// One line of a file list, the same for every kind of file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileRow {
    pub mark: &'static str,
    pub path: PathBuf,
    pub name: String,
    /// "+3 -1", "binary", "snapshot unavailable", ...
    pub stat: String,
    /// "staged", "untracked", ... (working tree only).
    pub state: Option<&'static str>,
}

fn name_of(path: &Path, from: Option<&Path>) -> String {
    match from {
        Some(from) => format!("{} -> {}", from.display(), path.display()),
        None => path.display().to_string(),
    }
}

fn git_mark(kind: GitChangeKind) -> &'static str {
    match kind {
        GitChangeKind::Added | GitChangeKind::Copied => "+",
        GitChangeKind::Deleted => "-",
        GitChangeKind::Renamed => ">",
        GitChangeKind::Modified | GitChangeKind::TypeChanged | GitChangeKind::Conflicted => "~",
    }
}

fn stat_text(binary: bool, additions: Option<u64>, deletions: Option<u64>) -> String {
    if binary {
        "binary".to_string()
    } else {
        LineStats::from_counts(additions, deletions).to_string()
    }
}

impl CommitFile {
    pub fn row(&self) -> FileRow {
        FileRow {
            mark: git_mark(self.kind),
            path: self.path.clone(),
            name: name_of(&self.path, self.old_path.as_deref()),
            stat: stat_text(self.binary, self.additions, self.deletions),
            state: None,
        }
    }
}

impl WorkingTreeFile {
    pub fn row(&self) -> FileRow {
        FileRow {
            mark: git_mark(self.kind),
            path: self.path.clone(),
            name: name_of(&self.path, self.old_path.as_deref()),
            stat: stat_text(self.binary, self.additions, self.deletions),
            state: Some(match (self.untracked, self.staged, self.unstaged) {
                (true, _, _) => "untracked",
                (_, true, true) => "staged, unstaged",
                (_, true, false) => "staged",
                _ => "unstaged",
            }),
        }
    }
}

impl ReviewFile {
    pub fn row(&self) -> FileRow {
        FileRow {
            mark: self.kind.mark(),
            path: self.path.clone(),
            name: name_of(&self.path, self.from_path.as_deref()),
            stat: if !self.snapshot {
                "snapshot unavailable".to_string()
            } else {
                stat_text(self.binary, self.additions, self.deletions)
            },
            state: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Building

/// Content of a snapshot side: `Ok(None)` for "no file" (created/deleted),
/// `Err(())` when the blob is not in the object database.
fn side(repo: &Repo, hash: Option<&str>) -> std::result::Result<Option<Vec<u8>>, ()> {
    match hash {
        None => Ok(None),
        Some(h) => snapshot::read_blob(repo.gix(), h)
            .ok()
            .flatten()
            .map(Some)
            .ok_or(()),
    }
}

fn review_file(repo: &Repo, change: &FileChange) -> ReviewFile {
    let sides = (
        side(repo, change.before_hash.as_deref()),
        side(repo, change.after_hash.as_deref()),
    );
    let (snapshot, binary, stats) = match sides {
        (Ok(before), Ok(after)) => {
            let b = before.unwrap_or_default();
            let a = after.unwrap_or_default();
            if diff::is_binary(&b) || diff::is_binary(&a) {
                (true, true, None)
            } else {
                (true, false, Some(diff::line_stats(&b, &a)))
            }
        }
        _ => (false, false, None),
    };
    ReviewFile {
        path: change.path.clone(),
        kind: change.kind,
        from_path: change.from_path.clone(),
        before_hash: change.before_hash.clone(),
        after_hash: change.after_hash.clone(),
        additions: stats.map(|s| s.additions),
        deletions: stats.map(|s| s.deletions),
        binary,
        snapshot,
        edits: change.edits,
    }
}

/// A commit's file with line stats from its blobs.
fn commit_file(repo: &Repo, file: &GitCommitFile) -> CommitFile {
    let before = side(repo, file.before_id.as_deref()).unwrap_or(None);
    let after = side(repo, file.after_id.as_deref()).unwrap_or(None);
    let b = before.as_deref().unwrap_or_default();
    let a = after.as_deref().unwrap_or_default();
    let binary = diff::is_binary(b) || diff::is_binary(a);
    let stats = (!binary).then(|| diff::line_stats(b, a));
    CommitFile {
        path: file.path.clone(),
        old_path: file.old_path.clone(),
        kind: file.kind,
        before_id: file.before_id.clone(),
        after_id: file.after_id.clone(),
        additions: stats.map(|s| s.additions),
        deletions: stats.map(|s| s.deletions),
        binary,
    }
}

/// Everything the trail knows about one checkpoint, before numbering.
struct PendingCheckpoint<'a> {
    id: &'a str,
    title: &'a Option<String>,
    annotation: &'a Option<String>,
    bulk: bool,
    commit: Option<&'a str>,
    attachment: Option<Attachment>,
    started_at: DateTime<Utc>,
    changes: &'a [FileChange],
}

fn number_checkpoint(
    repo: &Repo,
    p: &PendingCheckpoint<'_>,
    section: usize,
    number: usize,
) -> ReviewCheckpoint {
    let files: Vec<ReviewFile> = p.changes.iter().map(|c| review_file(repo, c)).collect();
    let mut stats = LineStats::default();
    for f in &files {
        stats.additions += f.additions.unwrap_or(0);
        stats.deletions += f.deletions.unwrap_or(0);
    }
    let (started_at, ended_at) = p
        .changes
        .iter()
        .fold((p.started_at, p.started_at), |(s, e), c| {
            (s.min(c.first_seen), e.max(c.last_seen))
        });
    ReviewCheckpoint {
        section,
        number,
        label: format!("{section}.{number}"),
        id: p.id.to_string(),
        title: p.title.clone(),
        annotation: p.annotation.clone(),
        started_at,
        ended_at,
        bulk: p.bulk,
        attachment: p.attachment,
        files,
        stats,
    }
}

/// The commit section for `commit`, with the checkpoints that led to it.
pub fn commit_review(
    repo: &Repo,
    number: usize,
    commit: &CommitInfo,
    checkpoints: Vec<ReviewCheckpoint>,
) -> CommitReview {
    let files: Vec<CommitFile> = commit.files.iter().map(|f| commit_file(repo, f)).collect();
    let mut stats = LineStats::default();
    for f in &files {
        stats.additions += f.additions.unwrap_or(0);
        stats.deletions += f.deletions.unwrap_or(0);
    }
    CommitReview {
        number,
        id: commit.id.clone(),
        short_id: commit.short_id.clone(),
        summary: commit.summary.clone(),
        author: commit.author.clone(),
        time: commit.time,
        is_merge: commit.parent_count > 1,
        diff_parent: commit.first_parent.clone(),
        checkpoints,
        files,
        stats,
    }
}

fn working_tree_review(
    repo: &Repo,
    number: usize,
    checkpoints: Vec<ReviewCheckpoint>,
) -> Result<WorkingTreeReview> {
    let entries = repo.status()?;
    let mut stats_by_path = std::collections::HashMap::new();
    for stat in repo.numstat("HEAD", &[])? {
        stats_by_path.insert(stat.path.clone(), stat);
    }
    for stat in crate::git::diff::untracked_stats(repo.workdir(), &entries) {
        stats_by_path.insert(stat.path.clone(), stat);
    }
    let mut files = Vec::new();
    let (mut staged, mut unstaged, mut untracked) = (0, 0, 0);
    for entry in &entries {
        if entry.untracked {
            untracked += 1;
        } else {
            staged += usize::from(entry.staged.is_some());
            unstaged += usize::from(entry.unstaged.is_some());
        }
        let stat = stats_by_path.get(&entry.path);
        let binary = stat.is_some_and(|s| s.is_binary());
        files.push(WorkingTreeFile {
            path: entry.path.clone(),
            old_path: entry.orig_path.clone(),
            kind: entry.effective_kind(),
            staged: entry.staged.is_some(),
            unstaged: entry.unstaged.is_some(),
            untracked: entry.untracked,
            additions: if binary {
                None
            } else {
                stat.and_then(|s| s.additions)
            },
            deletions: if binary {
                None
            } else {
                stat.and_then(|s| s.deletions)
            },
            binary,
        });
    }
    let mut stats = LineStats::default();
    for f in &files {
        stats.additions += f.additions.unwrap_or(0);
        stats.deletions += f.deletions.unwrap_or(0);
    }
    Ok(WorkingTreeReview {
        number,
        available: repo.live,
        checkpoints,
        files,
        staged,
        unstaged,
        untracked,
        stats,
    })
}

/// Build the review of a trail: commits with their checkpoints, then the
/// working tree. `current` says whether `repo` is the invoking worktree.
pub fn build(repo: &Repo, trail: &Trail, current: bool) -> Result<WorktreeReview> {
    // Commits in trail order (oldest first), and the checkpoints in reading
    // order (the metadata overlay may have permuted them).
    let mut commit_ids: Vec<String> = Vec::new();
    let mut pending: Vec<PendingCheckpoint<'_>> = Vec::new();
    for event in &trail.events {
        match &event.event_type {
            TrailEventType::Commit { id, .. } => commit_ids.push(id.clone()),
            TrailEventType::Checkpoint {
                id,
                title,
                annotation,
                bulk,
                hidden,
                commit,
                attachment,
                changes,
                ..
            } if !*hidden => pending.push(PendingCheckpoint {
                id,
                title,
                annotation,
                bulk: *bulk,
                commit: commit.as_deref(),
                attachment: *attachment,
                started_at: event.timestamp.unwrap_or_default(),
                changes,
            }),
            _ => {}
        }
    }
    let infos = crate::git::history::commits_between(
        repo.gix(),
        repo.head_id,
        trail.repository.since.start,
    )?;

    let mut sections = Vec::new();
    let mut flat = Vec::new();
    for (i, commit_id) in commit_ids.iter().enumerate() {
        let number = i + 1;
        let Some(info) = infos.iter().find(|c| &c.id == commit_id) else {
            continue;
        };
        let mut cps = Vec::new();
        for p in pending
            .iter()
            .filter(|p| p.commit == Some(commit_id.as_str()))
        {
            let cp = number_checkpoint(repo, p, number, cps.len() + 1);
            flat.push(cp.clone());
            cps.push(cp);
        }
        sections.push(ReviewSection::Commit(commit_review(
            repo, number, info, cps,
        )));
    }
    let wt_number = sections.len() + 1;
    let mut cps = Vec::new();
    for p in pending.iter().filter(|p| {
        // Unattached, or attached to a commit that is not in this window.
        p.commit
            .is_none_or(|c| !commit_ids.iter().any(|id| id == c))
    }) {
        let cp = number_checkpoint(repo, p, wt_number, cps.len() + 1);
        flat.push(cp.clone());
        cps.push(cp);
    }
    sections.push(ReviewSection::WorkingTree(working_tree_review(
        repo, wt_number, cps,
    )?));

    let mut distinct_cp = BTreeSet::new();
    let mut cp_stats = LineStats::default();
    for cp in &flat {
        cp_stats.additions += cp.stats.additions;
        cp_stats.deletions += cp.stats.deletions;
        for f in &cp.files {
            distinct_cp.insert(f.path.clone());
        }
    }
    let mut distinct = BTreeSet::new();
    let mut stats = LineStats::default();
    for s in &sections {
        let st = s.stats();
        stats.additions += st.additions;
        stats.deletions += st.deletions;
        for row in s.file_rows() {
            distinct.insert(row.path);
        }
    }
    let uncommitted = sections.last().map(|s| s.checkpoints().len()).unwrap_or(0);
    Ok(WorktreeReview {
        version: REVIEW_VERSION,
        repository: trail.repository.clone(),
        worktree: ReviewWorktree {
            id: repo.worktree_id(),
            path: repo.workdir().to_path_buf(),
            branch: repo.head.branch_name().map(str::to_string),
            exists: repo.live,
            current,
        },
        summary: ReviewSummary {
            commits: commit_ids.len(),
            checkpoints: flat.len(),
            uncommitted_checkpoints: uncommitted,
            files_changed: distinct.len(),
            stats,
        },
        files_changed: distinct_cp.len(),
        stats: cp_stats,
        sections,
        checkpoints: flat,
    })
}

/// `trail review <selector> <file>`: one file's diff.
#[derive(Debug, Clone, Serialize)]
pub struct FileDiffReport {
    pub repository: RepositoryContext,
    /// "commit 1 (a82fbc1) Add auth" or "checkpoint 1.2 (id) \"title\"".
    pub heading: String,
    pub file: FileRow,
    /// Unified diff text (empty when it cannot be shown).
    pub diff: String,
    /// False when the diff is empty because content is missing (no
    /// snapshot, removed worktree), not because nothing changed.
    pub available: bool,
}

/// The diff of `file` within what `selected` names.
pub fn file_diff(
    repo: &Repo,
    review: &WorktreeReview,
    selected: Selected<'_>,
    path: &Path,
) -> Result<FileDiffReport> {
    let (heading, row, target, available) = match selected {
        Selected::Checkpoint(cp) => {
            let file = cp.files.iter().find(|f| f.path == path).ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "{} is not part of checkpoint {} ({})",
                    path.display(),
                    cp.label,
                    cp.id
                ))
            })?;
            (
                format!(
                    "checkpoint {} ({}){}",
                    cp.label,
                    cp.id,
                    cp.title
                        .as_ref()
                        .map(|t| format!(" \"{t}\""))
                        .unwrap_or_default()
                ),
                file.row(),
                DiffTarget::CheckpointFile {
                    label: cp.label.clone(),
                    path: path.to_path_buf(),
                },
                file.snapshot,
            )
        }
        Selected::Section(ReviewSection::Commit(c)) => {
            let file = c.files.iter().find(|f| f.path == path).ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "{} is not part of commit {} ({})",
                    path.display(),
                    c.number,
                    c.short_id
                ))
            })?;
            (
                format!("commit {} ({}) {}", c.number, c.short_id, c.summary),
                file.row(),
                DiffTarget::CommitFile {
                    section: c.number,
                    path: path.to_path_buf(),
                },
                true,
            )
        }
        Selected::Section(ReviewSection::WorkingTree(w)) => {
            if !w.available {
                return Err(TrailError::WorktreeUnavailable(format!(
                    "the worktree was removed, so {} has no working tree state\n  hint: its checkpoints and commits can still be reviewed",
                    path.display()
                )));
            }
            let file = w.files.iter().find(|f| f.path == path).ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "{} has no uncommitted changes",
                    path.display()
                ))
            })?;
            (
                format!("working tree ({})", w.number),
                file.row(),
                DiffTarget::WorkingTreeFile {
                    path: path.to_path_buf(),
                },
                w.available,
            )
        }
    };
    let diff = if available {
        diff_for(repo, review, &target)?
    } else {
        String::new()
    };
    Ok(FileDiffReport {
        repository: review.repository.clone(),
        heading,
        file: row,
        diff,
        available,
    })
}

// ---------------------------------------------------------------------------
// Selection

/// What a selector on the command line named.
#[derive(Debug, Clone, Copy)]
pub enum Selected<'a> {
    Section(&'a ReviewSection),
    Checkpoint(&'a ReviewCheckpoint),
}

/// Find a section by number or commit id (prefix), or a checkpoint by
/// `section.number` label or id.
pub fn select<'a>(review: &'a WorktreeReview, selector: &str) -> Result<Selected<'a>> {
    let selector = selector.trim();
    if let Ok(n) = selector.parse::<usize>() {
        return review
            .sections
            .iter()
            .find(|s| s.number() == n)
            .map(Selected::Section)
            .ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "section {n} does not exist; `trail review` lists 1..{}",
                    review.sections.len()
                ))
            });
    }
    if let Some(cp) = review
        .checkpoints
        .iter()
        .find(|c| c.label == selector || c.id == selector)
    {
        return Ok(Selected::Checkpoint(cp));
    }
    if selector.len() >= 4 && selector.chars().all(|c| c.is_ascii_hexdigit()) {
        let hits: Vec<&ReviewSection> = review
            .sections
            .iter()
            .filter(|s| s.as_commit().is_some_and(|c| c.id.starts_with(selector)))
            .collect();
        match hits.len() {
            1 => return Ok(Selected::Section(hits[0])),
            n if n > 1 => {
                return Err(TrailError::InvalidSelection(format!(
                    "'{selector}' matches {n} commits in this review; use more digits"
                )))
            }
            _ => {}
        }
    }
    Err(TrailError::InvalidSelection(format!(
        "no checkpoint or commit {selector} in this review\n  hint: sections are numbers, checkpoints are <section>.<n>"
    )))
}

/// Find the section of `commit` (any unique prefix), if it is in the window.
pub fn section_of_commit<'a>(review: &'a WorktreeReview, id: &str) -> Option<&'a CommitReview> {
    review
        .sections
        .iter()
        .filter_map(ReviewSection::as_commit)
        .find(|c| c.id == id)
}

// ---------------------------------------------------------------------------
// Diffs

/// Something that can be shown as a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// parent tree → commit tree, every file.
    Commit {
        section: usize,
    },
    CommitFile {
        section: usize,
        path: PathBuf,
    },
    /// before checkpoint → after checkpoint, every file.
    Checkpoint {
        label: String,
    },
    CheckpointFile {
        label: String,
        path: PathBuf,
    },
    /// HEAD → working tree, every file.
    WorkingTree,
    WorkingTreeFile {
        path: PathBuf,
    },
}

/// The unified diff for `target`. Empty when nothing can be shown (missing
/// snapshot, removed worktree).
pub fn diff_for(repo: &Repo, review: &WorktreeReview, target: &DiffTarget) -> Result<String> {
    let section = |n: usize| {
        review
            .sections
            .iter()
            .find(|s| s.number() == n)
            .ok_or_else(|| TrailError::InvalidSelection(format!("section {n} does not exist")))
    };
    let checkpoint = |label: &str| {
        review
            .checkpoints
            .iter()
            .find(|c| c.label == label)
            .ok_or_else(|| TrailError::InvalidSelection(format!("no checkpoint {label}")))
    };
    match target {
        DiffTarget::Commit { section: n } => {
            let s = section(*n)?;
            match s {
                ReviewSection::Commit(c) => {
                    Ok(c.files.iter().map(|f| commit_file_diff(repo, f)).collect())
                }
                ReviewSection::WorkingTree(_) => diff_for(repo, review, &DiffTarget::WorkingTree),
            }
        }
        DiffTarget::CommitFile { section: n, path } => match section(*n)? {
            ReviewSection::Commit(c) => {
                let file = c.files.iter().find(|f| &f.path == path).ok_or_else(|| {
                    TrailError::InvalidSelection(format!(
                        "{} is not part of commit {} ({})",
                        path.display(),
                        c.number,
                        c.short_id
                    ))
                })?;
                Ok(commit_file_diff(repo, file))
            }
            ReviewSection::WorkingTree(_) => diff_for(
                repo,
                review,
                &DiffTarget::WorkingTreeFile { path: path.clone() },
            ),
        },
        DiffTarget::Checkpoint { label } => Ok(checkpoint(label)?
            .files
            .iter()
            .map(|f| checkpoint_file_diff(repo, f))
            .collect()),
        DiffTarget::CheckpointFile { label, path } => {
            let cp = checkpoint(label)?;
            let file = cp.files.iter().find(|f| &f.path == path).ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "{} is not part of checkpoint {} ({})",
                    path.display(),
                    cp.label,
                    cp.id
                ))
            })?;
            Ok(checkpoint_file_diff(repo, file))
        }
        DiffTarget::WorkingTree => {
            let ReviewSection::WorkingTree(w) = review
                .sections
                .last()
                .ok_or_else(|| TrailError::InvalidSelection("no working tree section".into()))?
            else {
                return Ok(String::new());
            };
            Ok(w.files
                .iter()
                .map(|f| working_tree_file_diff(repo, f))
                .collect::<Result<Vec<_>>>()?
                .concat())
        }
        DiffTarget::WorkingTreeFile { path } => {
            let ReviewSection::WorkingTree(w) = review
                .sections
                .last()
                .ok_or_else(|| TrailError::InvalidSelection("no working tree section".into()))?
            else {
                return Ok(String::new());
            };
            let file = w.files.iter().find(|f| &f.path == path).ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "{} has no uncommitted changes",
                    path.display()
                ))
            })?;
            working_tree_file_diff(repo, file)
        }
    }
}

/// Unified diff of one checkpoint file, rebuilt from its snapshots. Empty
/// when a side is missing from the object database.
pub fn checkpoint_file_diff(repo: &Repo, file: &ReviewFile) -> String {
    if !file.snapshot {
        return String::new();
    }
    let before = side(repo, file.before_hash.as_deref()).unwrap_or(None);
    let after = side(repo, file.after_hash.as_deref()).unwrap_or(None);
    diff::unified_diff(
        &file.path,
        file.from_path.as_deref(),
        before.as_deref(),
        after.as_deref(),
    )
}

/// Unified diff of one file of a commit: parent blob → commit blob.
pub fn commit_file_diff(repo: &Repo, file: &CommitFile) -> String {
    let before = side(repo, file.before_id.as_deref()).unwrap_or(None);
    let after = side(repo, file.after_id.as_deref()).unwrap_or(None);
    diff::unified_diff(
        &file.path,
        file.old_path.as_deref(),
        before.as_deref(),
        after.as_deref(),
    )
}

/// Unified diff of one working tree file: HEAD → disk. Untracked files are
/// shown as additions.
pub fn working_tree_file_diff(repo: &Repo, file: &WorkingTreeFile) -> Result<String> {
    if !repo.live {
        return Ok(String::new());
    }
    if file.untracked {
        let data = std::fs::read(repo.workdir().join(&file.path)).unwrap_or_default();
        return Ok(diff::unified_diff(&file.path, None, None, Some(&data)));
    }
    crate::git::diff::unified(repo.workdir(), "HEAD", &file.path)
}

/// Content of a file in a commit's tree, for `trail review <n> <file> --open`.
pub fn blob_at_commit(repo: &Repo, file: &CommitFile) -> Result<Option<Vec<u8>>> {
    match file.after_id.as_deref() {
        Some(id) => snapshot::read_blob(repo.gix(), id),
        None => Ok(None),
    }
}
