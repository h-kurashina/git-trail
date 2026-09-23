//! Unified diffs and line statistics between two blobs. Used for
//! checkpoint diffs (snapshot → snapshot) and commit diffs (tree → tree)
//! alike; the caller decides which two contents to compare.

use std::fmt::Write as _;
use std::path::Path;

use crate::git::diff::LineStats;

/// Unified diff context lines.
pub const CONTEXT_LINES: u32 = 3;

pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0)
}

/// Line counts of the change from `before` to `after`.
pub fn line_stats(before: &[u8], after: &[u8]) -> LineStats {
    use gix::diff::blob::{sources::byte_lines, Algorithm, Diff, InternedInput};
    let input = InternedInput::new(byte_lines(before), byte_lines(after));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut stats = LineStats::default();
    for hunk in diff.hunks() {
        stats.deletions += u64::from(hunk.before.end - hunk.before.start);
        stats.additions += u64::from(hunk.after.end - hunk.after.start);
    }
    stats
}

struct UnifiedSink {
    out: String,
}

impl gix::diff::blob::unified_diff::ConsumeHunk for UnifiedSink {
    type Out = String;

    fn consume_hunk(
        &mut self,
        header: gix::diff::blob::unified_diff::HunkHeader,
        lines: &[(gix::diff::blob::unified_diff::DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        use gix::diff::blob::unified_diff::DiffLineKind;
        let _ = writeln!(self.out, "{header}");
        for (kind, line) in lines {
            let prefix = match kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Add => '+',
                DiffLineKind::Remove => '-',
            };
            self.out.push(prefix);
            self.out.push_str(&String::from_utf8_lossy(line));
            if !line.ends_with(b"\n") {
                self.out.push_str("\n\\ No newline at end of file\n");
            }
        }
        Ok(())
    }

    fn finish(self) -> String {
        self.out
    }
}

/// Unified diff between two snapshots, with `---`/`+++` headers.
pub fn unified_diff(
    path: &Path,
    from_path: Option<&Path>,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> String {
    use gix::diff::blob::{
        sources::byte_lines, unified_diff::ContextSize, Algorithm, Diff, InternedInput, UnifiedDiff,
    };
    let mut out = String::new();
    let old_name = match (before, from_path) {
        (None, _) => "/dev/null".to_string(),
        (Some(_), Some(from)) => format!("a/{}", from.display()),
        (Some(_), None) => format!("a/{}", path.display()),
    };
    let new_name = match after {
        None => "/dev/null".to_string(),
        Some(_) => format!("b/{}", path.display()),
    };
    let _ = writeln!(out, "--- {old_name}");
    let _ = writeln!(out, "+++ {new_name}");
    let before = before.unwrap_or_default();
    let after = after.unwrap_or_default();
    if is_binary(before) || is_binary(after) {
        let _ = writeln!(out, "Binary files differ");
        return out;
    }
    let input = InternedInput::new(byte_lines(before), byte_lines(after));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let sink = UnifiedSink { out: String::new() };
    let body = UnifiedDiff::new(&diff, &input, sink, ContextSize::symmetrical(CONTEXT_LINES))
        .consume()
        .unwrap_or_default();
    out.push_str(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_has_headers_and_hunks() {
        let out = unified_diff(
            Path::new("a.rs"),
            None,
            Some(b"one\ntwo\nthree\n"),
            Some(b"one\n2\nthree\n"),
        );
        assert!(
            out.starts_with("--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n"),
            "{out}"
        );
        let created = unified_diff(Path::new("n.rs"), None, None, Some(b"new\n"));
        assert!(
            created.starts_with("--- /dev/null\n+++ b/n.rs\n"),
            "{created}"
        );
        assert!(created.contains("+new\n"));
        let deleted = unified_diff(Path::new("d.rs"), None, Some(b"old\n"), None);
        assert!(deleted.contains("+++ /dev/null\n"));
        assert!(deleted.contains("-old\n"));
        let renamed = unified_diff(
            Path::new("new.rs"),
            Some(Path::new("old.rs")),
            Some(b"x\n"),
            Some(b"x\n"),
        );
        assert!(renamed.starts_with("--- a/old.rs\n+++ b/new.rs\n"));
        assert!(
            !renamed.contains("@@"),
            "identical content has no hunks: {renamed}"
        );
        let binary = unified_diff(Path::new("b.bin"), None, Some(b"\0\x01"), Some(b"\0\x02"));
        assert!(binary.contains("Binary files differ"));
        let no_newline = unified_diff(Path::new("a"), None, Some(b"x"), Some(b"y"));
        assert!(no_newline.contains("\\ No newline at end of file"));
    }

    #[test]
    fn line_stats_count_hunk_sizes() {
        let s = line_stats(b"a\nb\nc\n", b"a\nc\nd\ne\n");
        assert_eq!(s.deletions, 1);
        assert_eq!(s.additions, 2);
    }
}
