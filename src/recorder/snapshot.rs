//! Snapshot storage: the content behind every `after_hash`.
//!
//! A blob hash alone only identifies content; nothing guarantees the object
//! database still has it (an uncommitted file may never be committed). The
//! recorder therefore writes each observed version into the object database
//! and, so `git gc` keeps it, points `refs/trail/sessions/<session id>` at a
//! commit whose tree lists every snapshot blob of the session.
//!
//! The tree is deliberately flat (`<blob id>` -> blob): checkpoints are
//! derived at read time and must not be baked into refs. The JSONL log says
//! which blob belongs to which path and moment.

use std::collections::BTreeSet;

use gix::bstr::BString;

use crate::error::{Result, TrailError};

/// Files larger than this are hashed and recorded but not snapshotted.
pub const SNAPSHOT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Refresh the protecting ref after this many new blobs, so a crash loses
/// at most a handful of snapshots to a later `git gc`.
pub const PROTECT_EVERY: usize = 25;

pub fn ref_name(session_id: &str) -> String {
    format!("refs/trail/sessions/{session_id}")
}

pub struct SnapshotStore<'r> {
    repo: &'r gix::Repository,
    session_id: String,
    blobs: BTreeSet<gix::ObjectId>,
    unprotected: usize,
    /// Commit the ref currently points at; each protection commit chains to
    /// the previous one, which is also what gix requires to update the ref.
    last_commit: Option<gix::ObjectId>,
}

impl<'r> SnapshotStore<'r> {
    pub fn new(repo: &'r gix::Repository, session_id: &str) -> Self {
        SnapshotStore {
            repo,
            session_id: session_id.to_string(),
            blobs: BTreeSet::new(),
            unprotected: 0,
            last_commit: None,
        }
    }

    /// Write `data` as a blob. Returns false when the file is over the size
    /// cap and was skipped.
    pub fn store(&mut self, data: &[u8]) -> Result<bool> {
        if data.len() > SNAPSHOT_MAX_BYTES {
            return Ok(false);
        }
        let id = self
            .repo
            .write_blob(data)
            .map_err(|e| TrailError::Git(format!("cannot write snapshot blob: {e}")))?
            .detach();
        if self.blobs.insert(id) {
            self.unprotected += 1;
        }
        Ok(true)
    }

    pub fn needs_protection(&self) -> bool {
        self.unprotected >= PROTECT_EVERY
    }

    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// Point `refs/trail/sessions/<id>` at a commit whose tree references
    /// every blob stored so far. Idempotent; a no-op when nothing was stored.
    pub fn protect(&mut self) -> Result<Option<gix::ObjectId>> {
        if self.blobs.is_empty() {
            return Ok(None);
        }
        let git =
            |e: &dyn std::fmt::Display| TrailError::Git(format!("cannot protect snapshots: {e}"));
        let mut tree = gix::objs::Tree::empty();
        for id in &self.blobs {
            tree.entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: BString::from(id.to_string()),
                oid: *id,
            });
        }
        // Hex names of equal length sort the same way as the set does.
        let tree_id = self.repo.write_object(&tree).map_err(|e| git(&e))?.detach();
        let now = format!("{} +0000", chrono::Utc::now().timestamp());
        let signature = gix::actor::SignatureRef {
            name: "trail".into(),
            email: "trail@localhost".into(),
            time: &now,
        };
        let commit = self
            .repo
            .commit_as(
                signature,
                signature,
                ref_name(&self.session_id),
                format!(
                    "trail snapshots for session {} ({} blobs)",
                    self.session_id,
                    self.blobs.len()
                ),
                tree_id,
                self.last_commit,
            )
            .map_err(|e| git(&e))?
            .detach();
        self.last_commit = Some(commit);
        self.unprotected = 0;
        Ok(Some(commit))
    }
}

/// Content of a snapshot blob, `None` when the object database no longer
/// (or never) had it.
pub fn read_blob(repo: &gix::Repository, hex: &str) -> Result<Option<Vec<u8>>> {
    // A hash that is not a valid object id cannot be in the database.
    let Ok(id) = gix::ObjectId::from_hex(hex.as_bytes()) else {
        return Ok(None);
    };
    match repo.try_find_object(id) {
        Ok(Some(object)) => Ok(Some(object.data.clone())),
        Ok(None) => Ok(None),
        Err(e) => Err(TrailError::Git(e.to_string())),
    }
}

/// True when the session's protecting ref exists.
pub fn has_snapshots(repo: &gix::Repository, session_id: &str) -> bool {
    matches!(repo.try_find_reference(&ref_name(session_id)), Ok(Some(_)))
}
