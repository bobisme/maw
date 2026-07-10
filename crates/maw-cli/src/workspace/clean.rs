//! `maw ws clean` — remove untracked files from a workspace, safely.
//!
//! A first-class, Prime-Invariant-respecting hygiene verb (bn-auu5, mess field
//! report bn-1m4d item 5). Worker agents leave untracked scratch (`.test-tmp/`,
//! fuzz corpora, bench dirs); the sync dirty-check correctly refuses to sync
//! over untracked files, but environment safety hooks block `rm -rf`,
//! `git clean -f`, and `mv`-then-delete inside `$HOME`. `maw ws clean` removes
//! the untracked files WITH a destroy-grade recovery snapshot first, giving
//! agents a guard-friendly way out.
//!
//! # Guarantees (Prime Invariant)
//!
//! * A recovery snapshot of the exact files being removed is captured and
//!   pinned under `refs/manifold/recovery/<ws>/clean-<ts>` BEFORE anything is
//!   deleted. If the snapshot fails, nothing is deleted.
//! * Tracked files (modified or not) are NEVER touched — only untracked (and,
//!   with `--ignored`, gitignore'd) files are removed.
//! * `--dry-run` deletes nothing and takes no snapshot.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::format::OutputFormat;
use crate::workspace::sync::checks::WorkspaceRebaseLock;
use crate::workspace::{DEFAULT_WORKSPACE, MawConfig, capture, repo_root, validate_workspace_name};
use maw_core::model::layout::LayoutFlavor;

/// Machine-readable result payload (`--format json`).
#[derive(Serialize)]
struct CleanReport {
    workspace: String,
    dry_run: bool,
    removed_count: usize,
    /// Repo-relative paths removed (or that would be removed under `--dry-run`).
    removed: Vec<String>,
    /// Distinct top-level entries (first path component) for a compact summary.
    top_level: Vec<String>,
    /// `Some` only after a real (non-dry-run) clean.
    recovery_ref: Option<String>,
    snapshot_oid: Option<String>,
    /// Executable command to restore the snapshot into a new workspace.
    restore_command: Option<String>,
}

/// Entry point for `maw ws clean`.
pub fn clean(
    name: Option<String>,
    paths: Vec<String>,
    dry_run: bool,
    ignored: bool,
    force: bool,
    format: OutputFormat,
) -> Result<()> {
    let root = repo_root()?;
    let flavor = LayoutFlavor::detect_with_env(&root);

    let target = name.unwrap_or_else(|| DEFAULT_WORKSPACE.to_string());
    let default_name = MawConfig::load(&root).map_or_else(
        |_| DEFAULT_WORKSPACE.to_string(),
        |c| c.default_workspace().to_owned(),
    );
    let is_default = target == DEFAULT_WORKSPACE || target == default_name;

    // The default workspace (repo root in consolidated layout) also collects
    // scratch, so it is cleanable — but require --force to avoid a fat-finger
    // wiping scratch next to real work.
    if is_default && !force {
        bail!(
            "Refusing to clean the default workspace without --force.\n  \
             The default workspace is the repo root; scratch there sits next to real work.\n  \
             Re-run with: maw ws clean {target} --force"
        );
    }

    // Resolve the on-disk worktree path (layout-aware).
    let ws_path = if is_default {
        flavor.default_target_path(&root, DEFAULT_WORKSPACE)
    } else {
        validate_workspace_name(&target)?;
        let p = flavor.workspace_path(&root, &target);
        if !p.exists() {
            bail!(
                "Workspace '{target}' does not exist.\n  Check available workspaces: maw ws list"
            );
        }
        p
    };

    // Take the SAME per-workspace lock sync/rebase use, so clean cannot race a
    // concurrent rebase of this workspace (bn-auu5 design point 5). NOT the
    // epoch lock — clean mutates only one workspace's untracked files.
    let _lock = match WorkspaceRebaseLock::try_acquire(&root, &target) {
        Ok(Some(guard)) => guard,
        Ok(None) => bail!(
            "Workspace '{target}' is busy: a sync, rebase, or clean is already running for it.\n  \
             Wait for it to finish, then retry."
        ),
        Err(e) => bail!("Failed to acquire workspace lock for '{target}': {e}"),
    };

    // Collect the untracked (and optionally ignored) files to remove.
    let candidates = collect_candidates(&ws_path, ignored, paths)?;

    if candidates.is_empty() {
        emit_report(
            format,
            &CleanReport {
                workspace: target,
                dry_run,
                removed_count: 0,
                removed: vec![],
                top_level: vec![],
                recovery_ref: None,
                snapshot_oid: None,
                restore_command: None,
            },
            "Nothing to clean.",
        );
        return Ok(());
    }

    if dry_run {
        let report = CleanReport {
            workspace: target,
            dry_run: true,
            removed_count: candidates.len(),
            top_level: top_level_of(&candidates),
            removed: candidates,
            recovery_ref: None,
            snapshot_oid: None,
            restore_command: None,
        };
        emit_report(format, &report, "");
        return Ok(());
    }

    // Snapshot BEFORE deleting anything. Abort on failure — Prime Invariant.
    let capture = capture::capture_before_clean(&ws_path, &target, &candidates)
        .context("failed to capture recovery snapshot before clean")?
        .context("clean capture produced no snapshot despite selected files")?;

    // Delete the untracked files, then prune directories left empty.
    remove_candidates(&ws_path, &candidates)?;

    let restore_command = format!(
        "maw ws recover --ref {} --to {target}-restored",
        capture.pinned_ref
    );
    let report = CleanReport {
        workspace: target,
        dry_run: false,
        removed_count: candidates.len(),
        top_level: top_level_of(&candidates),
        removed: candidates,
        recovery_ref: Some(capture.pinned_ref.clone()),
        snapshot_oid: Some(capture.commit_oid.as_str().to_owned()),
        restore_command: Some(restore_command),
    };
    emit_report(format, &report, "");
    Ok(())
}

/// List the untracked (and, when `include_ignored`, gitignore'd) files under
/// `ws_path`, filtered to `path_filters` when non-empty.
fn collect_candidates(
    ws_path: &Path,
    include_ignored: bool,
    path_filters: Vec<String>,
) -> Result<Vec<String>> {
    use maw_git::GitRepo as _;

    // Untracked, non-ignored files via gix (honors .gitignore AND — after the
    // bn-auu5 fix — the common-dir info/exclude on linked worktrees). This is
    // the SAME status source the sync dirty-check uses, so clean is exactly the
    // cure for the sync refusal.
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let mut set: BTreeSet<String> = repo
        .list_untracked()
        .map_err(|e| anyhow::anyhow!("failed to list untracked files: {e}"))?
        .into_iter()
        .collect();

    if include_ignored {
        // gix's status omits ignored files; list them via git (which natively
        // honors the common-dir info/exclude). Mirrors `git clean -x`.
        for p in git_ls_ignored(ws_path)? {
            set.insert(p);
        }
    }

    let mut candidates: Vec<String> = set.into_iter().collect();

    if !path_filters.is_empty() {
        candidates.retain(move |c| path_filters.iter().any(|f| path_matches_filter(c, f)));
    }

    candidates.sort();
    Ok(candidates)
}

/// `git ls-files --others --ignored --exclude-standard -z` — the gitignore'd
/// untracked files (used only for `--ignored`).
fn git_ls_ignored(ws_path: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(ws_path)
        .output()
        .context("failed to run git ls-files for ignored files")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git ls-files (ignored) failed: {}", stderr.trim());
    }
    Ok(out
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

/// Whether repo-relative `path` is selected by a `--paths` filter entry.
///
/// A filter matches when it equals the path exactly, or names a directory that
/// contains it (`filter` == a leading path component boundary).
fn path_matches_filter(path: &str, filter: &str) -> bool {
    let filter = filter.trim_end_matches('/');
    if path == filter {
        return true;
    }
    path.starts_with(&format!("{filter}/"))
}

/// Distinct top-level entries (first path component), sorted, for a compact
/// human summary.
fn top_level_of(paths: &[String]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for p in paths {
        let top = p.split('/').next().unwrap_or(p);
        set.insert(top.to_owned());
    }
    set.into_iter().collect()
}

/// Remove each candidate file (relative to `ws_path`) via `std::fs`, then prune
/// directories left empty. Uses `std::fs` (not a shelled `rm`), so it works in
/// environments whose safety hooks block deletion commands.
fn remove_candidates(ws_path: &Path, candidates: &[String]) -> Result<()> {
    let mut parents: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in candidates {
        let abs = ws_path.join(rel);
        match std::fs::symlink_metadata(&abs) {
            Ok(_) => {
                std::fs::remove_file(&abs)
                    .with_context(|| format!("failed to remove {}", abs.display()))?;
            }
            // Already gone (e.g. removed by a prior candidate under a symlink) —
            // idempotent, keep going.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!("failed to stat {}: {e}", abs.display()));
            }
        }
        let mut parent = abs.parent().map(Path::to_path_buf);
        while let Some(dir) = parent {
            if dir == ws_path {
                break;
            }
            parents.insert(dir.clone());
            parent = dir.parent().map(Path::to_path_buf);
        }
    }

    // Prune now-empty directories, deepest first (longest path first).
    let mut dirs: Vec<PathBuf> = parents.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.as_os_str().len()));
    for dir in dirs {
        // Best-effort: only removes when empty; a non-empty dir is left intact.
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

/// Render the result to the requested format. `empty_msg` (non-empty only for
/// the nothing-to-clean case) is printed verbatim for text/pretty.
fn emit_report(format: OutputFormat, report: &CleanReport, empty_msg: &str) {
    match format {
        OutputFormat::Json => {
            // Serialization of a plain struct cannot fail; fall back defensively.
            match serde_json::to_string_pretty(report) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("failed to serialize clean report: {e}"),
            }
        }
        OutputFormat::Text | OutputFormat::Pretty => {
            if !empty_msg.is_empty() {
                println!("{empty_msg}");
                return;
            }
            if report.dry_run {
                println!(
                    "Would remove {} untracked file(s) from '{}':",
                    report.removed_count, report.workspace
                );
                for t in &report.top_level {
                    println!("  {t}");
                }
                println!("Dry run: nothing was removed and no snapshot was taken.");
                return;
            }
            println!(
                "Removed {} untracked file(s) from '{}':",
                report.removed_count, report.workspace
            );
            for t in &report.top_level {
                println!("  {t}");
            }
            if let Some(ref r) = report.recovery_ref {
                println!("Recovery snapshot: {r}");
            }
            if let Some(ref cmd) = report.restore_command {
                println!("Restore all:  {cmd}");
                println!("Inspect:      maw ws recover {}", report.workspace);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matches_filter_exact_and_dir() {
        assert!(path_matches_filter("scratch.tmp", "scratch.tmp"));
        assert!(path_matches_filter(".test-tmp/deep/a.bin", ".test-tmp"));
        assert!(path_matches_filter(".test-tmp/deep/a.bin", ".test-tmp/"));
        assert!(path_matches_filter("a/b/c.txt", "a/b"));
        assert!(!path_matches_filter("scratchX.tmp", "scratch"));
        assert!(!path_matches_filter("other/file", ".test-tmp"));
        // Prefix must fall on a component boundary.
        assert!(!path_matches_filter(".test-tmpX/y", ".test-tmp"));
    }

    #[test]
    fn top_level_of_dedups_first_component() {
        let paths = vec![
            ".test-tmp/deep/a.bin".to_string(),
            ".test-tmp/b.bin".to_string(),
            "scratch.tmp".to_string(),
        ];
        assert_eq!(top_level_of(&paths), vec![".test-tmp", "scratch.tmp"]);
    }

    #[test]
    fn remove_candidates_removes_files_and_prunes_empty_dirs() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".test-tmp/deep")).expect("mkdir");
        std::fs::write(ws.join(".test-tmp/deep/a.bin"), "a").expect("write");
        std::fs::write(ws.join("scratch.tmp"), "s").expect("write");
        // A sibling file that is NOT a candidate keeps its directory alive.
        std::fs::create_dir_all(ws.join("keep-dir")).expect("mkdir");
        std::fs::write(ws.join("keep-dir/keep.txt"), "k").expect("write");
        std::fs::write(ws.join("keep-dir/gone.tmp"), "g").expect("write");

        let candidates = vec![
            ".test-tmp/deep/a.bin".to_string(),
            "scratch.tmp".to_string(),
            "keep-dir/gone.tmp".to_string(),
        ];
        remove_candidates(ws, &candidates).expect("remove");

        assert!(!ws.join("scratch.tmp").exists(), "scratch removed");
        assert!(!ws.join(".test-tmp").exists(), "emptied dir tree pruned");
        assert!(!ws.join("keep-dir/gone.tmp").exists(), "candidate removed");
        assert!(
            ws.join("keep-dir/keep.txt").exists(),
            "non-empty dir preserved"
        );
        assert!(ws.join("keep-dir").is_dir(), "non-empty dir kept");
    }

    #[test]
    fn remove_candidates_is_idempotent_for_missing_files() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ws = dir.path();
        // File does not exist — must not error.
        remove_candidates(ws, &["ghost.tmp".to_string()]).expect("no error on missing");
    }
}
