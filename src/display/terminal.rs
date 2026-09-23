//! Plain text rendering. No tables wider than a path, no colour unless stdout
//! is a terminal, so output pipes cleanly into other tools.

use std::fmt::Write as _;
use std::io::IsTerminal;

use chrono::{DateTime, Local, NaiveDate, Utc};

use super::plural;
use crate::git::baseline::{Baseline, BaselineKind};
use crate::git::diff::{FileStat, LineStats};
use crate::git::repository::HeadState;
use crate::git::worktree::WorktreeKind;
use crate::recorder::checkpoint::ChangeKind;
use crate::review::{
    CommitReview, FileDiffReport, FileRow, ReviewCheckpoint, ReviewSection, WorkingTreeReview,
    WorktreeReview,
};
use crate::trail::event::{Attachment, Confidence, EventSource, TrailEvent, TrailEventType};
use crate::trail::worktrees::WorktreesReport;
use crate::trail::{
    ChangesReport, DiffReport, InspectReport, RepositoryContext, SessionsReport, StatusReport,
    Trail,
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

fn file_stat_text(stat: &FileStat) -> String {
    if stat.is_binary() {
        return "binary".to_string();
    }
    LineStats::from_counts(stat.additions, stat.deletions).to_string()
}

/// `old -> new` for renames, the path alone otherwise.
fn path_text(path: &std::path::Path, from: Option<&std::path::Path>) -> String {
    match from {
        Some(from) => format!("{} -> {}", from.display(), path.display()),
        None => path.display().to_string(),
    }
}

fn head_line(ctx: &RepositoryContext, style: &Style, out: &mut String) {
    let _ = writeln!(out, "{}", style.bold("Repository"));
    let _ = writeln!(out, "  {}", ctx.name);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Worktree"));
    let _ = writeln!(out, "  {}", ctx.head.label());
    if ctx.worktree.kind == WorktreeKind::Linked {
        let _ = writeln!(
            out,
            "  {} {}",
            style.dim("linked worktree"),
            ctx.worktree.root.display()
        );
    }
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
    if ctx.since.kind != BaselineKind::BaseBranch {
        let _ = writeln!(out);
        let _ = writeln!(out, "{}", style.bold("Since"));
        let _ = writeln!(out, "  {}", baseline_text(&ctx.since));
    }
    let _ = writeln!(out);
}

/// "last push to origin/feature (8fa19d2)" plus the effective start when the
/// baseline commit is not an ancestor of HEAD (rebase, amend).
fn baseline_text(b: &Baseline) -> String {
    if b.start == b.commit {
        format!("{} ({})", b.label, b.short_commit)
    } else {
        format!(
            "{} ({}, common ancestor {})",
            b.label, b.short_commit, b.short_start
        )
    }
}

/// "2 created, 1 modified": how many changes of each kind a checkpoint holds.
fn change_counts(changes: &[crate::recorder::checkpoint::FileChange]) -> String {
    const KINDS: [(ChangeKind, &str); 4] = [
        (ChangeKind::Created, "created"),
        (ChangeKind::Modified, "modified"),
        (ChangeKind::Deleted, "deleted"),
        (ChangeKind::Renamed, "renamed"),
    ];
    KINDS
        .iter()
        .map(|(kind, label)| (changes.iter().filter(|c| c.kind == *kind).count(), label))
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect::<Vec<_>>()
        .join(", ")
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
            let n = event.files.len();
            let files = format!(" ({n} file{})", plural(n));
            format!("{kind} {short_id} {summary}{}", style.dim(&files))
        }
        TrailEventType::Checkpoint {
            id,
            title,
            bulk,
            changes,
            ..
        } => {
            let what = match title {
                Some(t) => format!("\"{t}\""),
                None => change_counts(changes),
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
            tags.push(stats.to_string());
        }
        match event.staged {
            Some(true) => tags.push("staged".into()),
            Some(false) => tags.push("unstaged".into()),
            None => {
                if matches!(event.event_type, TrailEventType::FileAdded)
                    && event.source == EventSource::Filesystem
                {
                    tags.push("untracked".into());
                }
            }
        }
        if event.confidence == Confidence::Inferred {
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
                            let _ = writeln!(
                                out,
                                "         {} {}",
                                c.kind.mark(),
                                path_text(&c.path, c.from_path.as_deref())
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Worktree-centric counts shown above the timeline.
fn overview_block(trail: &Trail, style: &Style, out: &mut String) {
    let s = &trail.summary;
    let _ = writeln!(out, "{}", style.bold("Commits"));
    let _ = writeln!(out, "  {}", s.commits);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Working tree"));
    let _ = writeln!(
        out,
        "  {} checkpoint{}",
        s.uncommitted_checkpoints,
        plural(s.uncommitted_checkpoints)
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Development history"));
    let _ = writeln!(
        out,
        "  {} checkpoint{}",
        s.checkpoints,
        plural(s.checkpoints)
    );
    let _ = writeln!(out);
}

fn summary_block(trail: &Trail, out: &mut String) {
    let _ = writeln!(out, "{RULE}");
    let s = &trail.summary;
    let _ = writeln!(out, "{} change{}", s.changes, plural(s.changes));
    let _ = writeln!(
        out,
        "{} file{} changed",
        s.files_changed,
        plural(s.files_changed)
    );
    let _ = writeln!(
        out,
        "{}",
        LineStats {
            additions: s.additions,
            deletions: s.deletions
        }
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
    overview_block(trail, &style, &mut out);
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
    let _ = writeln!(out, "{}", report.stats);
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
            let _ = writeln!(out, "{}", path_text(&file.path, file.old_path.as_deref()));
            let _ = writeln!(out, "  {}", file_stat_text(file));
            let _ = writeln!(out);
        }
    }
    let _ = writeln!(out, "{SHORT_RULE}");
    let _ = writeln!(
        out,
        "{} file{} changed, {}",
        report.files_changed,
        plural(report.files_changed),
        report.stats
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
            "  {} checkpoint{}{}",
            s.checkpoints,
            plural(s.checkpoints),
            if s.snapshots { "" } else { "  (no snapshots)" }
        );
        let _ = writeln!(out);
    }
    out
}

pub fn render_changes(report: &ChangesReport) -> String {
    let style = Style::detect();
    let ctx = &report.repository;
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!("Changes since {}", baseline_text(&ctx.since)))
    );
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!("{} on {}", ctx.name, ctx.head.label()))
    );
    let _ = writeln!(out, "{SHORT_RULE}");
    let _ = writeln!(out);

    let _ = writeln!(out, "{}", style.bold("Commits"));
    if report.commits.is_empty() {
        let _ = writeln!(out, "  {}", style.dim("none"));
    }
    for c in &report.commits {
        let _ = writeln!(out, "  {} {}", c.short_id, c.summary);
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "{}", style.bold("Files"));
    if report.files.is_empty() {
        let _ = writeln!(out, "  {}", style.dim("none"));
    }
    for f in &report.files {
        let mark = f.kind.mark();
        let state = match &f.status {
            Some(s) if s.untracked => "untracked",
            Some(s) if s.staged.is_some() && s.unstaged.is_some() => "staged, unstaged",
            Some(s) if s.staged.is_some() => "staged",
            Some(_) => "unstaged",
            None => "committed",
        };
        let name = path_text(&f.stat.path, f.stat.old_path.as_deref());
        let _ = writeln!(out, "  {mark} {name}");
        let _ = writeln!(
            out,
            "      {}  {}",
            file_stat_text(&f.stat),
            style.dim(state)
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{SHORT_RULE}");
    let c = &report.counts;
    let _ = writeln!(
        out,
        "{} commit{}, {} checkpoint{}",
        report.commits.len(),
        plural(report.commits.len()),
        report.checkpoints,
        plural(report.checkpoints)
    );
    let _ = writeln!(
        out,
        "{} file{} changed, {}  {}",
        report.files.len(),
        plural(report.files.len()),
        report.stats,
        style.dim(&format!(
            "(staged {}, unstaged {}, untracked {})",
            c.staged, c.unstaged, c.untracked
        ))
    );
    out
}

fn checkpoint_meta(cp: &ReviewCheckpoint, style: &Style, out: &mut String) {
    let _ = writeln!(
        out,
        "      {} - {}  {}",
        local(&cp.started_at).format("%H:%M"),
        local(&cp.ended_at).format("%H:%M"),
        style.dim(&cp.id)
    );
    let _ = writeln!(
        out,
        "      {} file{}  {}{}",
        cp.files.len(),
        plural(cp.files.len()),
        cp.stats,
        if cp.bulk { " (bulk)" } else { "" }
    );
    if let Some(note) = &cp.annotation {
        let _ = writeln!(out, "      {}", style.dim(note));
    }
}

fn file_row_line(row: &FileRow, style: &Style) -> String {
    match row.state {
        Some(state) => format!(
            "{} {}  {}  {}",
            row.mark,
            row.name,
            row.stat,
            style.dim(state)
        ),
        None => format!("{} {}  {}", row.mark, row.name, row.stat),
    }
}

fn section_heading(section: &ReviewSection, style: &Style) -> String {
    format!("[{}] {}", section.number(), style.bold(&section.heading()))
}

/// "2 checkpoints  5 files  +184 -31", plus the working tree state.
fn section_meta(section: &ReviewSection, style: &Style, out: &mut String) {
    let cps = section.checkpoints().len();
    let files = section.file_rows().len();
    match section {
        ReviewSection::Commit(c) => {
            let _ = writeln!(
                out,
                "    {}",
                style.dim(&format!(
                    "{}  {}{}",
                    c.author,
                    local(&c.time).format("%Y-%m-%d %H:%M"),
                    if c.is_merge {
                        "  merge (diff vs first parent)"
                    } else {
                        ""
                    }
                ))
            );
            let _ = writeln!(
                out,
                "    {} checkpoint{}  {} file{}  {}",
                cps,
                plural(cps),
                files,
                plural(files),
                c.stats
            );
        }
        ReviewSection::WorkingTree(w) => {
            if !w.available {
                let _ = writeln!(
                    out,
                    "    {}",
                    style.dim("worktree removed: no working tree state")
                );
            }
            let _ = writeln!(
                out,
                "    {} checkpoint{}  {} file{}  {}  {}",
                cps,
                plural(cps),
                files,
                plural(files),
                w.stats,
                style.dim(&format!(
                    "(staged {}, unstaged {}, untracked {})",
                    w.staged, w.unstaged, w.untracked
                ))
            );
        }
    }
}

fn checkpoint_line(cp: &ReviewCheckpoint, style: &Style) -> String {
    let mut line = format!(
        "[{}] {}  {}",
        cp.label,
        cp.display_title(),
        style.dim(&cp.stats.to_string())
    );
    if cp.attachment == Some(Attachment::Inferred) {
        line.push_str(&style.dim("  [inferred]"));
    }
    line
}

pub fn render_review(review: &WorktreeReview) -> String {
    let style = Style::detect();
    let ctx = &review.repository;
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Review"));
    let worktree = if review.worktree.exists {
        ctx.head.label()
    } else {
        format!("{}  {}", ctx.head.label(), style.dim("(worktree removed)"))
    };
    let _ = writeln!(out, "{worktree}");
    if !review.worktree.current {
        let _ = writeln!(
            out,
            "{}",
            style.dim(&format!(
                "worktree {}  {}",
                review.worktree.id,
                review.worktree.path.display()
            ))
        );
    }
    let _ = writeln!(out, "since {}", baseline_text(&ctx.since));
    let _ = writeln!(out);
    let _ = writeln!(out, "{RULE}");
    let _ = writeln!(out);
    for section in &review.sections {
        let _ = writeln!(out, "{}", section_heading(section, &style));
        section_meta(section, &style, &mut out);
        let _ = writeln!(out);
        let cps = section.checkpoints();
        if cps.is_empty() {
            let _ = writeln!(out, "    {}", style.dim("no recorded checkpoints"));
        }
        for cp in cps {
            let _ = writeln!(out, "    {}", checkpoint_line(cp, &style));
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "{RULE}");
    let s = &review.summary;
    let _ = writeln!(
        out,
        "{} commit{}, {} checkpoint{} ({} uncommitted), {} file{}, {}",
        s.commits,
        plural(s.commits),
        s.checkpoints,
        plural(s.checkpoints),
        s.uncommitted_checkpoints,
        s.files_changed,
        plural(s.files_changed),
        s.stats
    );
    let _ = writeln!(
        out,
        "{}",
        style.dim("trail review <n>  |  trail review <n>.<m>  |  trail review <sel> <file> [--open]  |  trail review --commit <rev>")
    );
    out
}

/// `trail review <n>` for a commit, and `trail review --commit <rev>`.
pub fn render_commit(c: &CommitReview) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Commit"));
    let _ = writeln!(out, "[{}] {} {}", c.number, c.short_id, c.summary);
    let _ = writeln!(
        out,
        "    {}",
        style.dim(&format!(
            "{}  {}{}",
            c.author,
            local(&c.time).format("%Y-%m-%d %H:%M"),
            if c.is_merge {
                "  merge (diff vs first parent)"
            } else {
                ""
            }
        ))
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Development path"));
    if c.checkpoints.is_empty() {
        let _ = writeln!(out, "    {}", style.dim("no recorded checkpoints"));
    }
    for cp in &c.checkpoints {
        let _ = writeln!(out, "    {}", checkpoint_line(cp, &style));
        checkpoint_meta(cp, &style, &mut out);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Final commit diff"));
    let _ = writeln!(
        out,
        "    {} file{}  {}",
        c.files.len(),
        plural(c.files.len()),
        c.stats
    );
    for f in &c.files {
        let _ = writeln!(out, "    {}", file_row_line(&f.row(), &style));
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "trail review {} <file>  shows the commit diff of a file;  trail review {}.<m> <file>  a checkpoint's",
            c.number, c.number
        ))
    );
    out
}

/// `trail review <n>` for the working tree.
pub fn render_working_tree(w: &WorkingTreeReview) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Working tree"));
    let _ = writeln!(out, "[{}]", w.number);
    if !w.available {
        let _ = writeln!(
            out,
            "    {}",
            style.dim("worktree removed: no working tree state")
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Development path"));
    if w.checkpoints.is_empty() {
        let _ = writeln!(out, "    {}", style.dim("no recorded checkpoints"));
    }
    for cp in &w.checkpoints {
        let _ = writeln!(out, "    {}", checkpoint_line(cp, &style));
        checkpoint_meta(cp, &style, &mut out);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Uncommitted changes"));
    let _ = writeln!(
        out,
        "    {} file{}  {}  {}",
        w.files.len(),
        plural(w.files.len()),
        w.stats,
        style.dim(&format!(
            "(staged {}, unstaged {}, untracked {})",
            w.staged, w.unstaged, w.untracked
        ))
    );
    for f in &w.files {
        let _ = writeln!(out, "    {}", file_row_line(&f.row(), &style));
    }
    out
}

pub fn render_section(section: &ReviewSection) -> String {
    match section {
        ReviewSection::Commit(c) => render_commit(c),
        ReviewSection::WorkingTree(w) => render_working_tree(w),
    }
}

pub fn render_review_checkpoint(cp: &ReviewCheckpoint) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", checkpoint_line(cp, &style));
    checkpoint_meta(cp, &style, &mut out);
    let _ = writeln!(out);
    for f in &cp.files {
        let _ = writeln!(out, "    {}", file_row_line(&f.row(), &style));
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "trail review {} <file>  shows what this checkpoint changed in a file",
            cp.label
        ))
    );
    out
}

pub fn render_review_file(report: &FileDiffReport) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "{}  {}",
            report.heading,
            file_row_line(&report.file, &Style { enabled: false })
        ))
    );
    if !report.available {
        let _ = writeln!(out, "snapshot unavailable for this change (recorded before snapshot support, larger than the cap, pruned, or the worktree was removed)");
        return out;
    }
    out.push_str(&report.diff);
    out
}

pub fn render_worktrees(report: &WorktreesReport) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold("Worktrees"));
    let _ = writeln!(out, "{}", style.dim(&report.repository));
    let _ = writeln!(out);
    for w in &report.worktrees {
        let mut tags = Vec::new();
        if w.current {
            tags.push("current");
        }
        if !w.exists {
            tags.push("removed");
        }
        let tag = if tags.is_empty() {
            String::new()
        } else {
            style.dim(&format!("  ({})", tags.join(", ")))
        };
        let _ = writeln!(out, "{}{tag}", style.bold(&w.id));
        let _ = writeln!(out, "  path: {}", w.path.display());
        let _ = writeln!(
            out,
            "  branch: {}",
            w.branch.as_deref().unwrap_or("(detached)")
        );
        match w.commits {
            Some(n) => {
                let _ = writeln!(out, "  {} commit{}", n, plural(n));
            }
            None if w.head.is_none() => {
                let _ = writeln!(out, "  {}", style.dim("last commit unknown"));
            }
            None => {}
        }
        let _ = writeln!(
            out,
            "  {} checkpoint{}",
            w.checkpoints,
            plural(w.checkpoints)
        );
        let _ = writeln!(out);
    }
    out
}
