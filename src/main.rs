//! `trail` — understand how your code changed, not just what changed.
//!
//! A local-first CLI that reconstructs the development history of a Git
//! worktree from information that is already on disk (commits, reflog, index,
//! working tree and file metadata). No network, no telemetry.

mod cli;
mod display;
mod edit;
mod error;
mod git;
mod recorder;
mod review;
mod trail;
mod tui;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
