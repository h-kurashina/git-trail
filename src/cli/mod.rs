//! Command line surface: argument parsing and dispatch.

mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Understand how your code changed, not just what changed.
#[derive(Debug, Parser)]
#[command(name = "trail", version, about, long_about = None)]
pub struct Cli {
    /// Base branch to compare against (default: main, master, then origin/HEAD)
    #[arg(long, global = true, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Emit machine readable JSON instead of text
    #[arg(long, global = true)]
    pub json: bool,

    /// Run as if trail was started in this directory
    #[arg(short = 'C', long = "path", global = true, value_name = "DIR")]
    pub path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show a compact summary of the current worktree
    Status,
    /// Show the development trail in detail (commits, reflog, working tree)
    History {
        /// Only show the most recent N events
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Show the diff against the base branch grouped by directory
    Diff,
    /// Show change information for a single file
    Inspect {
        /// File path, relative to the current directory or the repository root
        file: PathBuf,
    },
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    commands::dispatch(cli)
}
