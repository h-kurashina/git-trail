//! Wires CLI commands to the domain layer and the renderer.
//!
//! Each command builds a serializable report, then hands it either to the
//! terminal renderer or to `serde_json`. This is what keeps `--json` cheap.

use std::io::Write;

use serde::Serialize;

use super::{Cli, Command};
use crate::display::terminal;
use crate::git::repository::Repo;
use crate::trail::builder;

pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let start = match &cli.path {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let repo = Repo::discover(&start)?;
    let base = repo.resolve_base(cli.base.as_deref())?;

    match cli.command {
        None => {
            let trail = builder::build_trail(&repo, &base, builder::Scope::Overview)?;
            emit(cli.json, &trail, terminal::render_trail)
        }
        Some(Command::Status) => {
            let report = builder::build_status(&repo, &base)?;
            emit(cli.json, &report, terminal::render_status)
        }
        Some(Command::Diff) => {
            let report = builder::build_diff(&repo, &base)?;
            emit(cli.json, &report, terminal::render_diff)
        }
        Some(Command::Inspect { file }) => {
            let report = builder::build_inspect(&repo, &base, &start, &file)?;
            emit(cli.json, &report, terminal::render_inspect)
        }
    }
}

fn emit<T: Serialize>(json: bool, value: &T, render: fn(&T) -> String) -> anyhow::Result<()> {
    let out = if json {
        serde_json::to_string_pretty(value)? + "\n"
    } else {
        render(value)
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // A closed pipe (e.g. `trail | head`) is not an error worth reporting.
    if let Err(err) = lock.write_all(out.as_bytes()) {
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(err.into());
        }
    }
    Ok(())
}
