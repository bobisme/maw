use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use maw_core::refs as manifold_refs;

use super::checks::{sync_worktree_to_epoch, workspace_has_uncommitted_changes};

// ---------------------------------------------------------------------------
// Rebase conflict metadata
// ---------------------------------------------------------------------------

/// A single rebase conflict recorded as data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseConflict {
    /// File path relative to workspace root.
    pub path: String,
    /// The original commit SHA being replayed when conflict occurred.
    pub original_commit: String,
    /// Base content (merge base), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// "Ours" content (new epoch version), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    /// "Theirs" content (workspace commit version), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

/// Rebase conflict metadata stored in `.manifold/artifacts/ws/<name>/rebase-conflicts.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseConflicts {
    /// All conflicts from the rebase.
    pub conflicts: Vec<RebaseConflict>,
    /// The epoch OID before the rebase.
    pub rebase_from: String,
    /// The epoch OID after the rebase (target).
    pub rebase_to: String,
}

/// Path to the rebase conflicts JSON file for a workspace.
fn rebase_conflicts_path(root: &Path, ws_name: &str) -> std::path::PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("rebase-conflicts.json")
}

/// Read rebase conflicts for a workspace, if any.
pub fn read_rebase_conflicts(root: &Path, ws_name: &str) -> Option<RebaseConflicts> {
    let path = rebase_conflicts_path(root, ws_name);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Write rebase conflicts for a workspace.
fn write_rebase_conflicts(root: &Path, ws_name: &str, conflicts: &RebaseConflicts) -> Result<()> {
    let path = rebase_conflicts_path(root, ws_name);
    let dir = path.parent().expect("path always has parent");
    std::fs::create_dir_all(dir)?;
    let content = serde_json::to_string_pretty(conflicts)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Delete rebase conflicts file for a workspace (called on resolution).
pub fn delete_rebase_conflicts(root: &Path, ws_name: &str) -> Result<()> {
    let path = rebase_conflicts_path(root, ws_name);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rebase implementation
// ---------------------------------------------------------------------------

/// Replay workspace commits onto the current epoch via cherry-pick.
///
/// This is the core of `maw ws sync --rebase`. For each workspace commit
/// ahead of the old epoch:
/// 1. First, checkout the new epoch in the worktree (detached HEAD)
/// 2. For each commit in order, run `git cherry-pick --no-commit <sha>`
/// 3. If cherry-pick succeeds: stage changes, create new commit
/// 4. If cherry-pick conflicts: write conflict markers, record metadata, continue
/// 5. After all commits replayed: update workspace epoch ref
pub(super) fn rebase_workspace(
    root: &Path,
    ws_name: &str,
    old_epoch: &str,
    new_epoch: &str,
    ws_path: &Path,
    ahead_count: u32,
) -> Result<()> {
    // Safety: refuse to rebase if the workspace has uncommitted changes.
    let is_dirty = workspace_has_uncommitted_changes(ws_path).map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by rebase. \
             Commit or stash first.\n  \
             Check: git -C {} status",
            ws_path.display()
        );
    }

    println!(
        "Rebasing workspace '{ws_name}' ({ahead_count} commit(s)) onto epoch {}...",
        &new_epoch[..std::cmp::min(12, new_epoch.len())]
    );
    println!();

    // Collect commit SHAs to replay (oldest first).
    let commits = list_commits_ahead(ws_path, old_epoch)?;
    if commits.is_empty() {
        println!("No commits to replay. Performing normal sync.");
        sync_worktree_to_epoch(root, ws_name, new_epoch)?;
        println!();
        println!("Workspace synced successfully.");
        return Ok(());
    }

    // Step 1: Checkout the new epoch (detached HEAD).
    let output = Command::new("git")
        .args(["checkout", "--detach", new_epoch])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git checkout: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to checkout new epoch in workspace '{ws_name}': {}",
            stderr.trim()
        );
    }

    // Step 2: Replay each commit via cherry-pick.
    let mut conflicts: Vec<RebaseConflict> = Vec::new();
    let mut replayed = 0;
    let mut conflicted = 0;

    for (i, commit_sha) in commits.iter().enumerate() {
        let short_sha = &commit_sha[..std::cmp::min(12, commit_sha.len())];
        let commit_msg = get_commit_message(ws_path, commit_sha);

        // Try cherry-pick --no-commit to apply changes without auto-committing.
        let cp_output = Command::new("git")
            .args(["cherry-pick", "--no-commit", commit_sha])
            .current_dir(ws_path)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run git cherry-pick: {e}"))?;

        if cp_output.status.success() {
            // Cherry-pick succeeded — check if there are staged changes to commit.
            let msg = commit_msg.unwrap_or_else(|| format!("rebase: replay {short_sha}"));

            // Check for staged changes. If none, the original was an empty commit
            // or the changes were already applied.
            let diff_output = Command::new("git")
                .args(["diff", "--cached", "--quiet"])
                .current_dir(ws_path)
                .output();
            let has_staged_changes = diff_output
                .as_ref()
                .map(|o| !o.status.success())
                .unwrap_or(true); // assume changes if we can't tell

            if !has_staged_changes {
                println!(
                    "  [{}/{}] Skipped {short_sha} (no changes to apply)",
                    i + 1,
                    commits.len()
                );
                // Reset any index state for the next cherry-pick.
                let _ = Command::new("git")
                    .args(["reset", "HEAD"])
                    .current_dir(ws_path)
                    .output();
            } else {
                let commit_output = Command::new("git")
                    .args(["commit", "--no-verify", "-m", &msg])
                    .current_dir(ws_path)
                    .output()
                    .map_err(|e| anyhow::anyhow!("Failed to commit replayed changes: {e}"))?;

                if commit_output.status.success() {
                    replayed += 1;
                    println!(
                        "  [{}/{}] Replayed {short_sha}: {}",
                        i + 1,
                        commits.len(),
                        msg.lines().next().unwrap_or("(no message)")
                    );
                } else {
                    let stderr = String::from_utf8_lossy(&commit_output.stderr);
                    println!(
                        "  [{}/{}] Warning: commit after cherry-pick failed for {short_sha}: {}",
                        i + 1,
                        commits.len(),
                        stderr.trim()
                    );
                }
            }
        } else {
            // Cherry-pick failed (conflict). Record conflicts and continue.
            conflicted += 1;

            // Find which files have conflict markers.
            let conflict_files = list_conflicted_files(ws_path);

            println!(
                "  [{}/{}] CONFLICT replaying {short_sha}: {} file(s)",
                i + 1,
                commits.len(),
                conflict_files.len()
            );

            for cf in &conflict_files {
                println!("    - {cf}");

                // Read the conflict content from the working tree (has markers).
                // For the metadata, try to capture base/ours/theirs from the index stages.
                let (base, ours, theirs) = read_conflict_stages(ws_path, cf);

                conflicts.push(RebaseConflict {
                    path: cf.clone(),
                    original_commit: commit_sha.clone(),
                    base,
                    ours,
                    theirs,
                });
            }

            // Relabel git's conflict markers with meaningful names so
            // `maw ws resolve` can match them (bn-aao6).
            // Before: <<<<<<< HEAD / >>>>>>> abc123 (commit msg)
            // After:  <<<<<<< epoch (current) / >>>>>>> ws-name (workspace changes)
            for cf in &conflict_files {
                relabel_conflict_markers(ws_path, cf, ws_name);
            }

            // Add all conflicted files (with markers) to the index and commit.
            // This preserves the conflict markers in the history so the agent
            // can see and resolve them.
            let _ = Command::new("git")
                .args(["add", "--all"])
                .current_dir(ws_path)
                .output();
            let msg = format!(
                "rebase: conflict replaying {short_sha} ({} file(s))",
                conflict_files.len()
            );
            let _ = Command::new("git")
                .args(["commit", "--no-verify", "--allow-empty", "-m", &msg])
                .current_dir(ws_path)
                .output();
        }
    }

    // Step 3: Update workspace epoch ref.
    if let Ok(oid) = maw_core::model::types::GitOid::new(new_epoch) {
        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
        let _ = manifold_refs::write_ref(root, &epoch_ref, &oid);
    }

    // Step 4: Record conflict metadata and update workspace metadata.
    if !conflicts.is_empty() {
        let conflict_count = conflicts.len() as u32;

        // Write conflict metadata file.
        let rebase_meta = RebaseConflicts {
            conflicts,
            rebase_from: old_epoch.to_string(),
            rebase_to: new_epoch.to_string(),
        };
        write_rebase_conflicts(root, ws_name, &rebase_meta)?;

        println!();
        println!("Rebase complete: {replayed} commit(s) replayed, {conflicted} with conflicts.");
        println!("Workspace '{ws_name}' has {conflict_count} unresolved conflict(s).");
        println!();
        println!("Conflict markers use labeled sides:");
        println!("  <<<<<<< epoch   — current epoch version");
        println!("  ||||||| base");
        println!("  =======");
        println!("  >>>>>>> {ws_name}   — workspace changes");
        println!();
        println!("To resolve:");
        println!("  maw ws resolve {ws_name} --list                  # list conflicts");
        println!("  maw ws resolve {ws_name} --keep epoch            # keep epoch version");
        println!("  maw ws resolve {ws_name} --keep {ws_name}    # keep workspace version");
        println!("  maw ws resolve {ws_name} --keep both             # keep both sides");
        println!();
        println!("After resolving, commit and clear conflict state:");
        println!("  maw exec {ws_name} -- git add -A && maw exec {ws_name} -- git commit -m \"fix: resolve rebase conflicts\"");
        println!("  maw ws sync {ws_name}");
    } else {
        // No conflicts — clean up any stale conflict metadata.
        let _ = delete_rebase_conflicts(root, ws_name);

        println!();
        println!("Rebase complete: {replayed} commit(s) replayed cleanly.");
        println!("Workspace '{ws_name}' is now up to date.");
    }

    Ok(())
}

/// List commits ahead of the epoch, oldest first (for replay order).
fn list_commits_ahead(ws_path: &Path, epoch_oid: &str) -> Result<Vec<String>> {
    let range = format!("{epoch_oid}..HEAD");
    let output = Command::new("git")
        .args(["rev-list", "--reverse", &range])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git rev-list: {e}"))?;
    if !output.status.success() {
        bail!("Failed to list commits ahead of epoch");
    }
    let commits: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(commits)
}

/// Get the commit message for a given SHA.
pub(super) fn get_commit_message(ws_path: &Path, sha: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%B", sha])
        .current_dir(ws_path)
        .output()
        .ok()?;
    if output.status.success() {
        let msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if msg.is_empty() { None } else { Some(msg) }
    } else {
        None
    }
}

/// List files with unmerged (conflicted) entries in the index.
pub(super) fn list_conflicted_files(ws_path: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(ws_path)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => {
            // Fallback: try ls-files --unmerged
            let output2 = Command::new("git")
                .args(["ls-files", "--unmerged"])
                .current_dir(ws_path)
                .output();
            match output2 {
                Ok(o) if o.status.success() => {
                    let mut files: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter_map(|l| l.split('\t').nth(1).map(|s| s.to_string()))
                        .collect();
                    files.sort();
                    files.dedup();
                    files
                }
                _ => vec![],
            }
        }
    }
}

/// Read the three conflict stages (base/ours/theirs) from the git index for a file.
/// Returns (base, ours, theirs) as Option<String> for each stage.
fn read_conflict_stages(
    ws_path: &Path,
    file_path: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let read_stage = |stage: &str| -> Option<String> {
        let spec = format!(":{stage}:{file_path}");
        let output = Command::new("git")
            .args(["show", &spec])
            .current_dir(ws_path)
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout).ok()
        } else {
            None
        }
    };

    let base = read_stage("1");
    let ours = read_stage("2");
    let theirs = read_stage("3");
    (base, ours, theirs)
}

/// Relabel git's conflict markers with meaningful workspace/epoch names.
///
/// Git writes markers like:
///   `<<<<<<< HEAD`
///   `>>>>>>> abc123 (commit message)`
///
/// This rewrites them to:
///   `<<<<<<< epoch (current)`
///   `>>>>>>> ws-name (workspace changes)`
///
/// so that `maw ws resolve` can match them by name.
pub(super) fn relabel_conflict_markers(ws_path: &Path, rel_path: &str, ws_name: &str) {
    let full_path = ws_path.join(rel_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        if line.starts_with("<<<<<<<") {
            output.push_str("<<<<<<< epoch (current)");
        } else if line.starts_with(">>>>>>>") {
            output.push_str(&format!(">>>>>>> {ws_name} (workspace changes)"));
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }

    let _ = std::fs::write(&full_path, output);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_conflict_serialization_roundtrip() {
        let conflicts = RebaseConflicts {
            conflicts: vec![
                RebaseConflict {
                    path: "src/main.rs".to_string(),
                    original_commit: "a".repeat(40),
                    base: Some("base content".to_string()),
                    ours: Some("ours content".to_string()),
                    theirs: Some("theirs content".to_string()),
                },
                RebaseConflict {
                    path: "Cargo.toml".to_string(),
                    original_commit: "b".repeat(40),
                    base: None,
                    ours: Some("ours only".to_string()),
                    theirs: None,
                },
            ],
            rebase_from: "c".repeat(40),
            rebase_to: "d".repeat(40),
        };
        let json = serde_json::to_string_pretty(&conflicts).unwrap();
        let parsed: RebaseConflicts = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.conflicts.len(), 2);
        assert_eq!(parsed.conflicts[0].path, "src/main.rs");
        assert_eq!(parsed.conflicts[1].path, "Cargo.toml");
        assert_eq!(parsed.rebase_from, "c".repeat(40));
        assert_eq!(parsed.rebase_to, "d".repeat(40));
    }
}
