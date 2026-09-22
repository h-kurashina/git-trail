//! Wires CLI commands to the domain layer and the renderer.
//!
//! Each command builds a serializable report, then hands it either to the
//! terminal renderer or to `serde_json`. This is what keeps `--json` cheap.

use std::io::Write;

use serde::Serialize;

use super::{Cli, Command};
use crate::display::terminal;
use crate::edit;
use crate::git::baseline::SinceSpec;
use crate::git::repository::Repo;
use crate::recorder;
use crate::review;
use crate::trail::builder;

pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let start = match &cli.path {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let repo = Repo::discover(&start)?;

    // Recording does not need a base branch; everything else does.
    if let Some(Command::Start { stop_after, quiet }) = cli.command {
        return Ok(recorder::run(
            &repo,
            recorder::Options {
                stop_after: stop_after.map(std::time::Duration::from_secs),
                quiet,
                base: cli.base.clone(),
            },
        )?);
    }

    if let Some(Command::Sessions) = cli.command {
        let report = builder::build_sessions(&repo)?;
        return emit(cli.json, &report, terminal::render_sessions);
    }
    if let Some(Command::Open {
        file,
        at: None,
        print,
    }) = &cli.command
    {
        return Ok(edit::open_file(&repo, &start, file, *print)?);
    }

    let base = repo.resolve_base(cli.base.as_deref())?;
    // `trail changes` defaults to the last push; everything else to the base branch.
    let since = match (&cli.command, cli.since.as_deref()) {
        (Some(Command::Changes), None) => SinceSpec::Auto,
        (_, value) => SinceSpec::parse(value),
    };
    let baseline = repo.resolve_baseline(&since, &base)?;
    match cli.command {
        Some(Command::Start { .. })
        | Some(Command::Sessions)
        | Some(Command::Open { at: None, .. }) => {
            unreachable!("handled above")
        }
        Some(Command::Review {
            checkpoint,
            file,
            open,
        }) => {
            let trail = builder::build_trail(&repo, &base, &baseline, builder::Scope::Overview)?;
            let report = review::build(&repo, &trail);
            let Some(selector) = checkpoint else {
                return emit(cli.json, &report, terminal::render_review);
            };
            let selected = review::select(&report, &selector)?;
            let Some(file) = file else {
                return emit(cli.json, selected, terminal::render_review_checkpoint);
            };
            let rel = repo.relative_path(&start, &file)?;
            if open {
                return Ok(edit::open_at(
                    &repo,
                    &trail,
                    &start,
                    &file,
                    &selected.id,
                    false,
                )?);
            }
            let diff = review::file_diff(&repo, &report, selected, &rel)?;
            emit(cli.json, &diff, terminal::render_review_file)
        }
        Some(Command::Changes) => {
            let report = builder::build_changes(&repo, &base, &baseline)?;
            emit(cli.json, &report, terminal::render_changes)
        }
        Some(Command::Open {
            file,
            at: Some(checkpoint),
            print,
        }) => {
            let trail = builder::build_trail(&repo, &base, &baseline, builder::Scope::Overview)?;
            Ok(edit::open_at(
                &repo,
                &trail,
                &start,
                &file,
                &checkpoint,
                print,
            )?)
        }
        Some(Command::Edit { from, print }) => {
            let trail = builder::build_trail(&repo, &base, &baseline, builder::Scope::Overview)?;
            Ok(edit::run(&repo, &trail, from.as_deref(), print)?)
        }
        None => {
            let trail = builder::build_trail(&repo, &base, &baseline, builder::Scope::Overview)?;
            emit(cli.json, &trail, terminal::render_trail)
        }
        Some(Command::Status) => {
            let report = builder::build_status(&repo, &base)?;
            emit(cli.json, &report, terminal::render_status)
        }
        Some(Command::History { limit }) => {
            let mut trail =
                builder::build_trail(&repo, &base, &baseline, builder::Scope::Detailed)?;
            if let Some(n) = limit {
                trail.truncate_to_latest(n);
            }
            emit(cli.json, &trail, terminal::render_history)
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
