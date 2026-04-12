use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

use maw_core::model::types::{BaseEpoch, WorkspaceId};
use maw_core::refs as manifold_refs;

use crate::workspace::DEFAULT_WORKSPACE;

pub(super) fn is_default_workspace(name: &str) -> bool {
    name == DEFAULT_WORKSPACE
}

pub(super) fn workspace_name_from_cwd(root: &Path, cwd: &Path) -> String {
    let ws_root = root.join("ws");
    let Ok(relative) = cwd.strip_prefix(&ws_root) else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let Some(component) = relative.components().next() else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let std::path::Component::Normal(name) = component else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let Some(name) = name.to_str() else {
        return DEFAULT_WORKSPACE.to_string();
    };

    if WorkspaceId::new(name).is_ok() {
        name.to_owned()
    } else {
        DEFAULT_WORKSPACE.to_string()
    }
}

/// Count commits reachable from HEAD but not from `epoch_oid` inside a workspace.
///
/// Returns the number of committed-but-not-yet-merged commits in the workspace.
/// A result > 0 means the workspace has committed work that should be merged
/// before syncing; syncing over it would wipe those commits.
///
/// Returns `None` if git fails for any reason (invalid repo, unknown OID, etc.).
/// Callers MUST treat `None` as "has committed work" (i.e. refuse to sync) to
/// prevent data loss when the workspace state cannot be determined.
// TODO(gix): GitRepo doesn't have a rev-list --count equivalent. Keep CLI.
//
// Takes a [`BaseEpoch`] explicitly (not a bare `&str` or `CurrentEpoch`) so
// that the compiler catches accidental swaps. See bn-18dj for the bug this
// newtype is meant to prevent: passing the current epoch here would silently
// return 0 on stale workspaces and wipe their local commits on sync.
pub(super) fn committed_ahead_of_epoch(ws_path: &Path, base: &BaseEpoch) -> Option<u32> {
    let range = format!("{}..HEAD", base.as_str());
    let output = Command::new("git")
        .args(["rev-list", "--count", &range])
        .current_dir(ws_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

pub(super) fn workspace_has_uncommitted_changes(ws_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git status in {}: {e}", ws_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git status failed in {}: {}",
            ws_path.display(),
            stderr.trim()
        );
    }

    Ok(!output.stdout.is_empty())
}

/// Sync a single worktree to the given epoch commit.
///
/// Uses `git checkout --detach <epoch>` inside the worktree to update it.
/// This is safe because workspace changes are captured by the merge engine
/// via snapshot before any merge, so uncommitted changes are not lost
/// during the normal workflow. However, this function is only called
/// explicitly by the user/agent via `maw ws sync`.
pub(super) fn sync_worktree_to_epoch(root: &Path, ws_name: &str, epoch_oid: &str) -> Result<()> {
    let ws_path = root.join("ws").join(ws_name);
    if !ws_path.exists() {
        bail!("Workspace directory does not exist: {}", ws_path.display());
    }

    // Safety: refuse to sync if the workspace has any uncommitted changes.
    // `git checkout --detach` can clobber staged/unstaged tracked edits, and
    // untracked files may become orphaned or conflict with the new tree.
    let is_dirty = workspace_has_uncommitted_changes(&ws_path).map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by sync. \
             Commit or stash first.\n  \
             Check: git -C {} status",
            ws_path.display()
        );
    }

    // Detach HEAD at the new epoch to sync the workspace.
    // TODO(gix): checkout_tree() does not update HEAD, and write_ref("HEAD")
    // doesn't reliably create a detached HEAD in linked worktrees. Keep
    // `git checkout --detach` until gix gains proper worktree HEAD support.
    let output = Command::new("git")
        .args(["checkout", "--detach", epoch_oid])
        .current_dir(&ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git checkout in workspace '{ws_name}': {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to sync workspace '{ws_name}': {}\n  \
             Manual fix: git -C {} checkout --detach {epoch_oid}",
            stderr.trim(),
            ws_path.display()
        );
    }

    // Update the per-workspace creation epoch ref to the new epoch.
    // After sync, the workspace is rebased onto the new epoch, so
    // the epoch ref should reflect the new base.
    if let Ok(oid) = maw_core::model::types::GitOid::new(epoch_oid) {
        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
        let _ = manifold_refs::write_ref(root, &epoch_ref, &oid);
    }

    println!(
        "  \u{2713} {ws_name} - synced to epoch {}",
        &epoch_oid[..12]
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_workspace_name_from_workspace_path() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/ws/agent-1/src");
        assert_eq!(workspace_name_from_cwd(root, cwd), "agent-1");
    }

    #[test]
    fn falls_back_to_default_outside_workspace_tree() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/docs");
        assert_eq!(workspace_name_from_cwd(root, cwd), "default");
    }

    #[test]
    fn falls_back_to_default_for_invalid_workspace_segment() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/ws/not_valid");
        assert_eq!(workspace_name_from_cwd(root, cwd), "default");
    }

    #[test]
    fn detects_default_workspace_name() {
        assert!(is_default_workspace("default"));
        assert!(!is_default_workspace("agent-1"));
    }
}
