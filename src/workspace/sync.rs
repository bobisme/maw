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

    // After sync, check for and auto-resolve divergent commits on our working copy.
    // This happens when update-stale creates a fork of the workspace commit.
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
    // Get the working copy's change ID and commit ID
    let change_output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to get working copy change ID")?;

    let change_id = String::from_utf8_lossy(&change_output.stdout).trim().to_string();
    if change_id.is_empty() {
        return Ok(());
    }

    let current_output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "commit_id.short()"])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to get current commit ID")?;
    let current_commit_id = String::from_utf8_lossy(&current_output.stdout).trim().to_string();

    // Check if this change ID has divergent copies.
    // Must use change_id() revset function -- bare change_id errors on divergent changes.
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
    let divergent_commits: Vec<String> = divergent_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    if divergent_commits.len() <= 1 {
        return Ok(());
    }

    println!();
    println!(
        "Detected {} divergent copies of workspace commit {change_id}.",
        divergent_commits.len()
    );
    println!("  (This happens when sync forks your commit. Auto-resolving...)");

    // Collect diff and changed-file info for each copy.
    // diff/changed_files are None when the jj command fails (e.g., commit
    // already abandoned). We only auto-resolve when we have reliable data.
    struct CopyInfo {
        commit_id: String,
        is_current: bool,
        diff: Option<String>,
        changed_files: Option<HashSet<String>>,
    }

    let mut copies: Vec<CopyInfo> = Vec::new();

    for commit_id in &divergent_commits {
        let is_current = *commit_id == current_commit_id;

        // Full diff text (for identity comparison)
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

        // Changed file paths (for subset comparison).
        // --summary format: "M path/to/file" -- extract just the path.
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

    // Find the @ copy index
    let Some(current_idx) = copies.iter().position(|c| c.is_current) else {
        println!("  WARNING: Could not identify @ among divergent copies. Skipping auto-resolve.");
        return Ok(());
    };

    // Process each non-@ copy against @
    let mut unresolved = Vec::new();

    for i in 0..copies.len() {
        if i == current_idx {
            continue;
        }

        let other_id = copies[i].commit_id.clone();

        // If we couldn't read this copy's diff, skip auto-resolve (safe default).
        let Some(other_diff) = &copies[i].diff else {
            unresolved.push(other_id);
            continue;
        };

        // Case 1: non-@ is empty -> abandon
        if other_diff.is_empty() {
            abandon_copy(workspace_dir, &other_id, "empty");
            continue;
        }

        // Cases 2-4 require @'s diff data. If unavailable, skip.
        let (Some(current_diff), Some(current_files)) =
            (&copies[current_idx].diff, &copies[current_idx].changed_files)
        else {
            unresolved.push(other_id);
            continue;
        };

        // Case 2: identical diffs -> abandon non-@ (same changes, different parent)
        if other_diff == current_diff {
            abandon_copy(workspace_dir, &other_id, "identical to @");
            continue;
        }

        // Cases 3-4 require non-@'s file list. If unavailable, skip.
        let Some(other_files) = &copies[i].changed_files else {
            unresolved.push(other_id);
            continue;
        };

        // Case 3: non-@'s files <= @'s files -> abandon non-@ (@ has everything)
        if other_files.is_subset(current_files) {
            abandon_copy(workspace_dir, &other_id, "subset of @");
            continue;
        }

        // Case 4: @'s files <= non-@'s files -> squash non-@ into @ (recover extra files)
        if current_files.is_subset(other_files) {
            let extra: Vec<&String> = other_files
                .difference(current_files)
                .collect();

            // JJ_EDITOR=true prevents jj from opening an editor to merge
            // the differing descriptions of the two divergent copies.
            let squash_result = Command::new("jj")
                .args(["squash", "--from", &other_id, "--into", "@"])
                .env("JJ_EDITOR", "true")
                .current_dir(workspace_dir)
                .output();

            match squash_result {
                Ok(out) if out.status.success() => {
                    println!("  Squashed {other_id} into @ (recovered {} extra file(s): {}).",
                        extra.len(),
                        extra.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", "));
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    eprintln!("  WARNING: Could not squash {other_id} into @: {}", stderr.trim());
                    unresolved.push(other_id);
                }
                Err(e) => {
                    eprintln!("  WARNING: Could not squash {other_id} into @: {e}");
                    unresolved.push(other_id);
                }
            }
            continue;
        }

        // Case 5: overlapping but different -- cannot auto-resolve
        unresolved.push(other_id);
    }

    // Report unresolved copies with actionable instructions
    if unresolved.is_empty() {
        println!("  Divergence fully resolved.");
    } else {
        let ws_name = workspace_dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<ws>");

        println!();
        println!("  WARNING: {} divergent copy/copies could not be auto-resolved.", unresolved.len());
        println!("  @ and non-@ have overlapping but different changes.");
        println!("  To fix manually:");
        println!("    maw exec {ws_name} -- jj diff -r @             # see @ changes");
        for id in &unresolved {
            println!("    maw exec {ws_name} -- jj diff -r {id:<12}   # see this copy's changes");
        }
        println!();
        println!("  To merge a copy's extra changes into @:");
        for id in &unresolved {
            println!("    maw exec {ws_name} -- jj squash --from {id} --into @");
        }
        println!("  IMPORTANT: Do NOT abandon @ — this orphans the workspace pointer.");
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

    // Get all workspaces
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);

    // Parse workspace names (format: "name@: change_id ..." or "name: change_id ...")
    let workspace_names: Vec<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if workspace_names.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("Syncing {} workspace(s)...", workspace_names.len());
    println!();

    let mut synced = 0;
    let mut already_current = 0;
    let mut errors: Vec<String> = Vec::new();

    for ws in &workspace_names {
        // Validate workspace name to prevent path traversal (defense-in-depth)
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
                // Check for and resolve divergent commits after sync
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
        let mut cascade_stale = Vec::new();
        for ws in &workspace_names {
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

    Ok(())
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
