//! Human edits layered over the immutable raw log.
//!
//! The raw session files record what happened. This file records what people
//! later said about it: titles, notes, which checkpoints are hidden, which
//! changes were regrouped and in which order checkpoints should be read.
//! Applying the overlay never touches the raw log, and deleting the overlay
//! restores the recorder's view.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TrailError};
use crate::recorder::checkpoint::Checkpoint;

pub const METADATA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
}

/// A change regrouped by a human: `(from, path)` identifies it in the raw
/// log, `to` is the checkpoint it should be shown in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Move {
    pub from: String,
    pub path: PathBuf,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub version: u32,
    #[serde(default)]
    pub checkpoints: BTreeMap<String, CheckpointMeta>,
    #[serde(default)]
    pub moves: Vec<Move>,
    /// Reading order of checkpoints. Ids not listed keep their chronological
    /// position; listed ids are permuted among the slots they occupy.
    #[serde(default)]
    pub order: Vec<String>,
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata {
            version: METADATA_VERSION,
            checkpoints: BTreeMap::new(),
            moves: Vec::new(),
            order: Vec::new(),
        }
    }
}

pub fn metadata_path(common_dir: &Path) -> PathBuf {
    common_dir
        .join("trail")
        .join("metadata")
        .join("checkpoints.json")
}

impl Metadata {
    pub fn load(common_dir: &Path) -> Result<Self> {
        let path = metadata_path(common_dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                TrailError::Git(format!(
                    "trail metadata at {} is not valid JSON: {e}\n  hint: fix or remove the file; the raw session logs are untouched",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Metadata::default()),
            Err(e) => Err(TrailError::from_io(e, &path)),
        }
    }

    /// Write atomically: serialize to a sibling temp file, then rename over
    /// the old one, so a crash mid-write cannot leave a half-written overlay.
    pub fn save(&self, common_dir: &Path) -> Result<()> {
        let path = metadata_path(common_dir);
        let dir = path.parent().expect("metadata path has a parent");
        std::fs::create_dir_all(dir).map_err(|e| TrailError::from_io(e, dir))?;
        let tmp = dir.join(format!(".checkpoints.json.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| TrailError::Git(e.to_string()))?;
        std::fs::write(&tmp, bytes).map_err(|e| TrailError::from_io(e, &tmp))?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(TrailError::from_io(e, &path));
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty() && self.moves.is_empty() && self.order.is_empty()
    }

    /// Apply titles, hidden flags, annotations and moves. Ordering is applied
    /// by the trail builder because it needs the surrounding timeline.
    pub fn apply(&self, checkpoints: Vec<Checkpoint>) -> Vec<Checkpoint> {
        let mut checkpoints = checkpoints;
        for cp in checkpoints.iter_mut() {
            if let Some(meta) = self.checkpoints.get(&cp.id) {
                cp.title = meta.title.clone();
                cp.hidden = meta.hidden;
                cp.annotation = meta.annotation.clone();
            }
        }
        if self.moves.is_empty() {
            return checkpoints;
        }
        // Pull moved changes out of their origin ...
        let mut moved = Vec::new();
        for cp in checkpoints.iter_mut() {
            let mut kept = Vec::with_capacity(cp.changes.len());
            for change in cp.changes.drain(..) {
                match self
                    .moves
                    .iter()
                    .find(|m| m.from == change.origin && m.path == change.path)
                {
                    Some(m) => moved.push((m.to.clone(), change)),
                    None => kept.push(change),
                }
            }
            cp.changes = kept;
        }
        // ... and put them where the human wants them. A target that does
        // not exist (log deleted) leaves the change in its origin.
        for (to, change) in moved {
            match checkpoints.iter_mut().find(|cp| cp.id == to) {
                Some(cp) => cp.changes.push(change),
                None => {
                    if let Some(origin) = checkpoints.iter_mut().find(|cp| cp.id == change.origin) {
                        origin.changes.push(change);
                    }
                }
            }
        }
        for cp in checkpoints.iter_mut() {
            cp.changes
                .sort_by(|a, b| a.first_seen.cmp(&b.first_seen).then(a.path.cmp(&b.path)));
            cp.bulk = cp.changes.len() > crate::recorder::checkpoint::BULK_THRESHOLD;
        }
        checkpoints
            .into_iter()
            .filter(|cp| !cp.changes.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_is_atomic_and_load_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join(".git");
        assert!(Metadata::load(&common).unwrap().is_empty());
        let mut m = Metadata::default();
        m.checkpoints.insert(
            "s.1".into(),
            CheckpointMeta {
                title: Some("Auth".into()),
                hidden: true,
                annotation: None,
            },
        );
        m.save(&common).unwrap();
        assert_eq!(Metadata::load(&common).unwrap(), m);
        let leftovers: Vec<_> = std::fs::read_dir(common.join("trail/metadata"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn invalid_metadata_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join(".git");
        let path = metadata_path(&common);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        let err = Metadata::load(&common).unwrap_err().to_string();
        assert!(err.contains("not valid JSON"));
    }
}
