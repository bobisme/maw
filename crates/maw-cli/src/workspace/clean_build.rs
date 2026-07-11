//! `maw ws clean-build` — remove `target/` build directories.
//!
//! Formerly `maw ws clean` (renamed in bn-auu5 when `clean` became the
//! untracked-file remover). Behavior is unchanged: it deletes `target/` from
//! workspace build contexts to reclaim disk space.

use std::fs;

use anyhow::{Context, Result, bail};

use crate::workspace::{
    DEFAULT_WORKSPACE, MawConfig, get_backend, repo_root, validate_workspace_name,
};
use maw_core::backend::WorkspaceBackend;
use maw_core::model::layout::LayoutFlavor;
use maw_core::model::types::WorkspaceId;

/// Remove `target/` directories from workspace build contexts.
pub fn clean_build(name: Option<String>, all: bool) -> Result<()> {
    if all {
        return clean_all();
    }

    let root = repo_root()?;
    let flavor = LayoutFlavor::detect_with_env(&root);
    let default_name = MawConfig::load(&root)?.default_workspace().to_owned();
    let target = name.unwrap_or_else(|| default_name.clone());
    let is_default = target == DEFAULT_WORKSPACE || target == default_name;

    // bn-1s8d: In a consolidated-layout repo the default workspace IS the repo
    // root — it is not tracked in the worktrees-dir backend, so the
    // `backend.exists()` check would always return false and the old code
    // produced a misleading "does not exist / Fix: maw ws create 'default'".
    // Resolve the path layout-aware (same as `git_cwd` / `resolve_workspace_path_for_cd`).
    if is_default {
        let default_path = flavor.default_target_path(&root, &default_name);
        let _ = clean_workspace_path(&target, &default_path)?;
        return Ok(());
    }

    let workspace_id = WorkspaceId::new(&target)
        .map_err(|e| anyhow::anyhow!("Invalid workspace name '{target}': {e}"))?;

    let backend = get_backend()?;
    if !backend.exists(&workspace_id) {
        bail!("Workspace '{target}' does not exist\n  Check: maw ws list");
    }

    let path = backend.workspace_path(&workspace_id);
    let _ = clean_workspace_path(&target, &path)?;
    Ok(())
}

fn clean_all() -> Result<()> {
    // bn-1s8d: In the consolidated layout the default workspace is the repo
    // root — not tracked in the worktrees-dir backend. Include it first.
    let root = repo_root()?;
    let flavor = LayoutFlavor::detect_with_env(&root);
    let default_name = MawConfig::load(&root)?.default_workspace().to_owned();

    let mut cleaned = 0usize;
    let mut missing = 0usize;

    if flavor == LayoutFlavor::ConsolidatedMawDir {
        let default_path = flavor.default_target_path(&root, &default_name);
        match clean_workspace_path(&default_name, &default_path) {
            Ok(CleanOutcome::Cleaned) => cleaned += 1,
            Ok(CleanOutcome::Missing) => missing += 1,
            Err(err) => {
                bail!(
                    "Failed cleaning default workspace at {}: {}\n  Fix: remove manually or run maw ws clean-build",
                    default_path.display(),
                    err
                );
            }
        }
    }

    let backend = get_backend()?;
    let workspaces = backend
        .list()
        .map_err(|e| anyhow::anyhow!("Failed to list workspaces: {e}"))?;

    if workspaces.is_empty() && cleaned == 0 && missing == 0 {
        println!("No workspaces to clean.");
        return Ok(());
    }

    for workspace in &workspaces {
        let name = workspace.id.as_str();
        let path = backend.workspace_path(&workspace.id);
        match clean_workspace_path(name, &path) {
            Ok(CleanOutcome::Cleaned) => cleaned += 1,
            Ok(CleanOutcome::Missing) => missing += 1,
            Err(err) => {
                bail!(
                    "Failed cleaning workspace '{name}' at {}: {}\n  Fix: remove manually or run maw ws clean-build {name}",
                    path.display(),
                    err
                );
            }
        }
    }

    println!("Clean summary: {cleaned} workspace(s) cleaned, {missing} without target/");
    Ok(())
}

fn clean_workspace_path(name: &str, path: &std::path::Path) -> Result<CleanOutcome> {
    validate_workspace_name(name).context("Invalid workspace name")?;

    let target_path = path.join("target");
    if !target_path.exists() {
        println!(
            "No target/ directory in workspace '{name}' ({})",
            path.display()
        );
        return Ok(CleanOutcome::Missing);
    }

    if !target_path.is_dir() {
        bail!(
            "Found path '{}/target' but it is not a directory",
            path.display()
        );
    }

    fs::remove_dir_all(&target_path)
        .with_context(|| format!("removing {}", target_path.display()))?;

    println!("Cleaned workspace '{name}' ({})", path.display());
    Ok(CleanOutcome::Cleaned)
}

enum CleanOutcome {
    Cleaned,
    Missing,
}
