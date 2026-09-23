//! `trail review -i`: an interactive review browser.
//!
//! Three levels, one state machine: Development → Files → Diff. The
//! Development pane is a tree of sections (commits, then the working tree)
//! with their checkpoints; Files lists what the selected node changed; Diff
//! shows one file, or a whole node. Wide terminals show all three side by
//! side; narrow ones show the current level only. The state is the same in
//! both, only the layout differs.
//!
//! The browser owns no review logic: it displays a `WorktreeReview`, asks
//! the review module for diffs through a callback, and opens files through
//! the same code path as `trail review ... --open`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::display::plural;
use crate::error::{Result, TrailError};
use crate::git::repository::Repo;
use crate::review::{self, DiffTarget, FileRow, ReviewSection, Selected, WorktreeReview};
use crate::trail::Trail;

/// Terminals at least this wide get the three-pane layout.
pub const WIDE_LAYOUT_MIN_COLUMNS: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Development,
    Files,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Down,
    Up,
    Enter,
    Back,
    Diff,
    CommitDiff,
    Open,
    Quit,
}

/// A row of the Development tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    /// Index into `review.sections`.
    Section(usize),
    /// Section index, checkpoint index within it.
    Checkpoint(usize, usize),
}

impl Node {
    pub fn section(self) -> usize {
        match self {
            Node::Section(s) | Node::Checkpoint(s, _) => s,
        }
    }
}

/// What to open outside the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// The file as it was at the end of the checkpoint (snapshot).
    Snapshot {
        checkpoint_id: String,
        path: PathBuf,
    },
    /// The file as committed (blob of the commit tree).
    CommitFile { section: usize, path: PathBuf },
    /// The file in the working tree.
    WorktreeFile { path: PathBuf },
}

/// What the loop must do after a key was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// Leave the TUI, open the target, then come back.
    Open(OpenTarget),
}

pub struct App {
    pub review: WorktreeReview,
    pub level: Level,
    /// Cursor in the visible tree rows.
    pub cursor: usize,
    /// One flag per section.
    pub expanded: Vec<bool>,
    pub file: usize,
    /// Diff lines of what was last requested, loaded on demand.
    pub diff: Vec<String>,
    pub diff_title: String,
    /// True when the diff belongs to the selected file (Back returns to
    /// Files); false when it belongs to the whole node (Back returns to the
    /// tree).
    pub diff_of_file: bool,
    pub scroll: usize,
    pub status: Option<String>,
}

impl App {
    pub fn new(review: WorktreeReview) -> Self {
        let expanded = vec![true; review.sections.len()];
        App {
            review,
            level: Level::Development,
            cursor: 0,
            expanded,
            file: 0,
            diff: Vec::new(),
            diff_title: String::new(),
            diff_of_file: false,
            scroll: 0,
            status: None,
        }
    }

    /// Visible rows of the tree, in order.
    pub fn nodes(&self) -> Vec<Node> {
        let mut out = Vec::new();
        for (i, section) in self.review.sections.iter().enumerate() {
            out.push(Node::Section(i));
            if self.expanded[i] {
                for j in 0..section.checkpoints().len() {
                    out.push(Node::Checkpoint(i, j));
                }
            }
        }
        out
    }

    pub fn current_node(&self) -> Option<Node> {
        self.nodes().get(self.cursor).copied()
    }

    fn section(&self, i: usize) -> &ReviewSection {
        &self.review.sections[i]
    }

    /// Files of the current node.
    pub fn file_rows(&self) -> Vec<FileRow> {
        match self.current_node() {
            Some(Node::Section(i)) => self.section(i).file_rows(),
            Some(Node::Checkpoint(i, j)) => self.section(i).checkpoints()[j].file_rows(),
            None => Vec::new(),
        }
    }

    pub fn current_file(&self) -> Option<FileRow> {
        self.file_rows().into_iter().nth(self.file)
    }

    /// The diff the current node stands for as a whole.
    fn node_diff_target(&self) -> Option<(DiffTarget, String)> {
        match self.current_node()? {
            Node::Section(i) => match self.section(i) {
                ReviewSection::Commit(c) => Some((
                    DiffTarget::Commit { section: c.number },
                    format!("commit {} {}", c.short_id, c.summary),
                )),
                ReviewSection::WorkingTree(_) => {
                    Some((DiffTarget::WorkingTree, "working tree".to_string()))
                }
            },
            Node::Checkpoint(i, j) => {
                let cp = &self.section(i).checkpoints()[j];
                Some((
                    DiffTarget::Checkpoint {
                        label: cp.label.clone(),
                    },
                    format!("checkpoint {} {}", cp.label, cp.display_title()),
                ))
            }
        }
    }

    /// The diff of the selected file within the current node.
    fn file_diff_target(&self) -> Option<(DiffTarget, String)> {
        let row = self.current_file()?;
        let title = row.path.display().to_string();
        let target = match self.current_node()? {
            Node::Section(i) => match self.section(i) {
                ReviewSection::Commit(c) => DiffTarget::CommitFile {
                    section: c.number,
                    path: row.path,
                },
                ReviewSection::WorkingTree(_) => DiffTarget::WorkingTreeFile { path: row.path },
            },
            Node::Checkpoint(i, j) => DiffTarget::CheckpointFile {
                label: self.section(i).checkpoints()[j].label.clone(),
                path: row.path,
            },
        };
        Some((target, title))
    }

    /// The commit the current node belongs to, if any.
    fn commit_diff_target(&self) -> Option<(DiffTarget, String)> {
        let i = self.current_node()?.section();
        let c = self.section(i).as_commit()?;
        Some((
            DiffTarget::Commit { section: c.number },
            format!("commit {} {}", c.short_id, c.summary),
        ))
    }

    /// Advance the state machine. `load_diff` is only called when a diff is
    /// requested, so browsing stays cheap.
    pub fn handle(
        &mut self,
        key: Key,
        load_diff: &mut dyn FnMut(&DiffTarget) -> Result<String>,
    ) -> Effect {
        self.status = None;
        match (self.level, key) {
            (_, Key::Quit) => return Effect::Quit,
            (_, Key::CommitDiff) => match self.commit_diff_target() {
                Some((target, title)) => self.show_diff(target, title, false, load_diff),
                None => self.status = Some("the working tree has no commit diff yet".into()),
            },

            (Level::Development, Key::Down) => self.move_cursor(1),
            (Level::Development, Key::Up) => self.move_cursor(-1),
            (Level::Development, Key::Enter) => match self.current_node() {
                Some(Node::Section(i)) if !self.expanded[i] => self.expanded[i] = true,
                Some(_) => {
                    self.enter_files();
                }
                None => self.status = Some("nothing to review".into()),
            },
            (Level::Development, Key::Back) => match self.current_node() {
                Some(Node::Checkpoint(i, _)) => {
                    self.cursor = self
                        .nodes()
                        .iter()
                        .position(|n| *n == Node::Section(i))
                        .unwrap_or(0);
                }
                Some(Node::Section(i)) if self.expanded[i] => {
                    self.expanded[i] = false;
                    self.clamp_cursor();
                }
                _ => return Effect::Quit,
            },
            (Level::Development, Key::Diff) => match self.node_diff_target() {
                Some((target, title)) => self.show_diff(target, title, false, load_diff),
                None => self.status = Some("nothing to review".into()),
            },
            (Level::Development, Key::Open) => {
                self.status = Some("select a file first (Enter)".into());
            }

            (Level::Files, Key::Down) => self.move_file(1),
            (Level::Files, Key::Up) => self.move_file(-1),
            (Level::Files, Key::Enter) | (Level::Files, Key::Diff) => {
                match self.file_diff_target() {
                    Some((target, title)) => self.show_diff(target, title, true, load_diff),
                    None => self.status = Some("no file selected".into()),
                }
            }
            (Level::Files, Key::Back) => self.level = Level::Development,
            (Level::Files, Key::Open) | (Level::Diff, Key::Open) => {
                if let Some(target) = self.open_target() {
                    return Effect::Open(target);
                }
                self.status = Some("select a file first (Enter)".into());
            }

            (Level::Diff, Key::Down) => {
                if self.scroll + 1 < self.diff.len() {
                    self.scroll += 1;
                }
            }
            (Level::Diff, Key::Up) => self.scroll = self.scroll.saturating_sub(1),
            (Level::Diff, Key::Back) => {
                self.level = if self.diff_of_file {
                    Level::Files
                } else {
                    Level::Development
                };
            }
            (Level::Diff, Key::Enter) | (Level::Diff, Key::Diff) => {}
        }
        Effect::None
    }

    fn open_target(&self) -> Option<OpenTarget> {
        if !self.diff_of_file && self.level == Level::Diff {
            return None;
        }
        let row = self.current_file()?;
        Some(match self.current_node()? {
            Node::Section(i) => match self.section(i) {
                ReviewSection::Commit(c) => OpenTarget::CommitFile {
                    section: c.number,
                    path: row.path,
                },
                ReviewSection::WorkingTree(_) => OpenTarget::WorktreeFile { path: row.path },
            },
            Node::Checkpoint(i, j) => OpenTarget::Snapshot {
                checkpoint_id: self.section(i).checkpoints()[j].id.clone(),
                path: row.path,
            },
        })
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.nodes().len();
        if len == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        if next != self.cursor {
            self.cursor = next;
            self.file = 0;
            self.diff.clear();
        }
    }

    /// Keep the cursor on a visible row after the tree changed shape.
    fn clamp_cursor(&mut self) {
        let len = self.nodes().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    fn move_file(&mut self, delta: isize) {
        let len = self.file_rows().len();
        if len == 0 {
            return;
        }
        let next = (self.file as isize + delta).clamp(0, len as isize - 1) as usize;
        if next != self.file {
            self.file = next;
            self.diff.clear();
        }
    }

    fn enter_files(&mut self) -> bool {
        if self.file_rows().is_empty() {
            self.status = Some("no files in this selection".into());
            return false;
        }
        self.file = self.file.min(self.file_rows().len() - 1);
        self.level = Level::Files;
        true
    }

    fn show_diff(
        &mut self,
        target: DiffTarget,
        title: String,
        of_file: bool,
        load_diff: &mut dyn FnMut(&DiffTarget) -> Result<String>,
    ) {
        match load_diff(&target) {
            Ok(text) => {
                self.diff = if text.is_empty() {
                    vec!["(no diff: snapshot unavailable, binary, or nothing changed)".to_string()]
                } else {
                    text.lines().map(String::from).collect()
                };
                self.diff_title = title;
                self.diff_of_file = of_file;
                self.scroll = 0;
                self.level = Level::Diff;
            }
            Err(err) => self.status = Some(err.to_string()),
        }
    }
}

fn map_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
    match code {
        KeyCode::Char('j') | KeyCode::Down => Some(Key::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Key::Up),
        KeyCode::Enter | KeyCode::Char('l') => Some(Key::Enter),
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Backspace => Some(Key::Back),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Some(Key::Quit),
        KeyCode::Char('d') => Some(Key::Diff),
        KeyCode::Char('c') => Some(Key::CommitDiff),
        KeyCode::Char('o') => Some(Key::Open),
        KeyCode::Char('q') => Some(Key::Quit),
        _ => None,
    }
}

/// Run the browser until the user quits.
pub fn run(repo: &Repo, trail: &Trail, review: WorktreeReview, cwd: &Path) -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(TrailError::NotATerminal);
    }
    let mut app = App::new(review);
    // The loader borrows a snapshot of the review: it is immutable while the
    // browser runs, so a clone is cheap and keeps `app` free to mutate.
    let snapshot = app.review.clone();
    let mut load_diff = |target: &DiffTarget| review::diff_for(repo, &snapshot, target);

    let mut terminal = ratatui::init();
    let outcome = loop {
        if let Err(e) = terminal.draw(|frame| draw(frame, &mut app)) {
            break Err(TrailError::Io(e));
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => break Err(TrailError::Io(e)),
        };
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let Some(mapped) = map_key(key.code, key.modifiers) else {
            continue;
        };
        match app.handle(mapped, &mut load_diff) {
            Effect::None => {}
            Effect::Quit => break Ok(()),
            Effect::Open(target) => {
                ratatui::restore();
                let result = open(repo, trail, &snapshot, cwd, &target);
                terminal = ratatui::init();
                if let Err(err) = result {
                    app.status = Some(err.to_string());
                }
            }
        }
    };
    ratatui::restore();
    outcome
}

fn open(
    repo: &Repo,
    trail: &Trail,
    review: &WorktreeReview,
    cwd: &Path,
    target: &OpenTarget,
) -> Result<()> {
    let root = repo.workdir();
    match target {
        OpenTarget::Snapshot {
            checkpoint_id,
            path,
        } => crate::edit::open_at(repo, trail, cwd, &root.join(path), checkpoint_id, false),
        OpenTarget::WorktreeFile { path } => {
            crate::edit::open_file(repo, cwd, &root.join(path), false)
        }
        OpenTarget::CommitFile { section, path } => {
            let s = review
                .sections
                .iter()
                .find(|s| s.number() == *section)
                .ok_or_else(|| {
                    TrailError::InvalidSelection(format!("section {section} does not exist"))
                })?;
            crate::edit::open_selected(repo, trail, cwd, &root.join(path), Selected::Section(s))
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let wide = body.width >= WIDE_LAYOUT_MIN_COLUMNS;

    if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(28),
                Constraint::Percentage(42),
            ])
            .split(body);
        draw_development(frame, cols[0], app);
        draw_files(frame, cols[1], app);
        draw_diff(frame, cols[2], app);
    } else {
        match app.level {
            Level::Development => draw_development(frame, body, app),
            Level::Files => draw_files(frame, body, app),
            Level::Diff => draw_diff(frame, body, app),
        }
    }
    draw_status(frame, rows[1], app, wide);
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn draw_development(frame: &mut Frame, area: Rect, app: &App) {
    let ctx = &app.review.repository;
    let title = format!(
        " Development  {}{} since {} ",
        ctx.head.label(),
        if app.review.worktree.exists {
            ""
        } else {
            " (removed)"
        },
        ctx.since.label
    );
    let nodes = app.nodes();
    let items: Vec<ListItem> = nodes
        .iter()
        .map(|node| match *node {
            Node::Section(i) => {
                let s = &app.review.sections[i];
                let arrow = if app.expanded[i] { "▼" } else { "▶" };
                let cps = s.checkpoints().len();
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{arrow} [{}] {}  ", s.number(), s.heading())),
                    Span::styled(
                        format!("{} checkpoint{}  {}", cps, plural(cps), s.stats()),
                        dim(),
                    ),
                ]))
            }
            Node::Checkpoint(i, j) => {
                let s = &app.review.sections[i];
                let cp = &s.checkpoints()[j];
                let branch = if j + 1 == s.checkpoints().len() {
                    "└"
                } else {
                    "├"
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "  {branch} [{}] {}  ",
                        cp.label,
                        cp.display_title()
                    )),
                    Span::styled(cp.stats.to_string(), dim()),
                ]))
            }
        })
        .collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(pane_block(&title, app.level == Level::Development))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !empty {
        state.select(Some(app.cursor.min(nodes.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_files(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.current_node() {
        Some(Node::Section(i)) => format!(" Files  [{}] ", app.review.sections[i].number()),
        Some(Node::Checkpoint(i, j)) => format!(
            " Files  [{}] ",
            app.review.sections[i].checkpoints()[j].label
        ),
        None => " Files ".to_string(),
    };
    let rows = app.file_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let mut spans = vec![
                Span::raw(format!("{} {}  ", row.mark, row.name)),
                Span::styled(row.stat.clone(), dim()),
            ];
            if let Some(state) = row.state {
                spans.push(Span::styled(format!("  {state}"), dim()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(pane_block(&title, app.level == Level::Files))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !empty && app.level != Level::Development {
        state.select(Some(app.file.min(rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
    if empty {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(Paragraph::new(Span::styled("no files", dim())), inner);
    }
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &App) {
    let title = if app.level == Level::Diff {
        format!(" Diff  {} ", app.diff_title)
    } else {
        " Diff ".to_string()
    };
    let lines: Vec<Line> = if app.level == Level::Diff {
        app.diff
            .iter()
            .skip(app.scroll)
            .map(|l| {
                let style = if l.starts_with("+++") || l.starts_with("---") {
                    Style::default().add_modifier(Modifier::BOLD)
                } else if l.starts_with('+') {
                    Style::default().fg(Color::Green)
                } else if l.starts_with('-') {
                    Style::default().fg(Color::Red)
                } else if l.starts_with("@@") {
                    Style::default().fg(Color::Cyan)
                } else if l.starts_with("diff --git") {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(l.clone(), style))
            })
            .collect()
    } else {
        vec![Line::from(Span::styled(
            "d: diff of the selection   c: commit diff   Enter on a file: its diff",
            dim(),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines).block(pane_block(&title, app.level == Level::Diff)),
        area,
    );
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, wide: bool) {
    let hints = match app.level {
        Level::Development => {
            "j/k move  Enter expand/files  d diff  c commit diff  h/Esc collapse/back  q quit"
        }
        Level::Files => "j/k move  Enter/d diff  o open  c commit diff  h/Esc back  q quit",
        Level::Diff => "j/k scroll  o open  c commit diff  h/Esc back  q quit",
    };
    let text = match &app.status {
        Some(status) => Line::from(Span::styled(
            status.clone(),
            Style::default().fg(Color::Yellow),
        )),
        None => {
            let position = if wide {
                String::new()
            } else {
                format!(
                    "{}  ",
                    match app.level {
                        Level::Development => "Development",
                        Level::Files => "Development > Files",
                        Level::Diff => "Development > Files > Diff",
                    }
                )
            };
            Line::from(vec![
                Span::styled(position, Style::default().fg(Color::Cyan)),
                Span::styled(hints, dim()),
            ])
        }
    };
    frame.render_widget(Paragraph::new(text), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{ChangeKind as GitChangeKind, LineStats};
    use crate::review::{
        CommitFile, CommitReview, ReviewCheckpoint, ReviewFile, ReviewSummary, ReviewWorktree,
        WorkingTreeFile, WorkingTreeReview,
    };

    fn file(path: &str) -> ReviewFile {
        ReviewFile {
            path: PathBuf::from(path),
            kind: crate::recorder::checkpoint::ChangeKind::Modified,
            from_path: None,
            before_hash: Some("a".into()),
            after_hash: Some("b".into()),
            additions: Some(1),
            deletions: Some(0),
            binary: false,
            snapshot: true,
            edits: 1,
        }
    }

    fn checkpoint(section: usize, number: usize, files: &[&str]) -> ReviewCheckpoint {
        ReviewCheckpoint {
            section,
            number,
            label: format!("{section}.{number}"),
            id: format!("s.{section}{number}"),
            title: None,
            annotation: None,
            started_at: chrono::Utc::now(),
            ended_at: chrono::Utc::now(),
            bulk: false,
            attachment: None,
            files: files.iter().map(|f| file(f)).collect(),
            stats: LineStats::default(),
        }
    }

    fn commit(number: usize, checkpoints: Vec<ReviewCheckpoint>, files: &[&str]) -> ReviewSection {
        ReviewSection::Commit(CommitReview {
            number,
            id: format!("{number:040}"),
            short_id: format!("{number:07}"),
            summary: format!("commit {number}"),
            author: "t".into(),
            time: chrono::Utc::now(),
            is_merge: false,
            diff_parent: None,
            checkpoints,
            files: files
                .iter()
                .map(|f| CommitFile {
                    path: PathBuf::from(f),
                    old_path: None,
                    kind: GitChangeKind::Modified,
                    before_id: Some("a".into()),
                    after_id: Some("b".into()),
                    additions: Some(1),
                    deletions: Some(0),
                    binary: false,
                })
                .collect(),
            stats: LineStats::default(),
        })
    }

    fn working_tree(
        number: usize,
        checkpoints: Vec<ReviewCheckpoint>,
        files: &[&str],
    ) -> ReviewSection {
        ReviewSection::WorkingTree(WorkingTreeReview {
            number,
            available: true,
            checkpoints,
            files: files
                .iter()
                .map(|f| WorkingTreeFile {
                    path: PathBuf::from(f),
                    old_path: None,
                    kind: GitChangeKind::Modified,
                    staged: false,
                    unstaged: true,
                    untracked: false,
                    additions: Some(1),
                    deletions: Some(0),
                    binary: false,
                })
                .collect(),
            staged: 0,
            unstaged: files.len(),
            untracked: 0,
            stats: LineStats::default(),
        })
    }

    fn app(sections: Vec<ReviewSection>) -> App {
        let zero = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let repo_ctx = crate::trail::RepositoryContext {
            name: "t".into(),
            head: crate::git::repository::HeadState::Branch {
                name: "feature".into(),
            },
            base: "main".into(),
            merge_base: "abc".into(),
            since: crate::git::baseline::Baseline {
                kind: crate::git::baseline::BaselineKind::BaseBranch,
                label: "base main".into(),
                commit: zero,
                start: zero,
                short_commit: "0000000".into(),
                short_start: "0000000".into(),
                explicit: false,
            },
            worktree: crate::git::worktree::WorktreeInfo {
                root: PathBuf::from("/r"),
                git_dir: PathBuf::from("/r/.git"),
                common_dir: PathBuf::from("/r/.git"),
                kind: crate::git::worktree::WorktreeKind::Main,
                id: None,
                sibling_count: 0,
            },
            shallow: false,
        };
        let checkpoints = sections
            .iter()
            .flat_map(|s| s.checkpoints().to_vec())
            .collect();
        App::new(WorktreeReview {
            version: review::REVIEW_VERSION,
            repository: repo_ctx,
            worktree: ReviewWorktree {
                id: "main".into(),
                path: PathBuf::from("/r"),
                branch: Some("feature".into()),
                exists: true,
                current: true,
            },
            sections,
            checkpoints,
            summary: ReviewSummary::default(),
            files_changed: 0,
            stats: LineStats::default(),
        })
    }

    /// Two commits with two checkpoints each, one uncommitted checkpoint.
    fn typical() -> App {
        app(vec![
            commit(
                1,
                vec![
                    checkpoint(1, 1, &["a.rs", "b.rs"]),
                    checkpoint(1, 2, &["c.rs"]),
                ],
                &["a.rs", "b.rs", "c.rs"],
            ),
            commit(
                2,
                vec![checkpoint(2, 1, &["d.rs"]), checkpoint(2, 2, &["e.rs"])],
                &["d.rs", "e.rs"],
            ),
            working_tree(3, vec![checkpoint(3, 1, &["f.rs"])], &["f.rs"]),
        ])
    }

    type Loads = std::rc::Rc<std::cell::RefCell<Vec<DiffTarget>>>;

    fn recording_loader() -> (Loads, impl FnMut(&DiffTarget) -> Result<String>) {
        let loads = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = loads.clone();
        let load = move |t: &DiffTarget| {
            l.borrow_mut().push(t.clone());
            Ok("--- a\n+++ b\n@@\n-x\n+y\n".to_string())
        };
        (loads, load)
    }

    #[test]
    fn tree_starts_expanded_and_collapses() {
        let mut a = typical();
        let (_, mut load) = recording_loader();
        assert_eq!(a.nodes().len(), 3 + 5);
        assert_eq!(a.current_node(), Some(Node::Section(0)));
        // Back on an expanded commit collapses it.
        a.handle(Key::Back, &mut load);
        assert!(!a.expanded[0]);
        assert_eq!(a.nodes().len(), 3 + 3);
        // Enter on a collapsed commit expands it again.
        a.handle(Key::Enter, &mut load);
        assert!(a.expanded[0]);
        // Back on a collapsed top-level node quits.
        a.handle(Key::Back, &mut load);
        assert_eq!(a.handle(Key::Back, &mut load), Effect::Quit);
    }

    #[test]
    fn commit_to_checkpoint_to_files_to_diff() {
        let mut a = typical();
        let (loads, mut load) = recording_loader();
        a.handle(Key::Down, &mut load);
        assert_eq!(a.current_node(), Some(Node::Checkpoint(0, 0)));
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Files);
        assert_eq!(
            a.file_rows()
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"]
        );
        a.handle(Key::Down, &mut load);
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Diff);
        assert_eq!(
            loads.borrow().last(),
            Some(&DiffTarget::CheckpointFile {
                label: "1.1".into(),
                path: PathBuf::from("b.rs")
            })
        );
        assert_eq!(a.diff.len(), 5);
        a.handle(Key::Down, &mut load);
        a.handle(Key::Down, &mut load);
        assert_eq!(a.scroll, 2);
        // Back from a file diff returns to Files, then to the tree.
        a.handle(Key::Back, &mut load);
        assert_eq!(a.level, Level::Files);
        a.handle(Key::Back, &mut load);
        assert_eq!(a.level, Level::Development);
        // Back on a checkpoint jumps to its commit.
        a.handle(Key::Back, &mut load);
        assert_eq!(a.current_node(), Some(Node::Section(0)));
        assert_eq!(a.handle(Key::Quit, &mut load), Effect::Quit);
    }

    #[test]
    fn commit_files_and_commit_diff() {
        let mut a = typical();
        let (loads, mut load) = recording_loader();
        // Enter on an expanded commit lists the commit's own files.
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Files);
        assert_eq!(a.file_rows().len(), 3);
        a.handle(Key::Down, &mut load);
        a.handle(Key::Down, &mut load);
        a.handle(Key::Diff, &mut load);
        assert_eq!(
            loads.borrow().last(),
            Some(&DiffTarget::CommitFile {
                section: 1,
                path: PathBuf::from("c.rs")
            })
        );
        assert_eq!(
            a.handle(Key::Open, &mut load),
            Effect::Open(OpenTarget::CommitFile {
                section: 1,
                path: PathBuf::from("c.rs")
            })
        );
        // `c` anywhere inside a commit shows the whole commit diff, and Back
        // from it returns to where the user was.
        a.handle(Key::Back, &mut load);
        a.handle(Key::Back, &mut load);
        a.handle(Key::Down, &mut load); // checkpoint 1.1
        a.handle(Key::CommitDiff, &mut load);
        assert_eq!(a.level, Level::Diff);
        assert_eq!(
            loads.borrow().last(),
            Some(&DiffTarget::Commit { section: 1 })
        );
        assert!(!a.diff_of_file);
        assert!(a.handle(Key::Open, &mut load) == Effect::None && a.status.is_some());
        a.handle(Key::Back, &mut load);
        assert_eq!(a.level, Level::Development);
        assert_eq!(a.current_node(), Some(Node::Checkpoint(0, 0)));
        // `d` on a checkpoint shows the checkpoint diff.
        a.handle(Key::Diff, &mut load);
        assert_eq!(
            loads.borrow().last(),
            Some(&DiffTarget::Checkpoint {
                label: "1.1".into()
            })
        );
    }

    #[test]
    fn working_tree_node() {
        let mut a = typical();
        let (loads, mut load) = recording_loader();
        for _ in 0..6 {
            a.handle(Key::Down, &mut load);
        }
        assert_eq!(a.current_node(), Some(Node::Section(2)));
        a.handle(Key::CommitDiff, &mut load);
        assert_eq!(
            a.level,
            Level::Development,
            "working tree has no commit diff"
        );
        assert!(a.status.is_some());
        a.handle(Key::Diff, &mut load);
        assert_eq!(loads.borrow().last(), Some(&DiffTarget::WorkingTree));
        a.handle(Key::Back, &mut load);
        a.handle(Key::Enter, &mut load);
        assert_eq!(
            a.handle(Key::Open, &mut load),
            Effect::Open(OpenTarget::WorktreeFile {
                path: PathBuf::from("f.rs")
            })
        );
        a.handle(Key::Back, &mut load);
        a.handle(Key::Down, &mut load); // checkpoint 3.1
        a.handle(Key::Down, &mut load); // clamped at the end
        assert_eq!(a.current_node(), Some(Node::Checkpoint(2, 0)));
        a.handle(Key::Enter, &mut load);
        assert_eq!(
            a.handle(Key::Open, &mut load),
            Effect::Open(OpenTarget::Snapshot {
                checkpoint_id: "s.31".into(),
                path: PathBuf::from("f.rs")
            })
        );
    }

    #[test]
    fn empty_commit_and_commit_without_checkpoints() {
        let mut a = app(vec![
            commit(1, Vec::new(), &[]),
            working_tree(2, Vec::new(), &[]),
        ]);
        let (_, mut load) = recording_loader();
        assert_eq!(a.nodes().len(), 2);
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Development, "no files to enter");
        assert!(a.status.is_some());
        a.handle(Key::Open, &mut load);
        assert!(a.status.is_some());
        a.handle(Key::Down, &mut load);
        a.handle(Key::Down, &mut load);
        assert_eq!(a.current_node(), Some(Node::Section(1)));
        let mut empty = app(Vec::new());
        empty.handle(Key::Down, &mut load);
        empty.handle(Key::Enter, &mut load);
        assert_eq!(empty.level, Level::Development);
        assert!(empty.status.is_some());
    }

    #[test]
    fn cursor_is_clamped_when_the_tree_shrinks() {
        let mut a = typical();
        let (_, mut load) = recording_loader();
        for _ in 0..7 {
            a.handle(Key::Down, &mut load);
        }
        assert_eq!(a.cursor, 7);
        // Back on the last checkpoint jumps to its section, Back again
        // collapses it: the cursor lands on the now-last row.
        a.handle(Key::Back, &mut load);
        a.handle(Key::Back, &mut load);
        assert_eq!(a.current_node(), Some(Node::Section(2)));
        assert_eq!(a.cursor, 6);
        // Collapse everything else from above: cursor index stays visible.
        a.cursor = 0;
        a.handle(Key::Back, &mut load);
        a.handle(Key::Down, &mut load);
        a.handle(Key::Back, &mut load);
        assert_eq!(a.nodes().len(), 3);
        assert!(a.cursor < a.nodes().len());
    }
}
