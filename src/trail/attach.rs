//! Which commit a checkpoint was work towards.
//!
//! A checkpoint describes how something was made, a commit what was
//! confirmed. To show both, every checkpoint is attached to the commit that
//! followed it, or to the working tree when nothing was committed yet. The
//! rules, in order:
//!
//! 1. **Recorded**: the recorder saw HEAD move from `a` to `b` right after
//!    the checkpoint, and `b` is a reviewed commit whose first parent is `a`.
//!    That movement was a commit, not a checkout or reset.
//! 2. **Inferred, by author time**: HEAD moved to a commit that is no longer
//!    on the branch (amend, rebase). The rewritten commit keeps its author
//!    time, so a reviewed commit with the same author time is the same work.
//!    A movement onto a commit that is still on the branch is a checkout or
//!    reset and gets no such treatment.
//! 3. **Inferred, by time**: nothing usable was recorded (the recorder was
//!    stopped, or the movement was a checkout). The first reviewed commit
//!    created after the checkpoint ended is assumed to contain it.
//! 4. Otherwise the checkpoint is still uncommitted: working tree.
//!
//! Git facts are never invented: a commit without checkpoints simply has
//! none, and a checkpoint is never attached to a commit older than itself.

use chrono::{DateTime, Utc};

use crate::git::history::{CommitIdentity, CommitInfo};
use crate::recorder::checkpoint::CommitBoundary;
use crate::trail::event::Attachment;

/// The commit (by id) a checkpoint belongs to, and how sure we are.
/// `(None, None)` means "working tree".
pub fn attach(
    commits: &[CommitInfo],
    boundary: Option<&CommitBoundary>,
    ended_at: DateTime<Utc>,
    identity_of: &dyn Fn(&str) -> Option<CommitIdentity>,
) -> (Option<String>, Option<Attachment>) {
    if let Some(b) = boundary {
        // 1. A commit the recorder watched being made.
        if let Some(c) = commits
            .iter()
            .find(|c| c.id == b.to_head && c.first_parent.as_deref() == Some(b.from_head.as_str()))
        {
            return (Some(c.id.clone()), Some(Attachment::Recorded));
        }
        // 2. The same work, rewritten since. A movement onto a commit that
        //    is itself reviewed was a checkout or reset, not a rewrite.
        let to_is_reviewed = commits.iter().any(|c| c.id == b.to_head);
        if !to_is_reviewed {
            if let Some(c) = identity_of(&b.to_head).and_then(|id| rewritten(commits, &id)) {
                return (Some(c.id.clone()), Some(Attachment::Inferred));
            }
        }
    }
    // 3. The next commit in time.
    if let Some(c) = commits.iter().find(|c| c.time >= ended_at) {
        return (Some(c.id.clone()), Some(Attachment::Inferred));
    }
    (None, None)
}

/// The reviewed commit that is `original` rewritten. Author time is what a
/// rewrite preserves; when several commits share the same second, the
/// summary and then the first parent tell them apart.
fn rewritten<'a>(commits: &'a [CommitInfo], original: &CommitIdentity) -> Option<&'a CommitInfo> {
    let same_time: Vec<&CommitInfo> = commits
        .iter()
        .filter(|c| c.author_time == original.author_time)
        .collect();
    match same_time.len() {
        0 => None,
        1 => Some(same_time[0]),
        _ => same_time
            .iter()
            .find(|c| c.summary == original.summary)
            .or_else(|| {
                same_time
                    .iter()
                    .find(|c| c.first_parent == original.first_parent)
            })
            .copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn commit(id: &str, parent: Option<&str>, time: i64, author_time: i64) -> CommitInfo {
        CommitInfo {
            id: id.into(),
            short_id: id[..3.min(id.len())].into(),
            summary: String::new(),
            author: String::new(),
            time: t(time),
            author_time: t(author_time),
            parent_count: usize::from(parent.is_some()),
            first_parent: parent.map(String::from),
            files: Vec::new(),
        }
    }

    fn boundary(from: &str, to: &str) -> CommitBoundary {
        CommitBoundary {
            from_head: from.into(),
            to_head: to.into(),
        }
    }

    #[test]
    fn recorded_boundary_wins_when_it_was_a_commit() {
        let commits = vec![
            commit("a", Some("base"), 100, 100),
            commit("b", Some("a"), 200, 200),
        ];
        let none = |_: &str| None;
        assert_eq!(
            attach(&commits, Some(&boundary("a", "b")), t(150), &none),
            (Some("b".into()), Some(Attachment::Recorded))
        );
        // A movement that is not a commit (reset to a) falls through to
        // time: the next commit after the checkpoint is b, never a itself
        // even though a's identity is known.
        let lookup = |id: &str| (id == "a").then(|| CommitIdentity::of(&commits[0]));
        assert_eq!(
            attach(&commits, Some(&boundary("b", "a")), t(150), &lookup),
            (Some("b".into()), Some(Attachment::Inferred))
        );
    }

    #[test]
    fn rewritten_commit_is_found_by_author_time() {
        // x was amended into x2: same author time, new committer time.
        let commits = vec![commit("x2", Some("base"), 500, 100)];
        let x = CommitIdentity {
            author_time: t(100),
            summary: String::new(),
            first_parent: Some("base".into()),
        };
        let lookup = |id: &str| (id == "x").then(|| x.clone());
        assert_eq!(
            attach(&commits, Some(&boundary("base", "x")), t(90), &lookup),
            (Some("x2".into()), Some(Attachment::Inferred))
        );
        // Two commits in the same second: the summary tells them apart, and
        // a rebase (new parent, same summary) is still recognised.
        let mut a = commit("a2", Some("base"), 500, 100);
        a.summary = "A".into();
        let mut b = commit("b2", Some("a2"), 501, 100);
        b.summary = "B".into();
        let commits = vec![a, b];
        let original_b = CommitIdentity {
            author_time: t(100),
            summary: "B".into(),
            first_parent: Some("old-a".into()),
        };
        let lookup = |id: &str| (id == "b").then(|| original_b.clone());
        assert_eq!(
            attach(&commits, Some(&boundary("old-a", "b")), t(90), &lookup).0,
            Some("b2".into())
        );
    }

    #[test]
    fn without_a_boundary_the_next_commit_in_time_is_used() {
        let commits = vec![
            commit("a", None, 100, 100),
            commit("b", Some("a"), 300, 300),
        ];
        let none = |_: &str| None;
        assert_eq!(
            attach(&commits, None, t(200), &none),
            (Some("b".into()), Some(Attachment::Inferred))
        );
        assert_eq!(attach(&commits, None, t(50), &none).0, Some("a".into()));
        // Work after the last commit is uncommitted.
        assert_eq!(attach(&commits, None, t(400), &none), (None, None));
        // A commit that no checkpoint precedes simply has none: nothing to
        // assert here beyond "a" never claiming the later checkpoint.
    }
}
