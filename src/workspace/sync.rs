use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{jj_cwd, repo_root, validate_workspace_name};

pub(crate) fn sync(all: bool) -> Result<()> {
    if all {
        return sync_all();
    }

    let cwd = jj_cwd()?;

    // First check if we're stale
    let status_check = Command::new("jj")
        .args(["status"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj status")?;

    let stderr = String::from_utf8_lossy(&status_check.stderr);
    let is_stale = stderr.contains("working copy is stale");

    if !is_stale {
        println!("Workspace is up to date.");
        return Ok(());
    }

    println!("Workspace is stale (another workspace changed shared history), syncing...");
    println!();

    // Run update-stale and capture output
    let update_output = Command::new("jj")
        .args(["workspace", "update-stale"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace update-stale")?;

    // Show the output
    let stdout = String::from_utf8_lossy(&update_output.stdout);
    let stderr = String::from_utf8_lossy(&update_output.stderr);

    if !stdout.trim().is_empty() {
        println!("{stdout}");
    }
    if !stderr.trim().is_empty() {
        // jj often puts useful info in stderr
        for line in stderr.lines() {
            // Skip the "Concurrent modification" noise
            if !line.contains("Concurrent modification") {
                println!("{line}");
            }
        }
    }

    if !update_output.status.success() {
        bail!(
            "Failed to sync workspace.\n  Check workspace state: maw ws status\n  Manual fix: jj workspace update-stale"
        );
    }

    // After sync, detect and auto-abandon empty divergent copies (common case).
    // This is a lightweight check that never fails the sync.
    auto_abandon_empty_divergent(&cwd);

    // Then run the full divergent resolution for any remaining complex cases.
    resolve_divergent_working_copy(&cwd)?;

    println!();
    println!("Workspace synced successfully.");

    Ok(())
}

/// After sync, detect and auto-resolve divergent commits on the workspace's
/// working copy. When `jj workspace update-stale` runs, it can fork the
/// workspace commit into multiple versions (e.g. `change_id/0` and `change_id/1`).
///
/// Resolution strategy (never abandons @, which would orphan the workspace pointer):
/// 1. Empty non-@ copies -> abandon
/// 2. Identical diffs (same changes on different parents) -> abandon non-@
/// 3. Non-@'s files <= @'s files -> abandon non-@ (@ is superset)
/// 4. @'s files <= non-@'s files -> squash non-@ into @ (recover extra files)
/// 5. Overlapping but different -> warn with actionable instructions
pub(crate) fn resolve_divergent_working_copy(workspace_dir: &Path) -> Result<()> {
    let (change_id, current_commit_id) = get_working_copy_ids(workspace_dir)?;
    if change_id.is_empty() {
        return Ok(());
    }

    let divergent_commits = get_divergent_commits(workspace_dir, &change_id)?;
    if divergent_commits.len() <= 1 {
        return Ok(());
    }

    println!();
    println!(
        "Detected {} divergent copies of workspace commit {change_id}.",
        divergent_commits.len()
    );
    println!("  (This happens when sync forks your commit. Auto-resolving...)");

    let copies = collect_copy_info(workspace_dir, &divergent_commits, &current_commit_id);

    let Some(current_idx) = copies.iter().position(|c| c.is_current) else {
        println!("  WARNING: Could not identify @ among divergent copies. Skipping auto-resolve.");
        return Ok(());
    };

    let unresolved = resolve_divergent_copies(workspace_dir, &copies, current_idx);

    if unresolved.is_empty() {
        println!("  Divergence fully resolved.");
    } else {
        print_unresolved_guidance(workspace_dir, &unresolved);
    }

    Ok(())
}

/// Get the working copy's change ID and commit ID.
fn get_working_copy_ids(workspace_dir: &Path) -> Result<(String, String)> {
    let change_output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to get working copy change ID")?;

    let change_id = String::from_utf8_lossy(&change_output.stdout).trim().to_string();

    let current_output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "commit_id.short()"])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to get current commit ID")?;
    let current_commit_id = String::from_utf8_lossy(&current_output.stdout).trim().to_string();

    Ok((change_id, current_commit_id))
}

/// Query jj for divergent copies of the given change ID.
fn get_divergent_commits(workspace_dir: &Path, change_id: &str) -> Result<Vec<String>> {
    let revset = format!("change_id({change_id})");
    let divergent_output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            r#"if(divergent, commit_id.short() ++ "\n", "")"#,
        ])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to check for divergent commits")?;

    let divergent_text = String::from_utf8_lossy(&divergent_output.stdout);
    Ok(divergent_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(std::string::ToString::to_string)
        .collect())
}

/// Info about a single divergent copy (diff text and changed file paths).
struct CopyInfo {
    commit_id: String,
    is_current: bool,
    diff: Option<String>,
    changed_files: Option<HashSet<String>>,
}

/// Collect diff and changed-file info for each divergent copy.
fn collect_copy_info(
    workspace_dir: &Path,
    divergent_commits: &[String],
    current_commit_id: &str,
) -> Vec<CopyInfo> {
    let mut copies = Vec::new();

    for commit_id in divergent_commits {
        let is_current = *commit_id == current_commit_id;

        let diff_output = Command::new("jj")
            .args(["diff", "-r", commit_id])
            .current_dir(workspace_dir)
            .output();

        let diff = match diff_output {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            }
            _ => None,
        };

        let summary_output = Command::new("jj")
            .args(["diff", "-r", commit_id, "--summary"])
            .current_dir(workspace_dir)
            .output();

        let changed_files = match summary_output {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| l.split_whitespace().nth(1).map(String::from))
                    .collect::<HashSet<String>>())
            }
            _ => None,
        };

        copies.push(CopyInfo {
            commit_id: commit_id.clone(),
            is_current,
            diff,
            changed_files,
        });
    }

    copies
}

/// Try to resolve each non-@ divergent copy against @.
/// Returns the list of commit IDs that could not be auto-resolved.
fn resolve_divergent_copies(
    workspace_dir: &Path,
    copies: &[CopyInfo],
    current_idx: usize,
) -> Vec<String> {
    let mut unresolved = Vec::new();

    for (i, copy) in copies.iter().enumerate() {
        if i == current_idx {
            continue;
        }

        let other_id = &copy.commit_id;

        let Some(other_diff) = &copy.diff else {
            unresolved.push(other_id.clone());
            continue;
        };

        // Case 1: non-@ is empty -> abandon
        if other_diff.is_empty() {
            abandon_copy(workspace_dir, other_id, "empty");
            continue;
        }

        // Cases 2-4 require @'s diff data
        let (Some(current_diff), Some(current_files)) =
            (&copies[current_idx].diff, &copies[current_idx].changed_files)
        else {
            unresolved.push(other_id.clone());
            continue;
        };

        // Case 2: identical diffs -> abandon non-@
        if other_diff == current_diff {
            abandon_copy(workspace_dir, other_id, "identical to @");
            continue;
        }

        let Some(other_files) = &copy.changed_files else {
            unresolved.push(other_id.clone());
            continue;
        };

        // Case 3: non-@'s files subset of @ -> abandon non-@
        if other_files.is_subset(current_files) {
            abandon_copy(workspace_dir, other_id, "subset of @");
            continue;
        }

        // Case 4: @'s files subset of non-@ -> squash non-@ into @
        if current_files.is_subset(other_files) {
            if try_squash_into_current(workspace_dir, other_id, other_files, current_files) {
                continue;
            }
            unresolved.push(other_id.clone());
            continue;
        }

        // Case 5: overlapping but different
        unresolved.push(other_id.clone());
    }

    unresolved
}

/// Attempt to squash a non-@ copy into @ to recover extra files.
/// Returns true on success.
fn try_squash_into_current(
    workspace_dir: &Path,
    other_id: &str,
    other_files: &HashSet<String>,
    current_files: &HashSet<String>,
) -> bool {
    let extra: Vec<&String> = other_files.difference(current_files).collect();

    let squash_result = Command::new("jj")
        .args(["squash", "--from", other_id, "--into", "@"])
        .env("JJ_EDITOR", "true")
        .current_dir(workspace_dir)
        .output();

    match squash_result {
        Ok(out) if out.status.success() => {
            println!("  Squashed {other_id} into @ (recovered {} extra file(s): {}).",
                extra.len(),
                extra.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", "));
            true
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("  WARNING: Could not squash {other_id} into @: {}", stderr.trim());
            false
        }
        Err(e) => {
            eprintln!("  WARNING: Could not squash {other_id} into @: {e}");
            false
        }
    }
}

/// Print actionable guidance for unresolved divergent copies.
fn print_unresolved_guidance(workspace_dir: &Path, unresolved: &[String]) {
    let ws_name = workspace_dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<ws>");

    println!();
    println!("  WARNING: {} divergent copy/copies could not be auto-resolved.", unresolved.len());
    println!("  @ and non-@ have overlapping but different changes.");
    println!("  To fix manually:");
    println!("    maw exec {ws_name} -- jj diff -r @             # see @ changes");
    for id in unresolved {
        println!("    maw exec {ws_name} -- jj diff -r {id:<12}   # see this copy's changes");
    }
    println!();
    println!("  To merge a copy's extra changes into @:");
    for id in unresolved {
        println!("    maw exec {ws_name} -- jj squash --from {id} --into @");
    }
    println!("  IMPORTANT: Do NOT abandon @ — this orphans the workspace pointer.");
}

/// Lightweight post-sync check: detect divergent commits on the workspace's
/// working copy and auto-abandon empty copies. This is a simpler, faster check
/// than the full `resolve_divergent_working_copy` — it only handles the common
/// case where sync creates an empty fork of the workspace commit.
///
/// Resolution:
/// - If exactly one copy is empty and others have content, abandon the empty one.
/// - Otherwise, warn and let the user resolve manually.
///
/// This function never fails the sync — errors are printed as warnings.
pub(crate) fn auto_abandon_empty_divergent(workspace_dir: &Path) {
    if let Err(e) = auto_abandon_empty_divergent_inner(workspace_dir) {
        eprintln!("  WARNING: divergent commit check failed: {e}");
    }
}

fn auto_abandon_empty_divergent_inner(workspace_dir: &Path) -> Result<()> {
    // Step 1: Get the current workspace's change-id (short 8-char form)
    let change_output = Command::new("jj")
        .args([
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "change_id.short(8)",
        ])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to get working copy change ID")?;

    let change_id = String::from_utf8_lossy(&change_output.stdout)
        .trim()
        .to_string();
    if change_id.is_empty() {
        return Ok(());
    }

    // Step 2: Check for divergent copies using change_id() revset function
    let revset = format!("change_id({change_id})");
    let copies_output = Command::new("jj")
        .args([
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            r#"commit_id.short(8) ++ "\n""#,
        ])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to check for divergent commits")?;

    let copies_text = String::from_utf8_lossy(&copies_output.stdout);
    let commit_ids: Vec<String> = copies_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();

    // Not divergent if only one (or zero) copies
    if commit_ids.len() <= 1 {
        return Ok(());
    }

    // Step 3: For each copy, check if it's empty using jj diff --stat
    let mut empty_ids: Vec<String> = Vec::new();
    let mut nonempty_ids: Vec<String> = Vec::new();

    for commit_id in &commit_ids {
        let diff_output = Command::new("jj")
            .args(["diff", "--stat", "-r", commit_id])
            .current_dir(workspace_dir)
            .output()
            .with_context(|| format!("Failed to check diff for commit {commit_id}"))?;

        let stat_text = String::from_utf8_lossy(&diff_output.stdout);
        if stat_text.trim().is_empty() {
            empty_ids.push(commit_id.clone());
        } else {
            nonempty_ids.push(commit_id.clone());
        }
    }

    // Step 4: If exactly one copy is empty and the rest have content, auto-abandon
    if empty_ids.len() == 1 && !nonempty_ids.is_empty() {
        let empty_id = &empty_ids[0];
        let result = Command::new("jj")
            .args(["abandon", empty_id])
            .current_dir(workspace_dir)
            .output();

        match result {
            Ok(out) if out.status.success() => {
                println!(
                    "Resolved divergent commit: abandoned empty copy {empty_id}"
                );
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "  WARNING: Failed to abandon empty copy {empty_id}: {}",
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "  WARNING: Failed to abandon empty copy {empty_id}: {e}"
                );
            }
        }
    } else {
        // Both have content, both are empty, or multiple empties — warn
        let ws_name = workspace_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<ws>");
        println!(
            "WARNING: Divergent commits detected for {change_id}. \
             Manual resolution needed: maw exec {ws_name} -- jj log -r 'change_id({change_id})'"
        );
    }

    Ok(())
}

/// Abandon a non-@ divergent copy, logging the reason.
pub(crate) fn abandon_copy(workspace_dir: &Path, commit_id: &str, reason: &str) {
    let result = Command::new("jj")
        .args(["abandon", commit_id])
        .current_dir(workspace_dir)
        .output();

    match result {
        Ok(out) if out.status.success() => {
            println!("  Abandoned {reason} copy: {commit_id}");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("  Warning: Failed to abandon {commit_id}: {}", stderr.trim());
        }
        Err(e) => {
            eprintln!("  Warning: Failed to abandon {commit_id}: {e}");
        }
    }
}

/// Sync all workspaces at once
fn sync_all() -> Result<()> {
    let root = repo_root()?;
    let cwd = jj_cwd()?;

    let workspace_names = list_workspace_names(&cwd)?;

    if workspace_names.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("Syncing {} workspace(s)...", workspace_names.len());
    println!();

    let (synced, already_current, errors) = sync_each_workspace(&workspace_names, &root)?;

    println!();
    println!(
        "Results: {} synced, {} already current, {} errors",
        synced,
        already_current,
        errors.len()
    );

    if !errors.is_empty() {
        println!();
        println!("Errors:");
        for err in &errors {
            println!("  - {err}");
        }
    }

    // Re-check for cascade staleness: syncing one workspace can make others stale again
    if synced > 0 {
        check_cascade_staleness(&workspace_names, &root);
    }

    Ok(())
}

/// Parse workspace names from jj workspace list output.
fn list_workspace_names(cwd: &Path) -> Result<Vec<String>> {
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);
    Ok(ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Sync each workspace, returning (synced_count, already_current_count, errors).
fn sync_each_workspace(
    workspace_names: &[String],
    root: &Path,
) -> Result<(usize, usize, Vec<String>)> {
    let mut synced = 0;
    let mut already_current = 0;
    let mut errors: Vec<String> = Vec::new();

    for ws in workspace_names {
        if validate_workspace_name(ws).is_err() {
            errors.push(format!("{ws}: invalid workspace name (skipped)"));
            continue;
        }

        let path = root.join("ws").join(ws);

        if !path.exists() {
            errors.push(format!("{ws}: directory missing"));
            continue;
        }

        // Check if stale
        let status = Command::new("jj")
            .args(["status"])
            .current_dir(&path)
            .output()
            .context("Failed to run jj status")?;

        let stderr = String::from_utf8_lossy(&status.stderr);
        if !stderr.contains("working copy is stale") {
            already_current += 1;
            continue;
        }

        // Sync
        let sync_result = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&path)
            .output();

        match sync_result {
            Ok(out) if out.status.success() => {
                if let Err(e) = resolve_divergent_working_copy(&path) {
                    eprintln!("  \u{2713} {ws} - synced (divergent resolution failed: {e})");
                } else {
                    println!("  \u{2713} {ws} - synced");
                }
                synced += 1;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                errors.push(format!("{ws}: {}", err.trim()));
            }
            Err(e) => {
                errors.push(format!("{ws}: {e}"));
            }
        }
    }

    Ok((synced, already_current, errors))
}

/// After syncing, re-check all workspaces for cascade staleness and warn if found.
fn check_cascade_staleness(workspace_names: &[String], root: &Path) {
    let mut cascade_stale = Vec::new();
    for ws in workspace_names {
        if validate_workspace_name(ws).is_err() {
            continue;
        }
        let path = root.join("ws").join(ws);
        if !path.exists() {
            continue;
        }
        let status = Command::new("jj")
            .args(["status"])
            .current_dir(&path)
            .output();
        if let Ok(out) = status {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("working copy is stale") {
                cascade_stale.push(ws.clone());
            }
        }
    }
    if !cascade_stale.is_empty() {
        println!();
        println!(
            "WARNING: {} workspace(s) became stale again (cascade effect): {}",
            cascade_stale.len(),
            cascade_stale.join(", ")
        );
        println!("  This happens when syncing one workspace modifies shared history.");
        println!("  Re-run: maw ws sync --all");
        println!();
        println!("  Tip: To avoid cascading, sync only your workspace:");
        for ws in &cascade_stale {
            println!("    maw ws sync {ws}");
        }
    }
}

/// Auto-sync a stale workspace before running a command.
/// If the workspace is stale, runs update-stale + divergent resolution.
/// Returns Ok(()) whether or not it was stale (idempotent).
pub fn auto_sync_if_stale(name: &str, path: &Path) -> Result<()> {
    let output = Command::new("jj")
        .args(["status"])
        .current_dir(path)
        .output()
        .context("Failed to check workspace status")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains("working copy is stale") {
        return Ok(());
    }

    eprintln!("Workspace '{name}' is stale — auto-syncing before running command...");

    let update_output = Command::new("jj")
        .args(["workspace", "update-stale"])
        .current_dir(path)
        .output()
        .context("Failed to run jj workspace update-stale")?;

    if !update_output.status.success() {
        let err = String::from_utf8_lossy(&update_output.stderr);
        bail!(
            "Auto-sync failed for workspace '{name}': {}\n  \
             Manual fix: maw ws sync {name}",
            err.trim()
        );
    }

    // Resolve any divergent commits created by sync
    resolve_divergent_working_copy(path)?;

    eprintln!("Workspace '{name}' synced. Proceeding with command.");
    eprintln!();

    Ok(())
}

/// Sync stale workspaces before merge to avoid spurious conflicts.
///
/// When a workspace is stale (its base commit is behind main), merging can produce
/// conflicts even when changes don't overlap - just because the workspace is missing
/// intermediate commits from main. This is especially problematic for append-only
/// files where jj's line-based merge would normally just concatenate.
///
/// This function checks each workspace being merged and syncs any that are stale,
/// ensuring all workspace commits are based on current main before the merge.
pub(crate) fn sync_stale_workspaces_for_merge(workspaces: &[String], root: &Path) -> Result<()> {
    let ws_dir = root.join("ws");
    let mut synced_count = 0;

    for ws in workspaces {
        let ws_path = ws_dir.join(ws);
        if !ws_path.exists() {
            // Workspace directory doesn't exist - the merge will fail later with a clearer error
            continue;
        }

        // Check if workspace is stale
        let status_output = Command::new("jj")
            .args(["status"])
            .current_dir(&ws_path)
            .output()
            .with_context(|| format!("Failed to check status of workspace '{ws}'"))?;

        let stderr = String::from_utf8_lossy(&status_output.stderr);
        if !stderr.contains("working copy is stale") {
            continue;
        }

        // Workspace is stale - sync it
        println!("Syncing stale workspace '{ws}' before merge...");

        let update_output = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&ws_path)
            .output()
            .with_context(|| format!("Failed to sync stale workspace '{ws}'"))?;

        if !update_output.status.success() {
            let err = String::from_utf8_lossy(&update_output.stderr);
            bail!(
                "Failed to sync stale workspace '{ws}': {}\n  \
                 Manual fix: maw ws sync\n  \
                 Then retry: maw ws merge {}",
                err.trim(),
                workspaces.join(" ")
            );
        }

        // Resolve any divergent commits created by the sync
        resolve_divergent_working_copy(&ws_path)?;

        synced_count += 1;
    }

    if synced_count > 0 {
        println!(
            "Synced {synced_count} stale workspace(s). Proceeding with merge."
        );
        println!();
    }

    Ok(())
}
