use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use glob::Pattern;
use maw_git::GitRepo as _;
use serde::Serialize;

use crate::workspace::lifecycle::{LifecycleSignals, LifecycleState};
use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::{WorkspaceId, WorkspaceState};
use maw_core::refs as manifold_refs;

use super::{DEFAULT_WORKSPACE, get_backend, metadata, repo_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFormat {
    Patch,
    Stat,
    NameOnly,
    NameStatus,
    Json,
}

#[derive(Debug)]
enum AgainstMode {
    Default,
    Epoch,
    Branch(String),
    Oid(String),
}

#[derive(Debug)]
struct ResolvedRev {
    label: String,
    rev: String,
    oid: String,
}

#[derive(Debug, Clone)]
struct DiffEntry {
    path: String,
    old_path: Option<String>,
    status: String,
    additions: u32,
    deletions: u32,
    binary: bool,
}

#[derive(Debug, Serialize)]
struct DiffJsonOutput {
    workspace: String,
    against: DiffRevisionJson,
    head: DiffRevisionJson,
    stats: DiffStatsJson,
    files: Vec<DiffFileJson>,
    /// bn-242l (SG4 / `read_from_stale_workspace` mitigation):
    /// safe-cleanup vocabulary slug for the workspace being diffed.
    /// Pre-bn-242l, `maw ws diff --format json` returned a file list
    /// with no liveness signal — an agent inspecting diff output to
    /// decide "is this workspace ready to merge" then issued a verb
    /// inconsistent with a stale base. Carrying the same
    /// `lifecycle_state` slug `maw status --json` / `maw ws status`
    /// / `maw ws list` expose closes that misread loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_state: Option<LifecycleState>,
    /// bn-242l: count of epoch advances the workspace is behind, when
    /// stale. Absent when not stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    behind_epochs: Option<u32>,
    /// bn-242l: paste-able fix command for the workspace's current
    /// lifecycle state. Absent for `clean`/`integrated` workspaces.
    /// Mirrors the field on `WorkspaceInfo` / `WorkspaceEntry` /
    /// `StaleWorkspace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_command: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiffRevisionJson {
    label: String,
    rev: String,
    oid: String,
}

#[derive(Debug, Serialize)]
struct DiffStatsJson {
    files_changed: usize,
    added: usize,
    modified: usize,
    deleted: usize,
    renamed: usize,
    copied: usize,
    others: usize,
    additions: u32,
    deletions: u32,
}

#[derive(Debug, Serialize)]
struct DiffFileJson {
    path: String,
    old_path: Option<String>,
    status: String,
    additions: u32,
    deletions: u32,
    binary: bool,
}

pub fn diff(
    workspace: &str,
    against: Option<&str>,
    format: DiffFormat,
    paths: &[String],
) -> Result<()> {
    let ws_id = WorkspaceId::new(workspace)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{workspace}': {e}"))?;

    let root = repo_root()?;
    let backend = get_backend()?;

    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{workspace}' does not exist\n  Check: maw ws list\n  Fix: maw ws create {workspace}"
        );
    }

    let pathspecs = resolve_pathspecs(paths)?;

    let head = materialize_workspace_state(&backend, &root, &ws_id)?;
    let base = resolve_against(&backend, &root, &ws_id, against)?;

    // Use the workspace directory for diffing so that uncommitted/untracked
    // changes are included. We diff base_rev against the working tree (no
    // head_rev) and append untracked files as Added entries (bn-3bo8).
    let ws_path = backend.workspace_path(&ws_id);
    let diff_dir = if ws_path.exists() { &ws_path } else { &root };

    match format {
        DiffFormat::Patch => print_patch_worktree(diff_dir, &base.rev, &pathspecs)?,
        DiffFormat::Stat => print_stat_worktree(diff_dir, &base.rev, &pathspecs)?,
        DiffFormat::NameOnly | DiffFormat::NameStatus => {
            let mut entries = collect_diff_entries_worktree(diff_dir, &base.rev, &pathspecs)?;
            entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.status.cmp(&b.status)));
            if matches!(format, DiffFormat::NameOnly) {
                let mut names = BTreeSet::new();
                for e in &entries {
                    names.insert(e.path.clone());
                }
                for name in names {
                    println!("{name}");
                }
            } else {
                for e in &entries {
                    if let Some(old) = &e.old_path {
                        println!("{}\t{} -> {}", e.status, old, e.path);
                    } else {
                        println!("{}\t{}", e.status, e.path);
                    }
                }
            }
        }
        DiffFormat::Json => {
            let mut entries = collect_diff_entries_worktree(diff_dir, &base.rev, &pathspecs)?;
            entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.status.cmp(&b.status)));
            // bn-242l: compute the workspace's named lifecycle state so
            // the JSON payload carries the same safe-cleanup vocabulary
            // as `maw status --json` / `maw ws status` / `maw ws list`.
            // Agents inspecting `ws diff` output to decide their next
            // verb get the staleness signal up front.
            let lifecycle = classify_lifecycle_for_diff(&backend, &root, &ws_id);
            print_json(&ws_id, &base, &head, &entries, lifecycle.as_ref())?;
        }
    }

    Ok(())
}

/// Like `print_stat` but compares `base_rev` against the working tree (includes
/// uncommitted changes).
fn print_stat_worktree(ws_dir: &Path, base_rev: &str, pathspecs: &[String]) -> Result<()> {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let mut args = vec![
        "diff".to_string(),
        "--stat".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }

    if is_tty {
        let status = Command::new("git")
            .args(&args)
            .current_dir(ws_dir)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Failed to run git diff --stat")?;
        if !status.success() {
            bail!("git diff --stat exited with status {status}");
        }
    } else {
        args.insert(1, "--color=never".to_string());
        let out = git_stdout(ws_dir, &args)?;
        print!("{out}");
    }

    // Show untracked files in stat output
    let untracked = collect_untracked_files(ws_dir, pathspecs)?;
    for path in &untracked {
        println!(" {path} (untracked)");
    }

    Ok(())
}

/// Like `print_patch` but compares `base_rev` against the working tree.
fn print_patch_worktree(ws_dir: &Path, base_rev: &str, pathspecs: &[String]) -> Result<()> {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let mut diff_args = vec![
        "diff".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
    ];
    if !pathspecs.is_empty() {
        diff_args.push("--".to_string());
        diff_args.extend(pathspecs.iter().cloned());
    }

    if is_tty {
        let status = Command::new("git")
            .args(&diff_args)
            .current_dir(ws_dir)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Failed to run git diff")?;
        if !status.success() {
            bail!("git diff exited with status {status}");
        }
    } else {
        diff_args.insert(1, "--color=never".to_string());
        let patch = git_stdout(ws_dir, &diff_args)?;
        print!("{patch}");
    }

    // Append untracked file contents as pseudo-patches
    let untracked = collect_untracked_files(ws_dir, pathspecs)?;
    for path in &untracked {
        let full = ws_dir.join(path);
        if let Ok(content) = std::fs::read_to_string(&full) {
            println!("diff --git a/{path} b/{path}");
            println!("new file mode 100644");
            println!("--- /dev/null");
            println!("+++ b/{path}");
            let lines: Vec<&str> = content.lines().collect();
            println!("@@ -0,0 +1,{} @@", lines.len());
            for line in &lines {
                println!("+{line}");
            }
        }
    }

    Ok(())
}

/// Like `collect_diff_entries` but compares `base_rev` against the working tree
/// and appends untracked files as Added entries.
fn collect_diff_entries_worktree(
    ws_dir: &Path,
    base_rev: &str,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    let mut args = vec![
        "diff".to_string(),
        "--name-status".to_string(),
        "-z".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let raw = git_stdout_bytes(ws_dir, &args)?;
    let mut entries = parse_name_status_z(&raw)?;

    for entry in &mut entries {
        let stats = collect_numstat_for_entry_worktree(ws_dir, base_rev, entry)?;
        entry.additions = stats.0;
        entry.deletions = stats.1;
        entry.binary = stats.2;
    }

    // Append untracked files as Added entries
    let existing_paths: BTreeSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    let untracked = collect_untracked_files(ws_dir, pathspecs)?;
    for path in untracked {
        if existing_paths.contains(&path) {
            continue;
        }
        let full = ws_dir.join(&path);
        let line_count = std::fs::read_to_string(&full).map_or(0, |c| c.lines().count());
        entries.push(DiffEntry {
            path,
            old_path: None,
            status: "A".to_string(),
            additions: u32::try_from(line_count).unwrap_or(u32::MAX),
            deletions: 0,
            binary: false,
        });
    }

    Ok(entries)
}

/// Like `collect_numstat_for_entry` but compares against working tree.
fn collect_numstat_for_entry_worktree(
    ws_dir: &Path,
    base_rev: &str,
    entry: &DiffEntry,
) -> Result<(u32, u32, bool)> {
    let target_path = entry.path.as_str();
    let args = vec![
        "diff".to_string(),
        "--numstat".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
        "--".to_string(),
        target_path.to_string(),
    ];
    let raw = git_stdout(ws_dir, &args)?;
    parse_numstat_line(&raw)
}

/// Collect untracked files in the workspace directory.
fn collect_untracked_files(ws_dir: &Path, pathspecs: &[String]) -> Result<Vec<String>> {
    let mut args = vec![
        "ls-files".to_string(),
        "--others".to_string(),
        "--exclude-standard".to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let raw = git_stdout(ws_dir, &args)?;
    Ok(raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

fn print_json(
    ws_id: &WorkspaceId,
    base: &ResolvedRev,
    head: &ResolvedRev,
    entries: &[DiffEntry],
    lifecycle: Option<&DiffLifecycleInfo>,
) -> Result<()> {
    let stats = summarize(entries);
    let out = DiffJsonOutput {
        workspace: ws_id.as_str().to_owned(),
        against: DiffRevisionJson {
            label: base.label.clone(),
            rev: base.rev.clone(),
            oid: base.oid.clone(),
        },
        head: DiffRevisionJson {
            label: head.label.clone(),
            rev: head.rev.clone(),
            oid: head.oid.clone(),
        },
        stats,
        files: entries
            .iter()
            .map(|e| DiffFileJson {
                path: e.path.clone(),
                old_path: e.old_path.clone(),
                status: e.status.clone(),
                additions: e.additions,
                deletions: e.deletions,
                binary: e.binary,
            })
            .collect(),
        lifecycle_state: lifecycle.map(|l| l.state),
        behind_epochs: lifecycle.and_then(|l| l.behind_epochs),
        fix_command: lifecycle.and_then(|l| l.fix_command.clone()),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// bn-242l: lifecycle bundle for `maw ws diff --format json` output.
/// Keeps `print_json` total — the helper produces `None` only when
/// the backend has no record of the workspace (shouldn't happen
/// because `diff()` already gated on `backend.exists`), so this is
/// effectively always populated for non-default targets.
#[derive(Debug)]
struct DiffLifecycleInfo {
    state: LifecycleState,
    behind_epochs: Option<u32>,
    fix_command: Option<String>,
}

/// bn-242l: classify the lifecycle state of the diff target. Uses the
/// same signal set / priority order as `maw status --json` and `maw
/// ws status` / `maw ws list` so the four discovery surfaces cannot
/// disagree on whether a workspace is stale / committed-unintegrated /
/// dirty / clean. Conservative on errors — returns `None` rather
/// than crashing the diff command.
fn classify_lifecycle_for_diff<B: WorkspaceBackend>(
    backend: &B,
    root: &Path,
    ws_id: &WorkspaceId,
) -> Option<DiffLifecycleInfo>
where
    B::Error: std::fmt::Display,
{
    if ws_id.as_str() == DEFAULT_WORKSPACE {
        return None;
    }
    let workspaces = backend.list().ok()?;
    let ws = workspaces.into_iter().find(|w| &w.id == ws_id)?;
    let missing = !ws.path.exists();
    let rebase_conflicts = if missing {
        0
    } else {
        super::resolve::find_conflicted_files(&ws.path)
            .map_or(0, |f| u32::try_from(f.len()).unwrap_or(u32::MAX))
    };
    let has_uncommitted = if missing {
        false
    } else {
        maw_git::GixRepo::open(&ws.path)
            .ok()
            .and_then(|repo| repo.count_dirty_tracked().ok())
            .is_some_and(|c| c > 0)
    };
    let meta = metadata::read(root, ws.id.as_str()).unwrap_or_default();
    let signals = LifecycleSignals {
        missing,
        rebase_conflicts,
        is_stale: ws.state.is_stale(),
        commits_ahead: ws.commits_ahead,
        has_uncommitted,
        was_integrated: false,
            has_pinned_snapshot: false,
    };
    let state = LifecycleState::classify(signals);
    let behind = match ws.state {
        WorkspaceState::Stale { behind_epochs } => Some(behind_epochs),
        _ => None,
    };
    let fix_command = state.fix_command(ws.id.as_str(), meta.mode.is_persistent());
    Some(DiffLifecycleInfo {
        state,
        behind_epochs: behind,
        fix_command,
    })
}

fn summarize(entries: &[DiffEntry]) -> DiffStatsJson {
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut copied = 0usize;
    let mut others = 0usize;
    let mut additions = 0u32;
    let mut deletions = 0u32;

    for e in entries {
        match e.status.as_str() {
            "A" => added += 1,
            "M" => modified += 1,
            "D" => deleted += 1,
            "R" => renamed += 1,
            "C" => copied += 1,
            _ => others += 1,
        }
        additions = additions.saturating_add(e.additions);
        deletions = deletions.saturating_add(e.deletions);
    }

    DiffStatsJson {
        files_changed: entries.len(),
        added,
        modified,
        deleted,
        renamed,
        copied,
        others,
        additions,
        deletions,
    }
}

fn resolve_against<B: WorkspaceBackend>(
    backend: &B,
    root: &Path,
    workspace: &WorkspaceId,
    against_raw: Option<&str>,
) -> Result<ResolvedRev>
where
    B::Error: std::fmt::Display,
{
    match parse_against(against_raw) {
        AgainstMode::Default => {
            if workspace.as_str() == DEFAULT_WORKSPACE {
                return materialize_workspace_state(backend, root, workspace);
            }

            let default_id = WorkspaceId::new(DEFAULT_WORKSPACE)
                .expect("default workspace id should always be valid");
            if !backend.exists(&default_id) {
                bail!(
                    "Default workspace '{DEFAULT_WORKSPACE}' is missing\n  Fix: maw init\n  Check: maw doctor"
                );
            }
            materialize_workspace_state(backend, root, &default_id)
        }
        AgainstMode::Epoch => {
            let rev = manifold_refs::EPOCH_CURRENT.to_string();
            let oid = resolve_rev_oid(root, &rev).with_context(
                || "Current epoch ref is missing\n  Fix: maw init\n  Check: maw doctor",
            )?;
            Ok(ResolvedRev {
                label: "epoch".to_string(),
                rev,
                oid,
            })
        }
        AgainstMode::Branch(branch) => {
            let rev = if branch.starts_with("refs/") {
                branch
            } else {
                format!("refs/heads/{branch}")
            };
            let oid = resolve_rev_oid(root, &rev).with_context(|| {
                format!(
                    "Branch '{rev}' was not found\n  Check: git -C {} branch --list\n  Fix: use --against branch:<name>",
                    root.display()
                )
            })?;
            Ok(ResolvedRev {
                label: "branch".to_string(),
                rev,
                oid,
            })
        }
        AgainstMode::Oid(oid) => {
            let resolved = resolve_rev_oid(root, &oid).with_context(|| {
                format!(
                    "Revision '{oid}' is invalid\n  Check: git -C {} rev-parse {oid}",
                    root.display()
                )
            })?;
            Ok(ResolvedRev {
                label: "oid".to_string(),
                rev: oid,
                oid: resolved,
            })
        }
    }
}

fn materialize_workspace_state<B: WorkspaceBackend>(
    backend: &B,
    root: &Path,
    ws_id: &WorkspaceId,
) -> Result<ResolvedRev>
where
    B::Error: std::fmt::Display,
{
    backend
        .status(ws_id)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("Failed to inspect workspace '{}'.", ws_id.as_str()))?;

    let ws_ref = manifold_refs::workspace_state_ref(ws_id.as_str());
    if let Ok(oid) = resolve_rev_oid(root, &ws_ref) {
        return Ok(ResolvedRev {
            label: ws_id.as_str().to_owned(),
            rev: ws_ref,
            oid,
        });
    }

    let ws_path = backend.workspace_path(ws_id);
    let oid = resolve_rev_oid(&ws_path, "HEAD")
        .with_context(|| format!("Failed to resolve HEAD for workspace '{}'.", ws_id.as_str()))?;

    Ok(ResolvedRev {
        label: ws_id.as_str().to_owned(),
        rev: oid.clone(),
        oid,
    })
}

fn parse_against(raw: Option<&str>) -> AgainstMode {
    let Some(raw) = raw else {
        return AgainstMode::Default;
    };

    let value = raw.trim();
    if value.is_empty() || value == "default" {
        return AgainstMode::Default;
    }
    if value == "epoch" {
        return AgainstMode::Epoch;
    }
    if let Some(branch) = value.strip_prefix("branch:") {
        return AgainstMode::Branch(branch.to_string());
    }
    if let Some(oid) = value.strip_prefix("oid:") {
        return AgainstMode::Oid(oid.to_string());
    }
    if looks_like_oid(value) {
        return AgainstMode::Oid(value.to_string());
    }
    AgainstMode::Branch(value.to_string())
}

fn looks_like_oid(value: &str) -> bool {
    let len = value.len();
    (7..=40).contains(&len) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_pathspecs(paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in paths {
        let pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        Pattern::new(pattern)
            .with_context(|| format!("Invalid path glob '{pattern}' in --paths"))?;
        out.push(format!(":(glob){pattern}"));
    }
    Ok(out)
}

fn parse_numstat_line(raw: &str) -> Result<(u32, u32, bool)> {
    let Some(line) = raw.lines().next() else {
        return Ok((0, 0, false));
    };

    let mut parts = line.split('\t');
    let add = parts.next().unwrap_or("0");
    let del = parts.next().unwrap_or("0");
    if add == "-" || del == "-" {
        return Ok((0, 0, true));
    }
    let additions = add.parse::<u32>().unwrap_or(0);
    let deletions = del.parse::<u32>().unwrap_or(0);
    Ok((additions, deletions, false))
}

fn parse_name_status_z(raw: &[u8]) -> Result<Vec<DiffEntry>> {
    let mut entries = Vec::new();
    let tokens: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|t| !t.is_empty()).collect();
    let mut idx = 0usize;

    while idx < tokens.len() {
        let status_raw = std::str::from_utf8(tokens[idx])
            .context("invalid UTF-8 in git diff --name-status output")?
            .to_string();
        idx += 1;

        let status_code = status_raw
            .chars()
            .next()
            .map_or_else(|| "?".to_string(), |c| c.to_string());

        if status_code == "R" || status_code == "C" {
            if idx + 1 >= tokens.len() {
                bail!("malformed rename/copy record in git diff output");
            }
            let old_path = std::str::from_utf8(tokens[idx])
                .context("invalid UTF-8 path in diff output")?
                .to_string();
            let path = std::str::from_utf8(tokens[idx + 1])
                .context("invalid UTF-8 path in diff output")?
                .to_string();
            idx += 2;
            entries.push(DiffEntry {
                path,
                old_path: Some(old_path),
                status: status_code,
                additions: 0,
                deletions: 0,
                binary: false,
            });
            continue;
        }

        if idx >= tokens.len() {
            bail!("malformed record in git diff output");
        }
        let path = std::str::from_utf8(tokens[idx])
            .context("invalid UTF-8 path in diff output")?
            .to_string();
        idx += 1;
        entries.push(DiffEntry {
            path,
            old_path: None,
            status: status_code,
            additions: 0,
            deletions: 0,
            binary: false,
        });
    }

    Ok(entries)
}

fn resolve_rev_oid(root: &Path, rev: &str) -> Result<String> {
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let oid = repo
        .rev_parse(rev)
        .map_err(|e| anyhow::anyhow!("`rev-parse {rev}` failed: {e}"))?;
    Ok(oid.to_string())
}

// NOTE: git_stdout_simple was replaced by resolve_rev_oid for rev-parse calls.

fn git_stdout(dir: &Path, args: &[String]) -> Result<String> {
    let out = git_stdout_bytes(dir, args)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn git_stdout_bytes(dir: &Path, args: &[String]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_against_defaults_to_default() {
        assert!(matches!(parse_against(None), AgainstMode::Default));
        assert!(matches!(
            parse_against(Some("default")),
            AgainstMode::Default
        ));
    }

    #[test]
    fn parse_against_handles_epoch_branch_and_oid() {
        assert!(matches!(parse_against(Some("epoch")), AgainstMode::Epoch));
        assert!(matches!(
            parse_against(Some("branch:main")),
            AgainstMode::Branch(b) if b == "main"
        ));
        assert!(matches!(
            parse_against(Some("oid:abc1234")),
            AgainstMode::Oid(o) if o == "abc1234"
        ));
        assert!(matches!(
            parse_against(Some("a1b2c3d")),
            AgainstMode::Oid(o) if o == "a1b2c3d"
        ));
        assert!(matches!(
            parse_against(Some("feature/my-branch")),
            AgainstMode::Branch(b) if b == "feature/my-branch"
        ));
    }

    #[test]
    fn parse_name_status_z_parses_rename_and_modify() {
        let raw = b"R100\0old.rs\0new.rs\0M\0src/lib.rs\0";
        let parsed = parse_name_status_z(raw).expect("operation should succeed");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].status, "R");
        assert_eq!(parsed[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(parsed[0].path, "new.rs");
        assert_eq!(parsed[1].status, "M");
        assert_eq!(parsed[1].path, "src/lib.rs");
    }

    #[test]
    fn resolve_pathspecs_validates_globs() {
        let specs = resolve_pathspecs(&["src/**/*.rs".to_string(), "README*".to_string()])
            .expect("operation should succeed");
        assert_eq!(specs, vec![":(glob)src/**/*.rs", ":(glob)README*"]);
    }
}
