//! `trail review -i`: an interactive review browser.
//!
//! Three levels, one state machine: Checkpoints → Files → Diff. Wide
//! terminals show all three side by side; narrow ones show the current level
//! only. The browser owns no review logic: it displays a `ReviewReport` and
//! asks the review module for diffs, and hands file opening to the same code
//! path as `trail open --at`.

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
use crate::git::diff::LineStats;
use crate::git::repository::Repo;
use crate::review::{self, ReviewCheckpoint, ReviewFile, ReviewReport};
use crate::trail::Trail;

/// Terminals at least this wide get the three-pane layout.
pub const WIDE_LAYOUT_MIN_COLUMNS: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Checkpoints,
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
    Open,
    Quit,
}

/// What the loop must do after a key was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// Leave the TUI, open `path` as it was at the end of `checkpoint_id`,
    /// then come back.
    Open {
        checkpoint_id: String,
        path: PathBuf,
    },
}

pub struct App {
    pub report: ReviewReport,
    pub level: Level,
    pub checkpoint: usize,
    pub file: usize,
    /// Diff lines of the selected file, loaded on demand.
    pub diff: Vec<String>,
    pub scroll: usize,
    pub status: Option<String>,
}

impl App {
    pub fn new(report: ReviewReport) -> Self {
        App {
            report,
            level: Level::Checkpoints,
            checkpoint: 0,
            file: 0,
            diff: Vec::new(),
            scroll: 0,
            status: None,
        }
    }

    pub fn current_checkpoint(&self) -> Option<&ReviewCheckpoint> {
        self.report.checkpoints.get(self.checkpoint)
    }

    pub fn current_file(&self) -> Option<&ReviewFile> {
        self.current_checkpoint()?.files.get(self.file)
    }

    /// Advance the state machine. `load_diff` is only called when entering
    /// the diff level, so listing stays cheap.
    pub fn handle(
        &mut self,
        key: Key,
        load_diff: &mut dyn FnMut(&ReviewCheckpoint, &Path) -> Result<String>,
    ) -> Effect {
        self.status = None;
        match (self.level, key) {
            (_, Key::Quit) => return Effect::Quit,

            (Level::Checkpoints, Key::Down) => self.move_checkpoint(1),
            (Level::Checkpoints, Key::Up) => self.move_checkpoint(-1),
            (Level::Checkpoints, Key::Enter) => {
                self.enter_files();
            }
            (Level::Checkpoints, Key::Diff) => {
                if self.enter_files() {
                    self.enter_diff(load_diff);
                }
            }
            (Level::Checkpoints, Key::Back) => return Effect::Quit,
            (Level::Checkpoints, Key::Open) => {
                self.status = Some("select a file first (Enter)".into());
            }

            (Level::Files, Key::Down) => self.move_file(1),
            (Level::Files, Key::Up) => self.move_file(-1),
            (Level::Files, Key::Enter) | (Level::Files, Key::Diff) => self.enter_diff(load_diff),
            (Level::Files, Key::Back) => self.level = Level::Checkpoints,
            (Level::Files, Key::Open) | (Level::Diff, Key::Open) => {
                if let (Some(cp), Some(file)) = (self.current_checkpoint(), self.current_file()) {
                    return Effect::Open {
                        checkpoint_id: cp.id.clone(),
                        path: file.path.clone(),
                    };
                }
            }

            (Level::Diff, Key::Down) => {
                if self.scroll + 1 < self.diff.len() {
                    self.scroll += 1;
                }
            }
            (Level::Diff, Key::Up) => self.scroll = self.scroll.saturating_sub(1),
            (Level::Diff, Key::Back) => self.level = Level::Files,
            (Level::Diff, Key::Enter) | (Level::Diff, Key::Diff) => {}
        }
        Effect::None
    }

    fn move_checkpoint(&mut self, delta: isize) {
        let len = self.report.checkpoints.len();
        if len == 0 {
            return;
        }
        let next = (self.checkpoint as isize + delta).clamp(0, len as isize - 1) as usize;
        if next != self.checkpoint {
            self.checkpoint = next;
            self.file = 0;
            self.diff.clear();
        }
    }

    fn move_file(&mut self, delta: isize) {
        let len = self
            .current_checkpoint()
            .map(|c| c.files.len())
            .unwrap_or(0);
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
        match self.current_checkpoint() {
            Some(cp) if !cp.files.is_empty() => {
                self.level = Level::Files;
                true
            }
            _ => {
                self.status = Some("no checkpoints to review".into());
                false
            }
        }
    }

    fn enter_diff(
        &mut self,
        load_diff: &mut dyn FnMut(&ReviewCheckpoint, &Path) -> Result<String>,
    ) {
        let (Some(cp), Some(file)) = (self.current_checkpoint(), self.current_file()) else {
            return;
        };
        match load_diff(cp, &file.path) {
            Ok(text) => {
                self.diff = if text.is_empty() {
                    vec!["(no diff: snapshot unavailable or binary)".to_string()]
                } else {
                    text.lines().map(String::from).collect()
                };
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
        KeyCode::Char('d') => Some(Key::Diff),
        KeyCode::Char('o') => Some(Key::Open),
        KeyCode::Char('q') => Some(Key::Quit),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Some(Key::Quit),
        _ => None,
    }
}

/// Run the browser until the user quits.
pub fn run(repo: &Repo, trail: &Trail, report: ReviewReport, cwd: &Path) -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(TrailError::NotATerminal);
    }
    let mut app = App::new(report);
    let mut load_diff = |cp: &ReviewCheckpoint, path: &Path| -> Result<String> {
        // `report` is borrowed by `app`; rebuild the lookup from the ids.
        let file = cp.files.iter().find(|f| f.path == path).ok_or_else(|| {
            TrailError::InvalidSelection(format!("{} is not in the checkpoint", path.display()))
        })?;
        Ok(review::diff_text(repo, file))
    };

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
            Effect::Open {
                checkpoint_id,
                path,
            } => {
                ratatui::restore();
                let result = crate::edit::open_at(repo, trail, cwd, &path, &checkpoint_id, false);
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
                Constraint::Percentage(26),
                Constraint::Percentage(30),
                Constraint::Percentage(44),
            ])
            .split(body);
        draw_checkpoints(frame, cols[0], app);
        draw_files(frame, cols[1], app);
        draw_diff(frame, cols[2], app);
    } else {
        match app.level {
            Level::Checkpoints => draw_checkpoints(frame, body, app),
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

fn draw_checkpoints(frame: &mut Frame, area: Rect, app: &App) {
    let ctx = &app.report.repository;
    let title = format!(
        " Checkpoints  {} since {} ",
        ctx.head.label(),
        ctx.since.label
    );
    let items: Vec<ListItem> = app
        .report
        .checkpoints
        .iter()
        .map(|cp| {
            let files = format!("{} file{}", cp.files.len(), plural(cp.files.len()));
            ListItem::new(vec![
                Line::from(format!("[{}] {}", cp.number, cp.display_title())),
                Line::from(Span::styled(
                    format!(
                        "    {} - {}  {files}  {}",
                        cp.started_at.with_timezone(&chrono::Local).format("%H:%M"),
                        cp.ended_at.with_timezone(&chrono::Local).format("%H:%M"),
                        cp.stats
                    ),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
        })
        .collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(pane_block(&title, app.level == Level::Checkpoints))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !empty {
        state.select(Some(app.checkpoint));
    }
    frame.render_stateful_widget(list, area, &mut state);
    if empty {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(
                "no recorded checkpoints in this window (run `trail start` while working)",
            ),
            inner,
        );
    }
}

fn draw_files(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.current_checkpoint() {
        Some(cp) => format!(" Files  [{}] ", cp.number),
        None => " Files ".to_string(),
    };
    let items: Vec<ListItem> = app
        .current_checkpoint()
        .map(|cp| {
            cp.files
                .iter()
                .map(|f| {
                    let stat = if !f.snapshot {
                        "no snapshot".to_string()
                    } else if f.binary {
                        "binary".to_string()
                    } else {
                        LineStats::from_counts(f.additions, f.deletions).to_string()
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{} {}  ", f.kind.mark(), f.path.display())),
                        Span::styled(stat, Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect()
        })
        .unwrap_or_default();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(pane_block(&title, app.level == Level::Files))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !empty && app.level != Level::Checkpoints {
        state.select(Some(app.file));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.current_file() {
        Some(f) if app.level == Level::Diff => format!(" Diff  {} ", f.path.display()),
        _ => " Diff ".to_string(),
    };
    let lines: Vec<Line> = if app.level == Level::Diff {
        app.diff
            .iter()
            .skip(app.scroll)
            .map(|l| {
                let style = if l.starts_with('+') && !l.starts_with("+++") {
                    Style::default().fg(Color::Green)
                } else if l.starts_with('-') && !l.starts_with("---") {
                    Style::default().fg(Color::Red)
                } else if l.starts_with("@@") {
                    Style::default().fg(Color::Cyan)
                } else if l.starts_with("---") || l.starts_with("+++") {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(l.clone(), style))
            })
            .collect()
    } else {
        vec![Line::from(Span::styled(
            "Enter or d on a file shows what the checkpoint changed in it",
            Style::default().fg(Color::DarkGray),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines).block(pane_block(&title, app.level == Level::Diff)),
        area,
    );
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, wide: bool) {
    let hints = match app.level {
        Level::Checkpoints => "j/k move  Enter files  d diff  q quit",
        Level::Files => "j/k move  Enter/d diff  o open at checkpoint  h/Esc back  q quit",
        Level::Diff => "j/k scroll  o open at checkpoint  h/Esc back  q quit",
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
                        Level::Checkpoints => "Checkpoints",
                        Level::Files => "Checkpoints > Files",
                        Level::Diff => "Checkpoints > Files > Diff",
                    }
                )
            };
            Line::from(vec![
                Span::styled(position, Style::default().fg(Color::Cyan)),
                Span::styled(hints, Style::default().fg(Color::DarkGray)),
            ])
        }
    };
    frame.render_widget(Paragraph::new(text), area);
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn checkpoint(number: usize, files: &[&str]) -> ReviewCheckpoint {
        ReviewCheckpoint {
            number,
            id: format!("s.{number}"),
            title: None,
            annotation: None,
            started_at: chrono::Utc::now(),
            ended_at: chrono::Utc::now(),
            bulk: false,
            files: files.iter().map(|f| file(f)).collect(),
            stats: LineStats::default(),
        }
    }

    fn app(checkpoints: Vec<ReviewCheckpoint>) -> App {
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
        App::new(ReviewReport {
            repository: repo_ctx,
            checkpoints,
            files_changed: 0,
            stats: LineStats::default(),
        })
    }

    #[test]
    fn navigates_checkpoints_files_and_diff() {
        let mut a = app(vec![
            checkpoint(1, &["a.rs", "b.rs"]),
            checkpoint(2, &["c.rs"]),
        ]);
        let loads = std::cell::RefCell::new(Vec::new());
        let mut load = |cp: &ReviewCheckpoint, p: &Path| {
            loads
                .borrow_mut()
                .push(format!("{}:{}", cp.id, p.display()));
            Ok("--- a\n+++ b\n@@\n-x\n+y\n".to_string())
        };
        assert_eq!(a.handle(Key::Down, &mut load), Effect::None);
        assert_eq!(a.checkpoint, 1);
        a.handle(Key::Down, &mut load);
        assert_eq!(a.checkpoint, 1, "clamped at the end");
        a.handle(Key::Up, &mut load);
        assert_eq!(a.checkpoint, 0);
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Files);
        a.handle(Key::Down, &mut load);
        assert_eq!(a.file, 1);
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Diff);
        assert_eq!(a.diff.len(), 5);
        a.handle(Key::Down, &mut load);
        a.handle(Key::Down, &mut load);
        assert_eq!(a.scroll, 2);
        a.handle(Key::Back, &mut load);
        assert_eq!(a.level, Level::Files);
        a.handle(Key::Back, &mut load);
        assert_eq!(a.level, Level::Checkpoints);
        assert_eq!(a.handle(Key::Quit, &mut load), Effect::Quit);
        assert_eq!(*loads.borrow(), vec!["s.1:b.rs".to_string()]);
    }

    #[test]
    fn open_and_diff_shortcuts() {
        let mut a = app(vec![checkpoint(1, &["a.rs"])]);
        let mut load = |_: &ReviewCheckpoint, _: &Path| Ok(String::new());
        assert_eq!(a.handle(Key::Open, &mut load), Effect::None);
        assert!(a.status.is_some(), "open needs a file");
        a.handle(Key::Diff, &mut load);
        assert_eq!(
            a.level,
            Level::Diff,
            "d from checkpoints jumps to the first file's diff"
        );
        assert_eq!(
            a.diff,
            vec!["(no diff: snapshot unavailable or binary)".to_string()]
        );
        assert_eq!(
            a.handle(Key::Open, &mut load),
            Effect::Open {
                checkpoint_id: "s.1".into(),
                path: PathBuf::from("a.rs")
            }
        );
        // Back at checkpoints level, Back quits.
        a.handle(Key::Back, &mut load);
        a.handle(Key::Back, &mut load);
        assert_eq!(a.handle(Key::Back, &mut load), Effect::Quit);
    }

    #[test]
    fn empty_report_is_safe() {
        let mut a = app(Vec::new());
        let mut load = |_: &ReviewCheckpoint, _: &Path| Ok(String::new());
        a.handle(Key::Down, &mut load);
        a.handle(Key::Enter, &mut load);
        assert_eq!(a.level, Level::Checkpoints);
        assert!(a.status.is_some());
        a.handle(Key::Diff, &mut load);
        assert_eq!(a.level, Level::Checkpoints);
    }
}
