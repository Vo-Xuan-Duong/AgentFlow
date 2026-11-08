//! Read-only terminal monitor for commands launched from Codex Desktop.
//! This process neither starts agents nor owns their process trees.
//! Closing the monitor must never stop a Codex-launched AgentFlow task.
use crate::{load, state_root, task_dir, Task};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    fs::{self, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_millis(700);
const CYAN: Color = Color::Rgb(90, 210, 216);
const MUTED: Color = Color::Rgb(147, 160, 174);
const GREEN: Color = Color::Rgb(113, 210, 151);
const GOLD: Color = Color::Rgb(250, 201, 115);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Running,
    Interrupted,
    Stopped,
    Unknown,
}

impl Activity {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Interrupted => "INTERRUPTED",
            Self::Stopped => "STOPPED",
            Self::Unknown => "UNKNOWN",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Running => GREEN,
            Self::Interrupted => Color::LightRed,
            Self::Stopped => MUTED,
            Self::Unknown => GOLD,
        }
    }
}

fn running_phase(state: &str) -> bool {
    matches!(state, "planning" | "executing" | "validating" | "reviewing")
}

/// Only inspect an existing task.lock. Do not create files, modify state,
/// send cancellation requests, or acquire ownership of another agent.
fn detect_activity(root: &Path, task: &Task) -> Activity {
    if !running_phase(&task.state) {
        return Activity::Stopped;
    }
    let path = match task_dir(root, &task.id) {
        Ok(dir) => dir.join("task.lock"),
        Err(_) => return Activity::Unknown,
    };
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Activity::Interrupted;
        }
        Err(_) => return Activity::Unknown,
    };
    match file.try_lock() {
        Err(TryLockError::WouldBlock) => Activity::Running,
        Err(TryLockError::Error(_)) => Activity::Unknown,
        Ok(()) => {
            let _ = file.unlock();
            Activity::Interrupted
        }
    }
}

struct Snapshot {
    task: Task,
    activity: Activity,
}

fn scan(root: &Path) -> Result<Vec<Snapshot>> {
    let dir = state_root(root)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut tasks = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if let Ok(task) = load(root, &id) {
            let activity = detect_activity(root, &task);
            tasks.push(Snapshot { task, activity });
        }
    }
    tasks.sort_by(|a, b| b.task.id.cmp(&a.task.id));
    Ok(tasks)
}

struct App {
    root: PathBuf,
    tasks: Vec<Snapshot>,
    selected: Option<String>,
    last_refresh: Instant,
    message: Option<String>,
}

impl App {
    fn new(root: &Path) -> Self {
        let mut app = Self {
            root: root.to_owned(),
            tasks: Vec::new(),
            selected: None,
            last_refresh: Instant::now(),
            message: None,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        match scan(&self.root) {
            Ok(tasks) => {
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|id| !tasks.iter().any(|snapshot| &snapshot.task.id == id))
                {
                    self.selected = tasks.first().map(|snapshot| snapshot.task.id.clone());
                }
                self.tasks = tasks;
                self.message = None;
            }
            Err(error) => self.message = Some(format!("Cannot refresh task status: {error:#}")),
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.tasks.iter().position(|s| Some(&s.task.id) == self.selected.as_ref())
    }

    fn selected(&self) -> Option<&Snapshot> {
        self.selected_index().and_then(|index| self.tasks.get(index))
    }

    fn move_selection(&mut self, offset: isize) {
        if self.tasks.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or_default() as isize;
        let index = (current + offset).clamp(0, self.tasks.len() as isize - 1);
        self.selected = Some(self.tasks[index as usize].task.id.clone());
    }

    fn running_count(&self) -> usize {
        self.tasks.iter().filter(|snapshot| snapshot.activity == Activity::Running).count()
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_owned()
    } else {
        format!("{}…", value.chars().take(limit.saturating_sub(1)).collect::<String>())
    }
}

fn phase_label(task: &Task) -> &'static str {
    match task.state.as_str() {
        "planning" => "Codex is preparing a plan",
        "planned" => "Codex plan ready for your approval",
        "approved" => "Plan approved; Antigravity can run",
        "executing" => "Antigravity is writing code",
        "executed" => "Code changes ready for validation",
        "validating" => "Running project checks",
        "validated" => "Checks passed; ready for Codex review",
        "reviewing" => "Codex is reviewing the changes",
        "awaiting_human_review" => "Review available; waiting for your decision",
        "needs_attention" => "Attention required; inspect the task and logs",
        "validation_failed" => "Checks failed; inspect validation logs",
        _ => "Inspect the task state from Codex Desktop",
    }
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(10), Constraint::Length(3)])
        .split(frame.area());

    let online = Span::styled("MONITOR ONLINE", Style::default().fg(GREEN).add_modifier(Modifier::BOLD));
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" AgentFlow  ", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::raw("Codex plans  →  Antigravity codes    "),
        online,
        Span::styled(format!("    {} active task(s)", app.running_count()), Style::default().fg(GOLD)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(CYAN)));
    frame.render_widget(header, layout[0]);

    let columns = Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(37), Constraint::Percentage(63)])
        .split(layout[1]);
    let tasks: Vec<ListItem<'_>> = app.tasks.iter().map(|snapshot| {
        let request = snapshot.task.request.replace(['\n', '\r'], " ");
        let label = format!(
            "{}\n  {} · {}",
            truncate_chars(&request, 37),
            snapshot.activity.label(),
            truncate_chars(&snapshot.task.state, 22)
        );
        ListItem::new(label).style(Style::default().fg(snapshot.activity.color()))
    }).collect();
    let mut selection = ListState::default();
    selection.select(app.selected_index());
    frame.render_stateful_widget(
        List::new(tasks)
            .block(Block::default().title(" Tasks · Codex Desktop ").borders(Borders::ALL))
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().bg(Color::Rgb(26, 54, 64))
                .add_modifier(Modifier::BOLD)),
        columns[0],
        &mut selection,
    );

    detail(frame, columns[1], app);

    let footer_text = if let Some(error) = &app.message {
        format!(" {error}    ↑/↓ Select  ·  R Refresh  ·  Q/Esc Exit")
    } else {
        " Status only — type requests in Codex Desktop     ↑/↓ Select  ·  R Refresh  ·  Q/Esc Exit".to_owned()
    };
    frame.render_widget(
        Paragraph::new(footer_text)
            .style(Style::default().fg(MUTED))
            .block(Block::default().borders(Borders::ALL)),
        layout[2],
    );
}

fn detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(snapshot) = app.selected() {
        let task = &snapshot.task;
        lines.push(Line::from(vec![
            Span::styled("Agent  ", Style::default().fg(CYAN)),
            Span::raw(phase_label(task).to_owned()),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Process   ", Style::default().fg(MUTED)),
            Span::styled(snapshot.activity.label(), Style::default().fg(snapshot.activity.color())
                .add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(format!("Task      {}", task.id)));
        lines.push(Line::from(format!("State     {}", task.state)));
        lines.push(Line::from(format!("Branch    {}", task.branch)));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Request", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(task.request.clone()));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Worktree", Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(task.worktree.display().to_string()));
        if let Some(error) = &task.last_error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                truncate_chars(error, 300), Style::default().fg(Color::LightRed),
            )));
        }
        if snapshot.activity == Activity::Interrupted {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "No active lock: the foreground command may have closed unexpectedly.",
                Style::default().fg(GOLD),
            )));
            lines.push(Line::from(
                "Return to Codex Desktop and inspect before using agentflow recover.",
            ));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "AgentFlow is online", Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("No AgentFlow tasks were found for this repository."));
        lines.push(Line::from(""));
        lines.push(Line::from("Open Codex Desktop and describe what you want built."));
        lines.push(Line::from("Codex creates the plan, then delegates code to Antigravity."));
        lines.push(Line::from(""));
        lines.push(Line::from("This window is only a live status viewer."));
        lines.push(Line::from("Closing it does not cancel Codex Desktop tasks."));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" Agent status · read-only ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

struct ScreenGuard;

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Read-only: no worker thread and no subprocesses. The monitor stays
/// responsive while Codex Desktop launches the CLI in a different process.
pub fn run(root: &Path) -> Result<()> {
    enable_raw_mode().context("cannot enter raw terminal mode")?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("cannot enter alternate screen");
    }
    let _screen = ScreenGuard;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut app = App::new(root);
    loop {
        if app.last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh();
        }
        terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                match key.code {
                    KeyCode::Up => app.move_selection(-1),
                    KeyCode::Down => app.move_selection(1),
                    KeyCode::Char('r') | KeyCode::F(5) => app.refresh(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_phases_are_limited_to_actual_cli_execution() {
        assert!(running_phase("planning"));
        assert!(running_phase("executing"));
        assert!(running_phase("validating"));
        assert!(running_phase("reviewing"));
        for state in ["planned", "approved", "executed", "needs_attention", "validated"] {
            assert!(!running_phase(state));
        }
    }

    #[test]
    fn activity_labels_distinguish_monitor_from_work() {
        assert_eq!(Activity::Running.label(), "RUNNING");
        assert_eq!(Activity::Interrupted.label(), "INTERRUPTED");
        assert_eq!(Activity::Stopped.label(), "STOPPED");
        assert_eq!(phase_label(&Task {
            id: "t1".into(),
            request: "test".into(),
            root: PathBuf::from("C:\\repo"),
            worktree: PathBuf::from("C:\\worktree"),
            branch: "agentflow/t1".into(),
            base: "sha".into(),
            approved_plan_hash: None,
            state: "executing".into(),
            last_error: None,
        }), "Antigravity is writing code");
    }

    #[test]
    fn truncate_unicode_is_safe() {
        assert_eq!(truncate_chars("Tiếng Việt", 6), "Tiếng…");
        assert_eq!(truncate_chars("ok", 6), "ok");
    }

    #[test]
    fn stale_task_check_does_not_create_or_modify_a_lock() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox = std::env::temp_dir().join(format!(
            "agentflow-monitor-{}-{stamp}",
            std::process::id()
        ));
        let root = sandbox.join("repo");
        fs::create_dir_all(&root).unwrap();
        let task = Task {
            id: "t123".into(),
            request: "test".into(),
            root: root.clone(),
            worktree: root.clone(),
            branch: "agentflow/t123".into(),
            base: "sha".into(),
            approved_plan_hash: None,
            state: "executing".into(),
            last_error: None,
        };
        let lock_path = task_dir(&root, &task.id).unwrap().join("task.lock");
        assert!(!lock_path.exists());
        assert_eq!(detect_activity(&root, &task), Activity::Interrupted);
        assert!(!lock_path.exists(), "monitor must never create task locks");
        fs::remove_dir_all(sandbox).unwrap();
    }
}
