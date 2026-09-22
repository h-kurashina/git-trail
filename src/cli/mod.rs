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

    /// Start the trail at a baseline: push, upstream, base, auto or a revision
    #[arg(long, global = true, value_name = "BASELINE")]
    pub since: Option<String>,

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
    /// Show what changed since the last push (or upstream, or base)
    Changes,
    /// Review the trail checkpoint by checkpoint, with per-checkpoint diffs
    Review {
        /// Checkpoint number (from `trail review`) or id
        checkpoint: Option<String>,
        /// File within the checkpoint: prints the diff this checkpoint made
        file: Option<PathBuf>,
        /// Open the file as it was at the end of the checkpoint in $VISUAL / $EDITOR
        #[arg(long)]
        open: bool,
        /// Browse checkpoints, files and diffs interactively
        #[arg(short = 'i', long)]
        interactive: bool,
    },
    /// Show the development trail in detail (commits, reflog, working tree)
    History {
        /// Only show the most recent N events
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Show the diff against the base branch grouped by directory
    Diff,
    /// Record changes in this worktree until Ctrl+C
    Start {
        /// Stop automatically after N seconds (useful for scripts)
        #[arg(long, value_name = "SECONDS")]
        stop_after: Option<u64>,
        /// Do not print events while recording
        #[arg(long)]
        quiet: bool,
    },
    /// List recorded development sessions (including removed worktrees)
    Sessions,
    /// Edit checkpoint titles, notes, grouping and order in $VISUAL / $EDITOR
    Edit {
        /// Apply an already edited trail file instead of opening an editor
        #[arg(long, value_name = "FILE")]
        from: Option<PathBuf>,
        /// Print the editable text to stdout and exit
        #[arg(long)]
        print: bool,
    },
    /// Open a file of the worktree in $VISUAL / $EDITOR
    Open {
        /// File path, relative to the current directory or the repository root
        file: PathBuf,
        /// Open the file as it was at a checkpoint (see `trail history` for ids)
        #[arg(long, value_name = "CHECKPOINT")]
        at: Option<String>,
        /// Write the content to stdout instead of opening an editor
        #[arg(long)]
        print: bool,
    },
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
