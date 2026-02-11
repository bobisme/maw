use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use glob::Pattern;

use super::sync::resolve_divergent_working_copy;
use super::{jj_cwd, repo_root, MawConfig, DEFAULT_WORKSPACE};

/// Details about a single conflict region within a file.
struct ConflictRegion {
    start_line: usize,
    end_line: usize,
}

/// Details about conflicts in a single file.
struct ConflictFileDetail {
    path: String,
    regions: Vec<ConflictRegion>,
    sides: usize, // e.g. 2 for a 2-sided conflict
}

/// Scan a file on disk for jj conflict markers and return details.
///
/// jj uses `<<<<<<<` to open a conflict region and `>>>>>>>` to close it.
/// The number of sides is determined by counting `%%%%%%%` (diff-style)
/// and `+++++++` (snapshot-style) sections within each region.
fn scan_conflict_markers(file_path: &Path) -> Option<ConflictFileDetail> {
    let file = std::fs::File::open(file_path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut regions = Vec::new();
    let mut current_start: Option<usize> = None;
    let mut side_count: usize = 0;
    let mut max_sides: usize = 0;

    for (idx, line) in reader.lines().enumerate() {
        let line_num = idx + 1; // 1-indexed
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // skip unreadable lines (binary?)
        };

        if line.starts_with("<<<<<<<") {
            current_start = Some(line_num);
            side_count = 1; // the opening marker starts the first side
        } else if current_start.is_some()
            && (line.starts_with("%%%%%%%") || line.starts_with("+++++++"))
        {
            side_count += 1;
        } else if line.starts_with(">>>>>>>") {
            if let Some(start) = current_start.take() {
                regions.push(ConflictRegion {
                    start_line: start,
                    end_line: line_num,
                });
                if side_count > max_sides {
                    max_sides = side_count;
                }
                side_count = 0;
            }
        }
    }

    if regions.is_empty() {
        return None;
    }

    Some(ConflictFileDetail {
        path: String::new(), // caller fills this in
        regions,
        sides: max_sides,
    })
}

/// Print detailed conflict information with absolute workspace paths and resolution guidance.
fn print_conflict_guidance(
    conflicted_files: &[String],
    default_ws_path: &Path,
    default_ws_name: &str,
) {
    // Scan each conflicted file for marker details
    let mut details: Vec<ConflictFileDetail> = Vec::new();
    for file in conflicted_files {
        let abs_path = default_ws_path.join(file);
        if let Some(mut detail) = scan_conflict_markers(&abs_path) {
            detail.path = file.clone();
            details.push(detail);
        } else {
            // File exists in conflict state but we couldn't find markers
            // (could be binary or jj materialized differently)
            details.push(ConflictFileDetail {
                path: file.clone(),
                regions: Vec::new(),
                sides: 0,
            });
        }
    }

    // Print conflict summary
    println!();
    println!("Conflicts:");
    for detail in &details {
        if detail.regions.is_empty() {
            println!("  {:<40} conflict (could not locate markers)", detail.path);
        } else {
            let sides_label = if detail.sides > 0 {
                format!("{}-sided conflict", detail.sides)
            } else {
                "conflict".to_string()
            };
            let ranges: Vec<String> = detail
                .regions
                .iter()
                .map(|r| {
                    if r.start_line == r.end_line {
                        format!("line {}", r.start_line)
                    } else {
                        format!("lines {}-{}", r.start_line, r.end_line)
                    }
                })
                .collect();
            println!("  {:<40} {} ({})", detail.path, sides_label, ranges.join(", "));
        }
    }

    // Print resolution guidance with absolute paths
    let ws_display = default_ws_path.display();
    println!();
    println!("To resolve:");
    println!(
        "  1. Edit the conflicted files in {ws_display}/"
    );
    println!("     Remove conflict markers (<<<<<<< ... >>>>>>>), keeping the correct code");
    println!(
        "  2. Verify: maw exec {default_ws_name} -- jj status"
    );
    println!(
        "     (should show no more 'C' entries for resolved files)"
    );
    println!(
        "  3. Describe: maw exec {default_ws_name} -- jj describe -m \"resolve: merge conflicts\""
    );
}

/// Check for conflicts after merge and auto-resolve paths matching config patterns.
/// Returns true if there are remaining (unresolved) conflicts.
fn auto_resolve_conflicts(
    cwd: &Path,
    config: &MawConfig,
    branch: &str,
    root: &Path,
) -> Result<bool> {
    let default_ws = config.default_workspace();
    let default_ws_path = root.join("ws").join(default_ws);

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
        print_conflict_guidance(&conflicted_files, &default_ws_path, default_ws);
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

    // Report remaining conflicts with detailed guidance
    if !remaining_conflicts.is_empty() {
        println!();
        println!(
            "WARNING: {} conflict(s) remaining that need manual resolution.",
            remaining_conflicts.len()
        );
        print_conflict_guidance(&remaining_conflicts, &default_ws_path, default_ws);
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
    // If default has intermediate committed work between main and default@, rebase the
    // entire chain (not just the tip) to avoid orphaning those commits.
    let default_ws_path = root.join("ws").join(default_ws);
    if default_ws_path.exists() {
        // The merge operations above (squash) ran from root and
        // modified the commit graph. The default workspace may now be stale.
        // Update it BEFORE rebasing to avoid stale errors.
        let _ = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&default_ws_path)
            .output();

        // Auto-snapshot: if default workspace has local edits, commit them
        // before the rebase+restore sequence which would overwrite on-disk files.
        let status_output = Command::new("jj")
            .args(["status", "--color=never", "--no-pager"])
            .current_dir(&default_ws_path)
            .output();

        let has_local_edits = status_output
            .as_ref()
            .map(|o| {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains("Working copy changes:")
            })
            .unwrap_or(false);

        if has_local_edits {
            println!("Auto-snapshotting uncommitted changes in default workspace...");
            let snap = Command::new("jj")
                .args(["commit", "-m", "wip: auto-snapshot before merge"])
                .current_dir(&default_ws_path)
                .output();
            if snap.as_ref().map(|o| o.status.success()).unwrap_or(false) {
                println!("  Saved as 'wip: auto-snapshot before merge' commit.");
            } else {
                let stderr = snap
                    .as_ref()
                    .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                    .unwrap_or_default();
                eprintln!("WARNING: Failed to auto-save default workspace changes: {}", stderr.trim());
                eprintln!("  To preserve your changes manually, run:");
                eprintln!("    maw exec default -- jj commit -m \"wip: save before merge\"");
                eprintln!("  Then re-run the merge.");
                bail!("Could not auto-snapshot default workspace before merge.");
            }
        }

        // Check for intermediate commits between main and default@.
        // The revset `{branch}+..{default_ws}@` gives all commits strictly after
        // main up to and including default@. If there are commits beyond just
        // default@ itself, those are committed work that must move with the rebase.
        let chain_revset = format!("{branch}+..{default_ws}@");
        let chain_output = Command::new("jj")
            .args([
                "log",
                "-r",
                &chain_revset,
                "--no-graph",
                "--reversed",
                "-T",
                r#"change_id ++ "\n""#,
            ])
            .current_dir(&default_ws_path)
            .output();

        let chain_ids: Vec<String> = chain_output
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        let rebase_default = if chain_ids.len() > 1 {
            // There are intermediate commits. Rebase from the first commit after
            // main (the root of the chain) using -s to carry all descendants along.
            println!(
                "Preserving {} committed change(s) in default workspace ancestry.",
                chain_ids.len() - 1
            );
            Command::new("jj")
                .args(["rebase", "-s", &chain_ids[0], "-d", &final_rev])
                .current_dir(&default_ws_path)
                .output()
        } else {
            // No intermediate commits — just rebase the working copy tip.
            Command::new("jj")
                .args(["rebase", "-r", &format!("{default_ws}@"), "-d", &final_rev])
                .current_dir(&default_ws_path)
                .output()
        };

        if let Ok(output) = rebase_default
            && !output.status.success()
        {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Warning: Failed to rebase default workspace onto {}: {}", final_rev, stderr.trim());
            eprintln!("  On-disk files may not reflect the merge. Run: jj rebase -s {default_ws}@ -d {final_rev}");
        }

        // The rebase may have created a divergent commit -- auto-resolve it.
        let _ = resolve_divergent_working_copy(&default_ws_path);

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
    let has_conflicts = auto_resolve_conflicts(&cwd, &config, branch, &root)?;

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
