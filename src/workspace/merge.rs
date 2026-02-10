use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use glob::Pattern;

use super::sync::resolve_divergent_working_copy;
use super::{jj_cwd, repo_root, MawConfig, DEFAULT_WORKSPACE};

/// Check for conflicts after merge and auto-resolve paths matching config patterns.
/// Returns true if there are remaining (unresolved) conflicts.
fn auto_resolve_conflicts(cwd: &Path, config: &MawConfig, branch: &str) -> Result<bool> {
    // Check for conflicts
    let status_output = Command::new("jj")
        .args(["status"])
        .current_dir(cwd)
        .output()
        .context("Failed to check status")?;

    let status_text = String::from_utf8_lossy(&status_output.stdout);
    if !status_text.contains("conflict") {
        return Ok(false);
    }

    // Get list of conflicted files
    let conflicted_files = get_conflicted_files(cwd)?;
    if conflicted_files.is_empty() {
        return Ok(false);
    }

    // Check if we have patterns to auto-resolve
    let patterns = &config.merge.auto_resolve_from_main;
    if patterns.is_empty() {
        println!();
        println!("WARNING: Merge has conflicts that need resolution.");
        println!("Run `jj status` to see conflicted files.");
        return Ok(true);
    }

    // Compile glob patterns
    let compiled_patterns: Vec<Pattern> = patterns
        .iter()
        .filter_map(|p| Pattern::new(p).ok())
        .collect();

    // Find files to auto-resolve
    let mut auto_resolved = Vec::new();
    let mut remaining_conflicts = Vec::new();

    for file in &conflicted_files {
        let matches_pattern = compiled_patterns.iter().any(|pat| pat.matches(file));
        if matches_pattern {
            auto_resolved.push(file.clone());
        } else {
            remaining_conflicts.push(file.clone());
        }
    }

    // Auto-resolve matching files by restoring from main
    if !auto_resolved.is_empty() {
        println!();
        println!(
            "Auto-resolving {} file(s) from {branch} (via .maw.toml config):",
            auto_resolved.len()
        );
        for file in &auto_resolved {
            // Restore file from branch to resolve conflict
            let restore_output = Command::new("jj")
                .args(["restore", "--from", branch, file])
                .current_dir(cwd)
                .output()
                .context("Failed to restore file from main")?;

            if restore_output.status.success() {
                println!("  \u{2713} {file}");
            } else {
                let stderr = String::from_utf8_lossy(&restore_output.stderr);
                println!("  \u{2717} {file}: {}", stderr.trim());
                remaining_conflicts.push(file.clone());
            }
        }
    }

    // Report remaining conflicts
    if !remaining_conflicts.is_empty() {
        println!();
        println!(
            "WARNING: {} conflict(s) remaining that need manual resolution:",
            remaining_conflicts.len()
        );
        for file in &remaining_conflicts {
            println!("  - {file}");
        }
        println!();
        println!("Run `jj status` to see details.");
        return Ok(true);
    }

    println!();
    println!("All conflicts auto-resolved from main.");
    Ok(false)
}

/// Get list of files with conflicts from jj status output.
fn get_conflicted_files(cwd: &Path) -> Result<Vec<String>> {
    // Use jj status to get conflicted files
    // Format: "C filename" for conflicted files
    let output = Command::new("jj")
        .args(["status"])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj status")?;

    let status_text = String::from_utf8_lossy(&output.stdout);
    let mut files = Vec::new();

    for line in status_text.lines() {
        // jj status shows conflicts as "C path/to/file"
        if let Some(stripped) = line.strip_prefix("C ") {
            files.push(stripped.trim().to_string());
        }
    }

    Ok(files)
}

/// Run a list of hook commands. Returns Ok(()) if all succeed or hooks are empty.
/// For pre-merge hooks: aborts on first failure.
/// For post-merge hooks: warns but continues on failure.
fn run_hooks(hooks: &[String], hook_type: &str, root: &Path, abort_on_failure: bool) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }

    println!("Running {hook_type} hooks...");

    for (i, cmd) in hooks.iter().enumerate() {
        println!("  [{}/{}] {cmd}", i + 1, hooks.len());

        // Use shell to execute the command (allows pipes, redirects, etc.)
        // Security note: These commands come from .maw.toml which is checked into
        // the repo and controlled by the project owner. This is intentional and
        // similar to how git hooks, npm scripts, and Makefiles work.
        let output = Command::new("sh")
            .args(["-c", cmd])
            .current_dir(root)
            .output()
            .with_context(|| format!("Failed to execute hook command: {cmd}"))?;

        // Show output if any
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            for line in stdout.lines() {
                println!("      {line}");
            }
        }
        if !stderr.trim().is_empty() {
            for line in stderr.lines() {
                eprintln!("      {line}");
            }
        }

        if !output.status.success() {
            let exit_code = output.status.code().unwrap_or(-1);
            if abort_on_failure {
                bail!(
                    "{hook_type} hook failed (exit code {exit_code}): {cmd}\n  \
                     Merge aborted. Fix the issue and try again."
                );
            }
            eprintln!("  WARNING: {hook_type} hook failed (exit code {exit_code}): {cmd}");
        }
    }

    println!("{hook_type} hooks complete.");
    Ok(())
}

/// Preview what a merge would do without creating any commits.
/// Shows changes in each workspace and potential conflicts.
fn preview_merge(workspaces: &[String], cwd: &Path) -> Result<()> {
    println!("=== Merge Preview (dry run) ===");
    println!();

    if workspaces.len() == 1 {
        println!("Would adopt workspace: {}", workspaces[0]);
    } else {
        println!("Would merge workspaces: {}", workspaces.join(", "));
    }
    println!();

    // Show changes in each workspace using jj diff --stat
    println!("=== Changes by Workspace ===");
    println!();

    for ws in workspaces {
        println!("--- {ws} ---");

        // Get diff stats for the workspace using workspace@ syntax
        let diff_output = Command::new("jj")
            .args(["diff", "--stat", "-r", &format!("{ws}@")])
            .current_dir(cwd)
            .output()
            .with_context(|| format!("Failed to get diff for workspace {ws}"))?;

        if !diff_output.status.success() {
            let stderr = String::from_utf8_lossy(&diff_output.stderr);
            println!("  Could not get changes: {}", stderr.trim());
            println!();
            continue;
        }

        let diff_text = String::from_utf8_lossy(&diff_output.stdout);
        if diff_text.trim().is_empty() {
            println!("  (no changes)");
        } else {
            for line in diff_text.lines() {
                println!("  {line}");
            }
        }
        println!();
    }

    // Check for potential conflicts using files modified in multiple workspaces
    if workspaces.len() > 1 {
        println!("=== Potential Conflicts ===");
        println!();

        // Get files modified in each workspace
        let mut workspace_files: Vec<(String, Vec<String>)> = Vec::new();

        for ws in workspaces {
            let diff_output = Command::new("jj")
                .args(["diff", "--summary", "-r", &format!("{ws}@")])
                .current_dir(cwd)
                .output()
                .with_context(|| format!("Failed to get diff summary for {ws}"))?;

            if diff_output.status.success() {
                let diff_text = String::from_utf8_lossy(&diff_output.stdout);
                let files: Vec<String> = diff_text
                    .lines()
                    .filter_map(|line| {
                        // Format: "M path/to/file" or "A path/to/file"
                        line.split_whitespace().nth(1).map(std::string::ToString::to_string)
                    })
                    .collect();
                workspace_files.push((ws.clone(), files));
            }
        }

        // Find files modified in multiple workspaces
        let mut conflict_files: Vec<String> = Vec::new();
        for i in 0..workspace_files.len() {
            for j in (i + 1)..workspace_files.len() {
                let (ws1, files1) = &workspace_files[i];
                let (ws2, files2) = &workspace_files[j];
                for file in files1 {
                    if files2.contains(file) && !conflict_files.contains(file) {
                        conflict_files.push(file.clone());
                        println!("  ! {file} - modified in both '{ws1}' and '{ws2}'");
                    }
                }
            }
        }

        if conflict_files.is_empty() {
            println!("  (no overlapping changes detected)");
        } else {
            println!();
            println!("  Note: jj records conflicts in commits instead of blocking.");
            println!("  You can proceed and resolve conflicts after merge if needed.");
        }
        println!();
    }

    println!("=== Summary ===");
    println!();
    println!("To perform this merge, run without --dry-run:");
    println!("  maw ws merge {}", workspaces.join(" "));
    println!();

    Ok(())
}

pub(crate) fn merge(
    workspaces: &[String],
    destroy_after: bool,
    confirm: bool,
    message: Option<&str>,
    dry_run: bool,
    _auto_describe: bool, // No longer needed with linear merge approach
) -> Result<()> {
    let ws_to_merge = workspaces.to_vec();

    if ws_to_merge.is_empty() {
        println!("No workspaces to merge.");
        return Ok(());
    }

    let root = repo_root()?;
    let cwd = jj_cwd()?;

    // Load config early for hooks, auto-resolve settings, and branch name
    let config = MawConfig::load(&root)?;
    let default_ws = config.default_workspace();
    let branch = config.branch();

    // Reject merging the default workspace -- it's the merge target, not a source.
    // Merging it into itself is a no-op that can corrupt the working copy.
    if ws_to_merge.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot merge the default workspace \u{2014} it is the merge target, not a source.\n\
             \n  To advance {branch} to include your edits in {default_ws}:\n\
             \n    maw push --advance\n\
             \n  This moves the {branch} bookmark to your latest commit and pushes."
        );
    }

    // Preview mode: show what the merge would do without committing
    if dry_run {
        return preview_merge(&ws_to_merge, &cwd);
    }

    // Run pre-merge hooks (abort on failure)
    run_hooks(&config.hooks.pre_merge, "pre-merge", &root, true)?;

    // Sync stale workspaces before merge to avoid spurious conflicts.
    // When a workspace's base commit is behind main, merging can produce conflicts
    // even when changes don't overlap. Syncing first ensures all workspace commits
    // are based on current main.
    super::sync::sync_stale_workspaces_for_merge(&ws_to_merge, &root)?;

    if ws_to_merge.len() == 1 {
        println!("Adopting workspace: {}", ws_to_merge[0]);
    } else {
        println!("Merging workspaces: {}", ws_to_merge.join(", "));
    }
    println!();

    // Build revision references using workspace@ syntax
    let revisions: Vec<String> = ws_to_merge.iter().map(|ws| format!("{ws}@")).collect();

    // Record parent commit IDs before rebase, so we can abandon only these
    // specific scaffolding commits afterward (not commits from other workspaces).
    let parents_revset = revisions
        .iter()
        .map(|r| format!("parents({r})"))
        .collect::<Vec<_>>()
        .join(" | ");
    let parents_output = Command::new("jj")
        .args(["log", "-r", &parents_revset, "--no-graph", "-T", "commit_id ++ \"\\n\""])
        .current_dir(&cwd)
        .output();
    let pre_rebase_parent_ids: Vec<String> = parents_output
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    // Build merge commit message
    let msg = message.map_or_else(
        || {
            if ws_to_merge.len() == 1 {
                format!("merge: adopt work from {}", ws_to_merge[0])
            } else {
                format!("merge: combine work from {}", ws_to_merge.join(", "))
            }
        },
        ToString::to_string,
    );

    // NEW APPROACH: Rebase workspace commits directly onto main for linear history
    // This skips scaffolding commits and produces a cleaner graph.

    // Step 1: Rebase all workspace commits onto main
    let revset = revisions.join(" | ");
    let rebase_output = Command::new("jj")
        .args(["rebase", "-r", &revset, "-d", branch])
        .current_dir(&cwd)
        .output()
        .context("Failed to rebase workspace commits")?;

    if !rebase_output.status.success() {
        let stderr = String::from_utf8_lossy(&rebase_output.stderr);
        bail!(
            "Failed to rebase workspace commits onto {branch}: {}\n  Verify workspaces exist: maw ws list",
            stderr.trim()
        );
    }

    // Step 2: Apply message and squash if needed
    if ws_to_merge.len() > 1 {
        // Squash all but first into the first workspace's commit
        let first_ws = format!("{}@", ws_to_merge[0]);
        let others: Vec<String> = ws_to_merge[1..].iter().map(|ws| format!("{ws}@")).collect();
        let from_revset = others.join(" | ");

        let squash_output = Command::new("jj")
            .args([
                "squash",
                "--from",
                &from_revset,
                "--into",
                &first_ws,
                "-m",
                &msg,
            ])
            .current_dir(&cwd)
            .output()
            .context("Failed to squash workspace commits")?;

        if !squash_output.status.success() {
            let stderr = String::from_utf8_lossy(&squash_output.stderr);
            bail!("Failed to squash workspace commits: {}", stderr.trim());
        }
    } else if message.is_some() {
        // Single workspace: apply --message via jj describe
        let ws_rev = format!("{}@", ws_to_merge[0]);
        let describe_output = Command::new("jj")
            .args(["describe", "-r", &ws_rev, "-m", &msg])
            .current_dir(&cwd)
            .output()
            .context("Failed to describe workspace commit")?;

        if !describe_output.status.success() {
            let stderr = String::from_utf8_lossy(&describe_output.stderr);
            eprintln!("Warning: Failed to apply --message: {}", stderr.trim());
        }
    }

    // Compute the final merge revision for bookmark/rebase (before cleanup steps)
    let final_rev = format!("{}@", ws_to_merge[0]);

    // Step 3: Abandon orphaned scaffolding commits from the merged workspaces only.
    // Only target the specific parent commits recorded before rebase -- not all empty
    // commits in the repo, which could belong to other active workspaces.
    if !pre_rebase_parent_ids.is_empty() {
        let id_terms: Vec<String> = pre_rebase_parent_ids
            .iter()
            .map(|id| format!("id(\"{id}\")"))
            .collect();
        let abandon_revset = format!(
            "({}) & empty() & description(exact:'') & ~ancestors({branch}) & ~root()",
            id_terms.join(" | ")
        );
        let abandon_output = Command::new("jj")
            .args(["abandon", &abandon_revset])
            .current_dir(&cwd)
            .output();

        if let Ok(output) = abandon_output
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("Abandoned") {
                println!("Cleaned up scaffolding commits.");
            }
        }
    }

    // Step 4: Rebase default workspace onto new merge commit so on-disk files reflect the merge.
    // The default workspace's working copy is empty (no changes), so this is safe.
    let default_ws_path = root.join("ws").join(default_ws);
    if default_ws_path.exists() {
        // The merge operations above (squash) ran from root and
        // modified the commit graph. The default workspace may now be stale.
        // Update it BEFORE rebasing to avoid stale errors.
        let _ = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&default_ws_path)
            .output();

        let rebase_default = Command::new("jj")
            .args(["rebase", "-r", &format!("{default_ws}@"), "-d", &final_rev])
            .current_dir(&default_ws_path)
            .output();

        if let Ok(output) = rebase_default
            && !output.status.success()
        {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Warning: Failed to rebase default workspace onto {}: {}", final_rev, stderr.trim());
            eprintln!("  On-disk files may not reflect the merge. Run: jj rebase -r {default_ws}@ -d {final_rev}");
        }

        // The rebase may have created a divergent commit -- auto-resolve it.
        let _ = resolve_divergent_working_copy(&default_ws_path);

        // Check if default workspace has local edits that would be lost by restore.
        let status_output = Command::new("jj")
            .args(["status", "--color=never", "--no-pager"])
            .current_dir(&default_ws_path)
            .output();

        let has_local_edits = status_output
            .as_ref()
            .map(|o| {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // jj status shows "Working copy changes:" when there are edits
                stdout.contains("Working copy changes:")
            })
            .unwrap_or(false);

        if has_local_edits {
            eprintln!("WARNING: Default workspace has uncommitted changes that would be overwritten by merge.");
            eprintln!("  To preserve your changes, commit them first:");
            eprintln!("    maw exec default -- jj commit -m \"wip: save before merge\"");
            eprintln!("  Then re-run the merge.");
            bail!("Default workspace has dirty state. Commit or discard changes before merging.");
        }

        // Restore on-disk files from the parent commit. After rebasing the
        // working copy onto the new main, the commit tree is correct but
        // on-disk files may be missing. `jj restore` writes the parent's
        // files to disk.
        let _ = Command::new("jj")
            .args(["restore"])
            .current_dir(&default_ws_path)
            .output();

        // Final update-stale to clear any remaining stale state.
        // Operations above may have left the workspace stale again.
        let _ = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&default_ws_path)
            .output();
    }

    println!("Merged to {branch}: {msg}");
    let has_conflicts = auto_resolve_conflicts(&cwd, &config, branch)?;

    // Step 5: Move branch bookmark to the final commit (only if no conflicts).
    // We defer this until after conflict detection so the branch never points
    // at a conflicted commit.
    if !has_conflicts {
        let bookmark_output = Command::new("jj")
            .args(["bookmark", "set", branch, "-r", &final_rev])
            .current_dir(&cwd)
            .output()
            .context("Failed to move main bookmark")?;

        if !bookmark_output.status.success() {
            let stderr = String::from_utf8_lossy(&bookmark_output.stderr);
            eprintln!("Warning: Failed to move {branch} bookmark: {}", stderr.trim());
            eprintln!("  Run manually: jj bookmark set {branch} -r {final_rev}");
        }
    }

    // Optionally destroy workspaces (but not if there are conflicts!)
    // Never destroy the default workspace during merge --destroy.
    if destroy_after {
        let ws_to_destroy: Vec<String> = ws_to_merge
            .iter()
            .filter(|ws| ws.as_str() != default_ws)
            .cloned()
            .collect();

        if has_conflicts {
            println!("NOT destroying workspaces due to conflicts.");
            println!("Resolve conflicts first, then run:");
            for ws in &ws_to_destroy {
                println!("  maw ws destroy {ws}");
            }
        } else if confirm {
            println!();
            println!("Will destroy {} workspaces:", ws_to_destroy.len());
            for ws in &ws_to_destroy {
                println!("  - {ws}");
            }
            println!();
            print!("Continue? [y/N] ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Aborted. Workspaces kept. Merge commit still exists.");
                return Ok(());
            }

            destroy_workspaces(&ws_to_destroy, &root)?;
        } else {
            println!();
            destroy_workspaces(&ws_to_destroy, &root)?;
        }
    }

    // Run post-merge hooks (warn on failure but don't abort)
    run_hooks(&config.hooks.post_merge, "post-merge", &root, false)?;

    // Show next steps for pushing
    if !has_conflicts {
        println!();
        println!("Next: push to remote:");
        println!("  maw push");
    }

    Ok(())
}

fn destroy_workspaces(workspaces: &[String], root: &Path) -> Result<()> {
    println!("Cleaning up workspaces...");
    let ws_dir = root.join("ws");
    // Run jj commands from inside the default workspace to avoid stale
    // root working copy errors in the bare repo model.
    let jj_cwd = ws_dir.join(DEFAULT_WORKSPACE);
    let jj_cwd = if jj_cwd.exists() { &jj_cwd } else { root };
    for ws in workspaces {
        if ws == DEFAULT_WORKSPACE {
            println!("  Skipping default workspace");
            continue;
        }
        let path = ws_dir.join(ws);
        let _ = Command::new("jj")
            .args(["workspace", "forget", ws])
            .current_dir(jj_cwd)
            .status();
        if path.exists() {
            std::fs::remove_dir_all(&path).ok();
        }
        println!("  Destroyed: {ws}");
    }
    Ok(())
}
