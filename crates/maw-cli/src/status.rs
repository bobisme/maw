use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;
use maw_git::GitRepo as _;

use crate::changes::store::ChangesStore;
use crate::doctor;
use crate::format::OutputFormat;
use crate::push::{SyncStatus, main_sync_status_inner};
use crate::workspace::lifecycle::{LifecycleSignals, LifecycleState};
use crate::workspace::{self, MawConfig, get_backend};
use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceState;
use serde::Serialize;

const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const ANSI_ORANGE: &str = "\x1b[38;5;208m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_LIGHT_RED: &str = "\x1b[91m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RESET: &str = "\x1b[0m";

/// Brief repo/workspace status
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct StatusArgs {
    /// Terse, statusbar-friendly output
    #[arg(long)]
    pub oneline: bool,

    /// Status-bar output (face + workspace count + sync warning)
    #[arg(long, alias = "statusbar")]
    pub status_bar: bool,

    /// Use mouth emoji instead of faces
    #[arg(long)]
    pub mouth: bool,

    /// Refresh output every couple seconds
    #[arg(long)]
    pub watch: bool,

    /// Output format: text, json, pretty (auto-detected from TTY)
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

/// # Errors
///
/// Returns an error if status collection or rendering fails.
pub fn run(args: &StatusArgs) -> Result<()> {
    // Fast path: --status-bar skips expensive operations (gix status walk,
    // ChangesStore, full backend init) and uses lightweight alternatives.
    // This keeps starship prompt updates under ~50ms. (bn-2fk0 follow-up)
    if args.status_bar {
        let summary = collect_status_fast()?;
        print!("{}", summary.render_status_bar(args.mouth));
        return Ok(());
    }

    if args.oneline {
        let summary = collect_status()?;
        render(
            &summary,
            &RenderOptions {
                oneline: true,
                status_bar: false,
                mouth: args.mouth,
                watching: false,
            },
        );
        return Ok(());
    }

    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));

    // JSON mode: serialize and exit (no watch)
    if format == OutputFormat::Json {
        let summary = collect_status()?;
        let envelope = StatusEnvelope {
            workspaces: summary.workspace_names.clone(),
            workspace_details: summary.workspace_details.clone(),
            changes: summary.changes.clone(),
            open_changes: summary.changes.len(),
            changed_files: summary.changed_files.clone(),
            untracked_files: summary.untracked_files.clone(),
            is_stale: summary.is_stale,
            main_sync: summary.main_sync.oneline(),
            stray_root_files: summary.stray_root_files,
            advice: vec![],
            current_workspace: summary.current_workspace.clone(),
            current_workspace_state: summary.current_workspace_state,
            stale_workspaces: summary.stale_workspaces.clone(),
            integrate_ready: summary.integrate_ready,
        };
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    // Watch mode (text/pretty only)
    if args.watch {
        watch_loop(args)?;
    } else {
        let summary = collect_status()?;
        render_with_format(&summary, format, false)?;
    }

    Ok(())
}

fn watch_loop(args: &StatusArgs) -> Result<()> {
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    crossterm::execute!(io::stdout(), cursor::Hide).ok();

    let result = watch_loop_inner(args);

    // Always restore terminal state
    crossterm::execute!(io::stdout(), cursor::Show).ok();
    terminal::disable_raw_mode().ok();

    result
}

fn watch_loop_inner(args: &StatusArgs) -> Result<()> {
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));

    loop {
        let summary = collect_status()?;
        render_with_format(&summary, format, true)?;

        // Poll for quit keys during the sleep interval
        let deadline = std::time::Instant::now() + WATCH_INTERVAL;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if event::poll(remaining.min(Duration::from_millis(100)))?
                && let Event::Key(key) = event::read()?
            {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

#[derive(Serialize)]
struct StatusEnvelope {
    workspaces: Vec<String>,
    workspace_details: Vec<WorkspaceStatusItem>,
    changes: Vec<ChangeStatusItem>,
    open_changes: usize,
    changed_files: Vec<String>,
    untracked_files: Vec<String>,
    is_stale: bool,
    main_sync: String,
    stray_root_files: Vec<String>,
    advice: Vec<serde_json::Value>,
    /// bn-221b (SG4 / stale-state-self-healing): name of the workspace
    /// the cwd is currently inside, or `null` if the cwd is at the
    /// repo root (or outside any workspace). Lets the agent confirm
    /// "where am I" without a separate `maw ws status` call.
    #[serde(skip_serializing_if = "Option::is_none")]
    current_workspace: Option<String>,
    /// Lifecycle state of the cwd workspace (`current_workspace`),
    /// using the safe-cleanup vocabulary from the bn-221b mitigation
    /// class. Absent when `current_workspace` is null or the workspace
    /// could not be resolved through the backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    current_workspace_state: Option<LifecycleState>,
    /// bn-221b: every non-default workspace whose base epoch trails
    /// the current authoritative epoch. Each item carries its own
    /// `fix_command` so the agent's first attempt is the right one
    /// instead of a `maw ws merge → stale error → maw ws sync` cycle.
    /// Empty list when no workspace is stale.
    stale_workspaces: Vec<StaleWorkspace>,
    /// bn-221b: every non-default workspace with committed work that
    /// is not yet on the integration branch. Mirrors the "what to
    /// integrate" leg of the mitigation class.
    integrate_ready: Vec<IntegrateReady>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceStatusItem {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// bn-221b: short, stable lifecycle state for the workspace —
    /// `clean | dirty-uncommitted | committed-unintegrated | stale |
    /// conflicted | missing | integrated`. Lets agents branch on a
    /// named value without parsing free-text `state` strings.
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_state: Option<LifecycleState>,
    /// bn-221b: 12-char prefix of the workspace's base epoch OID.
    /// Mirrors what `maw ws list` shows so agents can correlate
    /// across the two outputs without an extra call.
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<String>,
    /// bn-221b: number of epoch advances the workspace is behind the
    /// current epoch (0 when not stale).
    #[serde(skip_serializing_if = "Option::is_none")]
    behind_epochs: Option<u32>,
    /// bn-221b: count of commits on the workspace HEAD ahead of its
    /// base epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    commits_ahead: Option<u32>,
    /// bn-221b: recommended next action for the workspace based on
    /// its lifecycle state. `null` for `clean`/`integrated`. Lets the
    /// agent paste rather than synthesize.
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_command: Option<String>,
}

/// bn-221b: structured stale-workspace summary embedded in
/// `StatusEnvelope::stale_workspaces`. Carries everything an agent
/// needs to resolve the staleness in a single follow-up command.
#[derive(Debug, Clone, Serialize)]
struct StaleWorkspace {
    name: String,
    behind_epochs: u32,
    mode: String,
    fix_command: String,
}

/// bn-221b: structured integrate-ready summary embedded in
/// `StatusEnvelope::integrate_ready`. Workspaces that have committed
/// work the agent might want to merge before doing anything else.
#[derive(Debug, Clone, Serialize)]
struct IntegrateReady {
    name: String,
    commits_ahead: u32,
    fix_command: String,
}

impl WorkspaceStatusItem {
    fn display(&self) -> String {
        self.branch.as_deref().map_or_else(
            || self.name.clone(),
            |branch| format!("{} [branch: {branch}]", self.name),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
struct ChangeStatusItem {
    change_id: String,
    branch: String,
    pr_number: Option<u64>,
    pr_state: Option<String>,
    pr_draft: Option<bool>,
}

impl ChangeStatusItem {
    fn display(&self) -> String {
        let mut s = format!("{} ({})", self.change_id, self.branch);
        match (self.pr_number, self.pr_state.as_deref(), self.pr_draft) {
            (Some(number), Some(state), Some(is_draft)) => {
                if is_draft {
                    let _ = write!(s, ", PR #{number} {state} draft");
                } else {
                    let _ = write!(s, ", PR #{number} {state}");
                }
            }
            (Some(number), Some(state), None) => {
                let _ = write!(s, ", PR #{number} {state}");
            }
            (Some(number), None, _) => {
                let _ = write!(s, ", PR #{number}");
            }
            _ => s.push_str(", no PR"),
        }
        s
    }
}

#[derive(Debug)]
struct StatusSummary {
    workspace_names: Vec<String>,
    workspace_details: Vec<WorkspaceStatusItem>,
    changes: Vec<ChangeStatusItem>,
    changed_files: Vec<String>,
    untracked_files: Vec<String>,
    is_stale: bool,
    main_sync: SyncStatus,
    stray_root_files: Vec<String>,
    /// bn-221b: cwd-detected workspace name (None when at repo root).
    current_workspace: Option<String>,
    /// bn-221b: lifecycle state of `current_workspace`.
    current_workspace_state: Option<LifecycleState>,
    /// bn-221b: stale non-default workspaces with their fix commands.
    stale_workspaces: Vec<StaleWorkspace>,
    /// bn-221b: workspaces with committed work ready to integrate.
    integrate_ready: Vec<IntegrateReady>,
}

impl StatusSummary {
    const fn issue_count(&self) -> usize {
        let mut count = 0;
        if !self.workspace_names.is_empty() {
            count += 1;
        }
        if !self.changed_files.is_empty() {
            count += 1;
        }
        if self.is_stale {
            count += 1;
        }
        if self.main_sync.is_warning() {
            count += 1;
        }
        count
    }

    fn render_oneline(&self) -> String {
        let check = green_check();
        let warn = yellow_warn();
        let stray = self.stray_root_files.len();
        let ws = self.workspace_names.len();
        let change_sets = self.changes.len();
        let changes = self.changed_files.len();
        let untracked = self.untracked_files.len();
        let mut parts = Vec::new();
        if stray > 0 {
            parts.push(format!("root-extra={stray}"));
        }
        parts.push(format!("ws={ws}{}", if ws == 0 { &check } else { &warn }));
        parts.push(format!("chgsets={change_sets}{check}"));
        parts.push(format!(
            "changes={changes}{}",
            if changes == 0 { &check } else { &warn }
        ));
        parts.push(format!(
            "untracked={untracked}{}",
            if untracked == 0 { &check } else { &warn }
        ));
        parts.push(format!(
            "main={}{}",
            self.main_sync.oneline(),
            if matches!(self.main_sync, SyncStatus::UpToDate) {
                &check
            } else {
                &warn
            }
        ));
        parts.push(format!(
            "default={}{}",
            if self.is_stale { "stale" } else { "fresh" },
            if self.is_stale { &warn } else { &check }
        ));

        format!("{}\n", parts.join(" "))
    }

    /// Plain text format — same structure as pretty but with [OK]/[WARN] instead of colored glyphs
    fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str("=== maw status ===\n");

        if !self.stray_root_files.is_empty() {
            let n = self.stray_root_files.len();
            let _ = writeln!(
                out,
                "[INFO] Root extras: {n} non-structural item(s) at repo root; run maw doctor for details"
            );
            for name in &self.stray_root_files {
                let _ = writeln!(out, "  - {name}");
            }
        }

        let ws_count = self.workspace_names.len();
        out.push_str(&text_status_line(
            "Non-default workspaces",
            &if ws_count == 0 {
                "none".to_string()
            } else {
                ws_count.to_string()
            },
            ws_count == 0,
        ));
        for workspace in self.workspace_display_items() {
            let _ = writeln!(out, "  - {workspace}");
        }

        let change_count = self.changes.len();
        out.push_str(&text_status_line(
            "Open changes",
            &if change_count == 0 {
                "none".to_string()
            } else {
                change_count.to_string()
            },
            true,
        ));
        for change in &self.changes {
            let _ = writeln!(out, "  - {}", change.display());
        }

        let change_count = self.changed_files.len();
        out.push_str(&text_status_line(
            "Working copy",
            &if change_count == 0 {
                "clean".to_string()
            } else {
                format!("{change_count} changed files")
            },
            change_count == 0,
        ));
        for file in &self.changed_files {
            let _ = writeln!(out, "  - {file}");
        }

        let untracked_count = self.untracked_files.len();
        out.push_str(&text_status_line(
            "Untracked files",
            &if untracked_count == 0 {
                "none".to_string()
            } else {
                untracked_count.to_string()
            },
            untracked_count == 0,
        ));
        for file in &self.untracked_files {
            let _ = writeln!(out, "  - {file}");
        }

        out.push_str(&text_status_line(
            "Main vs origin",
            &self.main_sync.describe(),
            matches!(self.main_sync, SyncStatus::UpToDate),
        ));

        out.push_str(&text_status_line(
            "Default workspace",
            &if self.is_stale {
                "stale (run: maw ws sync)".to_string()
            } else {
                "fresh".to_string()
            },
            !self.is_stale,
        ));

        out
    }

    fn render_status_bar(&self, mouth: bool) -> String {
        let issues = self.issue_count();
        let face = if mouth {
            "\u{1f444}"
        } else if issues == 0 {
            "\u{1f600}"
        } else if issues == 1 {
            "\u{1f62c}"
        } else {
            "\u{1f620}"
        };

        let mut out = String::new();
        out.push_str(face);

        let ws = self.workspace_names.len();
        if ws > 0 {
            let workspace = format!("\u{f0645} {ws}");
            let colored = colorize_orange(&workspace);
            out.push_str(&colored);
        }

        let changes = self.changed_files.len();
        if changes > 0 {
            let changes = format!("\u{eb43} {changes}");
            let colored = colorize_blue(&changes);
            out.push_str(&colored);
        }

        let untracked = self.untracked_files.len();
        if untracked > 0 {
            let untracked = format!("?{untracked}");
            out.push_str(&untracked);
        }

        if self.main_sync.is_warning() {
            let warning = colorize_light_red("\u{e726}");
            out.push_str(&warning);
        }

        format!("{out}\n")
    }

    fn render_multiline(&self) -> String {
        let mut out = String::new();
        out.push_str("=== maw status ===\n");

        if !self.stray_root_files.is_empty() {
            let n = self.stray_root_files.len();
            let _ = writeln!(
                out,
                "{}: {n} non-structural item(s) at repo root; run maw doctor for details",
                colorize_blue("Root extras"),
            );
            for name in &self.stray_root_files {
                let _ = writeln!(out, "  - {name}");
            }
        }

        let ws_count = self.workspace_names.len();
        out.push_str(&status_line(
            "Non-default workspaces",
            &if ws_count == 0 {
                "none".to_string()
            } else {
                ws_count.to_string()
            },
            ws_count == 0,
        ));
        for workspace in self.workspace_display_items() {
            let _ = writeln!(out, "  - {workspace}");
        }

        let change_count = self.changes.len();
        out.push_str(&status_line(
            "Open changes",
            &if change_count == 0 {
                "none".to_string()
            } else {
                change_count.to_string()
            },
            true,
        ));
        for change in &self.changes {
            let _ = writeln!(out, "  - {}", change.display());
        }

        let change_count = self.changed_files.len();
        out.push_str(&status_line(
            "Working copy",
            &if change_count == 0 {
                "clean".to_string()
            } else {
                format!("{change_count} changed files")
            },
            change_count == 0,
        ));
        for file in &self.changed_files {
            let _ = writeln!(out, "  - {file}");
        }

        let untracked_count = self.untracked_files.len();
        out.push_str(&status_line(
            "Untracked files",
            &if untracked_count == 0 {
                "none".to_string()
            } else {
                untracked_count.to_string()
            },
            untracked_count == 0,
        ));
        for file in &self.untracked_files {
            let _ = writeln!(out, "  - {file}");
        }

        out.push_str(&status_line(
            "Main vs origin",
            &self.main_sync.describe(),
            matches!(self.main_sync, SyncStatus::UpToDate),
        ));

        out.push_str(&status_line(
            "Default workspace",
            &if self.is_stale {
                "stale (run: maw ws sync)".to_string()
            } else {
                "fresh".to_string()
            },
            !self.is_stale,
        ));

        out
    }

    fn workspace_display_items(&self) -> Vec<String> {
        if self.workspace_details.is_empty() {
            return self.workspace_names.clone();
        }
        self.workspace_details
            .iter()
            .map(WorkspaceStatusItem::display)
            .collect()
    }
}

fn colorize_orange(value: &str) -> String {
    format!("{ANSI_ORANGE}{value}{ANSI_RESET}")
}

fn colorize_blue(value: &str) -> String {
    format!("{ANSI_BLUE}{value}{ANSI_RESET}")
}

fn colorize_light_red(value: &str) -> String {
    format!("{ANSI_LIGHT_RED}{value}{ANSI_RESET}")
}

fn green_check() -> String {
    format!("{ANSI_GREEN}✓{ANSI_RESET}")
}

fn yellow_warn() -> String {
    format!("{ANSI_YELLOW}⚠{ANSI_RESET}")
}

fn colorize_yellow(value: &str) -> String {
    format!("{ANSI_YELLOW}{value}{ANSI_RESET}")
}

/// Plain text status line with [OK]/[WARN] prefix.
fn text_status_line(label: &str, value: &str, ok: bool) -> String {
    if ok {
        format!("[OK] {label}: {value}\n")
    } else {
        format!("[WARN] {label}: {value}\n")
    }
}

/// Format a status line with glyph on the left.
/// `ok`: true → green ✓, false → yellow ⚠ with yellow value.
fn status_line(label: &str, value: &str, ok: bool) -> String {
    if ok {
        format!("{} {label}: {value}\n", green_check())
    } else {
        format!("{} {label}: {}\n", yellow_warn(), colorize_yellow(value))
    }
}

fn render_with_format(summary: &StatusSummary, format: OutputFormat, watching: bool) -> Result<()> {
    if watching {
        print!("\u{1b}[2J\u{1b}[H");
    }

    let output = match format {
        OutputFormat::Text => summary.render_text(),
        OutputFormat::Pretty => summary.render_multiline(),
        OutputFormat::Json => {
            bail!("JSON format should be handled before calling render")
        }
    };

    // In raw mode (watch), \n only moves cursor down without returning to
    // column 0.  Replace with \r\n so each line starts at the left edge.
    if watching {
        print!("{}", output.replace('\n', "\r\n"));
    } else {
        print!("{output}");
    }
    io::stdout().flush().ok();
    Ok(())
}

/// Options controlling status rendering.
#[allow(clippy::struct_excessive_bools)]
struct RenderOptions {
    /// Show a single-line summary.
    oneline: bool,
    /// Show a compact status bar.
    status_bar: bool,
    /// Include mouth indicator in status bar.
    mouth: bool,
    /// Clear screen before output (for watch mode).
    watching: bool,
}

fn render(summary: &StatusSummary, opts: &RenderOptions) {
    if opts.watching {
        print!("\u{1b}[2J\u{1b}[H");
    }

    let output = if opts.status_bar {
        summary.render_status_bar(opts.mouth)
    } else if opts.oneline {
        summary.render_oneline()
    } else {
        summary.render_multiline()
    };

    // In raw mode (watch), \n only moves cursor down without returning to
    // column 0.  Replace with \r\n so each line starts at the left edge.
    if opts.watching {
        print!("{}", output.replace('\n', "\r\n"));
    } else {
        print!("{output}");
    }
    io::stdout().flush().ok();
}

/// Collect repository status using git-native operations.
///
/// Gathers:
/// - Non-default workspace names (via git worktree backend)
/// - Changed/untracked files in the default workspace (via git status)
/// - Branch sync status vs origin (via git rev-list)
/// - Stray files at repo root
/// - bn-221b: per-workspace lifecycle state, stale list with fix
///   commands, and integrate-ready list — the "where am I / what is
///   stale / what to integrate" answers in one call so agents stop
///   paying a turn to discover staleness.
fn collect_status() -> Result<StatusSummary> {
    let root = workspace::repo_root()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();
    let default_ws_name = config.default_workspace();

    // Get raw WorkspaceInfo from the backend so we can derive lifecycle
    // signals without a second round of discovery calls. Layout-agnostic
    // per bn-2sw3 T3.2: we use the backend's path/state/epoch fields,
    // not raw `ws/` joins.
    let backend_workspaces = get_backend()
        .ok()
        .and_then(|backend| backend.list().ok())
        .unwrap_or_default();

    // Build the enriched per-workspace details for non-default
    // workspaces. (Default appears in `current_workspace`/`is_stale`,
    // not in this list, matching pre-bn-221b semantics.)
    let workspace_details: Vec<WorkspaceStatusItem> = backend_workspaces
        .iter()
        .filter(|ws| ws.id.as_str() != default_ws_name)
        .map(|ws| build_workspace_status_item(&root, ws))
        .collect();
    let workspace_names = workspace_details
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect();

    // Stale + integrate-ready slices, derived from the same backend
    // info we already collected. Each entry carries its own
    // `fix_command` — the load-bearing piece of the bn-221b fix.
    let stale_workspaces = collect_stale_workspaces(&root, &backend_workspaces, default_ws_name);
    let integrate_ready = collect_integrate_ready(&backend_workspaces, default_ws_name);

    // Get active changes from metadata store
    let changes = ChangesStore::open(&root)
        .list_active_records()
        .unwrap_or_default()
        .into_iter()
        .map(|record| ChangeStatusItem {
            change_id: record.change_id,
            branch: record.git.change_branch,
            pr_number: record.pr.as_ref().map(|pr| pr.number),
            pr_state: record.pr.as_ref().map(|pr| pr.state.clone()),
            pr_draft: record.pr.as_ref().map(|pr| pr.draft),
        })
        .collect();

    // Resolve the default workspace path via the backend (layout-agnostic
    // per bn-2sw3 T3.2) — falls back to the legacy root.join("ws") only
    // if the backend has no entry for it.
    let default_ws_path = backend_workspaces
        .iter()
        .find(|ws| ws.id.as_str() == default_ws_name)
        .map_or_else(
            || root.join("ws").join(default_ws_name),
            |ws| ws.path.clone(),
        );

    let (changed_files, untracked_files) = if default_ws_path.exists() {
        collect_git_status(&default_ws_path)?
    } else {
        (Vec::new(), Vec::new())
    };

    // The default workspace tracks the configured branch and should not be
    // treated as an ephemeral stale workspace.
    let is_stale = false;

    // bn-221b: cwd-detected workspace (None when at repo root). Use the
    // backend-provided paths to avoid hardcoding `ws/`.
    let current_workspace = detect_current_workspace_from_backend(&backend_workspaces);
    let current_workspace_state = current_workspace.as_deref().and_then(|name| {
        backend_workspaces
            .iter()
            .find(|ws| ws.id.as_str() == name)
            .map(|ws| {
                let signals = collect_lifecycle_signals(&root, ws);
                LifecycleState::classify(signals)
            })
    });

    // Check main branch sync status vs origin
    let main_sync = main_sync_status_inner(&root, branch);

    // Check for stray files at repo root
    let stray_root_files = doctor::stray_root_entries(&root);

    Ok(StatusSummary {
        workspace_names,
        workspace_details,
        changes,
        changed_files,
        untracked_files,
        is_stale,
        main_sync,
        stray_root_files,
        current_workspace,
        current_workspace_state,
        stale_workspaces,
        integrate_ready,
    })
}

/// bn-221b: collect the stale-workspace list with per-entry fix
/// commands. Skips the default workspace (it tracks the branch and
/// can't go stale in the ephemeral sense).
fn collect_stale_workspaces(
    root: &Path,
    backend_workspaces: &[maw_core::model::types::WorkspaceInfo],
    default_ws_name: &str,
) -> Vec<StaleWorkspace> {
    backend_workspaces
        .iter()
        .filter(|ws| ws.id.as_str() != default_ws_name)
        .filter_map(|ws| {
            let WorkspaceState::Stale { behind_epochs } = ws.state else {
                return None;
            };
            let meta = workspace::metadata::read(root, ws.id.as_str()).unwrap_or_default();
            let mode_persistent = meta.mode.is_persistent();
            let name = ws.id.as_str().to_string();
            let fix_command = LifecycleState::Stale
                .fix_command(&name, mode_persistent)
                .unwrap_or_else(|| format!("maw ws sync {name}"));
            Some(StaleWorkspace {
                name,
                behind_epochs,
                mode: format!("{}", meta.mode),
                fix_command,
            })
        })
        .collect()
}

/// bn-221b: collect workspaces with committed work that can be merged
/// into the integration target right now. Stale workspaces are
/// excluded — they appear in `stale_workspaces` with a sync fix
/// instead.
fn collect_integrate_ready(
    backend_workspaces: &[maw_core::model::types::WorkspaceInfo],
    default_ws_name: &str,
) -> Vec<IntegrateReady> {
    backend_workspaces
        .iter()
        .filter(|ws| ws.id.as_str() != default_ws_name)
        .filter(|ws| ws.commits_ahead > 0)
        .filter(|ws| !ws.state.is_stale())
        .map(|ws| {
            let name = ws.id.as_str().to_string();
            let fix_command = format!("maw ws merge {name} --into {default_ws_name} --check");
            IntegrateReady {
                name,
                commits_ahead: ws.commits_ahead,
                fix_command,
            }
        })
        .collect()
}

/// bn-221b: build the enriched per-workspace JSON item, deriving the
/// lifecycle state from already-collected signals.
fn build_workspace_status_item(
    root: &Path,
    ws: &maw_core::model::types::WorkspaceInfo,
) -> WorkspaceStatusItem {
    let name = ws.id.as_str().to_string();
    let meta = workspace::metadata::read(root, ws.id.as_str()).unwrap_or_default();
    let signals = collect_lifecycle_signals(root, ws);
    let state = LifecycleState::classify(signals);
    let behind = match ws.state {
        WorkspaceState::Stale { behind_epochs } => Some(behind_epochs),
        _ => None,
    };
    let fix_command = state.fix_command(&name, meta.mode.is_persistent());
    let epoch = if ws.epoch.as_str().len() >= 12 {
        Some(ws.epoch.as_str()[..12].to_string())
    } else {
        Some(ws.epoch.as_str().to_string())
    };
    WorkspaceStatusItem {
        name,
        branch: meta.branch,
        lifecycle_state: Some(state),
        epoch,
        behind_epochs: behind,
        commits_ahead: Some(ws.commits_ahead),
        fix_command,
    }
}

/// bn-221b: gather lifecycle signals for one workspace from the
/// information the backend already exposes plus cheap on-disk checks.
/// Conservative on errors — unknown signals default to "no friction"
/// so the classifier never falsely promotes a healthy ws to
/// `Conflicted` or `Missing`.
fn collect_lifecycle_signals(
    root: &Path,
    ws: &maw_core::model::types::WorkspaceInfo,
) -> LifecycleSignals {
    let missing = !ws.path.exists();
    // bn-16x2: derive conflicted-ness from the recorded rebase-conflict
    // sidecar (matching `maw ws merge --check`), not a tracked-content
    // marker scan that false-positives on legit `<<<<<<<` literals.
    // bn-8zqz: the sidecar is verified against reality; stale metadata
    // (manual resolution committed) is auto-cleared.
    let rebase_conflicts = if missing {
        0
    } else {
        workspace::conflict_state::effective_recorded_conflict_count(root, ws.id.as_str(), &ws.path)
    };
    let has_uncommitted = if missing {
        false
    } else {
        maw_git::GixRepo::open(&ws.path)
            .ok()
            .and_then(|repo| repo.count_dirty_tracked().ok())
            .is_some_and(|count| count > 0)
    };
    // bn-29fi: surface "this workspace was previously destroyed with a
    // pinned snapshot" as a signal so the classifier can promote a
    // missing workspace to `AbandonedWithSnapshot` (more specific than
    // plain `Missing`). Cheap on-disk check; layout-agnostic via
    // `destroy_preview::workspace_has_pinned_snapshot`.
    let has_pinned_snapshot =
        workspace::destroy_preview::workspace_has_pinned_snapshot(root, ws.id.as_str());
    LifecycleSignals {
        missing,
        rebase_conflicts,
        is_stale: ws.state.is_stale(),
        commits_ahead: ws.commits_ahead,
        has_uncommitted,
        was_integrated: false,
        has_pinned_snapshot,
    }
}

/// bn-221b: resolve "which workspace contains the cwd" by matching
/// the cwd against backend-provided workspace paths. Layout-agnostic:
/// no `ws/` literal. Returns the first workspace whose path is an
/// ancestor of (or equal to) the cwd.
fn detect_current_workspace_from_backend(
    workspaces: &[maw_core::model::types::WorkspaceInfo],
) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    workspaces
        .iter()
        .filter_map(|ws| {
            let ws_path = ws.path.canonicalize().unwrap_or_else(|_| ws.path.clone());
            if cwd.starts_with(&ws_path) {
                Some((ws_path.components().count(), ws.id.as_str().to_string()))
            } else {
                None
            }
        })
        // Most-specific match wins (longest path) — defends against
        // pathological setups where two workspaces nest.
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, name)| name)
}

/// Lightweight status collection for `--status-bar`.
///
/// Avoids the expensive operations in [`collect_status`]:
/// - Uses `git status --porcelain` instead of gix status (avoids full worktree walk)
/// - Counts workspace dirs directly instead of going through the backend
/// - Skips `ChangesStore` entirely (not shown in status bar)
/// - Uses `git rev-list --count` only when local != remote (fast OID comparison first)
fn collect_status_fast() -> Result<StatusSummary> {
    let root = workspace::repo_root()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();
    let default_ws_name = config.default_workspace();

    // Count non-default workspaces by reading ws/ directory entries.
    let ws_dir = root.join("ws");
    let workspace_names = if ws_dir.is_dir() {
        std::fs::read_dir(&ws_dir)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name != default_ws_name && e.file_type().ok()?.is_dir() {
                            Some(name)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // For the status bar we only need the COUNT of dirty tracked files —
    // not file names. We read the git index and stat each entry, comparing
    // mtime/size against the index cache. This is O(n) stat calls with no
    // hashing and no gix overhead.
    let default_ws_path = root.join("ws").join(default_ws_name);
    let changed_count = if default_ws_path.exists() {
        count_dirty_tracked_files(&default_ws_path)
    } else {
        0
    };
    let changed_files: Vec<String> = (0..changed_count).map(|_| String::new()).collect();
    let untracked_files = Vec::new();

    // Check main branch sync status vs origin.
    let main_sync = main_sync_status_inner(&root, branch);

    Ok(StatusSummary {
        workspace_details: workspace_names
            .iter()
            .map(|name| WorkspaceStatusItem {
                name: name.clone(),
                branch: None,
                lifecycle_state: None,
                epoch: None,
                behind_epochs: None,
                commits_ahead: None,
                fix_command: None,
            })
            .collect(),
        workspace_names,
        changes: Vec::new(),
        changed_files,
        untracked_files,
        is_stale: false,
        main_sync,
        stray_root_files: Vec::new(),
        // bn-221b: status-bar path is the prompt-display fast path; it
        // skips lifecycle classification to keep ~50ms target. The JSON
        // consumer uses collect_status() and gets the full vocabulary.
        current_workspace: None,
        current_workspace_state: None,
        stale_workspaces: Vec::new(),
        integrate_ready: Vec::new(),
    })
}

/// Count dirty tracked files using maw-git's fast index-stat check.
fn count_dirty_tracked_files(ws_path: &Path) -> usize {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return 0;
    };
    repo.count_dirty_tracked().unwrap_or(0)
}

/// Collect changed and untracked files from git status in a directory.
fn collect_git_status(ws_path: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let Ok(entries) = repo.status() else {
        return Ok((Vec::new(), Vec::new()));
    };

    let mut changed = Vec::new();
    let mut untracked = Vec::new();

    for entry in entries {
        let path = entry.path.clone();
        let path = path
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&path);
        let status_xy = match entry.status {
            maw_git::FileStatus::Untracked => "??",
            maw_git::FileStatus::Modified => " M",
            maw_git::FileStatus::Added => "A ",
            maw_git::FileStatus::Deleted => " D",
            maw_git::FileStatus::Renamed => "R ",
        };

        if status_xy == "??" {
            untracked.push(path.to_string());
        } else {
            changed.push(path.to_string());
        }
    }

    Ok((changed, untracked))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with_root_extras(stray_root_files: &[&str]) -> StatusSummary {
        StatusSummary {
            workspace_names: Vec::new(),
            workspace_details: Vec::new(),
            changes: Vec::new(),
            changed_files: Vec::new(),
            untracked_files: Vec::new(),
            is_stale: false,
            main_sync: SyncStatus::UpToDate,
            stray_root_files: stray_root_files
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            current_workspace: None,
            current_workspace_state: None,
            stale_workspaces: Vec::new(),
            integrate_ready: Vec::new(),
        }
    }

    #[test]
    fn root_extras_do_not_count_as_status_issues() {
        let summary = summary_with_root_extras(&["AGENTS.md"]);

        assert_eq!(summary.issue_count(), 0);
    }

    #[test]
    fn status_bar_omits_root_extras() {
        let summary = summary_with_root_extras(&["AGENTS.md", "notes"]);

        let output = summary.render_status_bar(false);

        assert!(!output.contains("ROOT"));
        assert!(!output.contains("AGENTS.md"));
        assert!(!output.contains("notes"));
    }

    #[test]
    fn text_status_softens_root_extra_message() {
        let summary = summary_with_root_extras(&["AGENTS.md"]);

        let output = summary.render_text();

        assert!(output.contains("[INFO] Root extras: 1 non-structural item(s)"));
        assert!(output.contains("  - AGENTS.md"));
        assert!(!output.contains("[WARN] ROOT NOT BARE"));
        assert!(!output.contains("run maw init to fix"));
    }

    #[test]
    fn oneline_status_uses_soft_root_extra_label() {
        let summary = summary_with_root_extras(&["AGENTS.md"]);

        let output = summary.render_oneline();

        assert!(output.contains("root-extra=1"));
        assert!(!output.contains("ROOT-NOT-BARE"));
    }
}
