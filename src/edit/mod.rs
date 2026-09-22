//! `trail edit`: the Development Trail as an editable text buffer.
//!
//! Like oil.nvim treats a directory as a buffer, this renders the trail's
//! metadata as plain text, hands it to `$VISUAL` / `$EDITOR`, and parses the
//! result back into the metadata overlay. Only metadata changes: the raw
//! session logs, the files and Git history are never written.

pub mod editor;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{Result, TrailError};
use crate::git::repository::Repo;
use crate::recorder::checkpoint::FileChange;
use crate::recorder::metadata::{CheckpointMeta, Metadata, Move};
use crate::recorder::snapshot;
use crate::trail::event::TrailEventType;
use crate::trail::Trail;

const HEADER: &str = "\
# Editing this file changes Trail metadata.
# File contents and Git history are not modified.
#
# [checkpoint:ID] starts a checkpoint. Fields: title, hidden (true/false), note.
# Move a file line into another checkpoint to regroup it. Reorder the
# checkpoint blocks to change their reading order. Files cannot be removed
# or invented. Lines starting with # are ignored; commits are context only.
";

/// One checkpoint as it appears in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: String,
    pub title: Option<String>,
    pub hidden: bool,
    pub note: Option<String>,
    pub files: Vec<PathBuf>,
}

/// The editable view of a trail: its checkpoints, in reading order, with
/// where every file change originally came from.
#[derive(Debug, Clone)]
pub struct Document {
    pub blocks: Vec<Block>,
    /// For each block id, the raw origin of every file line (same order).
    pub origins: HashMap<String, Vec<String>>,
}

impl Document {
    /// Extract the editable state from a built trail (overlay already applied).
    pub fn from_trail(trail: &Trail) -> Self {
        let mut blocks = Vec::new();
        let mut origins = HashMap::new();
        for event in &trail.events {
            if let TrailEventType::Checkpoint {
                id,
                title,
                annotation,
                hidden,
                changes,
                ..
            } = &event.event_type
            {
                origins.insert(
                    id.clone(),
                    changes.iter().map(|c| c.origin.clone()).collect(),
                );
                blocks.push(Block {
                    id: id.clone(),
                    title: title.clone(),
                    hidden: *hidden,
                    note: annotation.clone(),
                    files: changes.iter().map(|c| c.path.clone()).collect(),
                });
            }
        }
        Document { blocks, origins }
    }

    /// Render the buffer. Commits are interleaved as comments for context.
    pub fn render(&self, trail: &Trail) -> String {
        let mut out = String::new();
        out.push_str(&format!("# trail://{}\n", trail.repository.head.label()));
        out.push_str(HEADER);
        out.push('\n');
        if self.blocks.is_empty() {
            out.push_str("# No recorded checkpoints on this branch. Run `trail start` first.\n");
            return out;
        }
        let mut blocks = self.blocks.iter();
        for event in &trail.events {
            match &event.event_type {
                TrailEventType::Commit {
                    short_id, summary, ..
                } => {
                    out.push_str(&format!("# commit {short_id}  {summary}\n\n"));
                }
                TrailEventType::Checkpoint { .. } => {
                    if let Some(block) = blocks.next() {
                        render_block(&mut out, block);
                    }
                }
                _ => {}
            }
        }
        out
    }
}

fn render_block(out: &mut String, block: &Block) {
    out.push_str(&format!("[checkpoint:{}]\n", block.id));
    out.push_str(&format!(
        "title = {}\n",
        block.title.as_deref().unwrap_or("")
    ));
    out.push_str(&format!("hidden = {}\n", block.hidden));
    if let Some(note) = &block.note {
        out.push_str(&format!("note = {note}\n"));
    }
    out.push('\n');
    for file in &block.files {
        out.push_str(&format!("{}\n", file.display()));
    }
    out.push('\n');
}

/// Parse a buffer. Syntax only; cross-checking against the original
/// document happens in [`diff`].
pub fn parse(text: &str) -> Result<Vec<Block>> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            let Some(inner) = rest.strip_suffix(']') else {
                return Err(invalid(line_no, "unterminated section header"));
            };
            let Some((kind, id)) = inner.split_once(':') else {
                return Err(invalid(line_no, "expected [checkpoint:ID]"));
            };
            match kind {
                "checkpoint" => {}
                "commit" => {
                    return Err(invalid(
                        line_no,
                        "commits are shown as comments and cannot be edited here",
                    ))
                }
                other => return Err(invalid(line_no, &format!("unknown section kind '{other}'"))),
            }
            let id = id.trim();
            if id.is_empty() {
                return Err(invalid(line_no, "missing checkpoint id"));
            }
            if !seen.insert(id.to_string()) {
                return Err(invalid(line_no, &format!("checkpoint {id} appears twice")));
            }
            blocks.push(Block {
                id: id.to_string(),
                title: None,
                hidden: false,
                note: None,
                files: Vec::new(),
            });
            continue;
        }
        let Some(block) = blocks.last_mut() else {
            return Err(invalid(
                line_no,
                "content before the first [checkpoint:ID] header",
            ));
        };
        if let Some((key, value)) = split_field(trimmed) {
            match key {
                "title" => block.title = non_empty(value),
                "note" | "annotation" => block.note = non_empty(value),
                "hidden" => {
                    block.hidden = match value.trim() {
                        "true" | "yes" => true,
                        "false" | "no" | "" => false,
                        other => {
                            return Err(invalid(
                                line_no,
                                &format!("hidden must be true or false, got '{other}'"),
                            ))
                        }
                    }
                }
                other => return Err(invalid(line_no, &format!("unknown field '{other}'"))),
            }
            continue;
        }
        // Anything else is a file path.
        let path = PathBuf::from(trimmed);
        if block.files.contains(&path) {
            return Err(invalid(
                line_no,
                &format!(
                    "{} is listed twice in checkpoint {}",
                    path.display(),
                    block.id
                ),
            ));
        }
        block.files.push(path);
    }
    Ok(blocks)
}

/// `key = value` where key is a bare identifier. Paths never contain " = "
/// at this position in practice; a path that does can be prefixed with "./".
fn split_field(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((key, value.trim()))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn invalid(line: usize, message: &str) -> TrailError {
    TrailError::InvalidEdit(format!("line {line}: {message}"))
}

/// What a successful edit changed, for the summary line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EditSummary {
    pub titled: usize,
    pub hidden_changed: usize,
    pub noted: usize,
    pub moved: usize,
    pub reordered: bool,
}

impl EditSummary {
    pub fn is_empty(&self) -> bool {
        *self == EditSummary::default()
    }
}

/// Validate `edited` against `original` and fold the result into `metadata`.
/// Nothing is written here; the caller saves atomically on success.
pub fn diff(original: &Document, edited: &[Block], metadata: &mut Metadata) -> Result<EditSummary> {
    let known: BTreeMap<&str, &Block> =
        original.blocks.iter().map(|b| (b.id.as_str(), b)).collect();
    for block in edited {
        if !known.contains_key(block.id.as_str()) {
            return Err(TrailError::InvalidEdit(format!(
                "unknown checkpoint {}; checkpoints cannot be created here",
                block.id
            )));
        }
    }
    for id in known.keys() {
        if !edited.iter().any(|b| b.id == *id) {
            return Err(TrailError::InvalidEdit(format!(
                "checkpoint {id} was removed; hide it with `hidden = true` instead"
            )));
        }
    }

    // Every file occurrence must survive: pair the i-th occurrence of a path
    // in the original with the i-th occurrence in the edited buffer.
    let mut original_occurrences: HashMap<&Path, Vec<(&str, &str)>> = HashMap::new(); // path -> (origin, shown-in)
    for block in &original.blocks {
        let origins = &original.origins[&block.id];
        for (file, origin) in block.files.iter().zip(origins) {
            original_occurrences
                .entry(file.as_path())
                .or_default()
                .push((origin.as_str(), block.id.as_str()));
        }
    }
    let mut edited_occurrences: HashMap<&Path, Vec<&str>> = HashMap::new();
    for block in edited {
        for file in &block.files {
            edited_occurrences
                .entry(file.as_path())
                .or_default()
                .push(block.id.as_str());
        }
    }
    for (path, targets) in &edited_occurrences {
        let expected = original_occurrences.get(path).map(Vec::len).unwrap_or(0);
        if expected == 0 {
            return Err(TrailError::InvalidEdit(format!(
                "{} was not recorded in any checkpoint; files cannot be invented",
                path.display()
            )));
        }
        if targets.len() != expected {
            return Err(TrailError::InvalidEdit(format!(
                "{} appears {} time(s) but was recorded {} time(s); files cannot be removed or duplicated",
                path.display(),
                targets.len(),
                expected
            )));
        }
    }
    for (path, occurrences) in &original_occurrences {
        if !edited_occurrences.contains_key(path) {
            return Err(TrailError::InvalidEdit(format!(
                "{} (checkpoint {}) was removed; move it to another checkpoint or keep it",
                path.display(),
                occurrences[0].1
            )));
        }
    }

    let mut summary = EditSummary::default();
    let doc_ids: HashSet<&str> = known.keys().copied().collect();

    // Moves: regenerated for every change shown in this document. Occurrences
    // of the same path are paired with the block they were shown in when
    // possible, so reordering blocks does not look like a move.
    metadata
        .moves
        .retain(|m| !doc_ids.contains(m.from.as_str()) && !doc_ids.contains(m.to.as_str()));
    for (path, occurrences) in &original_occurrences {
        let targets = &edited_occurrences[path];
        let mut used = vec![false; targets.len()];
        let mut assignment: Vec<Option<&str>> = vec![None; occurrences.len()];
        for (i, (_, shown_in)) in occurrences.iter().enumerate() {
            if let Some(j) = targets
                .iter()
                .enumerate()
                .position(|(j, t)| !used[j] && t == shown_in)
            {
                used[j] = true;
                assignment[i] = Some(targets[j]);
            }
        }
        for slot in assignment.iter_mut().filter(|a| a.is_none()) {
            let j = used.iter().position(|u| !u).expect("counts were checked");
            used[j] = true;
            *slot = Some(targets[j]);
        }
        for ((origin, shown_in), target) in occurrences.iter().zip(assignment) {
            let target = target.expect("assigned above");
            if *origin != target {
                metadata.moves.push(Move {
                    from: origin.to_string(),
                    path: path.to_path_buf(),
                    to: target.to_string(),
                });
            }
            if *shown_in != target {
                summary.moved += 1;
            }
        }
    }

    // Titles, hidden flags and notes.
    for block in edited {
        let before = known[block.id.as_str()];
        if before.title != block.title {
            summary.titled += 1;
        }
        if before.hidden != block.hidden {
            summary.hidden_changed += 1;
        }
        if before.note != block.note {
            summary.noted += 1;
        }
        let meta = CheckpointMeta {
            title: block.title.clone(),
            hidden: block.hidden,
            annotation: block.note.clone(),
        };
        if meta == CheckpointMeta::default() {
            metadata.checkpoints.remove(&block.id);
        } else {
            metadata.checkpoints.insert(block.id.clone(), meta);
        }
    }

    // Order: the document's block order replaces the order of these ids.
    let original_order: Vec<&str> = original.blocks.iter().map(|b| b.id.as_str()).collect();
    let edited_order: Vec<&str> = edited.iter().map(|b| b.id.as_str()).collect();
    if original_order != edited_order {
        summary.reordered = true;
    }
    metadata.order.retain(|id| !doc_ids.contains(id.as_str()));
    if edited_order != chronological(original) {
        metadata
            .order
            .extend(edited_order.iter().map(|s| s.to_string()));
    }
    Ok(summary)
}

/// Ids in the order the recorder produced them (checkpoint ids sort by time).
fn chronological(original: &Document) -> Vec<&str> {
    let mut ids: Vec<&str> = original.blocks.iter().map(|b| b.id.as_str()).collect();
    ids.sort();
    ids
}

/// `trail edit`: render, hand to the editor (or read `--from`), validate,
/// save atomically.
pub fn run(repo: &Repo, trail: &Trail, from: Option<&Path>, print: bool) -> Result<()> {
    let document = Document::from_trail(trail);
    let text = document.render(trail);
    if print {
        print!("{text}");
        return Ok(());
    }

    let edited_text = match from {
        Some(path) => std::fs::read_to_string(path).map_err(|e| TrailError::from_io(e, path))?,
        None => {
            if document.blocks.is_empty() {
                println!("No recorded checkpoints on this branch. Run `trail start` first.");
                return Ok(());
            }
            let editor = editor::Editor::resolve()?;
            let buffer = scratch_path(&trail.repository.head.label());
            std::fs::write(&buffer, &text).map_err(|e| TrailError::from_io(e, &buffer))?;
            editor.open(&buffer)?;
            let edited =
                std::fs::read_to_string(&buffer).map_err(|e| TrailError::from_io(e, &buffer))?;
            if edited == text {
                let _ = std::fs::remove_file(&buffer);
                println!("No changes.");
                return Ok(());
            }
            match apply(repo, &document, &edited) {
                Ok(summary) => {
                    let _ = std::fs::remove_file(&buffer);
                    println!("{}", describe(&summary));
                    return Ok(());
                }
                Err(err) => {
                    eprintln!("Your edits were kept at {}", buffer.display());
                    eprintln!("  fix them and run: trail edit --from {}", buffer.display());
                    return Err(err);
                }
            }
        }
    };
    let summary = apply(repo, &document, &edited_text)?;
    println!("{}", describe(&summary));
    Ok(())
}

/// Parse, validate and save. The metadata file is only replaced when every
/// check passed, and then atomically.
fn apply(repo: &Repo, document: &Document, edited_text: &str) -> Result<EditSummary> {
    let blocks = parse(edited_text)?;
    let mut metadata = Metadata::load(&repo.worktree.common_dir)?;
    let summary = diff(document, &blocks, &mut metadata)?;
    metadata.save(&repo.worktree.common_dir)?;
    Ok(summary)
}

fn describe(summary: &EditSummary) -> String {
    if summary.is_empty() {
        return "No changes.".to_string();
    }
    let mut parts = Vec::new();
    if summary.titled > 0 {
        parts.push(format!("{} title(s)", summary.titled));
    }
    if summary.noted > 0 {
        parts.push(format!("{} note(s)", summary.noted));
    }
    if summary.hidden_changed > 0 {
        parts.push(format!("{} visibility change(s)", summary.hidden_changed));
    }
    if summary.moved > 0 {
        parts.push(format!("{} file(s) regrouped", summary.moved));
    }
    if summary.reordered {
        parts.push("checkpoints reordered".to_string());
    }
    format!("Updated trail metadata: {}", parts.join(", "))
}

fn scratch_path(branch: &str) -> PathBuf {
    let safe: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("trail-edit-{safe}-{}.trail", std::process::id()))
}

/// `trail open <file>`: the file as it is in the worktree.
pub fn open_file(repo: &Repo, cwd: &Path, file: &Path, print: bool) -> Result<()> {
    let rel = repo.relative_path(cwd, file)?;
    let abs = repo.workdir().join(&rel);
    if !abs.exists() {
        return Err(TrailError::FileNotFound(rel));
    }
    if print {
        let data = std::fs::read(&abs).map_err(|e| TrailError::from_io(e, &abs))?;
        return write_stdout(&data);
    }
    editor::Editor::resolve()?.open(&abs)
}

/// `trail open <file> --at <checkpoint>`: the file as it was when that
/// checkpoint ended, restored from the snapshot blobs in the object database.
pub fn open_at(
    repo: &Repo,
    trail: &Trail,
    cwd: &Path,
    file: &Path,
    checkpoint: &str,
    print: bool,
) -> Result<()> {
    let rel = repo.relative_path(cwd, file)?;
    let checkpoints: Vec<(&str, &chrono::DateTime<chrono::Utc>, &[FileChange])> = trail
        .events
        .iter()
        .filter_map(|e| match &e.event_type {
            TrailEventType::Checkpoint { id, changes, .. } => {
                Some((id.as_str(), e.timestamp.as_ref()?, changes.as_slice()))
            }
            _ => None,
        })
        .collect();
    let Some((_, target_started, _)) = checkpoints.iter().find(|(id, _, _)| *id == checkpoint)
    else {
        return Err(TrailError::SnapshotUnavailable(format!(
            "unknown checkpoint {checkpoint} on this branch (see `trail history`)"
        )));
    };
    // Latest recorded version of the path up to and including the checkpoint.
    let latest = checkpoints
        .iter()
        .filter(|(_, started, _)| *started <= *target_started)
        .flat_map(|(_, _, changes)| changes.iter())
        .filter(|c| c.path == rel)
        .max_by_key(|c| c.last_seen);
    let Some(change) = latest else {
        return Err(TrailError::SnapshotUnavailable(format!(
            "{} was not recorded in or before checkpoint {checkpoint}",
            rel.display()
        )));
    };
    let Some(hash) = &change.after_hash else {
        return Err(TrailError::SnapshotUnavailable(format!(
            "{} had been deleted by checkpoint {checkpoint}",
            rel.display()
        )));
    };
    let Some(data) = snapshot::read_blob(repo.gix(), hash)? else {
        return Err(TrailError::SnapshotUnavailable(format!(
            "no snapshot for {} at {checkpoint} (recorded before snapshot support, larger than {} MiB, or pruned)",
            rel.display(),
            snapshot::SNAPSHOT_MAX_BYTES / (1024 * 1024)
        )));
    };
    if print {
        return write_stdout(&data);
    }
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "trail-{}-{}-{name}",
        checkpoint.replace('/', "_"),
        std::process::id()
    ));
    std::fs::write(&tmp, &data).map_err(|e| TrailError::from_io(e, &tmp))?;
    eprintln!("{} at {checkpoint} -> {}", rel.display(), tmp.display());
    editor::Editor::resolve()?.open(&tmp)
}

fn write_stdout(data: &[u8]) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    if let Err(e) = lock.write_all(data) {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(TrailError::Io(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        let mut origins = HashMap::new();
        origins.insert(
            "s.1".to_string(),
            vec!["s.1".to_string(), "s.1".to_string()],
        );
        origins.insert(
            "s.2".to_string(),
            vec!["s.2".to_string(), "s.2".to_string()],
        );
        Document {
            blocks: vec![
                Block {
                    id: "s.1".into(),
                    title: None,
                    hidden: false,
                    note: None,
                    files: vec!["src/a.rs".into(), "src/b.rs".into()],
                },
                Block {
                    id: "s.2".into(),
                    title: Some("Tests".into()),
                    hidden: false,
                    note: None,
                    files: vec!["tests/a.rs".into(), "src/a.rs".into()],
                },
            ],
            origins,
        }
    }

    #[test]
    fn parses_blocks_fields_and_files() {
        let blocks = parse(
            "# comment\n\n[checkpoint:s.1]\ntitle = Auth\nhidden = true\nnote = why\n\nsrc/a.rs\nsrc/b.rs\n\n[checkpoint:s.2]\n\ntests/a.rs\n",
        )
        .unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].title.as_deref(), Some("Auth"));
        assert!(blocks[0].hidden);
        assert_eq!(blocks[0].note.as_deref(), Some("why"));
        assert_eq!(
            blocks[0].files,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
        assert_eq!(blocks[1].title, None);
    }

    #[test]
    fn rejects_bad_syntax() {
        assert!(parse("src/a.rs\n")
            .unwrap_err()
            .to_string()
            .contains("before the first"));
        assert!(parse("[checkpoint:s.1\n")
            .unwrap_err()
            .to_string()
            .contains("unterminated"));
        assert!(parse("[commit:abc]\n")
            .unwrap_err()
            .to_string()
            .contains("cannot be edited"));
        assert!(parse("[checkpoint:s.1]\ncolor = red\n")
            .unwrap_err()
            .to_string()
            .contains("unknown field"));
        assert!(parse("[checkpoint:s.1]\nhidden = maybe\n")
            .unwrap_err()
            .to_string()
            .contains("true or false"));
        assert!(parse("[checkpoint:s.1]\nsrc/a.rs\nsrc/a.rs\n")
            .unwrap_err()
            .to_string()
            .contains("twice"));
        assert!(parse("[checkpoint:s.1]\n[checkpoint:s.1]\n")
            .unwrap_err()
            .to_string()
            .contains("appears twice"));
    }

    #[test]
    fn diff_detects_title_hidden_move_and_order() {
        let original = doc();
        let edited = parse(
            "[checkpoint:s.2]\ntitle = Tests\nhidden = true\n\ntests/a.rs\nsrc/a.rs\nsrc/b.rs\n\n[checkpoint:s.1]\ntitle = Auth\n\nsrc/a.rs\n",
        )
        .unwrap();
        let mut metadata = Metadata::default();
        let summary = diff(&original, &edited, &mut metadata).unwrap();
        assert_eq!(summary.titled, 1);
        assert_eq!(summary.hidden_changed, 1);
        assert_eq!(summary.moved, 1);
        assert!(summary.reordered);
        assert_eq!(metadata.checkpoints["s.1"].title.as_deref(), Some("Auth"));
        assert!(metadata.checkpoints["s.2"].hidden);
        assert_eq!(
            metadata.moves,
            vec![Move {
                from: "s.1".into(),
                path: "src/b.rs".into(),
                to: "s.2".into()
            }]
        );
        assert_eq!(metadata.order, vec!["s.2".to_string(), "s.1".to_string()]);
    }

    #[test]
    fn diff_rejects_unknown_removed_or_invented() {
        let original = doc();
        let mut m = Metadata::default();
        let unknown = parse("[checkpoint:s.1]\nsrc/a.rs\nsrc/b.rs\n[checkpoint:s.2]\ntests/a.rs\nsrc/a.rs\n[checkpoint:s.9]\n").unwrap();
        assert!(diff(&original, &unknown, &mut m)
            .unwrap_err()
            .to_string()
            .contains("unknown checkpoint"));
        let removed_block = parse("[checkpoint:s.1]\nsrc/a.rs\nsrc/b.rs\ntests/a.rs\n").unwrap();
        assert!(diff(&original, &removed_block, &mut m)
            .unwrap_err()
            .to_string()
            .contains("was removed"));
        let removed_file =
            parse("[checkpoint:s.1]\nsrc/a.rs\n[checkpoint:s.2]\ntests/a.rs\nsrc/a.rs\n").unwrap();
        assert!(diff(&original, &removed_file, &mut m)
            .unwrap_err()
            .to_string()
            .contains("src/b.rs"));
        let invented = parse("[checkpoint:s.1]\nsrc/a.rs\nsrc/b.rs\nnew.rs\n[checkpoint:s.2]\ntests/a.rs\nsrc/a.rs\n").unwrap();
        assert!(diff(&original, &invented, &mut m)
            .unwrap_err()
            .to_string()
            .contains("invented"));
        assert!(m.is_empty(), "failed edits leave metadata untouched");
    }
}
