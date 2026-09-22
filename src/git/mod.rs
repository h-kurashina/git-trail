//! Git access layer.
//!
//! Repository structure (discovery, refs, HEAD, commits, reflog, worktrees) is
//! read with `gix`. Working tree status and line statistics come from the
//! `git` CLI, because reproducing git's exact status/numstat semantics in pure
//! Rust is not worth the complexity for an MVP. Every CLI call lives in this
//! module so the boundary is easy to see and easy to replace later.

pub mod baseline;
pub mod diff;
pub mod history;
pub mod repository;
pub mod worktree;

use std::path::Path;
use std::process::Command;

use crate::error::{Result, TrailError};

/// Run `git <args>` inside `workdir` and return stdout as raw bytes.
///
/// Callers always pass `-z` style arguments so paths with unusual characters
/// survive. `GIT_OPTIONAL_LOCKS=0` keeps read-only queries from touching the
/// index on disk.
pub(crate) fn run_git(workdir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.quotepath=off")
        .args(args)
        .current_dir(workdir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                TrailError::GitNotInstalled
            } else {
                TrailError::Io(err)
            }
        })?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("Permission denied") || stderr.contains("permission denied") {
            return Err(TrailError::PermissionDenied(workdir.to_path_buf()));
        }
        Err(TrailError::GitCommand {
            command: args.first().copied().unwrap_or("").to_string(),
            stderr,
        })
    }
}

/// Split NUL separated output into owned strings (lossy UTF-8).
pub(crate) fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}
