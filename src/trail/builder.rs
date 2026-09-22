//! Turns raw git facts into `Trail`s and reports.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::git::diff::{self, ChangeKind, DiffTarget, FileStat, LineStats, StatusEntry};
use crate::git::history;
use crate::git::repository::{BaseRef, Repo};
use crate::trail::event::{Confidence, EventSource, TrailEvent, TrailEventType};
use crate::trail::{
    kind_counts, DiffGroup, DiffReport, InspectCommit, InspectReport, RepositoryContext,
    StatusReport, Trail, TrailSummary,
};

/// How much history to reconstruct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Commits since the base plus working tree changes (`trail`).
    Overview,
    /// Overview plus reflog movements (`trail history`).
    Detailed,
}

fn context(repo: &Repo, base: &BaseRef) -> RepositoryContext {
    RepositoryContext {
        name: repo.name.clone(),
        head: repo.head.clone(),
        base: base.name.clone(),
        merge_base: repo.short_id(&base.merge_base),
        worktree: repo.worktree.clone(),
        shallow: repo.is_shallow,
    }
}

/// Line stats of merge-base..working tree, including untracked files.
/// This is the "what will end up in the PR" view.
fn stats_since_base(repo: &Repo, base: &BaseRef, entries: &[StatusEntry]) -> Result<Vec<FileStat>> {
    let rev = base.merge_base.to_string();
    let mut stats = diff::numstat(repo.workdir(), DiffTarget::Revision(&rev), &[])?;
    stats.extend(diff::untracked_stats(repo.workdir(), entries));
    Ok(stats)
}

/// Line stats of HEAD..working tree, including untracked files.
fn stats_since_head(repo: &Repo, entries: &[StatusEntry]) -> Result<Vec<FileStat>> {
    let mut stats = diff::numstat(repo.workdir(), DiffTarget::Revision("HEAD"), &[])?;
    stats.extend(diff::untracked_stats(repo.workdir(), entries));
    Ok(stats)
}

fn total(stats: &[FileStat]) -> LineStats {
    let mut total = LineStats::default();
    for s in stats {
        total.add(s);
    }
    total
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

pub fn build_trail(repo: &Repo, base: &BaseRef, scope: Scope) -> Result<Trail> {
    let entries = diff::status(repo.workdir())?;
    let commits = history::commits_between(repo.gix(), repo.head_id, base.merge_base)?;
    let head_stats = stats_since_head(repo, &entries)?;
    let base_stats = stats_since_base(repo, base, &entries)?;

    let mut events: Vec<TrailEvent> = Vec::new();

    // 1. Commits on this branch: exact timestamps, exact file lists.
    for commit in &commits {
        events.push(TrailEvent {
            timestamp: Some(commit.time),
            event_type: TrailEventType::Commit {
                id: commit.id.clone(),
                short_id: commit.short_id.clone(),
                summary: commit.summary.clone(),
                author: commit.author.clone(),
                is_merge: commit.parent_count > 1,
            },
            files: commit.files.iter().map(|f| f.path.clone()).collect(),
            source: EventSource::GitCommit,
            confidence: Confidence::Exact,
            stats: None,
            staged: None,
        });
    }

    // 2. Working tree changes: the change is exact, the time is the file's
    //    mtime and therefore only inferred.
    let stat_by_path: BTreeMap<&Path, &FileStat> =
        head_stats.iter().map(|s| (s.path.as_path(), s)).collect();
    for entry in &entries {
        let kind = entry.effective_kind();
        let event_type = match kind {
            ChangeKind::Added | ChangeKind::Copied => TrailEventType::FileAdded,
            ChangeKind::Deleted => TrailEventType::FileDeleted,
            ChangeKind::Renamed => TrailEventType::FileRenamed {
                from: entry.orig_path.clone().unwrap_or_default(),
            },
            ChangeKind::Modified | ChangeKind::TypeChanged | ChangeKind::Conflicted => {
                TrailEventType::FileModified
            }
        };
        let (timestamp, source) = if kind == ChangeKind::Deleted {
            (None, EventSource::WorkingTree)
        } else {
            (
                mtime(&repo.workdir().join(&entry.path)),
                EventSource::Filesystem,
            )
        };
        let stats = stat_by_path.get(entry.path.as_path()).map(|s| LineStats {
            additions: s.additions.unwrap_or(0),
            deletions: s.deletions.unwrap_or(0),
        });
        let staged = if entry.untracked {
            None
        } else {
            Some(entry.staged.is_some() && entry.unstaged.is_none())
        };
        events.push(TrailEvent {
            timestamp,
            event_type,
            files: vec![entry.path.clone()],
            source,
            confidence: Confidence::Inferred,
            stats,
            staged,
        });
    }

    // 3. Reflog: HEAD movements since the branch point (detailed view only).
    if scope == Scope::Detailed {
        let since = commits
            .first()
            .map(|c| c.time)
            .or_else(|| events.iter().filter_map(|e| e.timestamp).min())
            .unwrap_or_else(Utc::now);
        for entry in history::head_reflog(repo.gix(), since)? {
            // Plain commits are either already shown as Commit events or belong
            // to another branch; only HEAD movements that are not commits are
            // interesting here (checkout, rebase, reset, amend, merge, ...).
            if entry.action == "commit" || entry.action == "commit (initial)" {
                continue;
            }
            events.push(TrailEvent {
                timestamp: Some(entry.time),
                event_type: TrailEventType::RefUpdate {
                    action: entry.action,
                    message: entry.message,
                },
                files: Vec::new(),
                source: EventSource::GitReflog,
                confidence: Confidence::Exact,
                stats: None,
                staged: None,
            });
        }
    }

    // Chronological order; events without a time sink to the end.
    events.sort_by(|a, b| match (a.timestamp, b.timestamp) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let summary = TrailSummary {
        changes: events.iter().filter(|e| e.is_change()).count(),
        files_changed: base_stats.len(),
        additions: total(&base_stats).additions,
        deletions: total(&base_stats).deletions,
    };

    Ok(Trail {
        repository: context(repo, base),
        events,
        summary,
    })
}

pub fn build_status(repo: &Repo, base: &BaseRef) -> Result<StatusReport> {
    let entries = diff::status(repo.workdir())?;
    let stats = total(&stats_since_head(repo, &entries)?);
    let commits_ahead = history::commits_between(repo.gix(), repo.head_id, base.merge_base)?.len();
    Ok(StatusReport {
        repository: context(repo, base),
        counts: kind_counts(&entries),
        stats,
        commits_ahead,
        entries,
    })
}

pub fn build_diff(repo: &Repo, base: &BaseRef) -> Result<DiffReport> {
    let entries = diff::status(repo.workdir())?;
    let stats = stats_since_base(repo, base, &entries)?;

    let mut groups: BTreeMap<String, Vec<FileStat>> = BTreeMap::new();
    for stat in &stats {
        let dir = stat
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".into());
        groups.entry(dir).or_default().push(stat.clone());
    }
    let groups: Vec<DiffGroup> = groups
        .into_iter()
        .map(|(directory, mut files)| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            let stats = total(&files);
            DiffGroup {
                directory,
                files,
                stats,
            }
        })
        .collect();

    Ok(DiffReport {
        repository: context(repo, base),
        files_changed: stats.len(),
        stats: total(&stats),
        groups,
    })
}

pub fn build_inspect(
    repo: &Repo,
    base: &BaseRef,
    cwd: &Path,
    file: &Path,
) -> Result<InspectReport> {
    let rel = repo.relative_path(cwd, file)?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let rel = PathBuf::from(rel_str);
    let abs = repo.workdir().join(&rel);

    let entries = diff::status(repo.workdir())?;
    let status = entries.iter().find(|e| e.path == rel).cloned();
    let exists = abs.exists();

    let commits = history::file_commits(repo.workdir(), &rel, 20)?;
    let tracked = !commits.is_empty() || status.as_ref().map(|s| !s.untracked).unwrap_or(false);
    if !tracked && !exists {
        return Err(crate::error::TrailError::FileNotFound(rel));
    }

    let pick = |stats: Vec<FileStat>| stats.into_iter().find(|s| s.path == rel);
    let untracked = status.as_ref().map(|s| s.untracked).unwrap_or(false);
    let (worktree_stat, base_stat) = if untracked {
        let only = entries
            .iter()
            .filter(|e| e.path == rel)
            .cloned()
            .collect::<Vec<_>>();
        let stat = diff::untracked_stats(repo.workdir(), &only)
            .into_iter()
            .next();
        (stat.clone(), stat)
    } else {
        let merge_base = base.merge_base.to_string();
        (
            pick(diff::numstat(
                repo.workdir(),
                DiffTarget::Revision("HEAD"),
                &[&rel],
            )?),
            pick(diff::numstat(
                repo.workdir(),
                DiffTarget::Revision(&merge_base),
                &[&rel],
            )?),
        )
    };

    let on_branch: HashSet<String> =
        history::commits_between(repo.gix(), repo.head_id, base.merge_base)?
            .into_iter()
            .map(|c| c.id)
            .collect();
    let commits = commits
        .into_iter()
        .map(|c| InspectCommit {
            on_branch: on_branch.contains(&c.id),
            commit: c,
        })
        .collect();

    Ok(InspectReport {
        repository: context(repo, base),
        last_modified: if exists { mtime(&abs) } else { None },
        path: rel,
        tracked,
        exists,
        status,
        worktree_stat,
        base_stat,
        commits,
    })
}
