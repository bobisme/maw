use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::backend::WorkspaceBackend;
use crate::backend::git::GitWorktreeBackend;
use crate::refs;

/// Result of an epoch GC pass.
#[derive(Debug, Default)]
pub struct EpochGcReport {
    pub scanned: usize,
    pub kept: Vec<String>,
    pub removed: Vec<String>,
}

/// Run epoch GC for the current repo and print a concise summary.
#[allow(clippy::missing_errors_doc)]
pub fn run_cli(dry_run: bool) -> Result<()> {
    let root = repo_root()?;
    let report = gc_unreferenced_epochs(&root, dry_run)?;

    if report.scanned == 0 {
        println!("No epoch snapshots found in .manifold/epochs.");
        return Ok(());
    }

    if dry_run {
        println!(
            "GC preview: scanned {} epoch snapshot(s), would remove {}.",
            report.scanned,
            report.removed.len()
        );
        if !report.removed.is_empty() {
            println!("To apply: maw gc");
        }
    } else {
        println!(
            "Epoch GC complete: scanned {} snapshot(s), removed {}.",
            report.scanned,
            report.removed.len()
        );
    }

    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .output()
        .context("Failed to run git rev-parse --show-toplevel")?;

    if !output.status.success() {
        anyhow::bail!(
            "Not in a git repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

/// Remove unreferenced epoch snapshots from `.manifold/epochs`.
///
/// Snapshots use directory names `e-<40-hex-oid>`.
/// A snapshot is kept if its epoch is referenced by:
/// - any active workspace (via git worktree HEAD)
/// - `refs/manifold/epoch/current`
#[allow(clippy::missing_errors_doc)]
pub fn gc_unreferenced_epochs(root: &Path, dry_run: bool) -> Result<EpochGcReport> {
    let epochs_dir = root.join(".manifold").join("epochs");
    if !epochs_dir.exists() {
        return Ok(EpochGcReport::default());
    }

    let backend = GitWorktreeBackend::new(root.to_path_buf());
    let referenced = referenced_epochs(root, &backend)?;

    let mut report = EpochGcReport::default();

    for (oid, path) in epoch_snapshot_dirs(&epochs_dir)? {
        report.scanned += 1;
        if referenced.contains(&oid) {
            report.kept.push(oid);
        } else {
            if !dry_run {
                std::fs::remove_dir_all(&path).with_context(|| {
                    format!("Failed to remove epoch snapshot {}", path.display())
                })?;
            }
            report.removed.push(oid);
        }
    }

    report.kept.sort();
    report.removed.sort();
    Ok(report)
}

fn referenced_epochs(root: &Path, backend: &GitWorktreeBackend) -> Result<HashSet<String>> {
    let mut refs_set = HashSet::new();

    for ws in backend
        .list()
        .map_err(|e| anyhow::anyhow!("Failed to list workspaces for GC: {e}"))?
    {
        refs_set.insert(ws.epoch.as_str().to_owned());
    }

    if let Some(current) = refs::read_epoch_current(root)
        .map_err(|e| anyhow::anyhow!("Failed to read refs/manifold/epoch/current: {e}"))?
    {
        refs_set.insert(current.as_str().to_owned());
    }

    Ok(refs_set)
}

fn epoch_snapshot_dirs(epochs_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();

    for entry in std::fs::read_dir(epochs_dir)
        .with_context(|| format!("Failed to read {}", epochs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(name) = entry
            .file_name()
            .to_str()
            .map(std::borrow::ToOwned::to_owned)
        else {
            continue;
        };

        let Some(oid) = name.strip_prefix("e-") else {
            continue;
        };

        if is_hex_oid(oid) {
            out.push((oid.to_owned(), path));
        }
    }

    Ok(out)
}

fn is_hex_oid(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use crate::backend::WorkspaceBackend;
    use crate::model::types::{EpochId, WorkspaceId};

    use super::*;

    fn setup_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join("README.md"), "# test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let oid = String::from_utf8(out.stdout).unwrap().trim().to_string();

        fs::create_dir_all(root.join(".manifold/epochs")).unwrap();

        (dir, oid)
    }

    fn commit(root: &Path, file: &str) -> String {
        fs::write(root.join(file), file).unwrap();
        Command::new("git")
            .args(["add", file])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", file])
            .current_dir(root)
            .output()
            .unwrap();
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn gc_keeps_referenced_and_removes_unreferenced() {
        let (dir, epoch0) = setup_repo();
        let root = dir.path();

        let backend = GitWorktreeBackend::new(root.to_path_buf());
        let ws = WorkspaceId::new("persist").unwrap();
        backend
            .create(&ws, &EpochId::new(&epoch0).unwrap())
            .unwrap();

        let epoch1 = commit(root, "new.txt");
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", &epoch1])
            .current_dir(root)
            .output()
            .unwrap();

        fs::create_dir_all(root.join(format!(".manifold/epochs/e-{epoch0}"))).unwrap();
        fs::create_dir_all(root.join(format!(".manifold/epochs/e-{epoch1}"))).unwrap();
        let orphan = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(root.join(format!(".manifold/epochs/e-{orphan}"))).unwrap();

        let report = gc_unreferenced_epochs(root, false).unwrap();
        assert_eq!(report.removed, vec![orphan.to_string()]);
        assert!(root.join(format!(".manifold/epochs/e-{epoch0}")).exists());
        assert!(root.join(format!(".manifold/epochs/e-{epoch1}")).exists());
        assert!(!root.join(format!(".manifold/epochs/e-{orphan}")).exists());
    }

    #[test]
    fn gc_removes_epoch_after_workspace_destroyed() {
        let (dir, epoch0) = setup_repo();
        let root = dir.path();

        let backend = GitWorktreeBackend::new(root.to_path_buf());
        let ws = WorkspaceId::new("tempws").unwrap();
        backend
            .create(&ws, &EpochId::new(&epoch0).unwrap())
            .unwrap();
        fs::create_dir_all(root.join(format!(".manifold/epochs/e-{epoch0}"))).unwrap();

        // Keep current epoch elsewhere so epoch0 is not retained by epoch/current.
        let epoch1 = commit(root, "later.txt");
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", &epoch1])
            .current_dir(root)
            .output()
            .unwrap();

        backend.destroy(&ws).unwrap();

        // After destroying the last workspace referencing epoch0, run GC.
        // In production this would be called from the CLI layer after destroy.
        let report = gc_unreferenced_epochs(root, false).unwrap();
        assert_eq!(report.removed, vec![epoch0.clone()]);
        assert!(
            !root.join(format!(".manifold/epochs/e-{epoch0}")).exists(),
            "epoch snapshot should be GC'd after destroying last referencing workspace"
        );
    }
}
