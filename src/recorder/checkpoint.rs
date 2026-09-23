//! Raw events → checkpoints.
//!
//! A checkpoint is what a human should see: "between 13:02 and 13:08 these
//! files changed". It is derived, never stored; the raw log stays the only
//! source of truth and this module can be re-run with different constants.

use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::store::{RawEvent, RawEventKind, Record};
use crate::trail::event::{Confidence, EventSource};

/// Consecutive events on the same path closer than this are one edit.
pub const DEBOUNCE: Duration = Duration::milliseconds(500);
/// A gap longer than this between events starts a new checkpoint.
pub const WINDOW: Duration = Duration::seconds(30);
/// Checkpoints touching more files than this are flagged `bulk` so the
/// display can collapse them (formatters, generated code, ...).
pub const BULK_THRESHOLD: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    /// One-character marker used in file lists: `+ ~ - >`.
    pub fn mark(self) -> &'static str {
        match self {
            ChangeKind::Created => "+",
            ChangeKind::Modified => "~",
            ChangeKind::Deleted => "-",
            ChangeKind::Renamed => ">",
        }
    }
}

impl From<RawEventKind> for ChangeKind {
    fn from(kind: RawEventKind) -> Self {
        match kind {
            RawEventKind::Created => ChangeKind::Created,
            RawEventKind::Modified => ChangeKind::Modified,
            RawEventKind::Deleted => ChangeKind::Deleted,
            RawEventKind::Renamed => ChangeKind::Renamed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    /// Id of the checkpoint the recorder put this change in. Stays the same
    /// when a human moves the change to another checkpoint, so overlay edits
    /// can always be traced back to the raw log.
    pub origin: String,
    pub path: PathBuf,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<PathBuf>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Number of debounced edits folded into this change.
    pub edits: u32,
}

/// The HEAD movement the recorder observed right after a checkpoint. Only a
/// movement whose `to_head` has `from_head` as first parent is a commit; a
/// checkout, reset or rebase looks the same here and is told apart later,
/// with the repository at hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitBoundary {
    pub from_head: String,
    pub to_head: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub changes: Vec<FileChange>,
    pub bulk: bool,
    /// The HEAD movement that closed this checkpoint's run of work, when the
    /// recorder saw one before the session ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<CommitBoundary>,
    pub source: EventSource,
    pub confidence: Confidence,
    /// Human supplied, via the metadata overlay (`trail edit`).
    pub title: Option<String>,
    pub hidden: bool,
    pub annotation: Option<String>,
}

impl Checkpoint {
    fn new(session_id: &str, index: usize, first: &FileChange) -> Self {
        Checkpoint {
            id: format!("{session_id}.{index}"),
            session_id: session_id.to_string(),
            started_at: first.first_seen,
            ended_at: first.last_seen,
            changes: Vec::new(),
            bulk: false,
            boundary: None,
            source: EventSource::TrailRecorder,
            confidence: Confidence::Exact,
            title: None,
            hidden: false,
            annotation: None,
        }
    }

    fn absorb(&mut self, mut change: FileChange) {
        change.origin = self.id.clone();
        self.ended_at = self.ended_at.max(change.last_seen);
        match self.changes.iter_mut().find(|c| c.path == change.path) {
            Some(existing) => fold(existing, &change),
            None => self.changes.push(change),
        }
    }

    fn finish(mut self) -> Option<Self> {
        // A file created and deleted inside one checkpoint leaves no trace.
        self.changes
            .retain(|c| !(c.before_hash.is_none() && c.after_hash.is_none()));
        if self.changes.is_empty() {
            return None;
        }
        self.bulk = self.changes.len() > BULK_THRESHOLD;
        Some(self)
    }
}

fn change_from(event: &RawEvent) -> FileChange {
    FileChange {
        origin: String::new(), // filled in when the change joins a checkpoint
        path: event.path.clone(),
        kind: event.kind.into(),
        from_path: event.from_path.clone(),
        before_hash: event.before_hash.clone(),
        after_hash: event.after_hash.clone(),
        first_seen: event.timestamp,
        last_seen: event.timestamp,
        edits: 1,
    }
}

/// Merge a later change of the same path into an earlier one.
fn fold(into: &mut FileChange, later: &FileChange) {
    into.after_hash = later.after_hash.clone();
    into.last_seen = later.last_seen;
    into.edits += later.edits;
    into.kind = match (into.kind, later.kind) {
        (ChangeKind::Created, ChangeKind::Deleted) => ChangeKind::Deleted, // filtered out in finish()
        (ChangeKind::Created, _) => ChangeKind::Created,
        (ChangeKind::Renamed, ChangeKind::Deleted) => ChangeKind::Deleted,
        (ChangeKind::Renamed, _) => ChangeKind::Renamed,
        (_, ChangeKind::Deleted) => ChangeKind::Deleted,
        // Deleted then re-created: net effect is a modification (or nothing,
        // which finish() removes when both hashes are None).
        (ChangeKind::Deleted, ChangeKind::Created) => ChangeKind::Modified,
        (_, ChangeKind::Renamed) => {
            into.from_path = later.from_path.clone();
            ChangeKind::Renamed
        }
        (kind, _) => kind,
    };
}

/// Stage 1: fold bursts of events on the same path (closer than `DEBOUNCE`).
fn debounce(events: &[&RawEvent]) -> Vec<FileChange> {
    let mut out: Vec<FileChange> = Vec::new();
    for event in events {
        let change = change_from(event);
        if let Some(last) = out.last_mut() {
            if last.path == change.path && change.first_seen - last.last_seen <= DEBOUNCE {
                fold(last, &change);
                continue;
            }
        }
        out.push(change);
    }
    out
}

/// Stage 2: group debounced changes into checkpoints by time gap.
struct Grouper<'a> {
    session_id: &'a str,
    open: Option<Checkpoint>,
    done: Vec<Checkpoint>,
    next_index: usize,
}

impl<'a> Grouper<'a> {
    fn new(session_id: &'a str) -> Self {
        Grouper {
            session_id,
            open: None,
            done: Vec::new(),
            next_index: 1,
        }
    }

    /// Close the open checkpoint (a commit or end record is a hard boundary).
    fn close(&mut self) {
        if let Some(cp) = self.open.take().and_then(Checkpoint::finish) {
            self.done.push(cp);
        }
    }

    /// HEAD moved: every checkpoint since the previous movement was work
    /// towards `to_head`.
    fn head_moved(&mut self, from_head: &str, to_head: &str) {
        self.close();
        for cp in self.done.iter_mut().filter(|cp| cp.boundary.is_none()) {
            cp.boundary = Some(CommitBoundary {
                from_head: from_head.to_string(),
                to_head: to_head.to_string(),
            });
        }
    }

    fn push(&mut self, change: FileChange) {
        let starts_new = match &self.open {
            Some(cp) => change.first_seen - cp.ended_at > WINDOW,
            None => true,
        };
        if starts_new {
            self.close();
            self.open = Some(Checkpoint::new(self.session_id, self.next_index, &change));
            self.next_index += 1;
        }
        self.open.as_mut().expect("opened above").absorb(change);
    }

    /// Debounce a run of consecutive events and push the result.
    fn push_run(&mut self, run: &mut Vec<&RawEvent>) {
        for change in debounce(run) {
            self.push(change);
        }
        run.clear();
    }

    fn finish(mut self) -> Vec<Checkpoint> {
        self.close();
        self.done
    }
}

/// Build the checkpoints of one session from its records, in order.
pub fn build_checkpoints(session_id: &str, records: &[Record]) -> Vec<Checkpoint> {
    let mut grouper = Grouper::new(session_id);
    let mut run: Vec<&RawEvent> = Vec::new();
    for record in records {
        match record {
            Record::Event(event) => run.push(event),
            Record::Commit {
                from_head, to_head, ..
            } => {
                grouper.push_run(&mut run);
                grouper.head_moved(from_head, to_head);
            }
            Record::End(_) => {
                grouper.push_run(&mut run);
                grouper.close();
            }
            Record::Session(_) => {}
        }
    }
    grouper.push_run(&mut run);
    grouper.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::store::SessionEnd;
    use chrono::TimeZone;
    use std::path::Path;

    fn t(secs: i64, millis: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap() + Duration::milliseconds(millis)
    }

    fn ev(
        secs: i64,
        millis: i64,
        path: &str,
        kind: RawEventKind,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Record {
        Record::Event(RawEvent {
            timestamp: t(secs, millis),
            path: PathBuf::from(path),
            kind,
            from_path: None,
            before_hash: before.map(String::from),
            after_hash: after.map(String::from),
        })
    }

    #[test]
    fn debounce_folds_bursts_on_the_same_file() {
        let records = vec![
            ev(0, 0, "a.rs", RawEventKind::Modified, Some("h0"), Some("h1")),
            ev(
                0,
                200,
                "a.rs",
                RawEventKind::Modified,
                Some("h1"),
                Some("h2"),
            ),
            ev(
                0,
                400,
                "a.rs",
                RawEventKind::Modified,
                Some("h2"),
                Some("h3"),
            ),
        ];
        let cps = build_checkpoints("s", &records);
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].changes.len(), 1);
        let c = &cps[0].changes[0];
        assert_eq!(c.edits, 3);
        assert_eq!(c.before_hash.as_deref(), Some("h0"));
        assert_eq!(c.after_hash.as_deref(), Some("h3"));
        assert_eq!(cps[0].id, "s.1");
    }

    #[test]
    fn repeated_edits_beyond_debounce_still_fold_within_a_checkpoint() {
        let records = vec![
            ev(0, 0, "a.rs", RawEventKind::Modified, Some("h0"), Some("h1")),
            ev(5, 0, "a.rs", RawEventKind::Modified, Some("h1"), Some("h2")),
            ev(12, 0, "b.rs", RawEventKind::Created, None, Some("x")),
            ev(
                20,
                0,
                "a.rs",
                RawEventKind::Modified,
                Some("h2"),
                Some("h3"),
            ),
        ];
        let cps = build_checkpoints("s", &records);
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].changes.len(), 2);
        let a = cps[0]
            .changes
            .iter()
            .find(|c| c.path == Path::new("a.rs"))
            .unwrap();
        assert_eq!(a.edits, 3);
        assert_eq!(a.after_hash.as_deref(), Some("h3"));
        assert_eq!(cps[0].started_at, t(0, 0));
        assert_eq!(cps[0].ended_at, t(20, 0));
    }

    #[test]
    fn a_gap_longer_than_the_window_starts_a_new_checkpoint() {
        let records = vec![
            ev(0, 0, "a.rs", RawEventKind::Modified, Some("h0"), Some("h1")),
            ev(
                31,
                0,
                "a.rs",
                RawEventKind::Modified,
                Some("h1"),
                Some("h2"),
            ),
        ];
        let cps = build_checkpoints("s", &records);
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[1].id, "s.2");
        assert_eq!(cps[1].changes[0].before_hash.as_deref(), Some("h1"));
    }

    #[test]
    fn a_commit_closes_the_open_checkpoint() {
        let records = vec![
            ev(0, 0, "a.rs", RawEventKind::Modified, Some("h0"), Some("h1")),
            Record::Commit {
                timestamp: t(1, 0),
                from_head: "aaa".into(),
                to_head: "bbb".into(),
            },
            ev(2, 0, "a.rs", RawEventKind::Modified, Some("h1"), Some("h2")),
            Record::End(SessionEnd {
                ended_at: t(3, 0),
                events: 2,
            }),
        ];
        let cps = build_checkpoints("s", &records);
        assert_eq!(cps.len(), 2);
        assert!(cps[0].ended_at < t(1, 0));
        assert!(cps[1].started_at > t(1, 0));
        assert_eq!(
            cps[0].boundary,
            Some(CommitBoundary {
                from_head: "aaa".into(),
                to_head: "bbb".into()
            })
        );
        assert_eq!(cps[1].boundary, None, "nothing was committed after it");
    }

    #[test]
    fn every_checkpoint_before_a_commit_gets_its_boundary() {
        let records = vec![
            ev(0, 0, "a.rs", RawEventKind::Modified, Some("h0"), Some("h1")),
            ev(
                40,
                0,
                "a.rs",
                RawEventKind::Modified,
                Some("h1"),
                Some("h2"),
            ),
            Record::Commit {
                timestamp: t(41, 0),
                from_head: "aaa".into(),
                to_head: "bbb".into(),
            },
            ev(
                42,
                0,
                "a.rs",
                RawEventKind::Modified,
                Some("h2"),
                Some("h3"),
            ),
            Record::Commit {
                timestamp: t(43, 0),
                from_head: "bbb".into(),
                to_head: "ccc".into(),
            },
        ];
        let cps = build_checkpoints("s", &records);
        let to: Vec<Option<&str>> = cps
            .iter()
            .map(|c| c.boundary.as_ref().map(|b| b.to_head.as_str()))
            .collect();
        assert_eq!(to, vec![Some("bbb"), Some("bbb"), Some("ccc")]);
    }

    #[test]
    fn bulk_flag_and_create_delete_cancel() {
        let mut records = Vec::new();
        for i in 0..(BULK_THRESHOLD + 1) {
            records.push(ev(
                0,
                i as i64,
                &format!("f{i}.rs"),
                RawEventKind::Modified,
                Some("a"),
                Some("b"),
            ));
        }
        records.push(ev(1, 0, "tmp", RawEventKind::Created, None, Some("t")));
        records.push(ev(2, 0, "tmp", RawEventKind::Deleted, Some("t"), None));
        let cps = build_checkpoints("s", &records);
        assert_eq!(cps.len(), 1);
        assert!(cps[0].bulk);
        assert_eq!(cps[0].changes.len(), BULK_THRESHOLD + 1);
        assert!(!cps[0].changes.iter().any(|c| c.path == Path::new("tmp")));

        let small = build_checkpoints("s", &records[..3]);
        assert!(!small[0].bulk);
    }

    #[test]
    fn kinds_fold_sensibly() {
        let records = vec![
            ev(0, 0, "n.rs", RawEventKind::Created, None, Some("a")),
            ev(1, 0, "n.rs", RawEventKind::Modified, Some("a"), Some("b")),
            ev(2, 0, "m.rs", RawEventKind::Modified, Some("a"), Some("b")),
            ev(3, 0, "m.rs", RawEventKind::Deleted, Some("b"), None),
        ];
        let cps = build_checkpoints("s", &records);
        let n = cps[0]
            .changes
            .iter()
            .find(|c| c.path == Path::new("n.rs"))
            .unwrap();
        assert_eq!(n.kind, ChangeKind::Created);
        assert_eq!(n.after_hash.as_deref(), Some("b"));
        let m = cps[0]
            .changes
            .iter()
            .find(|c| c.path == Path::new("m.rs"))
            .unwrap();
        assert_eq!(m.kind, ChangeKind::Deleted);
    }
}
