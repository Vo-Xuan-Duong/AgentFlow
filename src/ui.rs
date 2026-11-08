//! Interactive, foreground terminal client for the existing AgentFlow state machine.
//! Agent commands run on a worker thread, but no detached service is started.
//! Closing the terminal ends this process and the Windows Job Object closes.
use crate::{
    approve, inspect_active_task, process, review, run as execute, start, state_root,
    task_dir, terminal, validate, Task,
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
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
    collections::{HashSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_millis(150);
const REFRESH: Duration = Duration::from_millis(500);
const MAX_INPUT: usize = 8000;
const CYAN: Color = Color::Rgb(75, 210, 212);
const MUTED: Color = Color::Rgb(139, 151, 166);
const GREEN: Color = Color::Rgb(120, 220, 156);
const GOLD: Color = Color::Rgb(249, 198, 102);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Compose,
    Tasks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Overview,
    Plan,
}

enum Job {
    Plan(String),
    Approve(String),
    Implement(String),
    Validate(String),
    Review(String),
}

impl Job {
    fn label(&self) -> &'static str {
        match self {
            Self::Plan(_) => "Codex planning",
            Self::Approve(_) => "Approving plan",
            Self::Implement(_) => "Antigravity coding",
            Self::Validate(_) => "Running checks",
            Self::Review(_) => "Codex reviewing",
        }
    }

    fn task_id(&self) -> Option<&str> {
        match self {
            Self::Plan(_) => None,
            Self::Approve(id) | Self::Implement(id) | Self::Validate(id) | Self::Review(id) => {
                Some(id)
            }
        }
    }

    fn execute(self, root: &Path) -> Result<()> {
        match self {
            Self::Plan(request) => start(root, request, None),
            Self::Approve(id) => approve(root, &id),
            Self::Implement(id) => execute(root, &id),
            Self::Validate(id) => validate(root, &id),
            Self::Review(id) => review(root, &id),
        }
    }
}

struct Finished {
    label: String,
    result: Result<(), String>,
}

struct Active {
    label: String,
    started: Instant,
    is_planning: bool,
    known: HashSet<String>,
}

struct App {
    root: PathBuf,
    input: String,
    tasks: Vec<Task>,
    selected: Option<String>,
    active: Option<Active>,
    focus: Focus,
    panel: Panel,
    plan_scroll: u16,
    events: VecDeque<String>,
    approve_pending: bool,
    quit_requested: bool,
    last_refresh: Instant,
}

impl App {
    fn new(root: &Path) -> Self {
        let mut app = Self {
            root: root.to_path_buf(),
            input: String::new(),
            tasks: Vec::new(),
            selected: None,
            active: None,
            focus: Focus::Compose,
            panel: Panel::Overview,
            plan_scroll: 0,
            events: VecDeque::new(),
            approve_pending: false,
            quit_requested: false,
            last_refresh: Instant::now(),
        };
        app.refresh();
        app.note("Welcome. Type a request, then Enter to ask Codex to plan it.");
        app
    }

    fn note(&mut self, message: impl Into<String>) {
        self.events.push_back(message.into());
        while self.events.len() > 8 {
            self.events.pop_front();
        }
    }

    fn current(&self) -> Option<&Task> {
        let id = self.selected.as_ref()?;
        self.tasks.iter().find(|task| &task.id == id)
    }

    fn selected_index(&self) -> Option<usize> {
        self.tasks.iter().position(|task| Some(&task.id) == self.selected.as_ref())
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let mut tasks = Vec::new();
        if let Ok(entries) = fs::read_dir(state_root(&self.root).unwrap_or_default()) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let id = entry.file_name().to_string_lossy().to_string();
                if let Ok((task, _)) = inspect_active_task(&self.root, &id) {
                    tasks.push(task);
                }
            }
        }
        tasks.sort_by(|a, b| b.id.cmp(&a.id));

        if let Some(active) = &self.active {
            if active.is_planning {
                if let Some(newest) = tasks.iter().find(|t| !active.known.contains(&t.id)) {
                    self.selected = Some(newest.id.clone());
                }
            }
        }
        if self.selected.as_ref().is_none_or(|id| !tasks.iter().any(|t| &t.id == id)) {
            self.selected = tasks.first().map(|task| task.id.clone());
        }
        self.tasks = tasks;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.tasks.is_empty() {
            return;
        }
        let index = self.selected_index().unwrap_or(0) as isize;
        let next = (index + delta).clamp(0, self.tasks.len() as isize - 1);
        self.selected = Some(self.tasks[next as usize].id.clone());
        self.panel = Panel::Overview;
        self.plan_scroll = 0;
        self.approve_pending = false;
    }

    fn schedule(&mut self, job: Job, tx: &Sender<Finished>) {
        if self.active.is_some() || self.quit_requested {
            self.note("A task is already running. Cancel or finish it first.");
            return;
        }
        let name = job.label().to_string();
        let is_planning = job.task_id().is_none();
        if let Some(id) = job.task_id() {
            self.selected = Some(id.to_owned());
        }
        self.active = Some(Active {
            label: name.clone(),
            started: Instant::now(),
            is_planning,
            known: self.tasks.iter().map(|t| t.id.clone()).collect(),
        });
        self.note(format!("{name} started. Close this terminal to stop the process."));
        let root = self.root.clone();
        let sender = tx.clone();
        thread::spawn(move || {
            // Suppress the regular CLI printout while the full-screen UI owns
            // stdout/stderr. Full subprocess logs still go to disk.
            let result = process::with_quiet(|| job.execute(&root)).map_err(|e| format!("{e:#}"));
            let _ = sender.send(Finished { label: name, result });
        });
    }

    fn submit(&mut self, tx: &Sender<Finished>) {
        let request = self.input.trim();
        if request.is_empty() {
            self.note("Type a request before pressing Enter.");
            return;
        }
        if request.chars().count() > MAX_INPUT {
            self.note("The prompt is too long.");
            return;
        }
        let request = request.to_owned();
        if self.active.is_none() {
            self.input.clear();
            self.focus = Focus::Tasks;
            self.approve_pending = false;
            self.schedule(Job::Plan(request), tx);
        } else {
            self.note("Wait until the running agent finishes.");
        }
    }

    fn action(&mut self, key: u8, tx: &Sender<Finished>) {
        let Some(task) = self.current() else {
            self.note("No task selected. Enter a request to start one.");
            return;
        };
        let id = task.id.clone();
        let state = task.state.clone();
        if key == 6 {
            if is_running(&state) {
                match crate::cancel(&self.root, &id) {
                    Ok(()) => self.note("Cancellation requested. Waiting for the agent to stop..."),
                    Err(error) => self.note(format!("Cancel failed: {error:#}")),
                }
            } else {
                self.note("This task is not running.");
            }
            return;
        }
        if self.active.is_some() {
            self.note("Another operation is running.");
            return;
        }
        match (key, state.as_str()) {
            (2, "planned") => {
                if !self.approve_pending {
                    self.approve_pending = true;
                    self.panel = Panel::Plan;
                    self.plan_scroll = 0;
                    self.note("Review the full plan (F7, PgUp/PgDn), then press F2 again to approve.");
                } else {
                    self.approve_pending = false;
                    self.schedule(Job::Approve(id), tx);
                }
            }
            (3, "approved") => self.schedule(Job::Implement(id), tx),
            (4, "executed") => self.schedule(Job::Validate(id), tx),
            (5, "validated") => self.schedule(Job::Review(id), tx),
            _ => self.note(format!("Action F{key} is not valid while state is {state}.")),
        }
    }

    fn request_quit(&mut self) {
        self.quit_requested = true;
        if self.active.is_some() {
            terminal::request_stop();
            self.note("Stopping the running agent and restoring the terminal...");
        }
    }

    fn handle_key(&mut self, key: KeyEvent, tx: &Sender<Finished>) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        match key.code {
            KeyCode::Esc => self.request_quit(),
            KeyCode::Tab => {
                self.focus = if self.focus == Focus::Compose {
                    Focus::Tasks
                } else {
                    Focus::Compose
                };
            }
            KeyCode::F(2..=6) => {
                if let KeyCode::F(n) = key.code {
                    self.action(n, tx);
                }
            }
            KeyCode::F(7) => {
                self.panel = if self.panel == Panel::Plan { Panel::Overview } else { Panel::Plan };
                self.plan_scroll = 0;
            }
            KeyCode::Up if self.focus == Focus::Tasks => self.move_selection(-1),
            KeyCode::Down if self.focus == Focus::Tasks => self.move_selection(1),
            KeyCode::PageDown => self.plan_scroll = self.plan_scroll.saturating_add(8),
            KeyCode::PageUp => self.plan_scroll = self.plan_scroll.saturating_sub(8),
            KeyCode::Enter if self.focus == Focus::Compose => self.submit(tx),
            KeyCode::Backspace if self.focus == Focus::Compose => {
                self.input.pop();
            }
            KeyCode::Char('u') if self.focus == Focus::Compose
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.input.clear();
            }
            KeyCode::Char(c)
                if self.focus == Focus::Compose && self.input.chars().count() < MAX_INPUT =>
            {
                self.input.push(c);
            }
            _ => {}
        }
    }

    fn handle_finished(&mut self, finished: Finished) {
        self.active = None;
        match finished.result {
            Ok(()) => self.note(format!("{} completed.", finished.label)),
            Err(message) => self.note(format!("{}: {message}", finished.label)),
        }
        self.refresh();
    }
}

fn is_running(state: &str) -> bool {
    matches!(state, "planning" | "executing" | "validating" | "reviewing")
}

fn accent(state: &str) -> Color {
    match state {
        "planned" | "approved" | "executed" | "validated" | "awaiting_human_review" => GREEN,
        "needs_attention" | "validation_failed" => Color::LightRed,
        "planning" | "executing" | "validating" | "reviewing" => GOLD,
        _ => MUTED,
    }
}

fn short(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let keep = max_chars.saturating_sub(1);
        format!("{}…", text.chars().take(keep).collect::<String>())
    }
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let full = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(5),
            Constraint::Length(2),
        ])
        .split(full);

    let activity = if let Some(active) = &app.active {
        format!("{} · {}s", active.label, active.started.elapsed().as_secs())
    } else {
        "Idle · no background service".to_owned()
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled("  AgentFlow  ", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::styled("Codex plans  →  Antigravity codes", Style::default().fg(Color::White)),
        Span::styled(format!("     {activity}"), Style::default().fg(GOLD)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(CYAN)));
    frame.render_widget(header, outer[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(31), Constraint::Percentage(69)])
        .split(outer[1]);
    let task_items: Vec<ListItem<'_>> = app.tasks.iter().map(|task| {
        let display = format!(
            "{}\n  {}",
            short(&task.request.replace('\n', " "), 27),
            short(&task.state, 25)
        );
        ListItem::new(display).style(Style::default().fg(accent(&task.state)))
    }).collect();
    let task_border = if app.focus == Focus::Tasks { CYAN } else { MUTED };
    let list = List::new(task_items)
        .block(Block::default().title(" Tasks  [Tab] ").borders(Borders::ALL)
            .border_style(Style::default().fg(task_border)))
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().bg(Color::Rgb(29, 54, 71))
            .add_modifier(Modifier::BOLD));
    let mut selection = ListState::default();
    selection.select(app.selected_index());
    frame.render_stateful_widget(list, cols[0], &mut selection);

    draw_detail(frame, cols[1], app);

    let editor_style = if app.focus == Focus::Compose { CYAN } else { MUTED };
    let prompt = if app.input.is_empty() {
        "Describe a task for Codex to plan. Enter to submit."
    } else {
        &app.input
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(if app.input.is_empty() { MUTED } else { Color::White }))
            .block(Block::default().title(" New task  ·  Enter to plan ").borders(Borders::ALL)
                .border_style(Style::default().fg(editor_style)))
            .wrap(Wrap { trim: false }),
        outer[2],
    );

    let footer = Paragraph::new(vec![
        Line::from(" F2 Approve · F3 Run Antigravity · F4 Validate · F5 Review · F6 Stop · F7 Plan · Esc Exit"),
        Line::from(" Tab Focus · ↑↓ Select task · PgUp/PgDn Scroll plan · Ctrl+U Clear input"),
    ])
    .style(Style::default().fg(MUTED));
    frame.render_widget(footer, outer[3]);
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = if app.panel == Panel::Plan { " Approved plan / review with PgUp PgDn " }
        else { " Conversation · workflow " };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(task) = app.current() {
        if app.panel == Panel::Plan {
            let path = task_dir(&app.root, &task.id)
                .map(|folder| folder.join("plan.md"));
            match path.and_then(|p| fs::read_to_string(p).map_err(Into::into)) {
                Ok(contents) => {
                    lines.push(Line::from(Span::styled("Review the plan carefully before approving.", Style::default().fg(GOLD))));
                    for line in contents.lines().take(3000) {
                        lines.push(Line::from(line.to_owned()));
                    }
                }
                Err(_) => lines.push(Line::from("No plan.md available yet.")),
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("You  ", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                Span::raw(task.request.clone()),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Task  ", Style::default().fg(MUTED)),
                Span::raw(task.id.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("State ", Style::default().fg(MUTED)),
                Span::styled(task.state.clone(), Style::default().fg(accent(&task.state))
                    .add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(""));
            let steps = [
                ("1", "Codex", "Build implementation plan", matches!(task.state.as_str(), "planned" | "approved" | "executing" | "executed" | "validating" | "validated" | "reviewing" | "awaiting_human_review")),
                ("2", "You", "Approve plan before coding", matches!(task.state.as_str(), "approved" | "executing" | "executed" | "validating" | "validated" | "reviewing" | "awaiting_human_review")),
                ("3", "Antigravity", "Edit files in isolated worktree", matches!(task.state.as_str(), "executed" | "validating" | "validated" | "reviewing" | "awaiting_human_review")),
                ("4", "Checks", "Run project-specific validation", matches!(task.state.as_str(), "validated" | "reviewing" | "awaiting_human_review")),
                ("5", "Codex", "Review changes; human decides", task.state == "awaiting_human_review"),
            ];
            for (num, role, description, done) in steps {
                lines.push(Line::from(vec![
                    Span::styled(if done { " ✓ " } else { " ○ " },
                        Style::default().fg(if done { GREEN } else { MUTED })),
                    Span::styled(format!("{num}. {role:<12}"), Style::default().fg(Color::White)),
                    Span::raw(description.to_owned()),
                ]));
            }
            if let Some(error) = &task.last_error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("Attention: {}", short(error, 240)),
                    Style::default().fg(Color::LightRed),
                )));
            }
            if task.state == "planned" {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "F7: Read plan  ·  F2 twice: Approve", Style::default().fg(GOLD)
                )));
            }
            if task.state == "executed" {
                lines.push(Line::from(Span::styled(
                    "Edit checks.json in the task artifact folder, then F4.",
                    Style::default().fg(GOLD),
                )));
            }
        }
    } else {
        lines.push(Line::from("Welcome to AgentFlow."));
        lines.push(Line::from(""));
        lines.push(Line::from("Write a request below. Codex creates the plan."));
        lines.push(Line::from("Approve it before Antigravity edits the code."));
        lines.push(Line::from(""));
        lines.push(Line::from("No daemon. No detached jobs. No automatic merge."));
    }

    if app.panel == Panel::Overview {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Recent activity", Style::default().fg(CYAN)
            .add_modifier(Modifier::BOLD))));
        for message in &app.events {
            lines.push(Line::from(format!("  • {message}")));
        }
        if app.quit_requested {
            lines.push(Line::from(Span::styled("Stopping agent before exit...", Style::default().fg(GOLD))));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)))
            .wrap(Wrap { trim: false })
            .scroll((if app.panel == Panel::Plan { app.plan_scroll } else { 0 }, 0)),
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

pub fn run(root: &Path) -> Result<()> {
    terminal::install()?;
    enable_raw_mode().context("could not enter raw terminal mode")?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("could not enter alternate screen");
    }
    let _screen = ScreenGuard;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (tx, rx): (Sender<Finished>, Receiver<Finished>) = mpsc::channel();
    let mut app = App::new(root);
    loop {
        while let Ok(finished) = rx.try_recv() {
            app.handle_finished(finished);
        }
        if terminal::stop_requested() && !app.quit_requested {
            app.request_quit();
        }
        if app.quit_requested && app.active.is_none() {
            break;
        }
        if app.last_refresh.elapsed() > REFRESH {
            app.refresh();
        }
        terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key, &tx),
                Event::Paste(text) if app.focus == Focus::Compose && app.active.is_none() => {
                    for ch in text.chars().filter(|ch| !ch.is_control()) {
                        if app.input.chars().count() >= MAX_INPUT { break; }
                        app.input.push(ch);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_colors_and_phase_classification_are_deterministic() {
        assert!(is_running("planning"));
        assert!(is_running("executing"));
        assert!(!is_running("approved"));
        assert!(!is_running("needs_attention"));
        assert_eq!(accent("validation_failed"), Color::LightRed);
    }

    #[test]
    fn shortened_text_handles_unicode_without_splitting_utf8() {
        assert_eq!(short("Tiếng Việt", 6), "Tiếng…");
        assert_eq!(short("hi", 6), "hi");
    }
}
