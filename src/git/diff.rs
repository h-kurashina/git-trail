//! Working tree status and line statistics (via the git CLI).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::git::{run_git, split_nul};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
    Copied,
    Conflicted,
}

impl ChangeKind {
    fn from_status_char(c: char) -> Option<Self> {
        match c {
            'A' => Some(ChangeKind::Added),
            'M' => Some(ChangeKind::Modified),
            'D' => Some(ChangeKind::Deleted),
            'R' => Some(ChangeKind::Renamed),
            'T' => Some(ChangeKind::TypeChanged),
            'C' => Some(ChangeKind::Copied),
            'U' => Some(ChangeKind::Conflicted),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "Added",
            ChangeKind::Modified => "Modified",
            ChangeKind::Deleted => "Deleted",
            ChangeKind::Renamed => "Renamed",
            ChangeKind::TypeChanged => "Type changed",
            ChangeKind::Copied => "Copied",
            ChangeKind::Conflicted => "Conflicted",
        }
    }
}

/// One entry of `git status`, with the index and worktree sides kept apart so
/// staged / unstaged / partially staged files can all be distinguished.
#[derive(Debug, Clone, Serialize)]
pub struct StatusEntry {
    pub path: PathBuf,
    /// Original path for renames and copies.
    pub orig_path: Option<PathBuf>,
    /// Change recorded in the index relative to HEAD (staged).
    pub staged: Option<ChangeKind>,
    /// Change in the working tree relative to the index (unstaged).
    pub unstaged: Option<ChangeKind>,
    /// File is not tracked by git at all.
    pub untracked: bool,
}

impl StatusEntry {
    /// The change a human would name when asked "what happened to this file?".
    ///
    /// The worktree side wins because it describes the state on disk; a file
    /// that was staged as added and then edited is still "added".
    pub fn effective_kind(&self) -> ChangeKind {
        if self.untracked {
            return ChangeKind::Added;
        }
        match (self.staged, self.unstaged) {
            (Some(ChangeKind::Added), _) => ChangeKind::Added,
            (Some(ChangeKind::Renamed), _) => ChangeKind::Renamed,
            (_, Some(k)) => k,
            (Some(k), None) => k,
            (None, None) => ChangeKind::Modified,
        }
    }
}

/// Parse `git status --porcelain=v2 -z` output.
pub fn parse_status_v2(bytes: &[u8]) -> Vec<StatusEntry> {
    let fields = split_nul(bytes);
    let mut entries = Vec::new();
    let mut iter = fields.into_iter();
    while let Some(line) = iter.next() {
        let mut parts = line.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        match tag {
            "1" | "2" | "u" => {
                // XY sub mH mI mW hH hI [Xscore] path
                let n_fields = match tag {
                    "1" => 8,
                    "2" => 9,
                    _ => 10,
                };
                let mut cols = rest.splitn(n_fields, ' ');
                let xy: Vec<char> = cols.next().unwrap_or("..").chars().collect();
                let path = cols.last().unwrap_or("").to_string();
                let orig_path = if tag == "2" { iter.next() } else { None };
                let x = xy.first().copied().unwrap_or('.');
                let y = xy.get(1).copied().unwrap_or('.');
                let (staged, unstaged) = if tag == "u" {
                    (Some(ChangeKind::Conflicted), Some(ChangeKind::Conflicted))
                } else {
                    (
                        ChangeKind::from_status_char(x),
                        ChangeKind::from_status_char(y),
                    )
                };
                entries.push(StatusEntry {
                    path: PathBuf::from(path),
                    orig_path: orig_path.map(PathBuf::from),
                    staged,
                    unstaged,
                    untracked: false,
                });
            }
            "?" => entries.push(StatusEntry {
                path: PathBuf::from(rest),
                orig_path: None,
                staged: None,
                unstaged: None,
                untracked: true,
            }),
            // "!" ignored entries are never requested; headers ("#") are skipped.
            _ => {}
        }
    }
    entries
}

/// `git status` for the whole worktree. One process, no directory walk of our
/// own; git's own rename detection (`status.renames`) yields `Renamed` entries.
pub fn status(workdir: &Path) -> Result<Vec<StatusEntry>> {
    let out = run_git(
        workdir,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    Ok(parse_status_v2(&out))
}

/// Line statistics for one file.
#[derive(Debug, Clone, Serialize)]
pub struct FileStat {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    /// `None` for binary files.
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

impl FileStat {
    pub fn is_binary(&self) -> bool {
        self.additions.is_none()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct LineStats {
    pub additions: u64,
    pub deletions: u64,
}

impl LineStats {
    pub fn add(&mut self, stat: &FileStat) {
        self.additions += stat.additions.unwrap_or(0);
        self.deletions += stat.deletions.unwrap_or(0);
    }

    /// Sum of several per-file stats.
    pub fn sum<'a>(stats: impl IntoIterator<Item = &'a FileStat>) -> Self {
        let mut total = LineStats::default();
        for s in stats {
            total.add(s);
        }
        total
    }

    /// From optional per-side counts (binary files count as zero).
    pub fn from_counts(additions: Option<u64>, deletions: Option<u64>) -> Self {
        LineStats {
            additions: additions.unwrap_or(0),
            deletions: deletions.unwrap_or(0),
        }
    }
}

/// `+3 -1`, `+3`, `-1` or `±0`: the notation every text view uses.
impl std::fmt::Display for LineStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.additions, self.deletions) {
            (0, 0) => write!(f, "±0"),
            (a, 0) => write!(f, "+{a}"),
            (0, d) => write!(f, "-{d}"),
            (a, d) => write!(f, "+{a} -{d}"),
        }
    }
}

/// Parse `git diff --numstat -z` output (with `-M`, renames appear as
/// `adds\tdels\t\0old\0new\0`).
pub fn parse_numstat(bytes: &[u8]) -> Vec<FileStat> {
    let mut stats = Vec::new();
    let mut chunks = bytes.split(|b| *b == 0).peekable();
    while let Some(chunk) = chunks.next() {
        if chunk.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(chunk);
        let mut cols = text.splitn(3, '\t');
        let adds = cols.next().unwrap_or("-");
        let dels = cols.next().unwrap_or("-");
        let path = cols.next().unwrap_or("");
        let parse = |s: &str| s.parse::<u64>().ok();
        let (path, old_path) = if path.is_empty() {
            let old = chunks
                .next()
                .map(|c| String::from_utf8_lossy(c).into_owned());
            let new = chunks
                .next()
                .map(|c| String::from_utf8_lossy(c).into_owned());
            (new.unwrap_or_default(), old)
        } else {
            (path.to_string(), None)
        };
        stats.push(FileStat {
            path: PathBuf::from(path),
            old_path: old_path.map(PathBuf::from),
            additions: parse(adds),
            deletions: parse(dels),
        });
    }
    stats
}

/// `git diff --numstat` of `rev` against the working tree, for tracked files.
/// Untracked files are handled by [`untracked_stats`] because git does not
/// include them in a diff.
pub fn numstat(workdir: &Path, rev: &str, paths: &[&Path]) -> Result<Vec<FileStat>> {
    let mut args: Vec<&str> = vec!["diff", "--numstat", "-z", "-M", rev];
    let path_strs: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if !path_strs.is_empty() {
        args.push("--");
        args.extend(path_strs.iter().map(String::as_str));
    }
    let out = run_git(workdir, &args)?;
    Ok(parse_numstat(&out))
}

/// Unified diff of one tracked file, `rev` against the working tree.
pub fn unified(workdir: &Path, rev: &str, path: &Path) -> Result<String> {
    let path = path.to_string_lossy();
    let out = run_git(
        workdir,
        &["diff", "--no-color", "--no-ext-diff", rev, "--", &path],
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Count lines of untracked files so they can join the totals as pure additions.
/// Files that look binary (NUL in the first 8 KiB) report `None`.
pub fn untracked_stats(workdir: &Path, entries: &[StatusEntry]) -> Vec<FileStat> {
    entries
        .iter()
        .filter(|e| e.untracked)
        .map(|e| FileStat {
            path: e.path.clone(),
            old_path: None,
            additions: count_lines(&workdir.join(&e.path)),
            deletions: Some(0),
        })
        .collect()
}

fn count_lines(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return None;
    }
    if bytes.is_empty() {
        return Some(0);
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    Some(if bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_v2_entries() {
        let raw = b"1 .M N... 100644 100644 100644 abc def src/a.rs\0\
                    1 A. N... 000000 100644 100644 000 111 src/new.rs\0\
                    2 R. N... 100644 100644 100644 aaa bbb R100 src/b.rs\0src/old.rs\0\
                    ? notes.txt\0";
        let entries = parse_status_v2(raw);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].unstaged, Some(ChangeKind::Modified));
        assert_eq!(entries[0].staged, None);
        assert_eq!(entries[1].staged, Some(ChangeKind::Added));
        assert_eq!(entries[2].staged, Some(ChangeKind::Renamed));
        assert_eq!(
            entries[2].orig_path.as_deref(),
            Some(Path::new("src/old.rs"))
        );
        assert!(entries[3].untracked);
        assert_eq!(entries[3].path, PathBuf::from("notes.txt"));
    }

    #[test]
    fn parses_numstat_with_renames_and_binary() {
        let raw = b"3\t1\tsrc/a.rs\x00-\t-\timg.png\x005\t0\t\x00old.rs\x00new.rs\x00";
        let stats = parse_numstat(raw);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].additions, Some(3));
        assert!(stats[1].is_binary());
        assert_eq!(stats[2].path, PathBuf::from("new.rs"));
        assert_eq!(stats[2].old_path.as_deref(), Some(Path::new("old.rs")));
    }
}
