//! Baselines: where a trail starts.
//!
//! The base branch answers "what will this PR contain". A baseline answers
//! "what happened since X", where X may be the last push, the upstream
//! branch, the base branch or any commit. Baselines are pure view filters:
//! they never change what is recorded, only how far back a command looks.

use gix::bstr::ByteSlice;
use serde::Serialize;

use crate::error::{Result, TrailError};
use crate::git::repository::{serialize_oid, BaseRef, HeadState, Repo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineKind {
    /// The commit most recently pushed from this branch (from the reflog of
    /// its remote-tracking ref).
    LastPush,
    /// The current tip of the remote-tracking branch.
    Upstream,
    /// The merge base with the base branch (`main` unless `--base`).
    BaseBranch,
    /// An explicit revision given with `--since <rev>`.
    Commit,
}

#[derive(Debug, Clone, Serialize)]
pub struct Baseline {
    pub kind: BaselineKind,
    /// Human label: "last push", "upstream origin/feature", "base main", ...
    pub label: String,
    /// The commit the baseline names.
    #[serde(serialize_with = "serialize_oid")]
    pub commit: gix::ObjectId,
    /// Where the trail actually starts: the merge base of `commit` and HEAD,
    /// which differs from `commit` after a rebase or amend.
    #[serde(serialize_with = "serialize_oid")]
    pub start: gix::ObjectId,
    pub short_commit: String,
    pub short_start: String,
    /// True when `--since` was given.
    pub explicit: bool,
}

/// Selector accepted by `--since`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinceSpec {
    Push,
    Upstream,
    Base,
    Auto,
    Rev(String),
}

impl SinceSpec {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None => SinceSpec::Base,
            Some("auto") => SinceSpec::Auto,
            Some("push") | Some("last-push") => SinceSpec::Push,
            Some("upstream") | Some("@{upstream}") | Some("@{u}") => SinceSpec::Upstream,
            Some("base") => SinceSpec::Base,
            Some(rev) => SinceSpec::Rev(rev.to_string()),
        }
    }
}

impl Repo {
    /// Resolve the baseline for this run.
    ///
    /// `Auto` (the default of `trail changes`) tries last push, then upstream,
    /// then the base branch, and the result says which one was chosen.
    pub fn resolve_baseline(&self, spec: &SinceSpec, base: &BaseRef) -> Result<Baseline> {
        let explicit = !matches!(spec, SinceSpec::Base | SinceSpec::Auto);
        let (kind, label, commit) = match spec {
            SinceSpec::Auto => {
                if let Some(found) = self.last_push()? {
                    found
                } else if let Some(found) = self.upstream()? {
                    found
                } else {
                    base_branch(base)
                }
            }
            SinceSpec::Push => self.last_push()?.ok_or_else(|| {
                TrailError::NoBaseline("no push of this branch is recorded in the reflog".into())
            })?,
            SinceSpec::Upstream => self
                .upstream()?
                .ok_or_else(|| TrailError::NoBaseline("this branch has no upstream".into()))?,
            SinceSpec::Base => base_branch(base),
            SinceSpec::Rev(rev) => {
                let id = self
                    .gix()
                    .rev_parse_single(rev.as_bytes())
                    .map_err(|_| {
                        TrailError::NoBaseline(format!("'{rev}' is not a known revision"))
                    })?
                    .detach();
                (
                    BaselineKind::Commit,
                    format!("commit {}", self.short_id(&id)),
                    id,
                )
            }
        };
        let start = match self.gix().merge_base(self.head_id, commit) {
            Ok(id) => id.detach(),
            Err(_) => {
                return Err(TrailError::NoBaseline(format!(
                    "{label} ({}) shares no history with HEAD",
                    self.short_id(&commit)
                )))
            }
        };
        Ok(Baseline {
            kind,
            label,
            short_commit: self.short_id(&commit),
            short_start: self.short_id(&start),
            commit,
            start,
            explicit,
        })
    }

    /// The remote-tracking ref of the current branch (push direction first,
    /// then fetch), with its current commit.
    fn tracking_ref(&self) -> Result<Option<(String, gix::Reference<'_>)>> {
        let HeadState::Branch { name } = &self.head else {
            return Ok(None);
        };
        let full = gix::refs::FullName::try_from(format!("refs/heads/{name}"))
            .map_err(|e| TrailError::Git(e.to_string()))?;
        for direction in [gix::remote::Direction::Push, gix::remote::Direction::Fetch] {
            let Some(Ok(tracking)) = self
                .gix()
                .branch_remote_tracking_ref_name(full.as_ref(), direction)
            else {
                continue;
            };
            if let Some(reference) = self
                .gix()
                .try_find_reference(tracking.as_ref())
                .map_err(|e| TrailError::Git(e.to_string()))?
            {
                let label = tracking.shorten().to_str_lossy().into_owned();
                return Ok(Some((label, reference)));
            }
        }
        Ok(None)
    }

    fn upstream(&self) -> Result<Option<(BaselineKind, String, gix::ObjectId)>> {
        let Some((label, reference)) = self.tracking_ref()? else {
            return Ok(None);
        };
        let id = reference
            .into_fully_peeled_id()
            .map_err(|e| TrailError::Git(e.to_string()))?
            .detach();
        Ok(Some((
            BaselineKind::Upstream,
            format!("upstream {label}"),
            id,
        )))
    }

    /// Newest `update by push` entry in the remote-tracking ref's reflog.
    fn last_push(&self) -> Result<Option<(BaselineKind, String, gix::ObjectId)>> {
        let Some((label, reference)) = self.tracking_ref()? else {
            return Ok(None);
        };
        let mut platform = reference.log_iter();
        let Some(iter) = platform.rev().map_err(TrailError::Io)? else {
            return Ok(None);
        };
        for line in iter {
            let Ok(line) = line else { continue };
            if line.message.starts_with(b"update by push") {
                return Ok(Some((
                    BaselineKind::LastPush,
                    format!("last push to {label}"),
                    line.new_oid,
                )));
            }
        }
        Ok(None)
    }
}

fn base_branch(base: &BaseRef) -> (BaselineKind, String, gix::ObjectId) {
    (
        BaselineKind::BaseBranch,
        format!("base {}", base.name),
        base.id,
    )
}
