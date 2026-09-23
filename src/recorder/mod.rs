//! `trail start`: record what changes in a worktree while a developer or an
//! agent works in it.
//!
//! Filesystem notifications are only a hint. Every hint is verified by hashing
//! the file's current content and comparing it with the last content we know
//! about, so editor saves without changes, mtime-only touches and duplicate
//! notifications never become events. The baseline for a path we have not
//! seen yet is the blob recorded in the git index. Renames are recognised by
//! content, not by the watcher: a path that vanished and a path that appeared
//! with the same content in one batch is one rename.

pub mod checkpoint;
pub mod metadata;
pub mod snapshot;
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
use store::{RawEvent, RawEventKind, SessionHeader, SessionWriter};

/// Decides whether observed paths became real changes.
///
/// Pure state machine: it never touches the filesystem itself, callers hand it
/// the current content hash of each path (`None` when the file is gone).
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

    /// Replace the index baseline (after a commit) without forgetting what
    /// was observed since.
    pub fn reset_baseline(&mut self, baseline: HashMap<PathBuf, String>) {
        self.baseline = baseline;
    }

    /// Feed the current hash of one path; returns an event when the content
    /// differs from what was known before.
    pub fn observe(
        &mut self,
        path: &Path,
        current: Option<String>,
        ts: DateTime<Utc>,
    ) -> Option<RawEvent> {
        let before = match self.known.get(path) {
            Some(known) => known.clone(),
            None => self.baseline.get(path).cloned(),
        };
        if before == current {
            self.known.insert(path.to_path_buf(), current);
            return None;
        }
        let kind = match (&before, &current) {
            (None, Some(_)) => RawEventKind::Created,
            (Some(_), None) => RawEventKind::Deleted,
            (None, None) => {
                self.known.insert(path.to_path_buf(), None);
                return None;
            }
            (Some(_), Some(_)) => RawEventKind::Modified,
        };
        self.known.insert(path.to_path_buf(), current.clone());
        Some(RawEvent {
            timestamp: ts,
            path: path.to_path_buf(),
            kind,
            from_path: None,
            before_hash: before,
            after_hash: current,
        })
    }

    /// Feed a whole batch of paths observed together. Deletions and creations
    /// with identical content are folded into one `Renamed` event.
    pub fn observe_batch(
        &mut self,
        batch: Vec<(PathBuf, Option<String>)>,
        ts: DateTime<Utc>,
    ) -> Vec<RawEvent> {
        let events: Vec<RawEvent> = batch
            .into_iter()
            .filter_map(|(path, hash)| self.observe(&path, hash, ts))
            .collect();
        pair_renames(events)
    }
}

/// Fold `Deleted(a, hash h)` + `Created(b, hash h)` into `Renamed(a -> b)`.
/// Only unambiguous 1:1 matches are paired; anything else stays as it is.
fn pair_renames(events: Vec<RawEvent>) -> Vec<RawEvent> {
    let mut deleted_by_hash: HashMap<String, Vec<usize>> = HashMap::new();
    let mut created_by_hash: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        match (e.kind, &e.before_hash, &e.after_hash) {
            (RawEventKind::Deleted, Some(h), _) => {
                deleted_by_hash.entry(h.clone()).or_default().push(i)
            }
            (RawEventKind::Created, _, Some(h)) => {
                created_by_hash.entry(h.clone()).or_default().push(i)
            }
            _ => {}
        }
    }
    // created index -> deleted index
    let mut pairs: HashMap<usize, usize> = HashMap::new();
    for (hash, deleted) in &deleted_by_hash {
        if let Some(created) = created_by_hash.get(hash) {
            if deleted.len() == 1 && created.len() == 1 {
                pairs.insert(created[0], deleted[0]);
            }
        }
    }
    let consumed: HashSet<usize> = pairs.values().copied().collect();
    let mut out = Vec::with_capacity(events.len());
    for (i, mut e) in events.iter().cloned().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        if let Some(deleted_idx) = pairs.get(&i) {
            e.kind = RawEventKind::Renamed;
            e.from_path = Some(events[*deleted_idx].path.clone());
            e.before_hash = events[*deleted_idx].before_hash.clone();
        }
        out.push(e);
    }
    out
}

/// Content of a regular file or symlink (its target), `None` for anything
/// else (missing, directory, socket, ...).
pub fn read_content(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        Some(
            std::fs::read_link(path)
                .ok()?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        )
    } else if meta.is_file() {
        std::fs::read(path).ok()
    } else {
        None
    }
}

/// Git blob id of `data`.
///
/// SHA-1 over the blob is not chosen for security but because it is the id
/// `git hash-object` produces, so the same id addresses the snapshot in the
/// object database.
pub fn blob_id(data: &[u8], hash_kind: gix::hash::Kind) -> Option<String> {
    gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, data)
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
    /// `--base` as given on the command line, used for the header only.
    pub base: Option<String>,
}

/// How long to wait for more notifications before hashing a batch. Editors
/// and agents write several files in quick succession; one batch means one
/// hash per file instead of one per notification, and lets renames pair up.
const BATCH_WINDOW: Duration = Duration::from_millis(250);

pub fn build_header(repo: &Repo, base: Option<&str>, started_at: DateTime<Utc>) -> SessionHeader {
    let base_commit = repo
        .resolve_base(base)
        .ok()
        .map(|b| b.merge_base.to_string());
    SessionHeader {
        version: store::FORMAT_VERSION,
        session_id: store::new_session_id(started_at),
        repository_root: repo
            .worktree
            .common_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.worktree.common_dir.clone()),
        worktree_id: store::worktree_id(repo),
        worktree_path: repo.workdir().to_path_buf(),
        branch: repo.head.branch_name().map(str::to_string),
        base_commit,
        start_head: Some(repo.head_id.to_string()),
        started_at,
    }
}

pub fn run(repo: &Repo, opts: Options) -> Result<()> {
    let root = repo.workdir().to_path_buf();
    let git_dir = repo.worktree.git_dir.clone();
    let dot_git = root.join(".git");
    let hash_kind = repo.gix().object_hash();

    let started_at = Utc::now();
    let header = build_header(repo, opts.base.as_deref(), started_at);
    let mut writer = SessionWriter::create(&store::session_dir(repo), &header)?;
    let mut tracker = Tracker::new(index_baseline(repo)?);
    let mut snapshots = snapshot::SnapshotStore::new(repo.gix(), &header.session_id);

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
    // HEAD and the index live in the worktree's git dir, branch refs in the
    // common dir. Notifications from there only trigger a HEAD re-check.
    for dir in [git_dir.clone(), repo.worktree.common_dir.join("refs")] {
        if dir.is_dir() {
            let _ = watcher.watch(&dir, RecursiveMode::Recursive);
        }
    }
    let mut last_head = repo.head_id.to_string();

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .map_err(|e| TrailError::Git(format!("cannot install Ctrl+C handler: {e}")))?;
    }

    if !opts.quiet {
        print_banner(repo, &header.session_id, writer.path());
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
        let pending = match next_batch(&rx) {
            Batch::Paths(paths) => paths,
            Batch::Idle => continue,
            Batch::Closed => break,
        };

        let ts = Utc::now();
        // Commit boundary: the watcher only says "something in .git moved";
        // gix decides whether HEAD actually changed.
        if let Ok(head) = current_head(repo) {
            if head != last_head {
                writer.record_commit(ts, &last_head, &head)?;
                if !opts.quiet {
                    println!(
                        "{}  HEAD moved to {}",
                        ts.with_timezone(&chrono::Local).format("%H:%M:%S"),
                        &head[..head.len().min(7)]
                    );
                }
                last_head = head;
                // Everything committed is now the baseline for later edits.
                if let Ok(baseline) = index_baseline(repo) {
                    tracker.reset_baseline(baseline);
                }
                snapshots.protect()?;
            }
        }
        let mut paths: Vec<PathBuf> = pending.into_iter().collect();
        paths.sort();
        let mut batch: Vec<(PathBuf, Option<String>)> = Vec::new();
        let mut contents: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        for abs in paths {
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
            let content = read_content(&abs);
            let hash = content.as_deref().and_then(|d| blob_id(d, hash_kind));
            if let Some(data) = content {
                contents.insert(rel.to_path_buf(), data);
            }
            batch.push((rel.to_path_buf(), hash));
        }
        for event in tracker.observe_batch(batch, ts) {
            writer.record(&event)?;
            // Snapshot the new content so `trail open --at` can show it later.
            if event.after_hash.is_some() {
                if let Some(data) = contents.get(&event.path) {
                    snapshots.store(data)?;
                }
            }
            if !opts.quiet {
                println!("{}", describe(&event));
            }
        }
        if snapshots.needs_protection() {
            snapshots.protect()?;
        }
    }

    let events = writer.events();
    let path = writer.path().to_path_buf();
    let protected = snapshots.protect()?;
    writer.finish(Utc::now())?;
    if !opts.quiet {
        println!();
        println!(
            "Recorded {} change{} to {}",
            events,
            crate::display::plural(events),
            path.display()
        );
        if protected.is_some() {
            println!(
                "Snapshots: {} blob{} kept under {}",
                snapshots.blob_count(),
                crate::display::plural(snapshots.blob_count()),
                snapshot::ref_name(&header.session_id)
            );
        }
    }
    Ok(())
}

fn print_banner(repo: &Repo, session_id: &str, log_path: &Path) {
    println!();
    println!("Recording development trail...");
    println!();
    println!("Repository");
    println!("  {}", repo.name);
    println!();
    println!("Worktree");
    println!("  {}", repo.head.label());
    if repo.worktree.id.is_some() {
        println!("  {}", repo.workdir().display());
    }
    println!();
    println!("Session");
    println!("  {session_id}");
    println!("  {}", log_path.display());
    println!();
    println!("Press Ctrl+C to stop.");
    println!();
}

enum Batch {
    /// Paths notified within one batch window.
    Paths(HashSet<PathBuf>),
    /// Nothing arrived; the caller re-checks its stop conditions.
    Idle,
    /// The watcher went away.
    Closed,
}

/// Wait for the first notification, then drain everything that arrives
/// within `BATCH_WINDOW`.
fn next_batch(rx: &mpsc::Receiver<notify::Result<notify::Event>>) -> Batch {
    let mut pending: HashSet<PathBuf> = HashSet::new();
    match rx.recv_timeout(Duration::from_millis(200)) {
        Ok(event) => collect_paths(event, &mut pending),
        Err(mpsc::RecvTimeoutError::Timeout) => return Batch::Idle,
        Err(mpsc::RecvTimeoutError::Disconnected) => return Batch::Closed,
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
    Batch::Paths(pending)
}

fn current_head(repo: &Repo) -> Result<String> {
    repo.gix()
        .head_id()
        .map(|id| id.to_string())
        .map_err(|e| TrailError::Git(e.to_string()))
}

fn describe(event: &RawEvent) -> String {
    let time = event
        .timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S");
    match event.kind {
        RawEventKind::Created => format!("{time}  created {}", event.path.display()),
        RawEventKind::Modified => format!("{time}  modified {}", event.path.display()),
        RawEventKind::Deleted => format!("{time}  deleted {}", event.path.display()),
        RawEventKind::Renamed => format!(
            "{time}  renamed {} -> {}",
            event
                .from_path
                .as_deref()
                .unwrap_or(Path::new("?"))
                .display(),
            event.path.display()
        ),
    }
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
        assert_eq!(e.kind, RawEventKind::Modified);
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
    fn create_then_delete() {
        let mut t = tracker();
        let e = t
            .observe(Path::new("new.txt"), Some("x".into()), Utc::now())
            .unwrap();
        assert_eq!(e.kind, RawEventKind::Created);
        assert!(e.before_hash.is_none());
        let e = t.observe(Path::new("new.txt"), None, Utc::now()).unwrap();
        assert_eq!(e.kind, RawEventKind::Deleted);
        assert!(e.after_hash.is_none());
        // Notification for a path that never existed (e.g. temp file already gone).
        assert!(t
            .observe(Path::new("ghost.tmp"), None, Utc::now())
            .is_none());
    }

    #[test]
    fn rename_is_detected_by_content() {
        let mut t = tracker();
        let events = t.observe_batch(
            vec![
                (PathBuf::from("src/a.rs"), None),
                (PathBuf::from("src/b.rs"), Some("h1".into())),
                (PathBuf::from("other.txt"), Some("zz".into())),
            ],
            Utc::now(),
        );
        assert_eq!(events.len(), 2);
        let renamed = events
            .iter()
            .find(|e| e.kind == RawEventKind::Renamed)
            .unwrap();
        assert_eq!(renamed.path, PathBuf::from("src/b.rs"));
        assert_eq!(renamed.from_path.as_deref(), Some(Path::new("src/a.rs")));
        assert_eq!(renamed.before_hash.as_deref(), Some("h1"));
        assert_eq!(renamed.after_hash.as_deref(), Some("h1"));
        assert!(events
            .iter()
            .any(|e| e.kind == RawEventKind::Created && e.path == Path::new("other.txt")));
    }

    #[test]
    fn ambiguous_renames_stay_separate() {
        let mut baseline = HashMap::new();
        baseline.insert(PathBuf::from("a"), "same".to_string());
        baseline.insert(PathBuf::from("b"), "same".to_string());
        let mut t = Tracker::new(baseline);
        let events = t.observe_batch(
            vec![
                (PathBuf::from("a"), None),
                (PathBuf::from("b"), None),
                (PathBuf::from("c"), Some("same".into())),
            ],
            Utc::now(),
        );
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.kind != RawEventKind::Renamed));
    }
}
