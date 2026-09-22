//! On-disk format of recorded sessions: append-only JSONL, one file per session.
//!
//! Sessions live in `<common_dir>/trail/worktrees/<worktree id>/` so they
//! survive `git worktree remove`. The first line is a `session` header, every
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHeader {
    pub version: u32,
    pub session_id: String,
    pub worktree_id: String,
    pub worktree_path: PathBuf,
    pub branch: String,
    /// HEAD when the session started.
    pub base_commit: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordedKind {
    Added,
    Modified,
    Deleted,
}

/// One observed change. Hashes are git blob ids of the content before and
/// after, so a later phase can turn a session into per-checkpoint diffs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordedEvent {
    pub ts: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: RecordedKind,
    pub path: PathBuf,
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
    Event(RecordedEvent),
    End(SessionEnd),
}

/// Directory holding this worktree's sessions.
pub fn session_dir(repo: &Repo) -> PathBuf {
    let id = repo.worktree.id.clone().unwrap_or_else(|| "main".into());
    repo.worktree
        .common_dir
        .join("trail")
        .join("worktrees")
        .join(id)
}

pub fn new_session_id(now: DateTime<Utc>) -> String {
    format!(
        "{}-{:04x}",
        now.format("%Y%m%dT%H%M%SZ"),
        std::process::id() & 0xffff
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

    pub fn record(&mut self, event: &RecordedEvent) -> Result<()> {
        self.events += 1;
        self.write(&Record::Event(event.clone()))
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

#[allow(dead_code)] // consumed by `trail history` once sessions are integrated (next PR)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_records() {
        let dir = tempfile::tempdir().unwrap();
        let header = SessionHeader {
            version: FORMAT_VERSION,
            session_id: "s1".into(),
            worktree_id: "main".into(),
            worktree_path: PathBuf::from("/repo"),
            branch: "feature".into(),
            base_commit: "abc".into(),
            started_at: Utc::now(),
        };
        let mut w = SessionWriter::create(dir.path(), &header).unwrap();
        let event = RecordedEvent {
            ts: Utc::now(),
            kind: RecordedKind::Modified,
            path: PathBuf::from("src/a.rs"),
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
            .write_all(b"{\"kind\":\"event\",\"ts\":")
            .unwrap();

        let records = read_session(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], Record::Session(header));
        assert_eq!(records[1], Record::Event(event));
        assert!(matches!(
            records[2],
            Record::End(SessionEnd { events: 1, .. })
        ));
    }
}
