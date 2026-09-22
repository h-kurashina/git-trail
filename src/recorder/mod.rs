//! `trail start`: record what changes in a worktree while a developer or an
//! agent works in it.
//!
//! Filesystem notifications are only a hint. Every hint is verified by hashing
//! the file's current content and comparing it with the last content we know
//! about, so editor saves without changes, mtime-only touches and duplicate
//! notifications never become events. The baseline for a path we have not
//! seen yet is the blob recorded in the git index.

pub mod store;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gix::bstr::ByteSlice;
use notify::{RecursiveMode, Watcher};

use crate::error::{Result, TrailError};
use crate::git::repository::Repo;
use store::{RecordedEvent, RecordedKind, SessionHeader, SessionWriter};

/// Decides whether an observed path became a real change.
///
/// Pure state machine: it never touches the filesystem itself, callers hand it
/// the current content hash (`None` when the file is gone).
pub struct Tracker {
    /// Last content hash we recorded (or learned from the index) per path.
    known: HashMap<PathBuf, Option<String>>,
    /// Blob ids from the git index, consulted the first time a path shows up.
    baseline: HashMap<PathBuf, String>,
}

impl Tracker {
    pub fn new(baseline: HashMap<PathBuf, String>) -> Self {
        Tracker {
            known: HashMap::new(),
            baseline,
        }
    }

    /// Feed the current hash of `path`; returns an event when the content
    /// differs from what was known before.
    pub fn observe(
        &mut self,
        path: &Path,
        current: Option<String>,
        ts: DateTime<Utc>,
    ) -> Option<RecordedEvent> {
        let before = match self.known.get(path) {
            Some(known) => known.clone(),
            None => self.baseline.get(path).cloned(),
        };
        if before == current {
            self.known.insert(path.to_path_buf(), current);
            return None;
        }
        let kind = match (&before, &current) {
            (None, Some(_)) => RecordedKind::Added,
            (Some(_), None) => RecordedKind::Deleted,
            (None, None) => {
                self.known.insert(path.to_path_buf(), None);
                return None;
            }
            (Some(_), Some(_)) => RecordedKind::Modified,
        };
        self.known.insert(path.to_path_buf(), current.clone());
        Some(RecordedEvent {
            ts,
            kind,
            path: path.to_path_buf(),
            before_hash: before,
            after_hash: current,
        })
    }
}

/// Git blob id of the file's content, `None` if it does not exist or is not a
/// regular file (directories and sockets are not tracked).
pub fn blob_hash(path: &Path, hash_kind: gix::hash::Kind) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let data = if meta.file_type().is_symlink() {
        std::fs::read_link(path)
            .ok()?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec()
    } else if meta.is_file() {
        std::fs::read(path).ok()?
    } else {
        return None;
    };
    gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &data)
        .ok()
        .map(|id| id.to_string())
}

/// Blob ids of every index entry, keyed by repository-relative path.
fn index_baseline(repo: &Repo) -> Result<HashMap<PathBuf, String>> {
    let index = repo
        .gix()
        .index_or_empty()
        .map_err(|e| TrailError::Git(e.to_string()))?;
    Ok(index
        .entries()
        .iter()
        .map(|entry| {
            let path = entry.path(&index).to_str_lossy().into_owned();
            (PathBuf::from(path), entry.id.to_string())
        })
        .collect())
}

pub struct Options {
    pub stop_after: Option<Duration>,
    pub quiet: bool,
}

/// How long to wait for more notifications before hashing a batch. Editors
/// and agents write several files in quick succession; one batch means one
/// hash per file instead of one per notification.
const BATCH_WINDOW: Duration = Duration::from_millis(250);

pub fn run(repo: &Repo, opts: Options) -> Result<()> {
    let root = repo.workdir().to_path_buf();
    let git_dir = repo.worktree.git_dir.clone();
    let dot_git = root.join(".git");
    let hash_kind = repo.gix().object_hash();

    let started_at = Utc::now();
    let header = SessionHeader {
        version: store::FORMAT_VERSION,
        session_id: store::new_session_id(started_at),
        worktree_id: repo.worktree.id.clone().unwrap_or_else(|| "main".into()),
        worktree_path: root.clone(),
        branch: repo.head.label(),
        base_commit: repo.head_id.to_string(),
        started_at,
    };
    let mut writer = SessionWriter::create(&store::session_dir(repo), &header)?;
    let mut tracker = Tracker::new(index_baseline(repo)?);

    let worktree = repo
        .gix()
        .worktree()
        .ok_or_else(|| TrailError::WorktreeUnavailable("no worktree".into()))?;
    let mut excludes = worktree
        .excludes(None)
        .map_err(|e| TrailError::Git(format!("cannot load ignore rules: {e}")))?;

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| TrailError::Git(format!("cannot start file watcher: {e}")))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| TrailError::Git(format!("cannot watch {}: {e}", root.display())))?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .map_err(|e| TrailError::Git(format!("cannot install Ctrl+C handler: {e}")))?;
    }

    if !opts.quiet {
        println!();
        println!("Recording development trail...");
        println!("Worktree: {}", header.branch);
        println!("Session: {}", header.session_id);
        println!("Log: {}", writer.path().display());
        println!();
        println!("Press Ctrl+C to stop.");
        println!();
    }

    let deadline = opts.stop_after.map(|d| Instant::now() + d);
    let is_ignored = |excludes: &mut gix::AttributeStack<'_>, rel: &Path, abs: &Path| -> bool {
        // Never record git's own files or our own session log.
        if abs.starts_with(&dot_git)
            || abs.starts_with(&git_dir)
            || abs.starts_with(&repo.worktree.common_dir)
        {
            return true;
        }
        let mode = match std::fs::symlink_metadata(abs) {
            Ok(m) if m.is_dir() => Some(gix::index::entry::Mode::DIR),
            _ => Some(gix::index::entry::Mode::FILE),
        };
        excludes
            .at_path(rel, mode)
            .map(|platform| platform.is_excluded())
            .unwrap_or(false)
    };

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }

        // Wait for the first notification, then drain everything that arrives
        // within the batch window.
        let mut pending: HashSet<PathBuf> = HashSet::new();
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => collect_paths(event, &mut pending),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let batch_end = Instant::now() + BATCH_WINDOW;
        while let Some(left) = batch_end
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        {
            match rx.recv_timeout(left) {
                Ok(event) => collect_paths(event, &mut pending),
                Err(_) => break,
            }
        }

        let ts = Utc::now();
        let mut batch: Vec<PathBuf> = pending.into_iter().collect();
        batch.sort();
        for abs in batch {
            let Ok(rel) = abs.strip_prefix(&root) else {
                continue;
            };
            if rel.as_os_str().is_empty() || is_ignored(&mut excludes, rel, &abs) {
                continue;
            }
            if std::fs::symlink_metadata(&abs)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let current = blob_hash(&abs, hash_kind);
            if let Some(event) = tracker.observe(rel, current, ts) {
                writer.record(&event)?;
                if !opts.quiet {
                    let label = match event.kind {
                        RecordedKind::Added => "added",
                        RecordedKind::Modified => "modified",
                        RecordedKind::Deleted => "deleted",
                    };
                    println!(
                        "{}  {} {}",
                        event.ts.with_timezone(&chrono::Local).format("%H:%M:%S"),
                        label,
                        event.path.display()
                    );
                }
            }
        }
    }

    let events = writer.events();
    let path = writer.path().to_path_buf();
    writer.finish(Utc::now())?;
    if !opts.quiet {
        println!();
        println!(
            "Recorded {} change{} to {}",
            events,
            if events == 1 { "" } else { "s" },
            path.display()
        );
    }
    Ok(())
}

fn collect_paths(event: notify::Result<notify::Event>, into: &mut HashSet<PathBuf>) {
    let Ok(event) = event else { return };
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return;
    }
    into.extend(event.paths);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> Tracker {
        let mut baseline = HashMap::new();
        baseline.insert(PathBuf::from("src/a.rs"), "h1".to_string());
        Tracker::new(baseline)
    }

    #[test]
    fn unchanged_content_is_not_an_event() {
        let mut t = tracker();
        assert!(t
            .observe(Path::new("src/a.rs"), Some("h1".into()), Utc::now())
            .is_none());
    }

    #[test]
    fn modification_uses_index_as_before() {
        let mut t = tracker();
        let e = t
            .observe(Path::new("src/a.rs"), Some("h2".into()), Utc::now())
            .unwrap();
        assert_eq!(e.kind, RecordedKind::Modified);
        assert_eq!(e.before_hash.as_deref(), Some("h1"));
        assert_eq!(e.after_hash.as_deref(), Some("h2"));
        // Same content again: deduplicated.
        assert!(t
            .observe(Path::new("src/a.rs"), Some("h2".into()), Utc::now())
            .is_none());
        // Back to the index content: still a modification, not silence.
        let e = t
            .observe(Path::new("src/a.rs"), Some("h1".into()), Utc::now())
            .unwrap();
        assert_eq!(e.before_hash.as_deref(), Some("h2"));
    }

    #[test]
    fn add_then_delete() {
        let mut t = tracker();
        let e = t
            .observe(Path::new("new.txt"), Some("x".into()), Utc::now())
            .unwrap();
        assert_eq!(e.kind, RecordedKind::Added);
        assert!(e.before_hash.is_none());
        let e = t.observe(Path::new("new.txt"), None, Utc::now()).unwrap();
        assert_eq!(e.kind, RecordedKind::Deleted);
        assert!(e.after_hash.is_none());
        // Notification for a path that never existed (e.g. temp file already gone).
        assert!(t
            .observe(Path::new("ghost.tmp"), None, Utc::now())
            .is_none());
    }
}
