//! Wires CLI commands to the domain layer and the renderer.
//!
//! Each command builds a serializable report, then hands it either to the
//! terminal renderer or to `serde_json`. This is what keeps `--json` cheap.

use std::path::PathBuf;

use serde::Serialize;

use super::{Cli, Command};
use crate::display::{terminal, write_stdout};
use crate::edit;
use crate::error::{Result, TrailError};
use crate::git::baseline::{Baseline, SinceSpec};
use crate::git::history;
use crate::git::repository::{BaseRef, Repo};
use crate::recorder;
use crate::review;
use crate::trail::builder::{self, Scope};
use crate::trail::worktrees;
use crate::tui;

pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let start = match &cli.path {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let repo = Repo::discover(&start)?;
    let json = cli.json;

    // Base branch and baseline are resolved lazily: recording, listing
    // sessions and opening the current file work without either.
    let window = |default: SinceSpec| -> Result<Window> {
        let base = repo.resolve_base(cli.base.as_deref())?;
        let since = match cli.since.as_deref() {
            None => default,
            value => SinceSpec::parse(value),
        };
        let baseline = repo.resolve_baseline(&since, &base)?;
        Ok(Window { base, baseline })
    };
    let overview = |w: &Window| builder::build_trail(&repo, &w.base, &w.baseline, Scope::Overview);

    match cli.command {
        Some(Command::Start { stop_after, quiet }) => Ok(recorder::run(
            &repo,
            recorder::Options {
                stop_after: stop_after.map(std::time::Duration::from_secs),
                quiet,
                base: cli.base.clone(),
            },
        )?),
        Some(Command::Sessions) => {
            let report = builder::build_sessions(&repo)?;
            emit(json, &report, terminal::render_sessions)
        }
        Some(Command::Open {
            file,
            at: None,
            print,
        }) => Ok(edit::open_file(&repo, &start, &file, print)?),
        Some(Command::Open {
            file,
            at: Some(checkpoint),
            print,
        }) => {
            let trail = overview(&window(SinceSpec::Base)?)?;
            Ok(edit::open_at(
                &repo,
                &trail,
                &start,
                &file,
                &checkpoint,
                print,
            )?)
        }
        Some(Command::Worktrees) => {
            let report = worktrees::build_worktrees(&repo, cli.base.as_deref())?;
            emit(json, &report, terminal::render_worktrees)
        }
        Some(Command::Review {
            selector,
            file,
            open,
            interactive,
            commit,
            worktree,
        }) => {
            // Another worktree: resolve everything against it instead.
            let target = match &worktree {
                Some(sel) => worktrees::resolve_worktree(&repo, sel)?,
                None => repo.clone_handle(),
            };
            let current = target.worktree_id() == repo.worktree_id();
            let repo = &target;
            let base = repo.resolve_base(cli.base.as_deref())?;
            let baseline = repo.resolve_baseline(&SinceSpec::parse(cli.since.as_deref()), &base)?;
            let trail = builder::build_trail(repo, &base, &baseline, Scope::Overview)?;
            let review = review::build(repo, &trail, current)?;
            if interactive {
                return Ok(tui::run(repo, &trail, review, &start)?);
            }
            if let Some(rev) = commit {
                // `--commit <rev> <file>`: the only positional is the file.
                let file = match (selector, file) {
                    (Some(sel), None) => Some(PathBuf::from(sel)),
                    (Some(_), Some(_)) => {
                        return Err(TrailError::InvalidSelection(
                            "--commit takes a file, not a section selector".into(),
                        )
                        .into())
                    }
                    (None, file) => file,
                };
                let id = repo.rev_parse(&rev)?;
                let section = match review::section_of_commit(&review, &id.to_string()) {
                    Some(c) => c.clone(),
                    None => {
                        // Outside the window: still a fact worth showing.
                        let info = history::commit_info(repo.gix(), id)?;
                        review::commit_review(repo, 0, &info, Vec::new())
                    }
                };
                let Some(file) = file else {
                    return emit(json, &section, terminal::render_commit);
                };
                let rel = repo.relative_path(&start, &file)?;
                let selected =
                    review::Selected::Section(&review::ReviewSection::Commit(section.clone()));
                if open {
                    return Ok(edit::open_selected(repo, &trail, &start, &file, selected)?);
                }
                let diff = review::file_diff(repo, &review, selected, &rel)?;
                return emit(json, &diff, terminal::render_review_file);
            }
            let Some(selector) = selector else {
                return emit(json, &review, terminal::render_review);
            };
            let selected = review::select(&review, &selector)?;
            let Some(file) = file else {
                return match selected {
                    review::Selected::Section(s) => emit(json, s, terminal::render_section),
                    review::Selected::Checkpoint(cp) => {
                        emit(json, cp, terminal::render_review_checkpoint)
                    }
                };
            };
            if open {
                return Ok(edit::open_selected(repo, &trail, &start, &file, selected)?);
            }
            let rel = repo.relative_path(&start, &file)?;
            let diff = review::file_diff(repo, &review, selected, &rel)?;
            emit(json, &diff, terminal::render_review_file)
        }
        // `trail changes` defaults to the last push; everything else to the base branch.
        Some(Command::Changes) => {
            let w = window(SinceSpec::Auto)?;
            let report = builder::build_changes(&repo, &w.base, &w.baseline)?;
            emit(json, &report, terminal::render_changes)
        }
        Some(Command::Edit { from, print }) => {
            let trail = overview(&window(SinceSpec::Base)?)?;
            Ok(edit::run(&repo, &trail, from.as_deref(), print)?)
        }
        None => {
            let trail = overview(&window(SinceSpec::Base)?)?;
            emit(json, &trail, terminal::render_trail)
        }
        Some(Command::Status) => {
            let w = window(SinceSpec::Base)?;
            let report = builder::build_status(&repo, &w.base)?;
            emit(json, &report, terminal::render_status)
        }
        Some(Command::History { limit }) => {
            let w = window(SinceSpec::Base)?;
            let mut trail = builder::build_trail(&repo, &w.base, &w.baseline, Scope::Detailed)?;
            if let Some(n) = limit {
                trail.truncate_to_latest(n);
            }
            emit(json, &trail, terminal::render_history)
        }
        Some(Command::Diff) => {
            let w = window(SinceSpec::Base)?;
            let report = builder::build_diff(&repo, &w.base)?;
            emit(json, &report, terminal::render_diff)
        }
        Some(Command::Inspect { file }) => {
            let w = window(SinceSpec::Base)?;
            let report = builder::build_inspect(&repo, &w.base, &start, &file)?;
            emit(json, &report, terminal::render_inspect)
        }
    }
}

/// The slice of history a command looks at: the base branch and the
/// baseline (`--since`) inside it.
struct Window {
    base: BaseRef,
    baseline: Baseline,
}

fn emit<T: Serialize>(json: bool, value: &T, render: fn(&T) -> String) -> anyhow::Result<()> {
    let out = if json {
        serde_json::to_string_pretty(value)? + "\n"
    } else {
        render(value)
    };
    Ok(write_stdout(out.as_bytes())?)
}
