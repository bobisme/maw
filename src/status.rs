use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;

use crate::backend::WorkspaceBackend;
use crate::doctor;
use crate::format::OutputFormat;
use crate::push::{SyncStatus, main_sync_status_inner};
use crate::workspace::{self, get_backend, MawConfig};
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

pub fn run(args: &StatusArgs) -> Result<()> {
    // Special modes take precedence over format
    if args.status_bar || args.oneline {
        let summary = collect_status()?;
        render(&summary, &RenderOptions {
            oneline: args.oneline,
            status_bar: args.status_bar,
            mouth: args.mouth,
            watching: false,
        });
        return Ok(());
    }

    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));

    // JSON mode: serialize and exit (no watch)
    if format == OutputFormat::Json {
        let summary = collect_status()?;
        let envelope = StatusEnvelope {
            workspaces: summary.workspace_names.clone(),
            changed_files: summary.changed_files.clone(),
            untracked_files: summary.untracked_files.clone(),
            is_stale: summary.is_stale,
            main_sync: summary.main_sync.oneline(),
            stray_root_files: summary.stray_root_files,
            advice: vec![],
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
                && let Event::Key(key) = event::read()? {
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
    changed_files: Vec<String>,
    untracked_files: Vec<String>,
    is_stale: bool,
    main_sync: String,
    stray_root_files: Vec<String>,
    advice: Vec<serde_json::Value>,
}

#[derive(Debug)]
struct StatusSummary {
    workspace_names: Vec<String>,
    changed_files: Vec<String>,
    untracked_files: Vec<String>,
    is_stale: bool,
    main_sync: SyncStatus,
    stray_root_files: Vec<String>,
}

impl StatusSummary {
    const fn issue_count(&self) -> usize {
        let mut count = 0;
        if !self.stray_root_files.is_empty() {
            count += 1;
        }
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
        let changes = self.changed_files.len();
        let untracked = self.untracked_files.len();
        let mut parts = Vec::new();
        if stray > 0 {
            parts.push(format!("ROOT-NOT-BARE={stray}{warn}"));
        }
        parts.push(format!("ws={ws}{}", if ws == 0 { &check } else { &warn }));
        parts.push(format!("changes={changes}{}", if changes == 0 { &check } else { &warn }));
        parts.push(format!("untracked={untracked}{}", if untracked == 0 { &check } else { &warn }));
        parts.push(format!("main={}{}", self.main_sync.oneline(), if matches!(self.main_sync, SyncStatus::UpToDate) { &check } else { &warn }));
        parts.push(format!("default={}{}", if self.is_stale { "stale" } else { "fresh" }, if self.is_stale { &warn } else { &check }));

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
                "[WARN] ROOT NOT BARE: {n} unexpected file(s) at repo root: run maw init to fix"
            );
            for name in &self.stray_root_files {
                let _ = writeln!(out, "  - {name}");
            }
        }

        let ws_count = self.workspace_names.len();
        out.push_str(&text_status_line(
            "Non-default workspaces",
            &if ws_count == 0 { "none".to_string() } else { ws_count.to_string() },
            ws_count == 0,
        ));
        for name in &self.workspace_names {
            let _ = writeln!(out, "  - {name}");
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
            &if untracked_count == 0 { "none".to_string() } else { untracked_count.to_string() },
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
            &if self.is_stale { "stale (run: maw ws sync)".to_string() } else { "fresh".to_string() },
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

        if !self.stray_root_files.is_empty() {
            out.push_str(&colorize_light_red("ROOT!"));
        }

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
                "{} {}: {}",
                colorize_light_red("⚠ ROOT NOT BARE"),
                colorize_light_red(&format!("{n} unexpected file(s) at repo root")),
                colorize_light_red("run maw init to fix"),
            );
            for name in &self.stray_root_files {
                let _ = writeln!(out, "  - {name}");
            }
        }

        let ws_count = self.workspace_names.len();
        out.push_str(&status_line(
            "Non-default workspaces",
            &if ws_count == 0 { "none".to_string() } else { ws_count.to_string() },
            ws_count == 0,
        ));
        for name in &self.workspace_names {
            let _ = writeln!(out, "  - {name}");
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
            &if untracked_count == 0 { "none".to_string() } else { untracked_count.to_string() },
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
            &if self.is_stale { "stale (run: maw ws sync)".to_string() } else { "fresh".to_string() },
            !self.is_stale,
        ));

        out
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

fn render_with_format(
    summary: &StatusSummary,
    format: OutputFormat,
    watching: bool,
) -> Result<()> {
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
/// - Whether the default workspace is stale (behind current epoch)
/// - Branch sync status vs origin (via git rev-list)
/// - Stray files at repo root
fn collect_status() -> Result<StatusSummary> {
    let root = workspace::repo_root()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();
    let default_ws_name = config.default_workspace();

    // Get non-default workspace names from backend
    let workspace_names = match get_backend() {
        Ok(backend) => {
            match backend.list() {
                Ok(infos) => infos
                    .into_iter()
                    .filter(|ws| ws.id.as_str() != default_ws_name)
                    .map(|ws| ws.id.as_str().to_string())
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    };

    // Get changed/untracked files in the default workspace
    let default_ws_path = root.join("ws").join(default_ws_name);
    let (changed_files, untracked_files) = if default_ws_path.exists() {
        collect_git_status(&default_ws_path)?
    } else {
        (Vec::new(), Vec::new())
    };

    // Check if default workspace is stale (behind current epoch)
    let is_stale = check_default_stale(&root, &default_ws_path);

    // Check main branch sync status vs origin
    let main_sync = main_sync_status_inner(&root, branch);

    // Check for stray files at repo root
    let stray_root_files = doctor::stray_root_entries(&root);

    Ok(StatusSummary {
        workspace_names,
        changed_files,
        untracked_files,
        is_stale,
        main_sync,
        stray_root_files,
    })
}

/// Collect changed and untracked files from `git status --porcelain` in a directory.
fn collect_git_status(ws_path: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git status")?;

    if !output.status.success() {
        return Ok((Vec::new(), Vec::new()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut changed = Vec::new();
    let mut untracked = Vec::new();

    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let status_xy = &line[..2];
        let path = &line[3..];
        let path = path
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(path);

        if status_xy == "??" {
            untracked.push(path.to_string());
        } else {
            changed.push(path.to_string());
        }
    }

    Ok((changed, untracked))
}

/// Check if the default workspace is stale (its HEAD differs from the current epoch).
fn check_default_stale(root: &Path, default_ws_path: &Path) -> bool {
    if !default_ws_path.exists() {
        return false;
    }

    // Read current epoch
    let epoch_output = Command::new("git")
        .args(["rev-parse", "refs/manifold/epoch/current"])
        .current_dir(root)
        .output();

    let epoch_oid = match epoch_output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => return false, // No epoch ref = can't be stale
    };

    // Read default workspace HEAD
    let ws_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(default_ws_path)
        .output();

    let ws_oid = match ws_head {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => return false,
    };

    epoch_oid != ws_oid
}
