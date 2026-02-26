#![allow(clippy::struct_field_names)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{Terminal, layout::Rect, prelude::CrosstermBackend};

use super::event::{self, AppEvent};
use super::ui;
use crate::backend::WorkspaceBackend;
use crate::push::main_sync_status_inner;

// ---------------------------------------------------------------------------
// File tree types
// ---------------------------------------------------------------------------

/// Status of a changed file relative to epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl FileStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
        }
    }

    pub const fn from_char(c: char) -> Self {
        match c {
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            _ => Self::Modified,
        }
    }
}

/// A node in the file tree: either a directory or a file.
#[derive(Debug, Clone)]
pub enum TreeNode {
    Dir {
        name: String,
        children: Vec<Self>,
        collapsed: bool,
    },
    File {
        name: String,
        status: FileStatus,
        full_path: String,
    },
}

impl TreeNode {
    pub fn name(&self) -> &str {
        match self {
            Self::Dir { name, .. } | Self::File { name, .. } => name,
        }
    }

    #[allow(dead_code)]
    pub const fn is_dir(&self) -> bool {
        matches!(self, Self::Dir { .. })
    }
}

/// Build a nested tree from a flat list of (status, path) pairs.
/// Directories are sorted first, then files, both alphabetical.
pub fn build_file_tree(files: &[(FileStatus, String)]) -> Vec<TreeNode> {
    fn build_children(
        dir_path: &str,
        dir_children: &BTreeMap<String, Vec<(String, Option<FileStatus>, String)>>,
    ) -> Vec<TreeNode> {
        let Some(entries) = dir_children.get(dir_path) else {
            return Vec::new();
        };

        let mut dirs: Vec<TreeNode> = Vec::new();
        let mut file_nodes: Vec<TreeNode> = Vec::new();

        // Deduplicate: a name might appear multiple times (once as dir registration).
        let mut seen_dirs: BTreeSet<String> = BTreeSet::new();
        let mut seen_files: BTreeSet<String> = BTreeSet::new();

        for (name, status, full_path) in entries {
            if let Some(st) = status {
                if seen_files.insert(name.clone()) {
                    file_nodes.push(TreeNode::File {
                        name: name.clone(),
                        status: *st,
                        full_path: full_path.clone(),
                    });
                }
            } else if seen_dirs.insert(name.clone()) {
                let children = build_children(full_path, dir_children);
                dirs.push(TreeNode::Dir {
                    name: name.clone(),
                    children,
                    collapsed: false,
                });
            }
        }

        // Sort dirs first alphabetically, then files alphabetically.
        dirs.sort_by(|a, b| a.name().cmp(b.name()));
        file_nodes.sort_by(|a, b| a.name().cmp(b.name()));

        dirs.extend(file_nodes);
        dirs
    }

    // Build an intermediate map: dir_path -> Vec<(name, status, full_path)>
    // and a set of known directories.
    let mut dir_children: BTreeMap<String, Vec<(String, Option<FileStatus>, String)>> =
        BTreeMap::new();
    let mut known_dirs: BTreeSet<String> = BTreeSet::new();

    for (status, path) in files {
        let parts: Vec<&str> = path.split('/').collect();
        // Ensure all ancestor dirs exist
        let mut ancestor = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i < parts.len() - 1 {
                // Directory part
                let parent = ancestor.clone();
                if !ancestor.is_empty() {
                    ancestor.push('/');
                }
                ancestor.push_str(part);
                if known_dirs.insert(ancestor.clone()) {
                    dir_children
                        .entry(parent)
                        .or_default()
                        .push((part.to_string(), None, ancestor.clone()));
                }
            } else {
                // File leaf
                dir_children
                    .entry(ancestor.clone())
                    .or_default()
                    .push((part.to_string(), Some(*status), path.clone()));
            }
        }
    }

    build_children("", &dir_children)
}

/// Flatten a tree into a list of `(depth, &TreeNode)` for rendering.
/// Skips children of collapsed directories.
pub fn flatten_tree(nodes: &[TreeNode], depth: usize) -> Vec<(usize, &TreeNode)> {
    let mut result = Vec::new();
    for node in nodes {
        result.push((depth, node));
        if let TreeNode::Dir {
            children,
            collapsed: false,
            ..
        } = node
        {
            result.extend(flatten_tree(children, depth + 1));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Workspace pane data
// ---------------------------------------------------------------------------

/// Per-workspace data shown in a pane.
#[derive(Debug, Clone)]
pub struct WorkspacePane {
    pub name: String,
    pub commit_count: u32,
    pub last_activity_secs: Option<u64>,
    pub is_stale: bool,
    pub is_dirty: bool,
    pub file_tree: Vec<TreeNode>,
    /// Flat list of file paths (for overlap detection).
    pub file_paths: Vec<String>,
}

/// An overlap between two workspaces on a specific file.
#[derive(Debug, Clone)]
pub struct OverlapEntry {
    pub ws_a: String,
    pub ws_b: String,
    pub path: String,
}

// ---------------------------------------------------------------------------
// Status warnings (from maw status)
// ---------------------------------------------------------------------------

/// A warning condition surfaced in the header bar.
#[derive(Debug, Clone)]
pub enum StatusWarning {
    /// Main branch is not in sync with origin.
    SyncIssue(String),
    /// Stray files at repo root (bare repo should have none).
    StrayRoot(usize),
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Main application state
pub struct App {
    pub workspaces: Vec<WorkspacePane>,
    pub focused_pane: usize,
    /// Selected row within the focused pane's flattened file tree.
    pub selected_row: usize,
    pub should_quit: bool,
    pub show_help: bool,
    pub epoch_hash: String,
    pub branch_name: String,
    pub warnings: Vec<StatusWarning>,
    pub overlaps: Vec<OverlapEntry>,
    /// Set of file paths that overlap (for highlighting).
    pub overlap_paths: HashMap<String, BTreeSet<String>>,
    last_refresh: Instant,
    /// Pane areas for mouse hit testing (updated each frame).
    pub pane_areas: Vec<Rect>,
}

impl App {
    pub fn new() -> Result<Self> {
        let mut app = Self {
            workspaces: Vec::new(),
            focused_pane: 0,
            selected_row: 0,
            should_quit: false,
            show_help: false,
            epoch_hash: String::new(),
            branch_name: String::new(),
            warnings: Vec::new(),
            overlaps: Vec::new(),
            overlap_paths: HashMap::new(),
            last_refresh: Instant::now(),
            pane_areas: Vec::new(),
        };
        app.refresh()?;
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        while !self.should_quit {
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
                AppEvent::Resize { .. } | AppEvent::Tick | AppEvent::Paste(_) => {}
            }

            // Periodic refresh every 2 seconds
            if self.last_refresh.elapsed() > Duration::from_secs(2) {
                self.refresh()?;
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
        // If help is shown, any key closes it
        if self.show_help {
            self.show_help = false;
            return Ok(());
        }

        match code {
            // Quit
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }

            // Help
            KeyCode::Char('?') => self.show_help = true,

            // Refresh
            KeyCode::Char('r') => self.refresh()?,

            // Panel cycling
            KeyCode::Tab => self.cycle_pane(1),
            KeyCode::BackTab => self.cycle_pane(-1),

            // Navigation within focused pane
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => self.selected_row = 0,
            KeyCode::Char('G') => {
                let max = self.flat_len_for_focused().saturating_sub(1);
                self.selected_row = max;
            }

            // Toggle collapse
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_collapse(),

            _ => {}
        }
        Ok(())
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
        if self.show_help {
            return;
        }

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Check which pane was clicked and focus it
                for (i, area) in self.pane_areas.iter().enumerate() {
                    if area.contains((x, y).into()) {
                        self.focused_pane = i;
                        // Calculate which row was clicked (accounting for border + header)
                        let relative_y = y.saturating_sub(area.y + 2);
                        let max = self.flat_len_for_pane(i).saturating_sub(1);
                        self.selected_row = (relative_y as usize).min(max);
                        break;
                    }
                }
            }
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::ScrollDown => self.move_selection(1),
            _ => {}
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::missing_const_for_fn)]
    fn cycle_pane(&mut self, direction: i32) {
        if self.workspaces.is_empty() {
            return;
        }
        let len = self.workspaces.len() as i32;
        self.focused_pane =
            (self.focused_pane as i32 + direction).rem_euclid(len) as usize;
        self.selected_row = 0;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn move_selection(&mut self, direction: i32) {
        let max = self.flat_len_for_focused();
        if max == 0 {
            return;
        }
        let len = max as i32;
        self.selected_row =
            (self.selected_row as i32 + direction).rem_euclid(len) as usize;
    }

    fn flat_len_for_focused(&self) -> usize {
        self.flat_len_for_pane(self.focused_pane)
    }

    fn flat_len_for_pane(&self, pane: usize) -> usize {
        self.workspaces
            .get(pane)
            .map_or(0, |ws| flatten_tree(&ws.file_tree, 0).len())
    }

    /// Toggle collapse on the selected node if it's a directory.
    fn toggle_collapse(&mut self) {
        let Some(ws) = self.workspaces.get_mut(self.focused_pane) else {
            return;
        };
        // Find the nth visible node and toggle it.
        let target = self.selected_row;
        toggle_node_at(&mut ws.file_tree, target, &mut 0);
    }

    // ------------------------------------------------------------------
    // Data fetching
    // ------------------------------------------------------------------

    pub fn refresh(&mut self) -> Result<()> {
        self.fetch_header_info();
        self.workspaces = Self::fetch_workspace_panes()?;
        self.compute_overlaps();
        self.last_refresh = Instant::now();

        // Clamp selections
        if self.focused_pane >= self.workspaces.len() && !self.workspaces.is_empty() {
            self.focused_pane = self.workspaces.len() - 1;
        }
        let max = self.flat_len_for_focused().saturating_sub(1);
        if self.selected_row > max {
            self.selected_row = max;
        }

        Ok(())
    }

    fn fetch_header_info(&mut self) {
        let repo_root = crate::workspace::repo_root().unwrap_or_else(|_| PathBuf::from("."));

        // Branch name from maw config (bare repos return "HEAD" from git rev-parse)
        let branch = if let Ok(config) = crate::workspace::MawConfig::load(&repo_root) {
            let b = config.branch().to_string();
            self.branch_name = b.clone();
            b
        } else {
            String::new()
        };

        // Epoch hash from refs/manifold/epoch/current
        if let Some(output) = Command::new("git")
            .args(["rev-parse", "--short=7", "refs/manifold/epoch/current"])
            .current_dir(&repo_root)
            .output()
            .ok()
            .filter(|o| o.status.success())
        {
            self.epoch_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }

        // Collect warnings from maw status checks
        self.warnings.clear();

        // Main vs origin sync
        if !branch.is_empty() {
            let sync = main_sync_status_inner(&repo_root, &branch);
            if sync.is_warning() {
                self.warnings
                    .push(StatusWarning::SyncIssue(sync.describe()));
            }
        }

        // Stray root files
        let stray = crate::doctor::stray_root_entries(&repo_root);
        if !stray.is_empty() {
            self.warnings.push(StatusWarning::StrayRoot(stray.len()));
        }
    }

    fn fetch_workspace_panes() -> Result<Vec<WorkspacePane>> {
        let backend = crate::workspace::get_backend()?;
        let infos = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;
        let repo_root = crate::workspace::repo_root()?;

        let mut panes = Vec::new();

        for info in &infos {
            let name = info.id.to_string();
            let ws_path = backend.workspace_path(&info.id);
            let is_stale = info.state.is_stale();

            // Get epoch diff: files changed relative to epoch
            let mut all_files = Self::fetch_epoch_diff(&repo_root, &ws_path);

            // Get commit count and last activity
            let (commit_count, last_activity_secs) =
                Self::fetch_commit_info(&repo_root, &ws_path);

            // Get uncommitted changes and merge into file list
            let dirty_files = Self::fetch_dirty_files(&ws_path);
            let is_dirty = !dirty_files.is_empty();
            // Merge dirty files: dirty status overrides epoch status for same path
            let epoch_paths: std::collections::HashSet<String> =
                all_files.iter().map(|(_, p)| p.clone()).collect();
            for (status, path) in dirty_files {
                if !epoch_paths.contains(&path) {
                    all_files.push((status, path));
                }
            }

            let file_paths: Vec<String> = all_files.iter().map(|(_, p)| p.clone()).collect();
            let file_tree = build_file_tree(&all_files);

            panes.push(WorkspacePane {
                name,
                commit_count,
                last_activity_secs,
                is_stale,
                is_dirty,
                file_tree,
                file_paths,
            });
        }

        panes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(panes)
    }

    /// Get files changed between epoch and workspace head.
    fn fetch_epoch_diff(repo_root: &Path, ws_path: &Path) -> Vec<(FileStatus, String)> {
        let output = Command::new("git")
            .args([
                "diff",
                "--name-status",
                "refs/manifold/epoch/current",
                "HEAD",
            ])
            .current_dir(ws_path)
            .env("GIT_DIR", repo_root.join(".git"))
            .output();

        let Ok(output) = output else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut files = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.len() < 2 {
                continue;
            }
            let status_char = line.chars().next().unwrap_or('M');
            // Handle renames: R100\toldpath\tnewpath
            let path = if status_char == 'R' {
                // Take the last tab-separated field (new path)
                line.split('\t').last().unwrap_or("").to_string()
            } else {
                line.split('\t').nth(1).unwrap_or("").to_string()
            };
            if !path.is_empty() {
                files.push((FileStatus::from_char(status_char), path));
            }
        }
        files
    }

    /// Get commit count and last activity (seconds ago) for a workspace.
    fn fetch_commit_info(
        repo_root: &Path,
        ws_path: &Path,
    ) -> (u32, Option<u64>) {
        // Commit count: number of commits between epoch and HEAD
        let count_output = Command::new("git")
            .args([
                "rev-list",
                "--count",
                "refs/manifold/epoch/current..HEAD",
            ])
            .current_dir(ws_path)
            .env("GIT_DIR", repo_root.join(".git"))
            .output();

        let commit_count = count_output
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            })
            .unwrap_or(0);

        // Last activity: time of most recent commit
        let time_output = Command::new("git")
            .args(["log", "-1", "--format=%ct", "HEAD"])
            .current_dir(ws_path)
            .env("GIT_DIR", repo_root.join(".git"))
            .output();

        let last_activity_secs = time_output
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let ts_str = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let ts: u64 = ts_str.parse().ok()?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                Some(now.saturating_sub(ts))
            });

        (commit_count, last_activity_secs)
    }

    /// Get uncommitted changes as (status, path) pairs. Empty vec = clean.
    fn fetch_dirty_files(ws_path: &Path) -> Vec<(FileStatus, String)> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(ws_path)
            .output();

        let Some(output) = output.ok().filter(|o| o.status.success()) else {
            return Vec::new();
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut files = Vec::new();
        for line in stdout.lines() {
            // Porcelain format: XY path (2 status chars + space + path)
            // Do NOT trim_start — the leading space in X position is meaningful.
            if line.len() < 4 {
                continue;
            }
            let xy = &line[..2];
            let path = line[3..].to_string();
            // Pick the most significant status: staged (X) takes priority, else working tree (Y)
            let status_char = {
                let x = xy.as_bytes()[0];
                let y = xy.as_bytes()[1];
                match (x, y) {
                    (b'?', _) => 'A',         // untracked → added
                    (b'D', _) | (_, b'D') => 'D',
                    (b'R', _) => 'R',
                    (b' ', c) | (c, _) => match c {
                        b'A' => 'A',
                        b'D' => 'D',
                        b'R' => 'R',
                        _ => 'M',
                    },
                }
            };
            if !path.is_empty() {
                files.push((FileStatus::from_char(status_char), path));
            }
        }
        files
    }

    /// Compute file-level overlaps across all workspace panes.
    fn compute_overlaps(&mut self) {
        self.overlaps.clear();
        self.overlap_paths.clear();

        let ws_count = self.workspaces.len();
        for i in 0..ws_count {
            for j in (i + 1)..ws_count {
                let a = &self.workspaces[i];
                let b = &self.workspaces[j];
                let a_set: BTreeSet<&str> =
                    a.file_paths.iter().map(String::as_str).collect();

                for path in &b.file_paths {
                    if a_set.contains(path.as_str()) {
                        self.overlaps.push(OverlapEntry {
                            ws_a: a.name.clone(),
                            ws_b: b.name.clone(),
                            path: path.clone(),
                        });
                        // Mark the path as overlapping in both workspaces
                        self.overlap_paths
                            .entry(a.name.clone())
                            .or_default()
                            .insert(path.clone());
                        self.overlap_paths
                            .entry(b.name.clone())
                            .or_default()
                            .insert(path.clone());
                    }
                }
            }
        }
    }
}

/// Format seconds-ago into a human readable string.
pub fn format_time_ago(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Recursively walk the tree to find and toggle the node at a given visible index.
#[allow(clippy::collapsible_if)]
fn toggle_node_at(nodes: &mut [TreeNode], target: usize, counter: &mut usize) -> bool {
    for node in nodes.iter_mut() {
        if *counter == target {
            if let TreeNode::Dir { collapsed, .. } = node {
                *collapsed = !*collapsed;
            }
            return true;
        }
        *counter += 1;

        if let TreeNode::Dir {
            children,
            collapsed: false,
            ..
        } = node
        {
            if toggle_node_at(children, target, counter) {
                return true;
            }
        }
    }
    false
}
