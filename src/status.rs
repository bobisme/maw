use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace::{self, MawConfig};

const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const ANSI_ORANGE: &str = "\x1b[38;5;208m";
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_LIGHT_RED: &str = "\x1b[91m";
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
}

pub fn run(args: StatusArgs) -> Result<()> {
    if args.watch {
        loop {
            let summary = collect_status()?;
            render(&summary, args.oneline, args.status_bar, args.mouth, true)?;
            thread::sleep(WATCH_INTERVAL);
        }
    } else {
        let summary = collect_status()?;
        render(&summary, args.oneline, args.status_bar, args.mouth, false)?;
    }

    Ok(())
}

#[derive(Debug)]
struct StatusSummary {
    total_workspaces: usize,
    workspaces: usize,
    change_count: usize,
    jj_change_count: usize,
    git_change_count: usize,
    git_untracked_count: usize,
    is_stale: bool,
    main_sync: MainSyncStatus,
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
        if self.workspaces > 0 {
            count += 1;
        }
        if self.change_count > 0 {
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
        let mut parts = vec![
            format!("ws={}", self.workspaces),
            format!("changes={}", self.change_count),
            format!("untracked={}", self.git_untracked_count),
            format!("main={}", self.main_sync.oneline()),
        ];

        if self.is_stale {
            parts.push("stale=1".to_string());
        } else {
            parts.push("stale=0".to_string());
        }

        format!("{}\n", parts.join(" "))
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

        if self.workspaces > 0 {
            let count = self.workspaces.to_string();
            let workspace = format!("\u{f0645} {count}");
            let colored = colorize_orange(&workspace);
            append_segment(&colored);
        }

        if self.change_count > 0 {
            let changes = format!("\u{eb43} {}", self.change_count);
            let colored = colorize_blue(&changes);
            append_segment(&colored);
        }

        if self.git_untracked_count > 0 {
            let untracked = format!("?{}", self.git_untracked_count);
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
        out.push_str(&format!(
            "Agent workspaces: {} ({} total)\n",
            self.workspaces, self.total_workspaces
        ));

        if self.change_count == 0 {
            out.push_str("Working copy changes: none\n");
        } else {
            out.push_str(&format!(
                "Working copy changes: {} (jj={}, git={})\n",
                self.change_count, self.jj_change_count, self.git_change_count
            ));
        }

        if self.git_untracked_count == 0 {
            out.push_str("Untracked files (git): none\n");
        } else {
            out.push_str(&format!(
                "Untracked files (git): {}\n",
                self.git_untracked_count
            ));
        }

        out.push_str(&format!("Main vs origin: {}\n", self.main_sync.describe()));

        if self.is_stale {
            out.push_str("Stale workspace: yes (run `maw ws sync`)\n");
        } else {
            out.push_str("Stale workspace: no\n");
        }

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

impl MainSyncStatus {
    fn is_warning(&self) -> bool {
        !matches!(self, MainSyncStatus::UpToDate)
    }

    fn oneline(&self) -> String {
        match self {
            MainSyncStatus::UpToDate => "sync".to_string(),
            MainSyncStatus::Ahead(ahead) => format!("ahead({ahead})"),
            MainSyncStatus::Behind(behind) => format!("behind({behind})"),
            MainSyncStatus::Diverged { ahead, behind } => {
                format!("diverged({ahead}/{behind})")
            }
            MainSyncStatus::NoMain => "no-main".to_string(),
            MainSyncStatus::NoRemote => "no-remote".to_string(),
            MainSyncStatus::Unknown(_) => "unknown".to_string(),
        }
    }

    fn describe(&self) -> String {
        match self {
            MainSyncStatus::UpToDate => "up to date".to_string(),
            MainSyncStatus::Ahead(ahead) => format!("ahead by {ahead} (not pushed)"),
            MainSyncStatus::Behind(behind) => format!("behind by {behind}"),
            MainSyncStatus::Diverged { ahead, behind } => {
                format!("diverged (ahead {ahead}, behind {behind})")
            }
            MainSyncStatus::NoMain => "missing main bookmark".to_string(),
            MainSyncStatus::NoRemote => "no main@origin bookmark".to_string(),
            MainSyncStatus::Unknown(reason) => format!("unknown ({reason})"),
        }
    }
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

    print!("{output}");
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
    let (total_workspaces, workspaces) = count_workspaces(&ws_list);

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
    let jj_change_count = parse_jj_change_count(&status_stdout);
    let git_counts = git_status_counts(&root).unwrap_or(GitStatusCounts::default());
    let change_count = jj_change_count.max(git_counts.changes);
    let is_stale = status_stderr.contains("working copy is stale");

    let config = MawConfig::load(&root).unwrap_or_default();
    let main_sync = main_sync_status(&cwd, config.branch())?;

    Ok(StatusSummary {
        total_workspaces,
        workspaces,
        change_count,
        jj_change_count,
        git_change_count: git_counts.changes,
        git_untracked_count: git_counts.untracked,
        is_stale,
        main_sync,
    })
}

fn count_workspaces(list: &str) -> (usize, usize) {
    let mut total = 0;
    let mut non_default = 0;

    for line in list.lines() {
        let Some((name_part, _)) = line.split_once(':') else {
            continue;
        };
        let name = name_part.trim().trim_end_matches('@').trim();
        if name.is_empty() {
            continue;
        }
        total += 1;
        if name != "default" {
            non_default += 1;
        }
    }

    (total, non_default)
}

fn parse_jj_change_count(status_stdout: &str) -> usize {
    let mut count = 0;
    let mut in_changes = false;

    for line in status_stdout.lines() {
        if line.starts_with("Working copy changes:") {
            in_changes = true;
            continue;
        }

        if in_changes {
            if line.trim().is_empty() {
                break;
            }

            if line.starts_with("Working copy ") || line.starts_with("Parent commit") {
                break;
            }

            if !line.trim().is_empty() {
                count += 1;
            }
        }
    }

    count
}

#[derive(Default, Debug, Clone, Copy)]
struct GitStatusCounts {
    changes: usize,
    untracked: usize,
}

fn git_status_counts(root: &Path) -> Result<GitStatusCounts> {
    let output = Command::new("git")
        .args(["status", "--porcelain=1", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .context("Failed to run git status")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = format!("{stderr}{stdout}");
        if message.contains("not a git repository") {
            return Ok(GitStatusCounts::default());
        }
        return Ok(GitStatusCounts::default());
    }

    let mut counts = GitStatusCounts::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        counts.changes += 1;
        if line.trim_start().starts_with("??") {
            counts.untracked += 1;
        }
    }

    Ok(counts)
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
