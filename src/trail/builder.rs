//! Turns raw git facts into `Trail`s and reports.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::git::diff::{self, ChangeKind, DiffTarget, FileStat, LineStats, StatusEntry};
use crate::git::history;
use crate::git::repository::{BaseRef, Repo};
use crate::recorder::checkpoint::{build_checkpoints, Checkpoint};
use crate::recorder::metadata::Metadata;
use crate::recorder::store::{self, SessionFile};
use crate::trail::event::{Confidence, EventSource, TrailEvent, TrailEventType};
use crate::trail::{
    kind_counts, DiffGroup, DiffReport, InspectCommit, InspectReport, RepositoryContext,
    SessionSummary, SessionsReport, StatusReport, Trail, TrailSummary,
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

    // 4. Recorded sessions: exact observations from `trail start`. Where a
    //    checkpoint covers a file, the mtime-based guess for that file is
    //    dropped so nothing is shown twice.
    let since = merge_base_time(repo, base);
    let (checkpoints, metadata) = recorded_checkpoints(repo, since)?;
    let mut covered: HashMap<&Path, DateTime<Utc>> = HashMap::new();
    for (cp, covered_until) in &checkpoints {
        for change in &cp.changes {
            let until = covered
                .entry(change.path.as_path())
                .or_insert(*covered_until);
            *until = (*until).max(*covered_until);
        }
    }
    events.retain(|e| {
        if e.confidence != Confidence::Inferred {
            return true;
        }
        let Some(path) = e.files.first() else {
            return true;
        };
        match (covered.get(path.as_path()), e.timestamp) {
            (Some(until), Some(ts)) => ts > *until + chrono::Duration::seconds(2),
            (Some(_), None) => false,
            (None, _) => true,
        }
    });
    for (cp, _) in checkpoints {
        events.push(TrailEvent {
            timestamp: Some(cp.started_at),
            files: cp.changes.iter().map(|c| c.path.clone()).collect(),
            event_type: TrailEventType::Checkpoint {
                id: cp.id,
                session_id: cp.session_id,
                title: cp.title,
                annotation: cp.annotation,
                bulk: cp.bulk,
                hidden: cp.hidden,
                changes: cp.changes,
            },
            source: EventSource::TrailRecorder,
            confidence: Confidence::Exact,
            stats: None,
            staged: None,
        });
    }

    // Chronological order; events without a time sink to the end.
    events.sort_by(|a, b| match (a.timestamp, b.timestamp) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    apply_order(&mut events, &metadata.order);

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

fn merge_base_time(repo: &Repo, base: &BaseRef) -> Option<DateTime<Utc>> {
    let commit = repo.gix().find_commit(base.merge_base).ok()?;
    let time = commit.time().ok()?;
    chrono::TimeZone::timestamp_opt(&Utc, time.seconds, 0).single()
}

/// Sessions that belong to this worktree, or that were recorded on this
/// branch *and* whose starting commit is part of the current history. The
/// second condition keeps a branch name that was deleted and later reused
/// from dragging in an unrelated trail.
fn sessions_for(repo: &Repo) -> Result<Vec<SessionFile>> {
    let worktree_id = store::worktree_id(repo);
    let branch = match &repo.head {
        crate::git::repository::HeadState::Branch { name } => Some(name.clone()),
        crate::git::repository::HeadState::Detached { .. } => None,
    };
    Ok(store::list_sessions(&repo.worktree.common_dir)?
        .into_iter()
        .filter(|s| {
            s.header.worktree_id == worktree_id
                || (branch.is_some()
                    && s.header.branch == branch
                    && s.header
                        .start_head
                        .as_deref()
                        .is_some_and(|h| is_ancestor_of_head(repo, h)))
        })
        .collect())
}

/// True when `oid` is reachable from HEAD (or is HEAD). Unknown or garbage
/// collected objects are not ancestors.
fn is_ancestor_of_head(repo: &Repo, oid: &str) -> bool {
    let Ok(id) = gix::ObjectId::from_hex(oid.as_bytes()) else {
        return false;
    };
    if id == repo.head_id {
        return true;
    }
    match repo.gix().merge_base(repo.head_id, id) {
        Ok(base) => base.detach() == id,
        Err(_) => false,
    }
}

/// A checkpoint plus the time until which its session kept watching.
type CoveredCheckpoint = (Checkpoint, DateTime<Utc>);

/// Checkpoints of the relevant sessions since the branch point, with the
/// metadata overlay applied, paired with the time until which their session
/// is known to have been watching.
fn recorded_checkpoints(
    repo: &Repo,
    since: Option<DateTime<Utc>>,
) -> Result<(Vec<CoveredCheckpoint>, Metadata)> {
    let mut raw = Vec::new();
    let mut covered_by_session: HashMap<String, DateTime<Utc>> = HashMap::new();
    for session in sessions_for(repo)? {
        // An open session (no end record) is assumed to still be watching.
        covered_by_session.insert(
            session.header.session_id.clone(),
            session.ended_at.unwrap_or_else(Utc::now),
        );
        raw.extend(build_checkpoints(
            &session.header.session_id,
            &session.records,
        ));
    }
    let metadata = Metadata::load(&repo.worktree.common_dir)?;
    let out = metadata
        .apply(raw)
        .into_iter()
        .filter(|cp| !since.is_some_and(|s| cp.ended_at < s))
        .map(|cp| {
            let until = covered_by_session
                .get(&cp.session_id)
                .copied()
                .unwrap_or_else(Utc::now);
            (cp, until)
        })
        .collect();
    Ok((out, metadata))
}

/// Apply the human reading order: checkpoints listed in `order` are permuted
/// among the timeline slots they occupy; everything else stays put.
fn apply_order(events: &mut [TrailEvent], order: &[String]) {
    if order.is_empty() {
        return;
    }
    let slots: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| match &e.event_type {
            TrailEventType::Checkpoint { id, .. } => order.contains(id),
            _ => false,
        })
        .map(|(i, _)| i)
        .collect();
    if slots.len() < 2 {
        return;
    }
    let mut taken: Vec<TrailEvent> = slots.iter().map(|i| events[*i].clone()).collect();
    taken.sort_by_key(|e| match &e.event_type {
        TrailEventType::Checkpoint { id, .. } => {
            order.iter().position(|o| o == id).unwrap_or(usize::MAX)
        }
        _ => usize::MAX,
    });
    for (slot, event) in slots.into_iter().zip(taken) {
        events[slot] = event;
    }
}

pub fn build_sessions(repo: &Repo) -> Result<SessionsReport> {
    let sessions = store::list_sessions(&repo.worktree.common_dir)?
        .into_iter()
        .map(|s| SessionSummary {
            checkpoints: build_checkpoints(&s.header.session_id, &s.records).len(),
            last_activity: s.last_activity(),
            worktree_exists: s.header.worktree_path.is_dir(),
            session_id: s.header.session_id,
            worktree_id: s.header.worktree_id,
            worktree_path: s.header.worktree_path,
            branch: s.header.branch,
            started_at: s.header.started_at,
            ended_at: s.ended_at,
            events: s.events,
            path: s.path,
        })
        .collect();
    Ok(SessionsReport {
        repository: repo.name.clone(),
        sessions,
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
