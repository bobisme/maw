use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;
use serde::Serialize;
use tracing::instrument;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::diff::compute_patchset;
use maw_core::model::types::{EpochId, WorkspaceId, WorkspaceMode};
use maw_core::oplog::read::read_head;
use maw_core::oplog::types::{OpPayload, Operation};
use maw_core::refs as manifold_refs;

use super::{
    DEFAULT_WORKSPACE, MawConfig, ensure_repo_root, get_backend, metadata,
    oplog_runtime::append_operation_with_runtime_checkpoint, repo_root,
    templates::WorkspaceTemplate, workspace_path, workspaces_dir,
};

#[instrument(skip(template), fields(workspace = name))]
pub fn create(
    name: &str,
    revision: Option<&str>,
    persistent: bool,
    template: Option<WorkspaceTemplate>,
) -> Result<()> {
    let root = ensure_repo_root()?;
    let backend = get_backend()?;
    let path = workspace_path(name)?;

    if path.exists() {
        bail!("Workspace already exists at {}", path.display());
    }

    // Ensure ws directory exists
    let ws_dir = workspaces_dir()?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

    let mode = if persistent {
        WorkspaceMode::Persistent
    } else {
        WorkspaceMode::Ephemeral
    };
    let template_profile = template.map(WorkspaceTemplate::profile);

    println!("Creating workspace '{name}' at ws/{name} ...");
    if persistent {
        println!(
            "  Mode: persistent (survives epoch advances; use `maw ws advance {name}` to rebase)"
        );
    }

    // Determine base epoch.
    // Use the provided revision, or fall back to refs/manifold/epoch/current,
    // or HEAD of the configured branch.
    let epoch = resolve_epoch(&root, revision)?;

    // Create workspace ID
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    // Create the workspace via backend
    let info = backend.create(&ws_id, &epoch)
        .map_err(|e| anyhow::anyhow!(
            "Failed to create workspace: {e}\n  Check: maw doctor\n  Verify name is not already used: maw ws list"
        ))?;

    if let Err(e) = record_workspace_create_op(&root, &ws_id, &epoch) {
        tracing::warn!("Failed to record workspace create in history: {e}");
    }

    // Write workspace metadata (mode + optional template defaults).
    // Keep the common case lean: if mode is ephemeral and no template is set,
    // metadata is omitted and defaults are inferred.
    if persistent || template_profile.is_some() {
        let meta = metadata::WorkspaceMetadata {
            mode,
            template,
            template_defaults: template_profile.as_ref().map(|p| p.defaults.clone()),
        };
        metadata::write(&root, name, &meta)
            .with_context(|| format!("Failed to write metadata for workspace '{name}'"))?;
    }

    if let Some(profile) = &template_profile {
        write_template_artifact(&info.path, profile)
            .with_context(|| format!("Failed to write template artifact for workspace '{name}'"))?;
    }

    // Get short commit ID for display
    let short_oid = &epoch.as_str()[..12];

    println!();
    println!("Workspace '{name}' ready!");
    println!();
    println!(
        "  Mode:   {}",
        if persistent {
            "persistent"
        } else {
            "ephemeral"
        }
    );
    if let Some(profile) = &template_profile {
        println!("  Template: {}", profile.template);
        println!("  Merge policy: {}", profile.defaults.merge_policy);
    }
    println!("  Epoch:  {short_oid} (base commit for this workspace)");
    println!("  Path:   {}/", info.path.display());
    println!();
    println!("  IMPORTANT: All file reads, writes, and edits must use this path.");
    println!("  This is your working directory for ALL operations, not just bash.");
    println!();
    println!("To start working:");
    println!();
    println!("  # Edit files under {}/", info.path.display());
    println!("  # Changes are detected automatically by the merge engine");
    println!();
    println!("  # Run commands in the workspace:");
    println!("  maw exec {name} -- cargo test");
    println!();
    if persistent {
        println!("Note: This is a PERSISTENT workspace. When the epoch advances:");
        println!("  maw ws advance {name}   # rebase onto latest epoch");
        println!("  maw ws status           # check staleness");
    } else {
        println!("Note: All edits in the workspace are tracked automatically.");
        println!("The merge engine captures changes when merging.");
    }

    Ok(())
}

#[derive(Serialize)]
struct WorkspaceTemplateArtifact {
    template: String,
    description: String,
    merge_policy: String,
    default_checks: Vec<String>,
    recommended_validation: Vec<String>,
}

fn write_template_artifact(
    workspace_path: &std::path::Path,
    profile: &super::templates::TemplateProfile,
) -> Result<()> {
    let artifact_path = workspace_path
        .join(".manifold")
        .join("workspace-template.json");
    if let Some(parent) = artifact_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let artifact = WorkspaceTemplateArtifact {
        template: profile.template.to_string(),
        description: profile.description.clone(),
        merge_policy: profile.defaults.merge_policy.clone(),
        default_checks: profile.defaults.default_checks.clone(),
        recommended_validation: profile.defaults.recommended_validation.clone(),
    };

    let content = serde_json::to_string_pretty(&artifact)
        .context("Failed to serialize workspace template artifact")?;
    std::fs::write(&artifact_path, content)
        .with_context(|| format!("Failed to write {}", artifact_path.display()))?;
    Ok(())
}

/// Resolve the epoch (base commit) for a new workspace.
///
/// Priority:
/// 1. Explicit revision (from --revision flag)
/// 2. refs/manifold/epoch/current (if set by `maw init`)
/// 3. HEAD of the configured branch
fn resolve_epoch(root: &std::path::Path, revision: Option<&str>) -> Result<EpochId> {
    if let Some(rev) = revision {
        // Resolve the user-specified revision to a full OID
        let repo = maw_git::GixRepo::open(root)
            .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
        let git_oid = repo.rev_parse(rev)
            .map_err(|e| anyhow::anyhow!("Cannot resolve revision '{rev}': {e}"))?;
        let oid = git_oid.to_string();
        return EpochId::new(&oid).map_err(|e| anyhow::anyhow!("Invalid commit OID: {e}"));
    }

    // Try refs/manifold/epoch/current first
    if let Ok(Some(oid)) = manifold_refs::read_epoch_current(root) {
        let epoch =
            EpochId::new(oid.as_str()).map_err(|e| anyhow::anyhow!("Invalid epoch OID: {e}"))?;

        // Check if the epoch and configured branch have diverged.
        // Auto-resync to avoid creating a workspace that can't merge.
        // This handles both cases:
        // - epoch behind branch (direct commits advanced branch)
        // - epoch ahead of branch (merge commit was dropped/reset)
        let config = MawConfig::load(root).unwrap_or_default();
        let branch = config.branch();
        let branch_ref = format!("refs/heads/{branch}");
        if let Ok(Some(branch_oid)) = manifold_refs::read_ref(root, &branch_ref)
            && oid != branch_oid {
                let branch_id = EpochId::new(branch_oid.as_str())
                    .map_err(|e| anyhow::anyhow!("Invalid branch OID: {e}"))?;
                manifold_refs::write_epoch_current(root, &branch_oid)
                    .map_err(|e| anyhow::anyhow!("Failed to resync epoch: {e}"))?;
                eprintln!(
                    "NOTE: epoch was out of sync with '{branch}' — auto-synced {} → {}",
                    &oid.as_str()[..12],
                    &branch_oid.as_str()[..12],
                );
                return Ok(branch_id);
            }

        return Ok(epoch);
    }

    // Fall back to configured branch HEAD
    let config = MawConfig::load(root).unwrap_or_default();
    let branch = config.branch();
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;

    match repo.rev_parse_opt(branch) {
        Ok(Some(git_oid)) => {
            let oid = git_oid.to_string();
            EpochId::new(&oid).map_err(|e| anyhow::anyhow!("Invalid branch OID: {e}"))
        }
        Ok(None) | Err(_) => {
            // Last resort: try HEAD
            match repo.rev_parse_opt("HEAD") {
                Ok(Some(git_oid)) => {
                    let oid = git_oid.to_string();
                    EpochId::new(&oid).map_err(|e| anyhow::anyhow!("Invalid HEAD OID: {e}"))
                }
                _ => bail!("No commits found. Run `maw init` first, or specify --revision."),
            }
        }
    }
}

#[instrument(fields(workspace = name))]
pub fn destroy(name: &str, confirm: bool, force: bool) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot destroy the default workspace");
    }
    // Also check config in case default_workspace is customized
    if let Ok(root) = repo_root()
        && let Ok(config) = MawConfig::load(&root)
        && name == config.default_workspace()
    {
        bail!("Cannot destroy the default workspace");
    }

    let root = ensure_repo_root()?;
    let path = workspace_path(name)?;

    if !path.exists() {
        println!(
            "Workspace '{name}' is already absent at {}.",
            path.display()
        );
        println!("No action needed.");
        return Ok(());
    }

    let backend = get_backend()?;
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;
    let status = backend
        .status(&ws_id)
        .map_err(|e| anyhow::anyhow!("Failed to inspect workspace state: {e}"))?;
    let touched_count = compute_patchset(&path, &status.base_epoch)
        .map(|patch_set| patch_set.len())
        .map_err(|e| anyhow::anyhow!("Failed to inspect local changes before destroy: {e}"))?;

    // FP: crash after status check but before any destructive action.
    maw::fp!("FP_DESTROY_AFTER_STATUS")?;

    if touched_count > 0 && !force {
        bail!(
            "Workspace '{name}' has {touched_count} unmerged change(s). Refusing destroy to avoid data loss.\n  \
             Review changes: maw ws touched {name} --format json\n  \
             Destroy anyway: maw ws destroy {name} --force"
        );
    }

    if confirm {
        println!("About to destroy workspace '{name}' at {}", path.display());
        println!("This will remove the workspace and delete the directory.");
        if touched_count > 0 {
            println!("WARNING: {touched_count} unmerged change(s) will be lost.");
        }
        println!();
        print!("Continue? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut capture_result = None;
    if force {
        capture_result = super::capture::capture_before_destroy(&path, name, status.base_epoch.oid())
            .map_err(|e| anyhow::anyhow!("Failed to capture workspace state before destroy: {e}"))?;
    }

    // Determine final head for destroy record before we destroy
    let final_head = super::capture::resolve_head(&path)
        .unwrap_or_else(|_| status.base_epoch.oid().clone());

    if let Err(e) = record_workspace_destroy_op(&root, &ws_id, &status.base_epoch) {
        tracing::warn!("Failed to record workspace destroy in history: {e}");
    }

    let artifact_path_result = super::destroy_record::write_destroy_record(
        &root,
        name,
        &status.base_epoch,
        &final_head,
        capture_result.as_ref(),
        super::destroy_record::DestroyReason::Destroy,
    );
    if let Err(ref e) = artifact_path_result {
        tracing::warn!("Failed to write destroy record: {e}");
    }

    // FP: crash after capture/record but before actual workspace deletion.
    // A crash here means the destroy record is written but the workspace
    // still exists on disk.
    maw::fp!("FP_DESTROY_BEFORE_DELETE")?;

    backend
        .destroy(&ws_id)
        .map_err(|e| anyhow::anyhow!("Failed to destroy workspace: {e}"))?;

    // Clean up workspace metadata (best-effort; don't fail destroy if missing).
    let _ = metadata::delete(&root, name);

    if force {
        if let Some(ref capture) = capture_result {
            let short_oid = &capture.commit_oid.as_str()[..12];
            println!("Snapshot saved: {short_oid}");
            println!("  Recover with: maw ws recover {name}");
            println!("Workspace '{name}' destroyed.");
            // Emit full recovery surface contract
            super::capture::emit_recovery_surface(
                name,
                capture,
                artifact_path_result.as_deref().ok(),
                false, // no merge commit — standalone destroy
                true,  // destroy operation succeeded
            );
        } else {
            println!("Workspace '{name}' destroyed. (nothing to snapshot)");
        }
    } else {
        println!("Workspace '{name}' destroyed.");
    }

    Ok(())
}

fn record_workspace_create_op(
    root: &std::path::Path,
    ws_id: &WorkspaceId,
    epoch: &EpochId,
) -> Result<()> {
    let previous_head =
        read_head(root, ws_id).map_err(|e| anyhow::anyhow!("read workspace history head: {e}"))?;
    let parent_ids = previous_head.iter().cloned().collect();

    let op = Operation {
        parent_ids,
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Create {
            epoch: epoch.clone(),
        },
    };

    append_operation_with_runtime_checkpoint(root, ws_id, &op, previous_head.as_ref())
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("append create op: {e}"))
}

fn record_workspace_destroy_op(
    root: &std::path::Path,
    ws_id: &WorkspaceId,
    base_epoch: &EpochId,
) -> Result<()> {
    let head = ensure_workspace_oplog_head(root, ws_id, base_epoch)?;

    let op = Operation {
        parent_ids: vec![head.clone()],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Destroy,
    };

    append_operation_with_runtime_checkpoint(root, ws_id, &op, Some(&head))
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("append destroy op: {e}"))
}

fn ensure_workspace_oplog_head(
    root: &std::path::Path,
    ws_id: &WorkspaceId,
    base_epoch: &EpochId,
) -> Result<maw_core::model::types::GitOid> {
    if let Some(head) =
        read_head(root, ws_id).map_err(|e| anyhow::anyhow!("read workspace history head: {e}"))?
    {
        return Ok(head);
    }

    let create_op = Operation {
        parent_ids: vec![],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Create {
            epoch: base_epoch.clone(),
        },
    };

    append_operation_with_runtime_checkpoint(root, ws_id, &create_op, None)
        .map_err(|e| anyhow::anyhow!("bootstrap workspace history: {e}"))
}



/// Attach (reconnect) an orphaned workspace directory.
/// In the git worktree model, this means creating a worktree entry
/// for an existing directory.
#[allow(clippy::too_many_lines)]
pub fn attach(name: &str, revision: Option<&str>) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot attach the default workspace (it's always tracked)");
    }

    ensure_repo_root()?;
    let root = repo_root()?;
    let path = workspace_path(name)?;

    // Check if directory exists
    if !path.exists() {
        bail!(
            "Workspace directory does not exist at {}\n  \
             The directory must exist to attach it.\n  \
             To create a new workspace: maw ws create {name}",
            path.display()
        );
    }

    // Check if workspace is already tracked by git worktree
    let backend = get_backend()?;
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    if backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' is already tracked.\n  \
             Use 'maw ws list' to see all workspaces."
        );
    }

    // Resolve epoch
    let epoch = resolve_epoch(&root, revision)?;

    println!(
        "Attaching workspace '{name}' at epoch {}...",
        &epoch.as_str()[..12]
    );

    // Move existing contents to a temp location
    let temp_backup = root.join("ws").join(format!(".{name}-attach-backup"));
    backup_workspace_contents(&path, &temp_backup)?;

    // Create the worktree via backend
    match backend.create(&ws_id, &epoch) {
        Ok(_) => {
            if let Err(e) = record_workspace_create_op(&root, &ws_id, &epoch) {
                tracing::warn!("Failed to record workspace create in history: {e}");
            }
            // Move contents back from backup, overwriting git-populated files
            restore_backup_overwrite(&temp_backup, &path)?;
            std::fs::remove_dir_all(&temp_backup).ok();
        }
        Err(e) => {
            // Restore backup on failure
            restore_backup_best_effort(&temp_backup, &path);
            let _ = std::fs::remove_dir_all(&temp_backup);
            bail!(
                "Failed to attach workspace: {e}\n  \
                 Your files have been restored.\n  \
                 Try: maw ws destroy {name} && maw ws create {name}"
            );
        }
    }

    println!();
    println!("Workspace '{name}' attached!");
    println!();
    println!("  Path: {}/", path.display());
    println!();
    println!("  NOTE: Your local files were preserved. They may differ from the");
    println!("  epoch's files. Run 'maw exec {name} -- git status' to see differences.");
    println!();
    println!("To continue working:");
    println!("  maw exec {name} -- git status");

    Ok(())
}

/// Move all workspace contents (except `.git`) into a backup directory,
/// then remove any stale `.git` file/directory so the workspace dir is empty.
fn backup_workspace_contents(workspace: &std::path::Path, backup: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(backup)
        .with_context(|| format!("Failed to create backup directory: {}", backup.display()))?;

    let entries: Vec<_> = std::fs::read_dir(workspace)
        .with_context(|| format!("Failed to read directory: {}", workspace.display()))?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name() != ".git" && e.file_name() != ".jj")
        .collect();

    for entry in &entries {
        let src = entry.path();
        let dst = backup.join(entry.file_name());
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to move {} to backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }

    // Remove the .git file/directory (stale workspace metadata)
    let git_entry = workspace.join(".git");
    if git_entry.exists() {
        if git_entry.is_dir() {
            std::fs::remove_dir_all(&git_entry)
                .with_context(|| "Failed to remove stale .git directory")?;
        } else {
            std::fs::remove_file(&git_entry).with_context(|| "Failed to remove stale .git file")?;
        }
    }

    // Also clean up .jj if present (legacy)
    let jj_dir = workspace.join(".jj");
    if jj_dir.exists() {
        std::fs::remove_dir_all(&jj_dir).ok();
    }

    Ok(())
}

/// Best-effort restore of backup contents (used on failure paths).
fn restore_backup_best_effort(backup: &std::path::Path, workspace: &std::path::Path) {
    for entry in std::fs::read_dir(backup)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
    {
        let src = entry.path();
        let dst = workspace.join(entry.file_name());
        let _ = std::fs::rename(&src, &dst);
    }
}

/// Restore backup contents into workspace, overwriting git-populated files.
fn restore_backup_overwrite(backup: &std::path::Path, workspace: &std::path::Path) -> Result<()> {
    for entry in std::fs::read_dir(backup)
        .with_context(|| "Failed to read backup directory")?
        .filter_map(std::result::Result::ok)
    {
        let src = entry.path();
        let dst = workspace.join(entry.file_name());
        // If git created the file, remove it first
        if dst.exists() {
            if dst.is_dir() {
                std::fs::remove_dir_all(&dst).ok();
            } else {
                std::fs::remove_file(&dst).ok();
            }
        }
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to restore {} from backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }
    Ok(())
}
