//! Plain text rendering. No tables wider than a path, no colour unless stdout
//! is a terminal, so output pipes cleanly into other tools.

use std::fmt::Write as _;
use std::io::IsTerminal;

use chrono::{DateTime, Local, NaiveDate, Utc};

use crate::git::diff::{FileStat, LineStats};
use crate::git::repository::HeadState;
use crate::git::worktree::WorktreeKind;
use crate::recorder::checkpoint::ChangeKind;
use crate::trail::event::{TrailEvent, TrailEventType};
use crate::trail::{
    DiffReport, InspectReport, RepositoryContext, SessionsReport, StatusReport, Trail,
};

const RULE: &str = "────────────────────────────────────";
const SHORT_RULE: &str = "────────────────────";

struct Style {
    enabled: bool,
}

impl Style {
    fn detect() -> Self {
        let enabled = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        Style { enabled }
    }
    fn bold(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn local(ts: &DateTime<Utc>) -> DateTime<Local> {
    ts.with_timezone(&Local)
}

fn plus_minus(stats: &LineStats) -> String {
    match (stats.additions, stats.deletions) {
        (0, 0) => "±0".to_string(),
        (a, 0) => format!("+{a}"),
        (0, d) => format!("-{d}"),
        (a, d) => format!("+{a} -{d}"),
    }
}

fn file_stat_text(stat: &FileStat) -> String {
    if stat.is_binary() {
        return "binary".to_string();
    }
    plus_minus(&LineStats {
        additions: stat.additions.unwrap_or(0),
        deletions: stat.deletions.unwrap_or(0),
    })
}

fn head_line(ctx: &RepositoryContext, style: &Style, out: &mut String) {
    let _ = writeln!(out, "{}", style.bold("Repository"));
    let _ = writeln!(out, "  {}", ctx.name);
    if ctx.worktree.kind == WorktreeKind::Linked {
        let _ = writeln!(
            out,
            "  {} {}",
            style.dim("linked worktree"),
            ctx.worktree.root.display()
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Branch"));
    let _ = writeln!(out, "  {}", ctx.head.label());
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Base"));
    let _ = writeln!(out, "  {}", ctx.base);
    if ctx.shallow {
        let _ = writeln!(
            out,
            "  {}",
            style.dim("(shallow clone: history may be incomplete)")
        );
    }
    let _ = writeln!(out);
}

fn event_line(event: &TrailEvent, detailed: bool, style: &Style) -> String {
    let time = match &event.timestamp {
        Some(ts) => local(ts).format("%H:%M").to_string(),
        None => "--:--".to_string(),
    };
    let body = match &event.event_type {
        TrailEventType::FileAdded => format!("Added {}", event.files[0].display()),
        TrailEventType::FileModified => format!("Modified {}", event.files[0].display()),
        TrailEventType::FileDeleted => format!("Deleted {}", event.files[0].display()),
        TrailEventType::FileRenamed { from } => {
            format!("Renamed {} -> {}", from.display(), event.files[0].display())
        }
        TrailEventType::Commit {
            short_id,
            summary,
            is_merge,
            ..
        } => {
            let kind = if *is_merge { "Merged" } else { "Committed" };
            let files = match event.files.len() {
                1 => " (1 file)".to_string(),
                n => format!(" ({n} files)"),
            };
            format!("{kind} {short_id} {summary}{}", style.dim(&files))
        }
        TrailEventType::Checkpoint {
            id,
            title,
            bulk,
            changes,
            ..
        } => {
            let mut counts = [0usize; 4];
            for c in changes {
                counts[match c.kind {
                    ChangeKind::Created => 0,
                    ChangeKind::Modified => 1,
                    ChangeKind::Deleted => 2,
                    ChangeKind::Renamed => 3,
                }] += 1;
            }
            let mut parts = Vec::new();
            for (n, label) in counts
                .iter()
                .zip(["created", "modified", "deleted", "renamed"])
            {
                if *n > 0 {
                    parts.push(format!("{n} {label}"));
                }
            }
            let what = match title {
                Some(t) => format!("\"{t}\""),
                None => parts.join(", "),
            };
            let bulk_tag = if *bulk {
                style.dim(&format!(" (bulk, {} files)", changes.len()))
            } else {
                String::new()
            };
            format!("Session {}  {what}{bulk_tag}", style.dim(id))
        }
        TrailEventType::RefUpdate { action, message } => {
            let text = if message.is_empty() {
                action.clone()
            } else {
                format!("{action}: {message}")
            };
            style.dim(&format!("HEAD {text}"))
        }
    };
    let mut line = format!("{time}  {body}");
    if detailed {
        let mut tags: Vec<String> = Vec::new();
        if let Some(stats) = &event.stats {
            tags.push(plus_minus(stats));
        }
        match event.staged {
            Some(true) => tags.push("staged".into()),
            Some(false) => tags.push("unstaged".into()),
            None => {
                if matches!(event.event_type, TrailEventType::FileAdded)
                    && event.source == crate::trail::event::EventSource::Filesystem
                {
                    tags.push("untracked".into());
                }
            }
        }
        if event.confidence == crate::trail::event::Confidence::Inferred {
            tags.push("time from mtime".into());
        }
        if !tags.is_empty() {
            let _ = write!(line, "  {}", style.dim(&format!("[{}]", tags.join(", "))));
        }
    }
    line
}

fn events_block(events: &[TrailEvent], detailed: bool, style: &Style, out: &mut String) {
    if events.is_empty() {
        let _ = writeln!(out, "  {}", style.dim("no changes since base"));
        return;
    }
    let mut current_day: Option<NaiveDate> = None;
    let mut printed_undated = false;
    for event in events {
        if let TrailEventType::Checkpoint { hidden: true, .. } = event.event_type {
            continue;
        }
        match &event.timestamp {
            Some(ts) => {
                let day = local(ts).date_naive();
                if current_day != Some(day) {
                    if current_day.is_some() {
                        let _ = writeln!(out);
                    }
                    let _ = writeln!(out, "{}", style.dim(&day.format("%Y-%m-%d").to_string()));
                    current_day = Some(day);
                }
            }
            None => {
                if !printed_undated {
                    if current_day.is_some() {
                        let _ = writeln!(out);
                    }
                    let _ = writeln!(out, "{}", style.dim("undated"));
                    printed_undated = true;
                }
            }
        }
        let _ = writeln!(out, "{}", event_line(event, detailed, style));
        if detailed {
            match &event.event_type {
                TrailEventType::Commit { .. } => {
                    for file in &event.files {
                        let _ = writeln!(out, "         {}", file.display());
                    }
                }
                TrailEventType::Checkpoint {
                    changes,
                    bulk,
                    annotation,
                    ..
                } => {
                    if let Some(note) = annotation {
                        let _ = writeln!(out, "         {}", style.dim(note));
                    }
                    if *bulk {
                        let _ = writeln!(
                            out,
                            "         {}",
                            style.dim(&format!(
                                "{} files changed (bulk, collapsed)",
                                changes.len()
                            ))
                        );
                    } else {
                        for c in changes {
                            let mark = match c.kind {
                                ChangeKind::Created => "+",
                                ChangeKind::Modified => "~",
                                ChangeKind::Deleted => "-",
                                ChangeKind::Renamed => ">",
                            };
                            match &c.from_path {
                                Some(from) => {
                                    let _ = writeln!(
                                        out,
                                        "         {mark} {} -> {}",
                                        from.display(),
                                        c.path.display()
                                    );
                                }
                                None => {
                                    let _ = writeln!(out, "         {mark} {}", c.path.display());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn summary_block(trail: &Trail, out: &mut String) {
    let _ = writeln!(out, "{RULE}");
    let s = &trail.summary;
    let _ = writeln!(
        out,
        "{} change{}",
        s.changes,
        if s.changes == 1 { "" } else { "s" }
    );
    let _ = writeln!(
        out,
        "{} file{} changed",
        s.files_changed,
        if s.files_changed == 1 { "" } else { "s" }
    );
    let _ = writeln!(
        out,
        "{}",
        plus_minus(&LineStats {
            additions: s.additions,
            deletions: s.deletions
        })
    );
}

pub fn render_trail(trail: &Trail) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Development Trail"));
    let _ = writeln!(out, "{RULE}");
    let _ = writeln!(out);
    head_line(&trail.repository, &style, &mut out);
    events_block(&trail.events, false, &style, &mut out);
    let _ = writeln!(out);
    summary_block(trail, &mut out);
    out
}

pub fn render_history(trail: &Trail) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Development Trail (detailed)"));
    let _ = writeln!(out, "{RULE}");
    let _ = writeln!(out);
    head_line(&trail.repository, &style, &mut out);
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "since merge base {}  (commits, HEAD reflog, working tree)",
            trail.repository.merge_base
        ))
    );
    let _ = writeln!(out);
    events_block(&trail.events, true, &style, &mut out);
    let _ = writeln!(out);
    summary_block(trail, &mut out);
    out
}

pub fn render_status(report: &StatusReport) -> String {
    let style = Style::detect();
    let ctx = &report.repository;
    let c = &report.counts;
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "Branch: {}", ctx.head.label());
    let _ = writeln!(out, "Base: {}", ctx.base);
    if ctx.worktree.kind == WorktreeKind::Linked {
        let _ = writeln!(out, "Worktree: {} (linked)", ctx.worktree.root.display());
    }
    if let HeadState::Branch { .. } = ctx.head {
        let _ = writeln!(out, "Commits ahead of base: {}", report.commits_ahead);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Modified: {}", c.modified);
    let _ = writeln!(out, "Added: {}", c.added);
    let _ = writeln!(out, "Deleted: {}", c.deleted);
    if c.renamed > 0 {
        let _ = writeln!(out, "Renamed: {}", c.renamed);
    }
    if c.type_changed > 0 {
        let _ = writeln!(out, "Type changed: {}", c.type_changed);
    }
    if c.conflicted > 0 {
        let _ = writeln!(out, "Conflicted: {}", c.conflicted);
    }
    let _ = writeln!(out, "Untracked: {}", c.untracked);
    let _ = writeln!(out);
    let _ = writeln!(out, "Staged: {}", c.staged);
    let _ = writeln!(out, "Unstaged: {}", c.unstaged);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", plus_minus(&report.stats));
    let _ = style; // reserved for future colouring
    out
}

pub fn render_diff(report: &DiffReport) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "{} vs {} (merge base {})",
            report.repository.head.label(),
            report.repository.base,
            report.repository.merge_base
        ))
    );
    let _ = writeln!(out);
    if report.groups.is_empty() {
        let _ = writeln!(out, "no differences from {}", report.repository.base);
        return out;
    }
    for group in &report.groups {
        let _ = writeln!(out, "{}", style.bold(&group.directory));
        let _ = writeln!(out, "{SHORT_RULE}");
        let _ = writeln!(out);
        for file in &group.files {
            match &file.old_path {
                Some(old) => {
                    let _ = writeln!(out, "{} -> {}", old.display(), file.path.display());
                }
                None => {
                    let _ = writeln!(out, "{}", file.path.display());
                }
            }
            let _ = writeln!(out, "  {}", file_stat_text(file));
            let _ = writeln!(out);
        }
    }
    let _ = writeln!(out, "{SHORT_RULE}");
    let _ = writeln!(
        out,
        "{} file{} changed, {}",
        report.files_changed,
        if report.files_changed == 1 { "" } else { "s" },
        plus_minus(&report.stats)
    );
    out
}

pub fn render_inspect(report: &InspectReport) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold(&report.path.display().to_string()));
    let _ = writeln!(out);

    let _ = writeln!(out, "{}", style.bold("Status"));
    let status_text = match &report.status {
        Some(entry) if entry.untracked => "Untracked".to_string(),
        Some(entry) => {
            let mut parts = Vec::new();
            if let Some(k) = entry.staged {
                parts.push(format!("{} (staged)", k.label()));
            }
            if let Some(k) = entry.unstaged {
                parts.push(format!("{} (unstaged)", k.label()));
            }
            if let Some(old) = &entry.orig_path {
                parts.push(format!("from {}", old.display()));
            }
            parts.join(", ")
        }
        None if !report.exists => "Not in working tree".to_string(),
        None => "Unchanged".to_string(),
    };
    let _ = writeln!(out, "  {status_text}");
    if let Some(ts) = &report.last_modified {
        let _ = writeln!(
            out,
            "  {}",
            style.dim(&format!(
                "last modified {}",
                local(ts).format("%Y-%m-%d %H:%M")
            ))
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "{}", style.bold("Diff"));
    match &report.worktree_stat {
        Some(stat) => {
            let _ = writeln!(out, "  {}  {}", file_stat_text(stat), style.dim("vs HEAD"));
        }
        None => {
            let _ = writeln!(out, "  {}", style.dim("no uncommitted changes"));
        }
    }
    match &report.base_stat {
        Some(stat) => {
            let _ = writeln!(
                out,
                "  {}  {}",
                file_stat_text(stat),
                style.dim(&format!("vs {}", report.repository.base))
            );
        }
        None => {
            let _ = writeln!(
                out,
                "  {}",
                style.dim(&format!("no changes vs {}", report.repository.base))
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "{}", style.bold("Related commits"));
    if report.commits.is_empty() {
        let _ = writeln!(out, "  {}", style.dim("none"));
    }
    for c in &report.commits {
        let marker = if c.on_branch { "*" } else { " " };
        let _ = writeln!(out, "{marker} {} {}", c.commit.short_id, c.commit.summary);
    }
    if report.commits.iter().any(|c| c.on_branch) {
        let _ = writeln!(out, "  {}", style.dim("* on this branch, not on base"));
    }
    out
}

pub fn render_sessions(report: &SessionsReport) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Development Sessions"));
    let _ = writeln!(out);
    if report.sessions.is_empty() {
        let _ = writeln!(
            out,
            "  {}",
            style.dim("no recorded sessions (run `trail start`)")
        );
        return out;
    }
    for s in &report.sessions {
        let _ = writeln!(out, "{}", style.bold(&s.session_id));
        let branch = s.branch.clone().unwrap_or_else(|| "(detached)".into());
        let worktree = if s.worktree_exists {
            String::new()
        } else {
            style.dim("  (worktree removed)")
        };
        let _ = writeln!(out, "  {branch}{worktree}");
        let start = local(&s.started_at);
        let end = match s.ended_at {
            Some(e) => local(&e).format("%H:%M").to_string(),
            None => format!("{} (open)", local(&s.last_activity).format("%H:%M")),
        };
        let _ = writeln!(out, "  {} - {end}", start.format("%Y-%m-%d %H:%M"));
        let _ = writeln!(
            out,
            "  {} checkpoint{}",
            s.checkpoints,
            if s.checkpoints == 1 { "" } else { "s" }
        );
        let _ = writeln!(out);
    }
    out
}
