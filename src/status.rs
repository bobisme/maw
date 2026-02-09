use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;

use crate::doctor;
use crate::format::OutputFormat;
use crate::workspace::{self, MawConfig};
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
}

pub fn run(args: StatusArgs) -> Result<()> {
    // Special modes take precedence over format
    if args.status_bar || args.oneline {
        let summary = collect_status()?;
        render(&summary, args.oneline, args.status_bar, args.mouth, false)?;
        return Ok(());
    }

    let format = OutputFormat::resolve(args.format);

    // JSON mode: serialize and exit (no watch)
    if format == OutputFormat::Json {
        let summary = collect_status()?;
        let envelope = StatusEnvelope {
            workspaces: summary.workspace_names.clone(),
            changed_files: summary.changed_files.clone(),
            untracked_files: summary.untracked_files.clone(),
            is_stale: summary.is_stale,
            main_sync: summary.main_sync.oneline(),
            stray_root_files: summary.stray_root_files.clone(),
            advice: vec![],
        };
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    // Watch mode (text/pretty only)
    if args.watch {
        watch_loop(&args)?;
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
    let format = OutputFormat::resolve(args.format);

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
    main_sync: MainSyncStatus,
    stray_root_files: Vec<String>,
}

#[derive(Debug)]
enum MainSyncStatus {
    UpToDate,
    Ahead(usize),
    Behind(usize),
    Diverged { ahead: usize, behind: usize },
    NoMain,
    NoRemote,
    Unknown(String),
}

impl StatusSummary {
    fn issue_count(&self) -> usize {
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
        parts.push(format!("main={}{}", self.main_sync.oneline(), if matches!(self.main_sync, MainSyncStatus::UpToDate) { &check } else { &warn }));
        parts.push(format!("default={}{}", if self.is_stale { "stale" } else { "fresh" }, if self.is_stale { &warn } else { &check }));

        format!("{}\n", parts.join(" "))
    }

    /// Plain text format — no ANSI codes, compact, agent-friendly
    fn render_text(&self) -> String {
        let stray = self.stray_root_files.len();
        let ws = self.workspace_names.len();
        let changes = self.changed_files.len();
        let untracked = self.untracked_files.len();
        let mut parts = Vec::new();
        if stray > 0 {
            parts.push(format!("ROOT-NOT-BARE={stray}"));
        }
        parts.push(format!("ws={ws}"));
        parts.push(format!("changes={changes}"));
        parts.push(format!("untracked={untracked}"));
        parts.push(format!("main={}", self.main_sync.oneline()));
        parts.push(format!("default={}", if self.is_stale { "stale" } else { "fresh" }));
        format!("{}\n", parts.join("  "))
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
        let mut has_segment = false;

        let mut append_segment = |segment: &str| {
            if !has_segment {
                has_segment = true;
            }
            out.push_str(segment);
        };

        if !self.stray_root_files.is_empty() {
            append_segment(&colorize_light_red("ROOT!"));
        }

        let ws = self.workspace_names.len();
        if ws > 0 {
            let workspace = format!("\u{f0645} {ws}");
            let colored = colorize_orange(&workspace);
            append_segment(&colored);
        }

        let changes = self.changed_files.len();
        if changes > 0 {
            let changes = format!("\u{eb43} {changes}");
            let colored = colorize_blue(&changes);
            append_segment(&colored);
        }

        let untracked = self.untracked_files.len();
        if untracked > 0 {
            let untracked = format!("?{untracked}");
            append_segment(&untracked);
        }

        if self.main_sync.is_warning() {
            let warning = colorize_light_red("\u{e726}");
            append_segment(&warning);
        }

        format!("{out}\n")
    }

    fn render_multiline(&self) -> String {
        let mut out = String::new();
        out.push_str("=== maw status ===\n");

        if !self.stray_root_files.is_empty() {
            let n = self.stray_root_files.len();
            out.push_str(&format!(
                "{} {}: {}\n",
                colorize_light_red("⚠ ROOT NOT BARE"),
                colorize_light_red(&format!("{n} unexpected file(s) at repo root")),
                colorize_light_red("run maw init to fix"),
            ));
            for name in &self.stray_root_files {
                out.push_str(&format!("  - {name}\n"));
            }
        }

        let ws_count = self.workspace_names.len();
        out.push_str(&status_line(
            "Non-default workspaces",
            &if ws_count == 0 { "none".to_string() } else { ws_count.to_string() },
            ws_count == 0,
        ));
        for name in &self.workspace_names {
            out.push_str(&format!("  - {name}\n"));
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
            out.push_str(&format!("  - {file}\n"));
        }

        let untracked_count = self.untracked_files.len();
        out.push_str(&status_line(
            "Untracked files",
            &if untracked_count == 0 { "none".to_string() } else { untracked_count.to_string() },
            untracked_count == 0,
        ));
        for file in &self.untracked_files {
            out.push_str(&format!("  - {file}\n"));
        }

        out.push_str(&status_line(
            "Main vs origin",
            &self.main_sync.describe(),
            matches!(self.main_sync, MainSyncStatus::UpToDate),
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

/// Format a status line with glyph on the left.
/// `ok`: true → green ✓, false → yellow ⚠ with yellow value.
fn status_line(label: &str, value: &str, ok: bool) -> String {
    if ok {
        format!("{} {label}: {value}\n", green_check())
    } else {
        format!("{} {label}: {}\n", yellow_warn(), colorize_yellow(value))
    }
}

impl MainSyncStatus {
    const fn is_warning(&self) -> bool {
        !matches!(self, Self::UpToDate)
    }

    fn oneline(&self) -> String {
        match self {
            Self::UpToDate => "sync".to_string(),
            Self::Ahead(ahead) => format!("ahead({ahead})"),
            Self::Behind(behind) => format!("behind({behind})"),
            Self::Diverged { ahead, behind } => {
                format!("diverged({ahead}/{behind})")
            }
            Self::NoMain => "no-main".to_string(),
            Self::NoRemote => "no-remote".to_string(),
            Self::Unknown(_) => "unknown".to_string(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::UpToDate => "up to date".to_string(),
            Self::Ahead(ahead) => format!("ahead by {ahead} (not pushed)"),
            Self::Behind(behind) => format!("behind by {behind}"),
            Self::Diverged { ahead, behind } => {
                format!("diverged (ahead {ahead}, behind {behind})")
            }
            Self::NoMain => "missing main bookmark".to_string(),
            Self::NoRemote => "no main@origin bookmark".to_string(),
            Self::Unknown(reason) => format!("unknown ({reason})"),
        }
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

fn render(
    summary: &StatusSummary,
    oneline: bool,
    status_bar: bool,
    mouth: bool,
    watching: bool,
) -> Result<()> {
    if watching {
        print!("\u{1b}[2J\u{1b}[H");
    }

    let output = if status_bar {
        summary.render_status_bar(mouth)
    } else if oneline {
        summary.render_oneline()
    } else {
        summary.render_multiline()
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

fn collect_status() -> Result<StatusSummary> {
    let root = workspace::repo_root()?;
    let cwd = workspace::jj_cwd()?;

    let ws_output = Command::new("jj")
        .args(["workspace", "list", "--color=never", "--no-pager"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    if !ws_output.status.success() {
        bail!(
            "jj workspace list failed: {}",
            String::from_utf8_lossy(&ws_output.stderr)
        );
    }

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);
    let workspace_names = non_default_workspace_names(&ws_list);

    let status_output = Command::new("jj")
        .args(["status", "--color=never", "--no-pager"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj status")?;

    if !status_output.status.success() {
        bail!(
            "jj status failed: {}",
            String::from_utf8_lossy(&status_output.stderr)
        );
    }

    let status_stdout = String::from_utf8_lossy(&status_output.stdout);
    let status_stderr = String::from_utf8_lossy(&status_output.stderr);
    let changed_files = parse_jj_changed_files(&status_stdout);
    let git_files = git_status_files(&root).unwrap_or_default();
    let is_stale = status_stderr.contains("working copy is stale");

    let config = MawConfig::load(&root).unwrap_or_default();
    let main_sync = main_sync_status(&cwd, config.branch())?;

    let stray_root_files = doctor::stray_root_entries(&root);

    Ok(StatusSummary {
        workspace_names,
        changed_files,
        untracked_files: git_files.untracked,
        is_stale,
        main_sync,
        stray_root_files,
    })
}

fn non_default_workspace_names(list: &str) -> Vec<String> {
    let mut names = Vec::new();

    for line in list.lines() {
        let Some((name_part, _)) = line.split_once(':') else {
            continue;
        };
        let name = name_part.trim().trim_end_matches('@').trim();
        if name.is_empty() || name == "default" {
            continue;
        }
        names.push(name.to_string());
    }

    names
}

fn parse_jj_changed_files(status_stdout: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut in_changes = false;

    for line in status_stdout.lines() {
        if line.starts_with("Working copy changes:") {
            in_changes = true;
            continue;
        }

        if in_changes {
            if line.trim().is_empty()
                || line.starts_with("Working copy ")
                || line.starts_with("Parent commit")
            {
                break;
            }

            // Lines look like "M src/status.rs" or "A new_file.rs"
            let trimmed = line.trim();
            if let Some(path) = trimmed.split_whitespace().last() {
                files.push(path.to_string());
            }
        }
    }

    files
}

#[derive(Default, Debug)]
struct GitStatusFiles {
    untracked: Vec<String>,
}

fn git_status_files(root: &Path) -> Result<GitStatusFiles> {
    let output = Command::new("git")
        .args(["status", "--porcelain=1", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .context("Failed to run git status")?;

    if !output.status.success() {
        return Ok(GitStatusFiles::default());
    }

    let mut files = GitStatusFiles::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Porcelain format: "?? path/to/file" for untracked
        if line.starts_with("??")
            && let Some(path) = line.get(3..) {
                files.untracked.push(path.trim().to_string());
            }
    }

    Ok(files)
}

fn main_sync_status(root: &Path, branch: &str) -> Result<MainSyncStatus> {
    let main_exists = match revset_exists(root, branch) {
        Ok(exists) => exists,
        Err(err) => return Ok(MainSyncStatus::Unknown(err.to_string())),
    };

    if !main_exists {
        return Ok(MainSyncStatus::NoMain);
    }

    let origin_ref = format!("{branch}@origin");
    let origin_exists = match revset_exists(root, &origin_ref) {
        Ok(exists) => exists,
        Err(err) => return Ok(MainSyncStatus::Unknown(err.to_string())),
    };

    if !origin_exists {
        return Ok(MainSyncStatus::NoRemote);
    }

    let ahead_revset = format!("{origin_ref}..{branch}");
    let behind_revset = format!("{branch}..{origin_ref}");
    let ahead = match count_revset(root, &ahead_revset) {
        Ok(count) => count,
        Err(err) => return Ok(MainSyncStatus::Unknown(err.to_string())),
    };
    let behind = match count_revset(root, &behind_revset) {
        Ok(count) => count,
        Err(err) => return Ok(MainSyncStatus::Unknown(err.to_string())),
    };

    Ok(match (ahead, behind) {
        (0, 0) => MainSyncStatus::UpToDate,
        (a, 0) => MainSyncStatus::Ahead(a),
        (0, b) => MainSyncStatus::Behind(b),
        (a, b) => MainSyncStatus::Diverged {
            ahead: a,
            behind: b,
        },
    })
}

fn revset_exists(root: &Path, revset: &str) -> Result<bool> {
    let output = Command::new("jj")
        .args([
            "log",
            "-r",
            revset,
            "--no-graph",
            "--color=never",
            "--no-pager",
            "-T",
            "change_id.short()",
        ])
        .current_dir(root)
        .output()
        .context("Failed to run jj log")?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = format!("{stderr}{stdout}");
    if message.contains("doesn't exist") || message.contains("not found") {
        return Ok(false);
    }

    bail!("jj log failed: {}", message.trim())
}

fn count_revset(root: &Path, revset: &str) -> Result<usize> {
    let output = Command::new("jj")
        .args([
            "log",
            "-r",
            revset,
            "--no-graph",
            "--color=never",
            "--no-pager",
            "-T",
            "change_id.short()",
        ])
        .current_dir(root)
        .output()
        .context("Failed to run jj log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = format!("{stderr}{stdout}");
        bail!("jj log failed for {revset}: {}", message.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}
