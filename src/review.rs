//! `trail review`: read the changes checkpoint by checkpoint.
//!
//! Git can only show `baseline..HEAD`. With snapshots, trail can show what
//! each checkpoint changed: the diff between a file's content before the
//! checkpoint and at its end. That is the unit of review here.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::{Result, TrailError};
use crate::git::diff::LineStats;
use crate::git::repository::Repo;
use crate::recorder::checkpoint::{ChangeKind, FileChange};
use crate::recorder::snapshot;
use crate::trail::event::TrailEventType;
use crate::trail::{RepositoryContext, Trail};

/// Unified diff context lines.
pub const CONTEXT_LINES: u32 = 3;

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
    /// 1-based position in this review (stable for one `--since` window).
    pub number: usize,
    pub id: String,
    pub title: Option<String>,
    pub annotation: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub bulk: bool,
    pub files: Vec<ReviewFile>,
    pub stats: LineStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewReport {
    pub repository: RepositoryContext,
    pub checkpoints: Vec<ReviewCheckpoint>,
    /// Distinct files across all checkpoints.
    pub files_changed: usize,
    pub stats: LineStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileDiffReport {
    pub repository: RepositoryContext,
    pub checkpoint: ReviewCheckpoint,
    pub file: ReviewFile,
    /// Unified diff text (empty for binary files or missing snapshots).
    pub diff: String,
}

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

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0)
}

/// Line counts of the change from `before` to `after`.
fn line_stats(before: &[u8], after: &[u8]) -> LineStats {
    use gix::diff::blob::{sources::byte_lines, Algorithm, Diff, InternedInput};
    let input = InternedInput::new(byte_lines(before), byte_lines(after));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut stats = LineStats::default();
    for hunk in diff.hunks() {
        stats.deletions += u64::from(hunk.before.end - hunk.before.start);
        stats.additions += u64::from(hunk.after.end - hunk.after.start);
    }
    stats
}

struct UnifiedSink {
    out: String,
}

impl gix::diff::blob::unified_diff::ConsumeHunk for UnifiedSink {
    type Out = String;

    fn consume_hunk(
        &mut self,
        header: gix::diff::blob::unified_diff::HunkHeader,
        lines: &[(gix::diff::blob::unified_diff::DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        use gix::diff::blob::unified_diff::DiffLineKind;
        let _ = writeln!(self.out, "{header}");
        for (kind, line) in lines {
            let prefix = match kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Add => '+',
                DiffLineKind::Remove => '-',
            };
            self.out.push(prefix);
            self.out.push_str(&String::from_utf8_lossy(line));
            if !line.ends_with(b"\n") {
                self.out.push_str("\n\\ No newline at end of file\n");
            }
        }
        Ok(())
    }

    fn finish(self) -> String {
        self.out
    }
}

/// Unified diff between two snapshots, with `---`/`+++` headers.
pub fn unified_diff(
    path: &Path,
    from_path: Option<&Path>,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> String {
    use gix::diff::blob::{
        sources::byte_lines, unified_diff::ContextSize, Algorithm, Diff, InternedInput, UnifiedDiff,
    };
    let mut out = String::new();
    let old_name = match (before, from_path) {
        (None, _) => "/dev/null".to_string(),
        (Some(_), Some(from)) => format!("a/{}", from.display()),
        (Some(_), None) => format!("a/{}", path.display()),
    };
    let new_name = match after {
        None => "/dev/null".to_string(),
        Some(_) => format!("b/{}", path.display()),
    };
    let _ = writeln!(out, "--- {old_name}");
    let _ = writeln!(out, "+++ {new_name}");
    let before = before.unwrap_or_default();
    let after = after.unwrap_or_default();
    if is_binary(before) || is_binary(after) {
        let _ = writeln!(out, "Binary files differ");
        return out;
    }
    let input = InternedInput::new(byte_lines(before), byte_lines(after));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let sink = UnifiedSink { out: String::new() };
    let body = UnifiedDiff::new(&diff, &input, sink, ContextSize::symmetrical(CONTEXT_LINES))
        .consume()
        .unwrap_or_default();
    out.push_str(&body);
    out
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
            if is_binary(&b) || is_binary(&a) {
                (true, true, None)
            } else {
                (true, false, Some(line_stats(&b, &a)))
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

/// Build the review of a trail: visible checkpoints, numbered in reading order.
pub fn build(repo: &Repo, trail: &Trail) -> ReviewReport {
    let mut checkpoints = Vec::new();
    let mut total = LineStats::default();
    let mut distinct = std::collections::BTreeSet::new();
    for event in &trail.events {
        let TrailEventType::Checkpoint {
            id,
            title,
            annotation,
            bulk,
            hidden,
            changes,
            ..
        } = &event.event_type
        else {
            continue;
        };
        if *hidden {
            continue;
        }
        let files: Vec<ReviewFile> = changes.iter().map(|c| review_file(repo, c)).collect();
        let mut stats = LineStats::default();
        for f in &files {
            stats.additions += f.additions.unwrap_or(0);
            stats.deletions += f.deletions.unwrap_or(0);
            distinct.insert(f.path.clone());
        }
        total.additions += stats.additions;
        total.deletions += stats.deletions;
        let (started_at, ended_at) = changes.iter().fold(
            (
                event.timestamp.unwrap_or_default(),
                event.timestamp.unwrap_or_default(),
            ),
            |(s, e), c| (s.min(c.first_seen), e.max(c.last_seen)),
        );
        checkpoints.push(ReviewCheckpoint {
            number: checkpoints.len() + 1,
            id: id.clone(),
            title: title.clone(),
            annotation: annotation.clone(),
            started_at,
            ended_at,
            bulk: *bulk,
            files,
            stats,
        });
    }
    ReviewReport {
        repository: trail.repository.clone(),
        files_changed: distinct.len(),
        stats: total,
        checkpoints,
    }
}

/// Find a checkpoint by review number or id.
pub fn select<'a>(report: &'a ReviewReport, selector: &str) -> Result<&'a ReviewCheckpoint> {
    if let Ok(n) = selector.parse::<usize>() {
        return report
            .checkpoints
            .iter()
            .find(|c| c.number == n)
            .ok_or_else(|| {
                TrailError::InvalidSelection(format!(
                    "checkpoint {n} does not exist; `trail review` lists 1..{}",
                    report.checkpoints.len()
                ))
            });
    }
    report
        .checkpoints
        .iter()
        .find(|c| c.id == selector)
        .ok_or_else(|| {
            TrailError::InvalidSelection(format!("no checkpoint {selector} in this review"))
        })
}

/// Diff of one file within one checkpoint.
pub fn file_diff(
    repo: &Repo,
    report: &ReviewReport,
    checkpoint: &ReviewCheckpoint,
    path: &Path,
) -> Result<FileDiffReport> {
    let file = checkpoint
        .files
        .iter()
        .find(|f| f.path == path)
        .ok_or_else(|| {
            TrailError::InvalidSelection(format!(
                "{} is not part of checkpoint {} ({})",
                path.display(),
                checkpoint.number,
                checkpoint.id
            ))
        })?
        .clone();
    let diff = if !file.snapshot {
        String::new()
    } else {
        let before = side(repo, file.before_hash.as_deref()).unwrap_or(None);
        let after = side(repo, file.after_hash.as_deref()).unwrap_or(None);
        unified_diff(
            &file.path,
            file.from_path.as_deref(),
            before.as_deref(),
            after.as_deref(),
        )
    };
    Ok(FileDiffReport {
        repository: report.repository.clone(),
        checkpoint: checkpoint.clone(),
        file,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_has_headers_and_hunks() {
        let out = unified_diff(
            Path::new("a.rs"),
            None,
            Some(b"one\ntwo\nthree\n"),
            Some(b"one\n2\nthree\n"),
        );
        assert!(
            out.starts_with("--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n"),
            "{out}"
        );
        let created = unified_diff(Path::new("n.rs"), None, None, Some(b"new\n"));
        assert!(
            created.starts_with("--- /dev/null\n+++ b/n.rs\n"),
            "{created}"
        );
        assert!(created.contains("+new\n"));
        let deleted = unified_diff(Path::new("d.rs"), None, Some(b"old\n"), None);
        assert!(deleted.contains("+++ /dev/null\n"));
        assert!(deleted.contains("-old\n"));
        let renamed = unified_diff(
            Path::new("new.rs"),
            Some(Path::new("old.rs")),
            Some(b"x\n"),
            Some(b"x\n"),
        );
        assert!(renamed.starts_with("--- a/old.rs\n+++ b/new.rs\n"));
        assert!(
            !renamed.contains("@@"),
            "identical content has no hunks: {renamed}"
        );
        let binary = unified_diff(Path::new("b.bin"), None, Some(b"\0\x01"), Some(b"\0\x02"));
        assert!(binary.contains("Binary files differ"));
        let no_newline = unified_diff(Path::new("a"), None, Some(b"x"), Some(b"y"));
        assert!(no_newline.contains("\\ No newline at end of file"));
    }

    #[test]
    fn line_stats_count_hunk_sizes() {
        let s = line_stats(b"a\nb\nc\n", b"a\nc\nd\ne\n");
        assert_eq!(s.deletions, 1);
        assert_eq!(s.additions, 2);
    }
}
