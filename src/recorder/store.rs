//! On-disk format of recorded sessions: append-only JSONL, one file per session.
//!
//! Sessions live in `<common_dir>/trail/worktrees/<worktree id>/sessions/` so
//! they survive `git worktree remove`. The first line is a `session` header, every
//! change is an `event` line, and a graceful stop appends an `end` line.
//! A crash simply leaves the file without an `end` line; every earlier line is
//! still valid.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, TrailError};
use crate::git::repository::Repo;

pub const FORMAT_VERSION: u32 = 1;

/// Everything needed to understand a session after its worktree is gone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHeader {
    pub version: u32,
    pub session_id: String,
    /// Root of the main worktree (the one that owns `.git`).
    pub repository_root: PathBuf,
    pub worktree_id: String,
    pub worktree_path: PathBuf,
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Merge base with the detected base branch, if one could be found.
    pub base_commit: Option<String>,
    /// HEAD when the session started.
    pub start_head: Option<String>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawEventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// One observed change, exactly as the recorder saw it.
///
/// Hashes are git blob ids of the content before and after, so a later phase
/// can store snapshots in the object database and rebuild per-checkpoint diffs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawEvent {
    pub timestamp: DateTime<Utc>,
    pub path: PathBuf,
    /// Serialized as `type`: `kind` is the record tag.
    #[serde(rename = "type")]
    pub kind: RawEventKind,
    /// Previous path for `Renamed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_path: Option<PathBuf>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEnd {
    pub ended_at: DateTime<Utc>,
    pub events: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Session(SessionHeader),
    Event(RawEvent),
    /// HEAD moved while recording (a commit, checkout, reset, ...). Written
    /// only after gix confirmed the OID really changed.
    Commit {
        timestamp: DateTime<Utc>,
        from_head: String,
        to_head: String,
    },
    End(SessionEnd),
}

/// Directory holding this worktree's sessions.
pub fn session_dir(repo: &Repo) -> PathBuf {
    repo.worktree
        .common_dir
        .join("trail")
        .join("worktrees")
        .join(worktree_id(repo))
        .join("sessions")
}

/// Identifier of the worktree inside the trail store: the linked worktree's
/// id under `.git/worktrees/`, or `main` for the main worktree.
pub fn worktree_id(repo: &Repo) -> String {
    repo.worktree.id.clone().unwrap_or_else(|| "main".into())
}

/// Sortable, human readable id: UTC time plus a few bits of the pid so two
/// recorders started in the same second do not collide.
pub fn new_session_id(now: DateTime<Utc>) -> String {
    format!(
        "{}-{:03x}",
        now.format("%Y%m%d-%H%M%S"),
        std::process::id() & 0xfff
    )
}

/// Append-only writer. Every record is flushed immediately so a crash loses at
/// most the line being written.
pub struct SessionWriter {
    path: PathBuf,
    out: BufWriter<File>,
    events: usize,
}

impl SessionWriter {
    pub fn create(dir: &Path, header: &SessionHeader) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| TrailError::from_io(e, dir))?;
        let path = dir.join(format!("session-{}.jsonl", header.session_id));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|e| TrailError::from_io(e, &path))?;
        let mut writer = SessionWriter {
            path,
            out: BufWriter::new(file),
            events: 0,
        };
        writer.write(&Record::Session(header.clone()))?;
        Ok(writer)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn events(&self) -> usize {
        self.events
    }

    pub fn record(&mut self, event: &RawEvent) -> Result<()> {
        self.events += 1;
        self.write(&Record::Event(event.clone()))
    }

    pub fn record_commit(
        &mut self,
        timestamp: DateTime<Utc>,
        from_head: &str,
        to_head: &str,
    ) -> Result<()> {
        self.write(&Record::Commit {
            timestamp,
            from_head: from_head.to_string(),
            to_head: to_head.to_string(),
        })
    }

    pub fn finish(mut self, ended_at: DateTime<Utc>) -> Result<()> {
        let end = SessionEnd {
            ended_at,
            events: self.events,
        };
        self.write(&Record::End(end))
    }

    fn write(&mut self, record: &Record) -> Result<()> {
        let line = serde_json::to_string(record).map_err(|e| TrailError::Git(e.to_string()))?;
        self.out.write_all(line.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        Ok(())
    }
}

/// Read every parseable record of a session file. Corrupt or truncated lines
/// are skipped so a crashed session is still readable.
pub fn read_session(path: &Path) -> Result<Vec<Record>> {
    let file = File::open(path).map_err(|e| TrailError::from_io(e, path))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if let Ok(record) = serde_json::from_str::<Record>(&line) {
            records.push(record);
        }
    }
    Ok(records)
}

/// A session file as found on disk.
#[derive(Debug, Clone, Serialize)]
pub struct SessionFile {
    pub path: PathBuf,
    pub header: SessionHeader,
    #[serde(skip)]
    pub records: Vec<Record>,
    pub ended_at: Option<DateTime<Utc>>,
    pub events: usize,
    pub commits: usize,
}

impl SessionFile {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let records = read_session(path)?;
        let Some(Record::Session(header)) = records.first().cloned() else {
            return Ok(None); // not a session file (or its first line is corrupt)
        };
        let ended_at = records.iter().find_map(|r| match r {
            Record::End(end) => Some(end.ended_at),
            _ => None,
        });
        Ok(Some(SessionFile {
            path: path.to_path_buf(),
            events: records
                .iter()
                .filter(|r| matches!(r, Record::Event(_)))
                .count(),
            commits: records
                .iter()
                .filter(|r| matches!(r, Record::Commit { .. }))
                .count(),
            header,
            records,
            ended_at,
        }))
    }

    /// Timestamp of the last record, used while a session is still open.
    pub fn last_activity(&self) -> DateTime<Utc> {
        self.records
            .iter()
            .rev()
            .map(|r| match r {
                Record::Event(e) => e.timestamp,
                Record::Commit { timestamp, .. } => *timestamp,
                Record::End(e) => e.ended_at,
                Record::Session(h) => h.started_at,
            })
            .next()
            .unwrap_or(self.header.started_at)
    }
}

/// Every session stored for the repository, across all worktrees (including
/// ones that no longer exist), oldest first.
pub fn list_sessions(common_dir: &Path) -> Result<Vec<SessionFile>> {
    let root = common_dir.join("trail").join("worktrees");
    let mut sessions = Vec::new();
    let Ok(worktrees) = std::fs::read_dir(&root) else {
        return Ok(sessions);
    };
    for worktree in worktrees.flatten() {
        let dir = worktree.path().join("sessions");
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(session) = SessionFile::load(&path)? {
                    sessions.push(session);
                }
            }
        }
    }
    sessions.sort_by_key(|s| s.header.started_at);
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_records() {
        let dir = tempfile::tempdir().unwrap();
        let header = SessionHeader {
            version: FORMAT_VERSION,
            session_id: "s1".into(),
            repository_root: PathBuf::from("/repo"),
            worktree_id: "main".into(),
            worktree_path: PathBuf::from("/repo"),
            branch: Some("feature".into()),
            base_commit: Some("abc".into()),
            start_head: Some("def".into()),
            started_at: Utc::now(),
        };
        let mut w = SessionWriter::create(dir.path(), &header).unwrap();
        let event = RawEvent {
            timestamp: Utc::now(),
            kind: RawEventKind::Modified,
            path: PathBuf::from("src/a.rs"),
            from_path: None,
            before_hash: Some("1".into()),
            after_hash: Some("2".into()),
        };
        w.record(&event).unwrap();
        let path = w.path().to_path_buf();
        w.finish(Utc::now()).unwrap();

        // Append garbage to simulate a torn write.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"kind\":\"event\",\"timestamp\":")
            .unwrap();

        let records = read_session(&path).unwrap();
        assert_eq!(
            records.len(),
            3,
            "{}",
            std::fs::read_to_string(&path).unwrap()
        );
        assert_eq!(records[0], Record::Session(header));
        assert_eq!(records[1], Record::Event(event));
        assert!(matches!(
            records[2],
            Record::End(SessionEnd { events: 1, .. })
        ));
    }
}
