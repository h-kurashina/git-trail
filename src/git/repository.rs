//! Repository handle: discovery, HEAD, base branch resolution and merge base.

use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use serde::Serialize;

use crate::error::{Result, TrailError};
use crate::git::normalize_path;
use crate::git::worktree::WorktreeInfo;

/// State of HEAD as far as trail cares.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeadState {
    Branch { name: String },
    Detached { short_id: String },
}

impl HeadState {
    /// Branch name, `None` on a detached HEAD.
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            HeadState::Branch { name } => Some(name),
            HeadState::Detached { .. } => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            HeadState::Branch { name } => name.clone(),
            HeadState::Detached { short_id } => format!("(detached HEAD at {short_id})"),
        }
    }
}

/// A resolved base branch: the name we show and the commit it points to.
#[derive(Debug, Clone, Serialize)]
pub struct BaseRef {
    pub name: String,
    #[serde(serialize_with = "serialize_oid")]
    pub id: gix::ObjectId,
    /// Best common ancestor of HEAD and the base; every "since branching" query
    /// starts here.
    #[serde(serialize_with = "serialize_oid")]
    pub merge_base: gix::ObjectId,
    /// True when `--base` was passed explicitly.
    pub explicit: bool,
}

pub fn serialize_oid<S: serde::Serializer>(
    id: &gix::ObjectId,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&id.to_string())
}

pub struct Repo {
    inner: gix::Repository,
    pub name: String,
    pub worktree: WorktreeInfo,
    pub head: HeadState,
    pub head_id: gix::ObjectId,
    pub is_shallow: bool,
}

impl Repo {
    /// Discover the repository that contains `start`, walking upwards like git does.
    pub fn discover(start: &Path) -> Result<Self> {
        if let Err(err) = std::fs::metadata(start) {
            return Err(TrailError::from_io(err, start));
        }

        let inner = match gix::discover(start) {
            Ok(repo) => repo,
            Err(err) => {
                let text = format!("{err:?}");
                if text.contains("PermissionDenied") {
                    return Err(TrailError::PermissionDenied(start.to_path_buf()));
                }
                return Err(TrailError::NotARepository(start.to_path_buf()));
            }
        };

        if inner.is_bare() || inner.workdir().is_none() {
            return Err(TrailError::BareRepository(inner.git_dir().to_path_buf()));
        }

        let worktree = WorktreeInfo::from_repo(&inner)?;
        let head_ref = inner.head().map_err(|e| TrailError::Git(e.to_string()))?;
        if head_ref.is_unborn() {
            return Err(TrailError::EmptyRepository);
        }
        let head_id = head_ref.id().ok_or(TrailError::EmptyRepository)?.detach();
        let head = if head_ref.is_detached() {
            HeadState::Detached {
                short_id: short_id(&inner, &head_id),
            }
        } else {
            let name = head_ref
                .referent_name()
                .map(|n| n.shorten().to_str_lossy().into_owned())
                .unwrap_or_else(|| "HEAD".into());
            HeadState::Branch { name }
        };

        let name = repo_name(&worktree.common_dir);
        let is_shallow = inner.is_shallow();

        Ok(Repo {
            inner,
            name,
            worktree,
            head,
            head_id,
            is_shallow,
        })
    }

    pub fn gix(&self) -> &gix::Repository {
        &self.inner
    }

    pub fn workdir(&self) -> &Path {
        &self.worktree.root
    }

    /// Resolve the base branch.
    ///
    /// Priority: explicit `--base`, then `main`, `master`, then the remote
    /// default branch recorded in `refs/remotes/origin/HEAD`.
    pub fn resolve_base(&self, explicit: Option<&str>) -> Result<BaseRef> {
        let (name, id) = match explicit {
            Some(name) => self
                .lookup_branch(name)?
                .ok_or_else(|| TrailError::BaseBranchNotFound(name.to_string()))?,
            None => self.detect_default_base()?,
        };

        let merge_base = match self.inner.merge_base(self.head_id, id) {
            Ok(id) => id.detach(),
            Err(_) => {
                return Err(TrailError::NoMergeBase {
                    base: name,
                    shallow: self.is_shallow,
                })
            }
        };

        Ok(BaseRef {
            name,
            id,
            merge_base,
            explicit: explicit.is_some(),
        })
    }

    fn detect_default_base(&self) -> Result<(String, gix::ObjectId)> {
        for candidate in ["main", "master"] {
            if let Some(found) = self.lookup_branch(candidate)? {
                return Ok(found);
            }
        }
        // Remote default branch: refs/remotes/origin/HEAD -> refs/remotes/origin/<name>
        if let Some(reference) = self.try_find("refs/remotes/origin/HEAD")? {
            if let gix::refs::TargetRef::Symbolic(target) = reference.target() {
                let short = target.shorten().to_str_lossy().into_owned();
                if let Some(found) = self.lookup_branch(&short)? {
                    return Ok(found);
                }
            }
        }
        Err(TrailError::NoBaseBranch)
    }

    /// Look up `name` as a local branch, then as `origin/<name>`, then as any ref.
    fn lookup_branch(&self, name: &str) -> Result<Option<(String, gix::ObjectId)>> {
        let candidates = [
            (format!("refs/heads/{name}"), name.to_string()),
            (format!("refs/remotes/{name}"), name.to_string()),
            (
                format!("refs/remotes/origin/{name}"),
                format!("origin/{name}"),
            ),
            (name.to_string(), name.to_string()),
        ];
        for (full, label) in candidates {
            if let Some(reference) = self.try_find(&full)? {
                let id = reference
                    .into_fully_peeled_id()
                    .map_err(|e| TrailError::Git(e.to_string()))?
                    .detach();
                return Ok(Some((label, id)));
            }
        }
        Ok(None)
    }

    fn try_find(&self, full_name: &str) -> Result<Option<gix::Reference<'_>>> {
        self.inner
            .try_find_reference(full_name)
            .map_err(|e| TrailError::Git(e.to_string()))
    }

    pub fn short_id(&self, id: &gix::ObjectId) -> String {
        short_id(&self.inner, id)
    }

    /// Turn a user supplied path into a repository-relative, slash separated path.
    pub fn relative_path(&self, cwd: &Path, file: &Path) -> Result<PathBuf> {
        let absolute = if file.is_absolute() {
            file.to_path_buf()
        } else {
            cwd.join(file)
        };
        let absolute = normalize_path(&absolute);
        let root = normalize_path(self.workdir());
        match absolute.strip_prefix(&root) {
            Ok(rel) => Ok(rel.to_path_buf()),
            // Outside the worktree as given: assume the user typed a path
            // relative to the repository root.
            Err(_) => Ok(normalize_path(file)),
        }
    }
}

fn short_id(repo: &gix::Repository, id: &gix::ObjectId) -> String {
    use gix::prelude::ObjectIdExt;
    id.attach(repo)
        .shorten()
        .map(|p| p.to_string())
        .unwrap_or_else(|_| id.to_hex_with_len(7).to_string())
}

fn repo_name(common_dir: &Path) -> String {
    // `<repo>/.git` -> "<repo>", `<repo>.git` (bare style) -> "<repo>"
    let dir = if common_dir.file_name().map(|n| n == ".git").unwrap_or(false) {
        common_dir.parent().unwrap_or(common_dir)
    } else {
        common_dir
    };
    dir.file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
        .unwrap_or_else(|| "repository".into())
}
