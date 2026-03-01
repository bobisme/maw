use std::fs;

use anyhow::{Context, Result, bail};

use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceId;
use crate::workspace::{DEFAULT_WORKSPACE, get_backend, validate_workspace_name};

/// Remove `target/` directories from workspace build contexts.
pub fn clean(name: Option<String>, all: bool) -> Result<()> {
    if all {
        return clean_all();
    }

    let target = name.unwrap_or_else(|| DEFAULT_WORKSPACE.to_string());
    let workspace_id =
        WorkspaceId::new(&target).map_err(|e| anyhow::anyhow!("Invalid workspace name '{target}': {e}"))?;

    let backend = get_backend()?;
    if !backend.exists(&workspace_id) {
        bail!(
            "Workspace '{target}' does not exist\n  Check: maw ws list\n  Fix: maw ws create '{target}'"
        );
    }

    let path = backend.workspace_path(&workspace_id);
    let _ = clean_workspace_path(&target, &path)?;
    Ok(())
}

fn clean_all() -> Result<()> {
    let backend = get_backend()?;
    let workspaces = backend
        .list()
        .map_err(|e| anyhow::anyhow!("Failed to list workspaces: {e}"))?;

    if workspaces.is_empty() {
        println!("No workspaces to clean.");
        return Ok(());
    }

    let mut cleaned = 0usize;
    let mut missing = 0usize;
    for workspace in &workspaces {
        let name = workspace.id.as_str();
        let path = backend.workspace_path(&workspace.id);
        match clean_workspace_path(name, &path) {
            Ok(CleanOutcome::Cleaned) => cleaned += 1,
            Ok(CleanOutcome::Missing) => missing += 1,
            Err(err) => {
                bail!(
                    "Failed cleaning workspace '{name}' at {}: {}\n  Fix: remove manually or run maw ws clean {name}",
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
        println!("No target/ directory in workspace '{name}' ({})", path.display());
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
