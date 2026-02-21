#![allow(clippy::struct_field_names)]

use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{Terminal, layout::Rect, prelude::CrosstermBackend};

use super::event::{self, AppEvent};
use super::ui;
use crate::backend::WorkspaceBackend;

/// Which panel is currently focused
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Workspaces,
    Commits,
    Issues,
    Details,
}

/// A workspace entry
#[derive(Debug, Clone)]
pub struct Workspace {
    pub name: String,
    pub change_id: String,
    pub description: String,
    pub is_current: bool,
    pub is_stale: bool,
}

/// A commit entry
#[derive(Debug, Clone)]
pub struct Commit {
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
    pub is_immutable: bool,
    pub is_conflict: bool,
    pub is_working_copy: bool,
}

/// A file change entry
#[derive(Debug, Clone)]
pub struct FileChange {
    pub status: char, // M, A, D, R
    pub path: String,
}

/// A beads issue entry
#[derive(Debug, Clone)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub priority: u8,
    pub status: String,
    pub kind: String,
}

/// Active popup state
#[derive(Debug, Clone)]
pub enum Popup {
    /// Create workspace - stores the input text
    CreateWorkspace {
        input: String,
        error: Option<String>,
    },
    /// Confirm destroy workspace
    ConfirmDestroy { name: String },
    /// Operation result message
    Message {
        title: String,
        message: String,
        is_error: bool,
    },
}

/// Main application state
pub struct App {
    pub workspaces: Vec<Workspace>,
    pub commits: Vec<Commit>,
    pub file_changes: Vec<FileChange>,
    pub issues: Vec<Issue>,
    pub selected_workspace: usize,
    pub selected_commit: usize,
    pub selected_issue: usize,
    pub focused_panel: Panel,
    pub should_quit: bool,
    pub show_help: bool,
    pub popup: Option<Popup>,
    pub command_log: Vec<String>,
    pub beads_available: bool,
    last_refresh: Instant,
    // Panel areas for mouse hit testing (updated each frame)
    pub panel_areas: PanelAreas,
}

/// Stores the screen areas of each panel for mouse hit testing
#[derive(Debug, Clone, Default)]
pub struct PanelAreas {
    pub workspaces: Rect,
    pub commits: Rect,
    pub issues: Rect,
    pub details: Rect,
}

impl App {
    fn short_oid(oid: &str) -> String {
        oid.chars().take(12).collect()
    }

    pub fn new() -> Result<Self> {
        // Check if beads is available
        let beads_available = std::path::Path::new(".beads").exists();

        let mut app = Self {
            workspaces: Vec::new(),
            commits: Vec::new(),
            file_changes: Vec::new(),
            issues: Vec::new(),
            selected_workspace: 0,
            selected_commit: 0,
            selected_issue: 0,
            focused_panel: Panel::Workspaces,
            should_quit: false,
            show_help: false,
            popup: None,
            command_log: Vec::new(),
            beads_available,
            last_refresh: Instant::now(),
            panel_areas: PanelAreas::default(),
        };
        app.refresh()?;
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        while !self.should_quit {
            // Update panel areas for mouse hit testing
            let size = terminal.size()?;
            self.update_panel_areas(Rect::new(0, 0, size.width, size.height));

            // Draw UI
            terminal.draw(|frame| ui::draw(frame, self))?;

            // Handle one event with timeout for periodic refresh.
            match event::next_event(Duration::from_millis(100))? {
                AppEvent::Key(key) => {
                    self.handle_key(key.code, key.modifiers)?;
                }
                AppEvent::Mouse(mouse) => {
                    self.handle_mouse(mouse.kind, mouse.column, mouse.row);
                }
                AppEvent::Resize { .. } | AppEvent::Tick => {}
                AppEvent::Paste(text) => {
                    // Pasted text is only accepted by popup text inputs.
                    for ch in text.chars() {
                        self.handle_key(KeyCode::Char(ch), KeyModifiers::NONE)?;
                    }
                }
            }

            // Periodic refresh every 2 seconds
            if self.last_refresh.elapsed() > Duration::from_secs(2) {
                self.refresh()?;
            }
        }
        Ok(())
    }

    /// Calculate panel areas based on terminal size (must match ui.rs layout)
    fn update_panel_areas(&mut self, size: Rect) {
        use ratatui::layout::{Constraint, Direction, Layout};

        // Top-level: main area + status bar (1 line)
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(size);
        let main_area = outer[0];

        // Main: left 40% | right 60%
        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(main_area);

        // Left: workspaces 40% | commits 40% | issues 20%
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Percentage(40),
                Constraint::Percentage(20),
            ])
            .split(main[0]);

        // Right: details 70% | command log 30%
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(main[1]);

        self.panel_areas = PanelAreas {
            workspaces: left[0],
            commits: left[1],
            issues: left[2],
            details: right[0],
        };
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
        // If help is shown, any key closes it
        if self.show_help {
            self.show_help = false;
            return Ok(());
        }

        // Handle popup input
        if let Some(popup) = &self.popup.clone() {
            return self.handle_popup_key(code, modifiers, popup);
        }

        match code {
            // Quit
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }

            // Help
            KeyCode::Char('?') => self.show_help = true,

            // Panel switching
            KeyCode::Char('1') => self.focused_panel = Panel::Workspaces,
            KeyCode::Char('2') => self.focused_panel = Panel::Commits,
            KeyCode::Char('3') => self.focused_panel = Panel::Issues,
            KeyCode::Char('0') => self.focused_panel = Panel::Details,
            KeyCode::Tab => self.cycle_panel(1),
            KeyCode::BackTab => self.cycle_panel(-1),

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => self.go_to_top(),
            KeyCode::Char('G') => self.go_to_bottom(),

            // Refresh
            KeyCode::Char('r') => self.refresh()?,

            // Workspace actions (only when workspaces panel focused)
            KeyCode::Char('c') if self.focused_panel == Panel::Workspaces => {
                self.popup = Some(Popup::CreateWorkspace {
                    input: String::new(),
                    error: None,
                });
            }
            KeyCode::Char('d') if self.focused_panel == Panel::Workspaces => {
                if let Some(ws) = self.workspaces.get(self.selected_workspace) {
                    if ws.is_current {
                        self.popup = Some(Popup::Message {
                            title: "Cannot Destroy".to_string(),
                            message: "Cannot destroy the current workspace".to_string(),
                            is_error: true,
                        });
                    } else {
                        self.popup = Some(Popup::ConfirmDestroy {
                            name: ws.name.clone(),
                        });
                    }
                }
            }
            KeyCode::Char('s') if self.focused_panel == Panel::Workspaces => {
                self.sync_workspace()?;
            }
            KeyCode::Char('m') if self.focused_panel == Panel::Workspaces => {
                self.merge_selected_workspace()?;
            }
            KeyCode::Char('M') if self.focused_panel == Panel::Workspaces => {
                self.merge_all_workspaces()?;
            }

            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::option_if_let_else)]
    fn handle_popup_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        popup: &Popup,
    ) -> Result<()> {
        match popup {
            Popup::CreateWorkspace { input, .. } => {
                let new_input = Self::handle_text_input(input, code, modifiers);
                match code {
                    KeyCode::Esc => self.popup = None,
                    KeyCode::Enter => {
                        if input.is_empty() {
                            self.popup = Some(Popup::CreateWorkspace {
                                input: input.clone(),
                                error: Some("Name cannot be empty".to_string()),
                            });
                        } else {
                            let name = input.clone();
                            self.popup = None;
                            self.create_workspace(&name)?;
                        }
                    }
                    _ => {
                        if let Some(new_input) = new_input {
                            self.popup = Some(Popup::CreateWorkspace {
                                input: new_input,
                                error: None,
                            });
                        }
                    }
                }
            }
            Popup::ConfirmDestroy { name } => match code {
                KeyCode::Char('y' | 'Y') => {
                    let name = name.clone();
                    self.popup = None;
                    self.destroy_workspace(&name)?;
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.popup = None;
                }
                _ => {}
            },
            Popup::Message { .. } => {
                // Any key closes message popup
                self.popup = None;
            }
        }
        Ok(())
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
        // Ignore mouse events when popup is open
        if self.popup.is_some() || self.show_help {
            return;
        }

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Check which panel was clicked and focus it
                if self.panel_areas.workspaces.contains((x, y).into()) {
                    self.focused_panel = Panel::Workspaces;
                    // Calculate which item was clicked (accounting for border)
                    let relative_y = y.saturating_sub(self.panel_areas.workspaces.y + 1);
                    if (relative_y as usize) < self.workspaces.len() {
                        self.selected_workspace = relative_y as usize;
                    }
                } else if self.panel_areas.commits.contains((x, y).into()) {
                    self.focused_panel = Panel::Commits;
                    let relative_y = y.saturating_sub(self.panel_areas.commits.y + 1);
                    if (relative_y as usize) < self.commits.len() {
                        self.selected_commit = relative_y as usize;
                    }
                } else if self.panel_areas.issues.contains((x, y).into()) {
                    self.focused_panel = Panel::Issues;
                    let relative_y = y.saturating_sub(self.panel_areas.issues.y + 1);
                    if (relative_y as usize) < self.issues.len() {
                        self.selected_issue = relative_y as usize;
                    }
                } else if self.panel_areas.details.contains((x, y).into()) {
                    self.focused_panel = Panel::Details;
                }
            }
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
            }
            _ => {}
        }
    }

    /// Handle text input with common terminal shortcuts
    /// Returns `Some(new_string)` if input was modified, None otherwise
    #[allow(clippy::option_if_let_else)]
    fn handle_text_input(current: &str, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);

        match code {
            // Basic editing
            KeyCode::Backspace if ctrl => {
                // Ctrl+Backspace / Ctrl+W: delete word backward
                let trimmed = current.trim_end();
                if let Some(last_space) = trimmed.rfind(' ') {
                    Some(trimmed[..=last_space].to_string())
                } else {
                    Some(String::new())
                }
            }
            KeyCode::Backspace => {
                let mut s = current.to_string();
                s.pop();
                Some(s)
            }
            KeyCode::Char('w') if ctrl => {
                // Ctrl+W: delete word backward
                let trimmed = current.trim_end();
                if let Some(last_space) = trimmed.rfind(' ') {
                    Some(trimmed[..=last_space].to_string())
                } else {
                    Some(String::new())
                }
            }
            KeyCode::Char('u') if ctrl => {
                // Ctrl+U: clear line
                Some(String::new())
            }
            KeyCode::Char('a') if ctrl => {
                // Ctrl+A: move to beginning (we don't have cursor, so no-op)
                None
            }
            KeyCode::Char('e') if ctrl => {
                // Ctrl+E: move to end (we don't have cursor, so no-op)
                None
            }
            KeyCode::Char('k') if ctrl => {
                // Ctrl+K: kill to end of line (we're always at end, so no-op)
                None
            }
            KeyCode::Char(c) if !ctrl => {
                // Regular character input
                Some(format!("{current}{c}"))
            }
            _ => None,
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn cycle_panel(&mut self, direction: i32) {
        let panels = [
            Panel::Workspaces,
            Panel::Commits,
            Panel::Issues,
            Panel::Details,
        ];
        let current = panels
            .iter()
            .position(|&p| p == self.focused_panel)
            .unwrap_or(0);
        let next = (current as i32 + direction).rem_euclid(panels.len() as i32) as usize;
        self.focused_panel = panels[next];
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    const fn move_selection(&mut self, direction: i32) {
        match self.focused_panel {
            Panel::Workspaces if !self.workspaces.is_empty() => {
                let len = self.workspaces.len() as i32;
                self.selected_workspace =
                    (self.selected_workspace as i32 + direction).rem_euclid(len) as usize;
            }
            Panel::Commits if !self.commits.is_empty() => {
                let len = self.commits.len() as i32;
                self.selected_commit =
                    (self.selected_commit as i32 + direction).rem_euclid(len) as usize;
            }
            Panel::Issues if !self.issues.is_empty() => {
                let len = self.issues.len() as i32;
                self.selected_issue =
                    (self.selected_issue as i32 + direction).rem_euclid(len) as usize;
            }
            Panel::Workspaces | Panel::Commits | Panel::Issues | Panel::Details => {}
        }
    }

    const fn go_to_top(&mut self) {
        match self.focused_panel {
            Panel::Workspaces => self.selected_workspace = 0,
            Panel::Commits => self.selected_commit = 0,
            Panel::Issues => self.selected_issue = 0,
            Panel::Details => {}
        }
    }

    const fn go_to_bottom(&mut self) {
        match self.focused_panel {
            Panel::Workspaces if !self.workspaces.is_empty() => {
                self.selected_workspace = self.workspaces.len() - 1;
            }
            Panel::Commits if !self.commits.is_empty() => {
                self.selected_commit = self.commits.len() - 1;
            }
            Panel::Issues if !self.issues.is_empty() => {
                self.selected_issue = self.issues.len() - 1;
            }
            Panel::Workspaces | Panel::Commits | Panel::Issues | Panel::Details => {}
        }
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.workspaces = self.fetch_workspaces()?;
        self.commits = self.fetch_commits()?;
        self.file_changes = self.fetch_file_changes()?;
        if self.beads_available {
            self.issues = self.fetch_issues()?;
        }
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn fetch_workspaces(&mut self) -> Result<Vec<Workspace>> {
        self.log_command("maw workspace backend list/status");

        let backend = crate::workspace::get_backend()?;
        let infos = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut workspaces = Vec::new();

        for info in infos {
            let status = backend
                .status(&info.id)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let name = info.id.to_string();
            let is_current = cwd.starts_with(&info.path);
            let description = info.path.display().to_string();
            workspaces.push(Workspace {
                name,
                change_id: Self::short_oid(info.epoch.as_str()),
                description,
                is_current,
                is_stale: status.is_stale,
            });
        }

        workspaces.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(workspaces)
    }

    fn fetch_file_changes(&mut self) -> Result<Vec<FileChange>> {
        self.log_command("git status --short");

        let git_cwd = crate::workspace::git_cwd()?;
        let output = Command::new("git")
            .args(["status", "--short"])
            .current_dir(&git_cwd)
            .output()
            .context("Failed to run git status --short")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut changes = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.len() >= 3 {
                let mut chars = line.chars();
                let x = chars.next().unwrap_or(' ');
                let y = chars.next().unwrap_or(' ');
                let status = if x == ' ' { y } else { x };
                let path = line[2..].trim().to_string();
                changes.push(FileChange { status, path });
            }
        }

        Ok(changes)
    }

    fn fetch_commits(&mut self) -> Result<Vec<Commit>> {
        self.log_command("git log --oneline");

        let git_cwd = crate::workspace::git_cwd()?;
        let output = Command::new("git")
            .args([
                "log",
                "--all",
                "--decorate=short",
                "--pretty=format:%h%x00%H%x00%D%x00%s",
                "-n",
                "200",
            ])
            .current_dir(&git_cwd)
            .output()
            .context("Failed to run git log")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut commits = Vec::new();

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\x00').collect();
            if parts.len() >= 4 {
                let decorations = parts[2];
                commits.push(Commit {
                    change_id: parts[0].to_string(),
                    commit_id: parts[1].to_string(),
                    is_immutable: decorations.contains("tag:"),
                    is_conflict: false,
                    is_working_copy: decorations.contains("HEAD ->"),
                    description: parts[3].to_string(),
                });
            }
        }

        Ok(commits)
    }

    fn fetch_issues(&mut self) -> Result<Vec<Issue>> {
        self.log_command("br list --format=json");

        let output = Command::new("br")
            .args(["list", "--format=json"])
            .output()
            .context("Failed to run br list")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Parse JSON manually to avoid adding serde dependency
        // Format: [{"id":"bd-xxx","title":"...","priority":1,"status":"open",...},...]
        let mut issues = Vec::new();

        // Simple JSON parsing - find each object
        for obj in stdout.split("},") {
            let obj = obj.trim_start_matches('[').trim_end_matches(']');
            if obj.is_empty() {
                continue;
            }

            #[allow(clippy::option_if_let_else)]
            let get_field = |name: &str| -> Option<String> {
                let key = format!("\"{name}\":");
                let start = obj.find(&key)? + key.len();
                let rest = &obj[start..];
                if let Some(inner) = rest.strip_prefix('"') {
                    // String value
                    let end = inner.find('"')?;
                    Some(inner[..end].to_string())
                } else {
                    // Numeric value
                    let end = rest.find([',', '}'])?;
                    Some(rest[..end].trim().to_string())
                }
            };

            if let (Some(id), Some(title), Some(priority), Some(status), Some(kind)) = (
                get_field("id"),
                get_field("title"),
                get_field("priority"),
                get_field("status"),
                get_field("issue_type"),
            ) {
                issues.push(Issue {
                    id,
                    title,
                    priority: priority.parse().unwrap_or(5),
                    status,
                    kind,
                });
            }
        }

        Ok(issues)
    }

    fn log_command(&mut self, cmd: &str) {
        self.command_log.push(format!("> {cmd}"));
        if self.command_log.len() > 50 {
            self.command_log.remove(0);
        }
    }

    // Workspace actions

    fn create_workspace(&mut self, name: &str) -> Result<()> {
        self.log_command(&format!("maw ws create {name}"));

        let output = Command::new("maw")
            .args(["ws", "create", name])
            .output()
            .context("Failed to run maw ws create")?;

        if output.status.success() {
            self.popup = Some(Popup::Message {
                title: "Success".to_string(),
                message: format!("Created workspace '{name}'"),
                is_error: false,
            });
            self.refresh()?;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            self.popup = Some(Popup::Message {
                title: "Error".to_string(),
                message: stderr.trim().to_string(),
                is_error: true,
            });
        }
        Ok(())
    }

    fn destroy_workspace(&mut self, name: &str) -> Result<()> {
        self.log_command(&format!("maw ws destroy {name} --force"));

        let output = Command::new("maw")
            .args(["ws", "destroy", name, "--force"])
            .output()
            .context("Failed to run maw ws destroy")?;

        if output.status.success() {
            self.popup = Some(Popup::Message {
                title: "Success".to_string(),
                message: format!("Destroyed workspace '{name}'"),
                is_error: false,
            });
            // Adjust selection if needed
            if self.selected_workspace >= self.workspaces.len().saturating_sub(1) {
                self.selected_workspace = self.selected_workspace.saturating_sub(1);
            }
            self.refresh()?;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            self.popup = Some(Popup::Message {
                title: "Error".to_string(),
                message: stderr.trim().to_string(),
                is_error: true,
            });
        }
        Ok(())
    }

    fn sync_workspace(&mut self) -> Result<()> {
        self.log_command("maw ws sync");

        let output = Command::new("maw")
            .args(["ws", "sync"])
            .output()
            .context("Failed to run maw ws sync")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            self.popup = Some(Popup::Message {
                title: "Sync".to_string(),
                message: stdout.trim().to_string(),
                is_error: false,
            });
            self.refresh()?;
        } else {
            self.popup = Some(Popup::Message {
                title: "Error".to_string(),
                message: stderr.trim().to_string(),
                is_error: true,
            });
        }
        Ok(())
    }

    fn merge_selected_workspace(&mut self) -> Result<()> {
        if let Some(ws) = self.workspaces.get(self.selected_workspace) {
            if ws.is_current {
                self.popup = Some(Popup::Message {
                    title: "Cannot Merge".to_string(),
                    message: "Cannot merge the current workspace into itself".to_string(),
                    is_error: true,
                });
                return Ok(());
            }

            let name = ws.name.clone();
            self.log_command(&format!("maw ws merge {name}"));

            let output = Command::new("maw")
                .args(["ws", "merge", &name])
                .output()
                .context("Failed to run maw ws merge")?;

            if output.status.success() {
                self.popup = Some(Popup::Message {
                    title: "Success".to_string(),
                    message: format!("Merged workspace '{name}'"),
                    is_error: false,
                });
                self.refresh()?;
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                self.popup = Some(Popup::Message {
                    title: "Error".to_string(),
                    message: stderr.trim().to_string(),
                    is_error: true,
                });
            }
        }
        Ok(())
    }

    fn merge_all_workspaces(&mut self) -> Result<()> {
        let names: Vec<String> = self
            .workspaces
            .iter()
            .filter(|ws| !ws.is_current)
            .map(|ws| ws.name.clone())
            .collect();

        if names.is_empty() {
            self.popup = Some(Popup::Message {
                title: "No Workspaces".to_string(),
                message: "No other workspaces to merge".to_string(),
                is_error: true,
            });
            return Ok(());
        }

        let cmd_str = format!("maw ws merge {}", names.join(" "));
        self.log_command(&cmd_str);

        let mut args: Vec<&str> = vec!["ws", "merge"];
        for name in &names {
            args.push(name);
        }

        let output = Command::new("maw")
            .args(&args)
            .output()
            .context("Failed to run maw ws merge")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            self.popup = Some(Popup::Message {
                title: "Success".to_string(),
                message: stdout.trim().to_string(),
                is_error: false,
            });
            self.refresh()?;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            self.popup = Some(Popup::Message {
                title: "Error".to_string(),
                message: stderr.trim().to_string(),
                is_error: true,
            });
        }
        Ok(())
    }
}
