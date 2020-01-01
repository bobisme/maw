//! `maw ws recover` — list, inspect, and recover snapshots from destroyed workspaces.
//!
//! # Subcommands
//!
//! - `maw ws recover` — list all destroyed workspaces with snapshots
//! - `maw ws recover <name>` — show full destroy history for a workspace
//! - `maw ws recover <name> --show <path>` — show a specific file from the snapshot
//! - `maw ws recover <name> --to <new-name>` — restore snapshot into a new workspace

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::format::OutputFormat;

use super::destroy_record::{
    self, DestroyRecord, RecordCaptureMode,
};
use super::{repo_root, validate_workspace_name, workspace_path};

// ---------------------------------------------------------------------------
// List all destroyed workspaces
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DestroyedWorkspaceSummary {
    name: String,
    destroyed_at: String,
    capture_mode: String,
    snapshot_oid: Option<String>,
    dirty_file_count: usize,
}

#[derive(Serialize)]
struct RecoverListEnvelope {
    destroyed_workspaces: Vec<DestroyedWorkspaceSummary>,
    advice: Vec<String>,
}

pub fn list_destroyed(format: OutputFormat) -> Result<()> {
    let root = repo_root()?;
    let names = destroy_record::list_destroyed_workspaces(&root)?;

    if names.is_empty() {
        match format {
            OutputFormat::Json => {
                let envelope = RecoverListEnvelope {
                    destroyed_workspaces: vec![],
                    advice: vec!["No destroyed workspaces with snapshots found.".to_string()],
                };
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            }
            _ => {
                println!("No destroyed workspaces with snapshots found.");
            }
        }
        return Ok(());
    }

    let mut summaries = Vec::new();
    for name in &names {
        if let Ok(Some(latest)) = destroy_record::read_latest_pointer(&root, name) {
            if let Ok(record) = destroy_record::read_record(&root, name, &latest.record) {
                summaries.push(DestroyedWorkspaceSummary {
                    name: name.clone(),
                    destroyed_at: record.destroyed_at.clone(),
                    capture_mode: record.capture_mode.to_string(),
                    snapshot_oid: record.snapshot_oid.as_ref().map(|o| o[..12].to_string()),
                    dirty_file_count: record.dirty_files.len(),
                });
            }
        }
    }

    match format {
        OutputFormat::Json => {
            let envelope = RecoverListEnvelope {
                destroyed_workspaces: summaries,
                advice: vec![
                    "Inspect: maw ws recover <name>".to_string(),
                    "Show file: maw ws recover <name> --show <path>".to_string(),
                    "Restore: maw ws recover <name> --to <new-name>".to_string(),
                ],
            };
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        OutputFormat::Text => {
            print_list_text(&summaries);
        }
        OutputFormat::Pretty => {
            print_list_pretty(&summaries, format);
        }
    }

    Ok(())
}

fn print_list_text(summaries: &[DestroyedWorkspaceSummary]) {
    println!("NAME\tDESTROYED_AT\tCAPTURE\tSNAPSHOT\tDIRTY_FILES");
    for s in summaries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            s.name,
            s.destroyed_at,
            s.capture_mode,
            s.snapshot_oid.as_deref().unwrap_or("-"),
            s.dirty_file_count,
        );
    }
    println!();
    println!("Next: maw ws recover <name>");
}

fn print_list_pretty(summaries: &[DestroyedWorkspaceSummary], format: OutputFormat) {
    let use_color = format.should_use_color();

    if use_color {
        println!("\x1b[1mDestroyed workspaces with snapshots:\x1b[0m");
    } else {
        println!("Destroyed workspaces with snapshots:");
    }
    println!();

    // Calculate column widths
    let name_width = summaries
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    for s in summaries {
        let snapshot_display = s
            .snapshot_oid
            .as_deref()
            .unwrap_or("-");
        let dirty_suffix = if s.dirty_file_count > 0 {
            format!(" ({} dirty files)", s.dirty_file_count)
        } else {
            String::new()
        };

        if use_color {
            println!(
                "  \x1b[1;36m{:<width$}\x1b[0m  {}  {:<14} {}{}",
                s.name,
                s.destroyed_at,
                s.capture_mode,
                snapshot_display,
                dirty_suffix,
                width = name_width,
            );
        } else {
            println!(
                "  {:<width$}  {}  {:<14} {}{}",
                s.name,
                s.destroyed_at,
                s.capture_mode,
                snapshot_display,
                dirty_suffix,
                width = name_width,
            );
        }
    }

    println!();
    if use_color {
        println!("\x1b[90mNext: maw ws recover <name>\x1b[0m");
    } else {
        println!("Next: maw ws recover <name>");
    }
}

// ---------------------------------------------------------------------------
// Show destroy history for a specific workspace
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RecoverShowEnvelope {
    workspace: String,
    records: Vec<DestroyRecord>,
    advice: Vec<String>,
}

pub fn show_workspace(name: &str, format: OutputFormat) -> Result<()> {
    validate_workspace_name(name)?;
    let root = repo_root()?;

    let record_files = destroy_record::list_record_files(&root, name)?;
    if record_files.is_empty() {
        bail!(
            "No destroy records found for workspace '{name}'.\n  \
             List destroyed workspaces: maw ws recover"
        );
    }

    let mut records = Vec::new();
    for filename in &record_files {
        let record = destroy_record::read_record(&root, name, filename)?;
        records.push(record);
    }

    match format {
        OutputFormat::Json => {
            let envelope = RecoverShowEnvelope {
                workspace: name.to_string(),
                records,
                advice: vec![
                    "Show file: maw ws recover <name> --show <path>".to_string(),
                    "Restore: maw ws recover <name> --to <new-name>".to_string(),
                ],
            };
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        OutputFormat::Text => print_show_text(name, &records),
        OutputFormat::Pretty => print_show_pretty(name, &records, format),
    }

    Ok(())
}

fn print_show_text(name: &str, records: &[DestroyRecord]) {
    println!("Destroy history for workspace '{name}' ({} record(s)):", records.len());
    println!();
    for (i, r) in records.iter().enumerate() {
        println!("--- Record {} ---", i + 1);
        println!("destroyed_at:  {}", r.destroyed_at);
        println!("capture_mode:  {}", r.capture_mode);
        println!("final_head:    {}", r.final_head);
        if let Some(ref oid) = r.snapshot_oid {
            println!("snapshot_oid:  {oid}");
        }
        if let Some(ref sref) = r.snapshot_ref {
            println!("snapshot_ref:  {sref}");
        }
        if let Some(ref href) = r.final_head_ref {
            println!("final_head_ref: {href}");
        }
        println!("base_epoch:    {}", r.base_epoch);
        println!("reason:        {:?}", r.destroy_reason);
        println!("tool_version:  {}", r.tool_version);
        if !r.dirty_files.is_empty() {
            println!("dirty_files ({}):", r.dirty_files.len());
            for f in &r.dirty_files {
                println!("  {f}");
            }
        }
        println!();
    }
    println!("Next: maw ws recover {name} --show <path>");
    println!("      maw ws recover {name} --to <new-name>");
}

fn print_show_pretty(name: &str, records: &[DestroyRecord], format: OutputFormat) {
    let use_color = format.should_use_color();

    if use_color {
        println!(
            "\x1b[1mDestroy history for '{}' ({} record(s)):\x1b[0m",
            name,
            records.len()
        );
    } else {
        println!(
            "Destroy history for '{}' ({} record(s)):",
            name,
            records.len()
        );
    }
    println!();

    for (i, r) in records.iter().enumerate() {
        let header = format!("Record {}", i + 1);
        if use_color {
            println!("  \x1b[1;33m{header}\x1b[0m");
        } else {
            println!("  {header}");
        }
        println!("    Destroyed:  {}", r.destroyed_at);
        println!("    Capture:    {}", r.capture_mode);
        println!("    Final HEAD: {}...", &r.final_head[..12]);
        if let Some(ref oid) = r.snapshot_oid {
            println!("    Snapshot:   {}...", &oid[..oid.len().min(12)]);
        }
        if let Some(ref sref) = r.snapshot_ref {
            println!("    Pin ref:    {sref}");
        }
        println!("    Epoch:      {}...", &r.base_epoch[..12]);
        println!("    Reason:     {:?}", r.destroy_reason);
        if !r.dirty_files.is_empty() {
            println!("    Dirty files ({}):", r.dirty_files.len());
            for f in &r.dirty_files {
                println!("      {f}");
            }
        }
        println!();
    }

    if use_color {
        println!("\x1b[90mNext: maw ws recover {name} --show <path>\x1b[0m");
        println!("\x1b[90m      maw ws recover {name} --to <new-name>\x1b[0m");
    } else {
        println!("Next: maw ws recover {name} --show <path>");
        println!("      maw ws recover {name} --to <new-name>");
    }
}

// ---------------------------------------------------------------------------
// Show a specific file from the snapshot
// ---------------------------------------------------------------------------

pub fn show_file(name: &str, path: &str) -> Result<()> {
    validate_workspace_name(name)?;
    validate_show_path(path)?;
    let root = repo_root()?;

    let latest = destroy_record::read_latest_pointer(&root, name)?
        .with_context(|| format!("No destroy records found for workspace '{name}'"))?;
    let record = destroy_record::read_record(&root, name, &latest.record)?;

    let oid = resolve_recoverable_oid(&record)?;

    // Use git show <oid>:<path> to retrieve the file content.
    // Run from the git common dir (repo root) so the ref resolves.
    let git_cwd = super::git_cwd()?;
    let output = Command::new("git")
        .args(["show", &format!("{oid}:{path}")])
        .current_dir(&git_cwd)
        .output()
        .context("failed to run git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist") || stderr.contains("exists on disk, but not in") {
            bail!(
                "File '{path}' not found in snapshot {oid_short} for workspace '{name}'.\n  \
                 List dirty files: maw ws recover {name}",
                oid_short = &oid[..oid.len().min(12)],
            );
        }
        bail!("git show failed: {}", stderr.trim());
    }

    // Write raw content to stdout (binary-safe isn't needed for text, but let's be correct)
    use std::io::Write;
    std::io::stdout()
        .write_all(&output.stdout)
        .context("write to stdout")?;

    Ok(())
}

/// Validate the `--show` path argument against directory traversal.
fn validate_show_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("Path cannot be empty");
    }
    if path.starts_with('/') {
        bail!("Path must be relative (no leading '/')");
    }
    if path.contains('\0') {
        bail!("Path cannot contain null bytes");
    }
    // Reject path traversal components
    for component in path.split('/') {
        if component == ".." {
            bail!("Path cannot contain '..' components (directory traversal)");
        }
    }
    Ok(())
}

/// Resolve the OID to use for file retrieval from a destroy record.
///
/// For `DirtySnapshot` mode, uses the snapshot OID (the stash commit).
/// For `HeadOnly` mode, uses the final HEAD.
/// For `None` mode, fails — nothing was captured.
fn resolve_recoverable_oid(record: &DestroyRecord) -> Result<String> {
    match record.capture_mode {
        RecordCaptureMode::DirtySnapshot => record
            .snapshot_oid
            .clone()
            .context("destroy record has dirty_snapshot mode but no snapshot_oid"),
        RecordCaptureMode::HeadOnly => Ok(record.final_head.clone()),
        RecordCaptureMode::None => bail!(
            "No snapshot was captured for this workspace (capture_mode=none).\n  \
             The workspace was clean at its epoch when destroyed."
        ),
    }
}

// ---------------------------------------------------------------------------
// Restore snapshot to a new workspace
// ---------------------------------------------------------------------------

pub fn restore_to(name: &str, new_name: &str) -> Result<()> {
    validate_workspace_name(name)?;
    validate_workspace_name(new_name)?;
    let root = repo_root()?;

    // Check that the destination doesn't already exist
    let dest_path = workspace_path(new_name)?;
    if dest_path.exists() {
        bail!(
            "Workspace '{new_name}' already exists at {}.\n  \
             Choose a different name.",
            dest_path.display()
        );
    }

    // Check that the source name isn't an active workspace
    // (it's fine if a destroyed workspace has the same name as a live one —
    // the user asked to restore it to a *different* name)

    let latest = destroy_record::read_latest_pointer(&root, name)?
        .with_context(|| format!("No destroy records found for workspace '{name}'"))?;
    let record = destroy_record::read_record(&root, name, &latest.record)?;

    let oid = resolve_recoverable_oid(&record)?;

    // Step 1: Create the new workspace via the standard create path
    println!("Creating workspace '{new_name}' from snapshot of '{name}'...");
    super::create::create(new_name, None, false, None)?;

    // Step 2: Populate from the snapshot using git read-tree + checkout-index
    let new_ws_path = workspace_path(new_name)?;
    populate_from_snapshot(&new_ws_path, &oid)?;

    println!();
    println!("Restored snapshot of '{name}' into workspace '{new_name}'.");
    println!();
    println!("  Snapshot:  {}...", &oid[..oid.len().min(12)]);
    println!("  Path:      {}/", new_ws_path.display());
    if !record.dirty_files.is_empty() {
        println!(
            "  Recovered: {} dirty file(s)",
            record.dirty_files.len()
        );
    }
    println!();
    println!("Next: maw exec {new_name} -- git status");
    println!("      maw exec {new_name} -- git diff");

    Ok(())
}

/// Populate a workspace from a snapshot OID using git2-style operations.
///
/// Uses `git read-tree` to load the snapshot tree into the index, then
/// `git checkout-index` to materialize the files.
fn populate_from_snapshot(ws_path: &std::path::Path, oid: &str) -> Result<()> {
    // For stash commits (WorktreeCapture), the worktree state is stored
    // in the third parent's tree. But `git read-tree` on the stash commit
    // itself accesses the top-level tree which includes the index state.
    //
    // The safest approach: use `git checkout <oid> -- .` which overwrites
    // the working tree with the snapshot's content.

    // First, read the snapshot tree into the index
    let read_tree = Command::new("git")
        .args(["read-tree", oid])
        .current_dir(ws_path)
        .output()
        .context("git read-tree failed")?;

    if !read_tree.status.success() {
        let stderr = String::from_utf8_lossy(&read_tree.stderr);
        bail!("git read-tree failed: {}", stderr.trim());
    }

    // Then checkout the index to the working tree (overwrite existing files)
    let checkout = Command::new("git")
        .args(["checkout-index", "--all", "--force"])
        .current_dir(ws_path)
        .output()
        .context("git checkout-index failed")?;

    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        bail!("git checkout-index failed: {}", stderr.trim());
    }

    // Reset the index back to HEAD so the workspace shows snapshot files
    // as unstaged modifications (not staged additions)
    let reset = Command::new("git")
        .args(["reset"])
        .current_dir(ws_path)
        .output()
        .context("git reset failed")?;

    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr);
        tracing::warn!("git reset after populate failed: {}", stderr.trim());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_show_path_rejects_traversal() {
        assert!(validate_show_path("../etc/passwd").is_err());
        assert!(validate_show_path("foo/../bar").is_err());
        assert!(validate_show_path("foo/../../bar").is_err());
    }

    #[test]
    fn validate_show_path_rejects_absolute() {
        assert!(validate_show_path("/etc/passwd").is_err());
    }

    #[test]
    fn validate_show_path_rejects_empty() {
        assert!(validate_show_path("").is_err());
    }

    #[test]
    fn validate_show_path_rejects_null() {
        assert!(validate_show_path("foo\0bar").is_err());
    }

    #[test]
    fn validate_show_path_allows_normal_paths() {
        assert!(validate_show_path("src/main.rs").is_ok());
        assert!(validate_show_path("README.md").is_ok());
        assert!(validate_show_path("src/foo/bar.rs").is_ok());
        assert!(validate_show_path(".hidden").is_ok());
        assert!(validate_show_path("foo/./bar").is_ok()); // "." is fine, only ".." is traversal
    }

    #[test]
    fn resolve_oid_dirty_snapshot() {
        let record = DestroyRecord {
            workspace_id: "test".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
            final_head: "a".repeat(40),
            final_head_ref: None,
            snapshot_oid: Some("b".repeat(40)),
            snapshot_ref: Some("refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_string()),
            capture_mode: RecordCaptureMode::DirtySnapshot,
            dirty_files: vec!["foo.rs".to_string()],
            base_epoch: "c".repeat(40),
            destroy_reason: super::destroy_record::DestroyReason::Destroy,
            tool_version: "0.47.0".to_string(),
        };
        let oid = resolve_recoverable_oid(&record).unwrap();
        assert_eq!(oid, "b".repeat(40));
    }

    #[test]
    fn resolve_oid_head_only() {
        let record = DestroyRecord {
            workspace_id: "test".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
            final_head: "a".repeat(40),
            final_head_ref: Some("refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_string()),
            snapshot_oid: None,
            snapshot_ref: None,
            capture_mode: RecordCaptureMode::HeadOnly,
            dirty_files: vec![],
            base_epoch: "c".repeat(40),
            destroy_reason: super::destroy_record::DestroyReason::Destroy,
            tool_version: "0.47.0".to_string(),
        };
        let oid = resolve_recoverable_oid(&record).unwrap();
        assert_eq!(oid, "a".repeat(40));
    }

    #[test]
    fn resolve_oid_none_fails() {
        let record = DestroyRecord {
            workspace_id: "test".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
            final_head: "a".repeat(40),
            final_head_ref: None,
            snapshot_oid: None,
            snapshot_ref: None,
            capture_mode: RecordCaptureMode::None,
            dirty_files: vec![],
            base_epoch: "c".repeat(40),
            destroy_reason: super::destroy_record::DestroyReason::Destroy,
            tool_version: "0.47.0".to_string(),
        };
        assert!(resolve_recoverable_oid(&record).is_err());
    }
}
