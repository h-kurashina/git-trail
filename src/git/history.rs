//! Commit history and reflog (via gix), plus per-file history (via git log --follow).

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use gix::bstr::ByteSlice;
use serde::Serialize;

use crate::error::{Result, TrailError};
use crate::git::diff::ChangeKind;
use crate::git::run_git;

#[derive(Debug, Clone, Serialize)]
pub struct CommitFile {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub author: String,
    pub time: DateTime<Utc>,
    pub parent_count: usize,
    /// Files touched relative to the first parent (empty tree for root commits).
    pub files: Vec<CommitFile>,
}

/// Commits reachable from `head` but not from `hidden` (i.e. `hidden..head`),
/// oldest first. Rename detection is intentionally off here; `trail inspect`
/// uses `git log --follow` when a single file's rename chain matters.
pub fn commits_between(
    repo: &gix::Repository,
    head: gix::ObjectId,
    hidden: gix::ObjectId,
) -> Result<Vec<CommitInfo>> {
    let git = |e: &dyn std::fmt::Display| TrailError::Git(e.to_string());
    let walk = repo
        .rev_walk([head])
        .with_hidden([hidden])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
        .map_err(|e| git(&e))?;

    let mut commits = Vec::new();
    for info in walk {
        let info = info.map_err(|e| git(&e))?;
        let commit = info.object().map_err(|e| git(&e))?;
        let time = commit.time().map_err(|e| git(&e))?;
        let summary = commit
            .message()
            .map(|m| m.summary().to_str_lossy().into_owned())
            .unwrap_or_else(|_| {
                commit
                    .message_raw_sloppy()
                    .to_str_lossy()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string()
            });
        let author = commit
            .author()
            .map(|a| a.name.to_str_lossy().into_owned())
            .unwrap_or_default();
        let parents: Vec<_> = commit.parent_ids().collect();
        let files = files_of_commit(repo, &commit, parents.first().map(|p| p.detach()))?;
        commits.push(CommitInfo {
            id: commit.id().to_string(),
            short_id: commit
                .id()
                .shorten()
                .map(|p| p.to_string())
                .unwrap_or_else(|_| commit.id().to_hex_with_len(7).to_string()),
            summary,
            author,
            time: Utc
                .timestamp_opt(time.seconds, 0)
                .single()
                .unwrap_or_default(),
            parent_count: parents.len(),
            files,
        });
    }
    commits.reverse();
    Ok(commits)
}

fn files_of_commit(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    first_parent: Option<gix::ObjectId>,
) -> Result<Vec<CommitFile>> {
    let git = |e: &dyn std::fmt::Display| TrailError::Git(e.to_string());
    let new_tree = commit.tree().map_err(|e| git(&e))?;
    let old_tree = match first_parent {
        Some(id) => repo
            .find_commit(id)
            .map_err(|e| git(&e))?
            .tree()
            .map_err(|e| git(&e))?,
        None => repo.empty_tree(),
    };

    let mut files = Vec::new();
    let mut platform = old_tree.changes().map_err(|e| git(&e))?;
    platform.options(|opts| {
        opts.track_path();
        opts.track_rewrites(None);
    });
    platform
        .for_each_to_obtain_tree(&new_tree, |change| {
            use gix::object::tree::diff::Change;
            let (location, mode, kind) = match &change {
                Change::Addition {
                    location,
                    entry_mode,
                    ..
                } => (*location, *entry_mode, ChangeKind::Added),
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => (*location, *entry_mode, ChangeKind::Deleted),
                Change::Modification {
                    location,
                    entry_mode,
                    ..
                } => (*location, *entry_mode, ChangeKind::Modified),
                Change::Rewrite {
                    location,
                    entry_mode,
                    ..
                } => (*location, *entry_mode, ChangeKind::Renamed),
            };
            if !mode.is_tree() {
                files.push(CommitFile {
                    path: PathBuf::from(location.to_str_lossy().into_owned()),
                    kind,
                });
            }
            Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue(()))
        })
        .map_err(|e| git(&e))?;
    Ok(files)
}

#[derive(Debug, Clone, Serialize)]
pub struct ReflogEntry {
    pub time: DateTime<Utc>,
    pub previous_id: String,
    pub new_id: String,
    /// e.g. "commit", "checkout", "rebase (finish)", "reset"
    pub action: String,
    pub message: String,
}

/// Entries of the HEAD reflog newer than `since`, oldest first.
/// A missing reflog (e.g. `core.logAllRefUpdates=false`) yields an empty list.
pub fn head_reflog(repo: &gix::Repository, since: DateTime<Utc>) -> Result<Vec<ReflogEntry>> {
    let head = repo.head().map_err(|e| TrailError::Git(e.to_string()))?;
    let mut platform = head.log_iter();
    let mut entries = Vec::new();
    let Some(iter) = platform.rev().map_err(TrailError::Io)? else {
        return Ok(entries);
    };
    for line in iter {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue, // a corrupt line should not hide the rest
        };
        let time = Utc
            .timestamp_opt(line.signature.time.seconds, 0)
            .single()
            .unwrap_or_default();
        if time < since {
            break;
        }
        let message = line.message.to_str_lossy().into_owned();
        let (action, detail) = match message.split_once(": ") {
            Some((a, d)) => (a.to_string(), d.to_string()),
            None => (message.clone(), String::new()),
        };
        entries.push(ReflogEntry {
            time,
            previous_id: line.previous_oid.to_string(),
            new_id: line.new_oid.to_string(),
            action,
            message: detail,
        });
    }
    entries.reverse();
    Ok(entries)
}

#[derive(Debug, Clone, Serialize)]
pub struct FileCommit {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub time: DateTime<Utc>,
}

/// Commits that touched `path` (following renames), newest first, at most `limit`.
pub fn file_commits(workdir: &Path, path: &Path, limit: usize) -> Result<Vec<FileCommit>> {
    let limit = limit.to_string();
    let path = path.to_string_lossy();
    let out = run_git(
        workdir,
        &[
            "log",
            "--follow",
            "--format=%H%x1f%h%x1f%ct%x1f%s",
            "-n",
            &limit,
            "--",
            &path,
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut cols = line.split('\x1f');
            let id = cols.next()?.to_string();
            let short_id = cols.next()?.to_string();
            let secs: i64 = cols.next()?.parse().ok()?;
            let summary = cols.next().unwrap_or("").to_string();
            Some(FileCommit {
                id,
                short_id,
                summary,
                time: Utc.timestamp_opt(secs, 0).single()?,
            })
        })
        .collect())
}
