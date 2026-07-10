use std::io::{self, Write};
use std::process::Command;

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

use crate::changes::store::ChangesStore;
use crate::format::OutputFormat;

use super::destroy_guidance::DestroyRefusal;
use super::{
    DEFAULT_WORKSPACE, MawConfig, create_lock::WorkspaceCreateLock, ensure_repo_root, get_backend,
    metadata, oplog_runtime::append_operation_with_runtime_checkpoint, repo_root,
    templates::WorkspaceTemplate, workspace_path, workspaces_dir,
};

#[instrument(skip(template), fields(workspace = name))]
pub fn create(
    name: &str,
    from: Option<&str>,
    change: Option<&str>,
    persistent: bool,
    template: Option<WorkspaceTemplate>,
    description: Option<&str>,
) -> Result<()> {
    create_with_output(name, from, change, persistent, template, description, true)
}

#[instrument(skip(template), fields(workspace = name))]
pub fn create_quiet(
    name: &str,
    from: Option<&str>,
    change: Option<&str>,
    persistent: bool,
    template: Option<WorkspaceTemplate>,
    description: Option<&str>,
) -> Result<()> {
    create_with_output(name, from, change, persistent, template, description, false)
}

#[instrument(skip(template), fields(workspace = name, emit_output))]
#[expect(
    clippy::too_many_lines,
    reason = "workspace creation has ordered validation, backend, and metadata steps"
)]
fn create_with_output(
    name: &str,
    from: Option<&str>,
    change: Option<&str>,
    persistent: bool,
    template: Option<WorkspaceTemplate>,
    description: Option<&str>,
    emit_output: bool,
) -> Result<()> {
    let root = ensure_repo_root()?;
    let backend = get_backend()?;
    // `workspace_path` validates the name; do this before locking so an
    // invalid name fails fast without touching the lock directory.
    let path = workspace_path(name)?;

    // Make create atomic for this workspace *name* (bn-3bbc). Without this
    // lock, concurrent `maw ws create <same-name>` is a TOCTOU race: every
    // caller passes the `path.exists()` check before any has finished the
    // backend `worktree add`, so they all "succeed", clobber each other's
    // worktree, and may leave the workspace MISSING.
    //
    // `acquire` blocks until the lock is free, so concurrent same-name
    // creates serialize: exactly one caller wins and performs the real
    // create; the losers wake up, see the workspace now exists (the
    // re-check below), and fail fast with a clear error. The lock is held
    // for the whole critical section (existence check + backend create +
    // metadata write + success banner) and released by RAII `Drop` on every
    // exit path — success, early `bail!`, or panic. The lock is per-name,
    // so concurrent creates of *different* names never block each other.
    let _create_lock = WorkspaceCreateLock::acquire(&root, name).with_context(|| {
        format!("Failed to acquire create lock for workspace '{name}'\n  Check: maw doctor")
    })?;

    // Existence check is now race-safe: it runs under the exclusive
    // per-name lock, so a losing concurrent creator observes the winner's
    // completed workspace here and exits with an accurate error instead of
    // a false success banner / clobbered worktree.
    if path.exists() {
        bail!(
            "workspace '{name}' already exists\n  Check: maw ws list\n  \
             To recreate: maw ws destroy {name} && maw ws create {name}"
        );
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

    if emit_output {
        // Path display is layout-aware: v2 → ws/<name>, consolidated →
        // .maw/workspaces/<name>. SP5 §6 risk #1 (hidden-dir invisibility):
        // always name the full path so agents can navigate without guessing.
        let display_path = path
            .strip_prefix(&root)
            .map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
        println!("Creating workspace '{name}' at {display_path} ...");
    }
    if persistent && emit_output {
        println!(
            "  Mode: persistent (survives epoch advances; use `maw ws advance {name}` to rebase)"
        );
    }

    // Determine source revision and optional change binding.
    let (source_revision, bound_change_id) = resolve_workspace_source(&root, from, change)?;
    let attached_branch = if persistent || bound_change_id.is_some() {
        resolve_attached_branch(
            &root,
            source_revision.as_deref(),
            bound_change_id.as_deref(),
        )?
    } else {
        None
    };

    // Determine base epoch from resolved source revision.
    let epoch = resolve_epoch(&root, source_revision.as_deref())?;

    // bn-1abp: an explicit --from/--change IS honored as the workspace base,
    // but maw's merge model is epoch-based: a workspace whose base differs
    // from the current epoch is treated as stale, and the next `maw ws
    // merge` (sibling auto-rebase) or `maw ws sync` will rebase it onto the
    // epoch. Say so LOUDLY at create time instead of letting the agent
    // discover it when files move underneath them.
    if emit_output
        && from.is_some()
        && let Ok(Some(current)) = manifold_refs::read_epoch_current(&root)
        && current.as_str() != epoch.as_str()
    {
        eprintln!(
            "NOTE: --from {} resolves to {}, which differs from the current epoch {}.",
            from.unwrap_or_default(),
            &epoch.as_str()[..12],
            &current.as_str()[..12],
        );
        eprintln!(
            "  maw workspaces are epoch-based: the workspace starts at {}, but later merges \
             auto-rebase it onto the new epoch (skip per-merge with `maw ws merge --no-auto-rebase`).",
            &epoch.as_str()[..12],
        );
    }

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

    // Write workspace metadata (mode + optional template defaults + description).
    // Keep the common case lean: if mode is ephemeral and no template is set
    // and no description, metadata is omitted and defaults are inferred.
    if persistent
        || template_profile.is_some()
        || bound_change_id.is_some()
        || attached_branch.is_some()
        || description.is_some()
    {
        let meta = metadata::WorkspaceMetadata {
            mode,
            template,
            template_defaults: template_profile.as_ref().map(|p| p.defaults.clone()),
            change_id: bound_change_id.clone(),
            branch: attached_branch.clone(),
            description: description.map(str::to_owned),
        };
        metadata::write(&root, name, &meta)
            .with_context(|| format!("Failed to write metadata for workspace '{name}'"))?;
    }

    if let Some(change_id) = bound_change_id.as_deref() {
        bind_workspace_to_change(&root, name, change_id)?;
    }

    if let Some(profile) = &template_profile {
        write_template_artifact(&info.path, profile)
            .with_context(|| format!("Failed to write template artifact for workspace '{name}'"))?;
    }

    // Get short commit ID for display
    let short_oid = &epoch.as_str()[..12];

    if emit_output {
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
        if let Some(desc) = description {
            println!("  Description: {desc}");
        }
        if let Some(profile) = &template_profile {
            println!("  Template: {}", profile.template);
            println!("  Merge policy: {}", profile.defaults.merge_policy);
        }
        if let Some(change_id) = bound_change_id.as_deref() {
            println!("  Change: {change_id}");
        }
        if let Some(branch) = attached_branch.as_deref() {
            println!("  Branch: {branch}");
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
        if let Some(change_id) = bound_change_id.as_deref() {
            if name == change_id {
                println!("  maw ws create --change {change_id} <agent-workspace>");
                println!("  maw ws merge <agent-workspace> --into change:{change_id} --destroy");
            } else {
                println!("  maw ws merge {name} --into change:{change_id} --destroy");
            }
        } else if attached_branch.is_some() && persistent {
            println!("  maw ws merge <agent-workspace> --into {name} --destroy");
        } else {
            println!("  maw ws merge {name} --into default --destroy");
        }
        println!();
        if persistent {
            println!("Note: This is a PERSISTENT workspace. When the epoch advances:");
            println!("  maw ws advance {name}   # rebase onto latest epoch");
            println!("  maw ws status           # check staleness");
        } else {
            println!("Note: All edits in the workspace are tracked automatically.");
            println!("The merge engine captures changes when merging.");
        }
    }

    Ok(())
}

fn resolve_workspace_source(
    root: &std::path::Path,
    from: Option<&str>,
    change: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    if let Some(change_id) = change {
        let store = ChangesStore::open(root);
        let record = store.read_active_record(change_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Change '{change_id}' not found.\n  Next: list known changes: maw changes list"
            )
        })?;
        let change_branch = record.git.change_branch.trim();
        if change_branch.is_empty() {
            bail!("Change '{change_id}' has no configured branch in metadata.");
        }
        return Ok((Some(change_branch.to_owned()), Some(change_id.to_owned())));
    }

    if let Some(from_value) = from {
        return Ok((Some(resolve_from_source(root, from_value)?), None));
    }

    Ok((None, None))
}

fn resolve_from_source(root: &std::path::Path, from: &str) -> Result<String> {
    // Remote-tracking source: fetch remote branch first.
    if let Some((remote, branch)) = from.split_once('/')
        && !remote.is_empty()
        && !branch.is_empty()
        && remote_exists(root, remote)?
    {
        let fetch = Command::new("git")
            .args(["fetch", remote, branch, "--no-tags", "--quiet"])
            .current_dir(root)
            .output()
            .context("Failed to run git fetch for workspace source")?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            bail!(
                "Failed to fetch workspace source '{}': {}",
                from,
                stderr.trim()
            );
        }
        return Ok(from.to_owned());
    }

    // Workspace source shorthand: if `from` names an active workspace, resolve
    // to its current git HEAD commit.  We must NOT return the oplog ref
    // `refs/manifold/head/<workspace>` — that points to an operation blob, not
    // a commit, so rev-parse would yield a blob OID and worktree creation
    // would fail with "expected commit or tree, got blob".
    let workspace_head_ref = manifold_refs::workspace_head_ref(from);
    if manifold_refs::read_ref(root, &workspace_head_ref)
        .map_err(|e| anyhow::anyhow!("Failed to read workspace source ref: {e}"))?
        .is_some()
    {
        let ws_path =
            maw_core::model::layout::LayoutFlavor::detect_with_env(root).workspace_path(root, from);
        let repo = maw_git::GixRepo::open(&ws_path)
            .map_err(|e| anyhow::anyhow!("Failed to open workspace '{from}': {e}"))?;
        let head_oid = repo
            .rev_parse("HEAD")
            .map_err(|e| anyhow::anyhow!("Failed to resolve HEAD of workspace '{from}': {e}"))?;
        return Ok(head_oid.to_string());
    }

    Ok(from.to_owned())
}

fn remote_exists(root: &std::path::Path, remote: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(root)
        .output()
        .context("Failed to run git remote get-url")?;
    Ok(output.status.success())
}

fn resolve_attached_branch(
    root: &std::path::Path,
    source_revision: Option<&str>,
    bound_change_id: Option<&str>,
) -> Result<Option<String>> {
    if let Some(change_id) = bound_change_id {
        let store = ChangesStore::open(root);
        let record = store.read_active_record(change_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Change '{change_id}' not found while resolving workspace branch attachment."
            )
        })?;
        let branch = record.git.change_branch.trim();
        if branch.is_empty() {
            return Ok(None);
        }
        return Ok(Some(branch.to_owned()));
    }

    let Some(source) = source_revision else {
        return Ok(None);
    };
    let Some(branch) = local_branch_if_exists(root, source)? else {
        return Ok(None);
    };
    let config = MawConfig::load(root).unwrap_or_default();
    if branch == config.branch() {
        return Ok(None);
    }
    Ok(Some(branch))
}

fn local_branch_if_exists(root: &std::path::Path, branch: &str) -> Result<Option<String>> {
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') || branch.contains("..") {
        return Ok(None);
    }
    let branch_ref = format!("refs/heads/{branch}");
    let Some(_) = manifold_refs::read_ref(root, &branch_ref)
        .map_err(|e| anyhow::anyhow!("Failed to read branch ref '{branch_ref}': {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(branch.to_owned()))
}

/// Attach an existing workspace to a local branch for `maw ws merge --into <workspace>`.
pub fn attach_branch(name: &str, branch: &str) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("The default workspace already tracks the configured main branch.");
    }

    let root = ensure_repo_root()?;
    let path = workspace_path(name)?;
    let backend = get_backend()?;
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;
    if !path.exists() {
        bail!(
            "Workspace '{name}' does not exist at {}.\n  Check available workspaces: maw ws list",
            path.display()
        );
    }
    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' is not tracked by maw.\n  To fix: create it with `maw ws create {name} --from <branch>`, or reconnect it first with `maw ws attach {name}`."
        );
    }

    let branch = local_branch_if_exists(&root, branch)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Local branch '{branch}' does not exist.\n  Create it first, then retry: git branch {branch}"
        )
    })?;

    let mut meta = metadata::read(&root, name)?;
    meta.mode = WorkspaceMode::Persistent;
    meta.branch = Some(branch.clone());
    metadata::write(&root, name, &meta)
        .with_context(|| format!("Failed to write metadata for workspace '{name}'"))?;

    println!("Workspace '{name}' attached to branch '{branch}'.");
    println!("Merge into it with: maw ws merge <workspace> --into {name} --destroy");
    Ok(())
}

fn bind_workspace_to_change(
    root: &std::path::Path,
    workspace_name: &str,
    change_id: &str,
) -> Result<()> {
    let store = ChangesStore::open(root);
    let mut record = store.read_active_record(change_id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Change '{change_id}' not found while binding workspace '{workspace_name}'."
        )
    })?;

    if !record
        .workspaces
        .linked
        .iter()
        .any(|workspace| workspace == workspace_name)
    {
        record.workspaces.linked.push(workspace_name.to_owned());
    }
    if record.workspaces.primary.is_empty() {
        workspace_name.clone_into(&mut record.workspaces.primary);
    }

    store.with_lock("bind workspace to change", |locked| {
        let mut index = store.read_index()?;
        index.set_workspace_mapping(workspace_name, change_id);
        locked.write_index(&index)?;
        locked.write_active_record(&record)
    })
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
    // bn-1lj2: layout-aware so the artifact lands in the same manifold dir the
    // rest of the codebase resolves (consolidated → `.maw/manifold/`).
    let artifact_path = maw_core::model::layout::LayoutFlavor::detect_with_env(workspace_path)
        .manifold_dir(workspace_path)
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
/// 1. Explicit source revision (from --from or --change)
/// 2. refs/manifold/epoch/current (if set by `maw init`)
/// 3. HEAD of the configured branch
fn resolve_epoch(root: &std::path::Path, revision: Option<&str>) -> Result<EpochId> {
    if let Some(rev) = revision {
        // Resolve the user-specified revision to a full OID
        let repo = maw_git::GixRepo::open(root)
            .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
        let git_oid = repo
            .rev_parse(rev)
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
            && oid != branch_oid
        {
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

/// Count commits on the workspace HEAD ahead of `base_epoch_oid`.
///
/// Used by the structured `DestroyRefusal` (bn-c6l3 / bn-voy5) to
/// classify the workspace lifecycle state without requiring callers
/// to round-trip through `backend.list()` (which would materialize
/// every workspace's metadata just to read one field). Returns `None`
/// if the workspace repo can't be opened, HEAD doesn't resolve, or
/// the ancestry walk fails — the caller treats `None` as "0 commits
/// ahead" (conservative: classifies as `DirtyUncommitted`, which still
/// refuses and still surfaces the safer commit-then-merge path).
fn workspace_commits_ahead(ws_path: &std::path::Path, base_epoch_oid: &str) -> Option<u32> {
    use maw_git::GitRepo as _;
    let repo = maw_git::GixRepo::open(ws_path).ok()?;
    let head_oid = repo.rev_parse("HEAD").ok()?;
    let base_oid: maw_git::GitOid = base_epoch_oid.parse().ok()?;
    if head_oid == base_oid {
        return Some(0);
    }
    repo.count_commits_between(base_oid, head_oid).ok()
}

/// Refuse to destroy a workspace that a *live* in-flight merge has frozen as
/// a source workspace (bn-cm63).
///
/// `maw ws destroy` deletes every ref owned by the workspace, including its
/// oplog head (`refs/manifold/head/<ws>`). A concurrent `maw ws merge <ws>`
/// freezes the source at PREPARE and, after a successful COMMIT, appends a
/// `Merge` op to that workspace's oplog via `record_merge_operations`. If
/// destroy already deleted the head ref, that append re-bootstraps a fresh
/// oplog head — resurrecting a ref destroy intended to remove and leaving a
/// permanently dangling blob ref with no owning workspace.
///
/// Resolution: serialize. If a non-terminal `.manifold/merge-state.json`
/// lists this workspace in `sources` and its owner process is **alive**, the
/// destroy is refused with an actionable message. An orphaned or
/// indeterminate merge-state must NOT block destroy forever — that would
/// regress bn-2wyh — so those cases surface the `maw ws merge --abort`
/// recovery hint instead, consistent with prepare.rs / doctor behavior.
fn guard_destroy_against_inflight_merge(root: &std::path::Path, name: &str) -> Result<()> {
    use maw_core::merge_state::{DEFAULT_STALE_AFTER_SECS, MergeStateFile, Staleness};

    let state_path = MergeStateFile::default_path(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
    );
    // No merge in progress (NotFound) or an unreadable/corrupt merge-state:
    // nothing to serialize against. A corrupt merge-state is surfaced
    // separately by `maw doctor`; it must not wedge destroy.
    let Ok(state) = MergeStateFile::read(&state_path) else {
        return Ok(());
    };

    // A terminal merge-state (Complete/Aborted) is leftover and harmless —
    // the merge is over and will not touch this workspace's oplog again.
    if state.phase.is_terminal() {
        return Ok(());
    }

    // Only block if THIS workspace is one of the merge's frozen sources.
    let is_source = state.sources.iter().any(|s| s.as_str() == name);
    if !is_source {
        return Ok(());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match state.staleness(now, DEFAULT_STALE_AFTER_SECS) {
        Staleness::Live => {
            let pid = state
                .owner_pid
                .map_or_else(|| "?".to_string(), |p| p.to_string());
            bail!(
                "Workspace '{name}' is being merged right now (merge phase: {}, pid: {pid}). \
                 Refusing destroy: it would orphan the workspace's oplog head ref.\n  \
                 Wait for the merge to finish, then: maw ws destroy {name} --force\n  \
                 (Tip: merge with --destroy to merge and clean up atomically.)",
                state.phase
            )
        }
        Staleness::Orphaned | Staleness::Indeterminate => {
            // The merge process is gone (or unprovable). Do NOT block destroy
            // forever — surface the recovery path. The caller can clear the
            // stale merge-state and retry; we mirror the prepare/doctor hint
            // rather than silently proceeding into the same race window.
            bail!(
                "Workspace '{name}' is listed as a source in a stale merge-state \
                 (phase: {}, owner process not running). The interrupted merge will \
                 not complete on its own.\n  \
                 Clear it first: maw ws merge --abort\n  \
                 Then: maw ws destroy {name} --force",
                state.phase
            )
        }
    }
}

#[instrument(fields(workspace = name))]
#[expect(
    clippy::too_many_lines,
    reason = "destroy command keeps safety checks and cleanup in one flow"
)]
pub fn destroy(name: &str, confirm: bool, force: bool, format: Option<OutputFormat>) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        // bn-21qy: In consolidated layout an impostor .maw/workspaces/default
        // can exist if someone bypassed the create guard (e.g. via a pre-fix
        // binary or git worktree add directly). Detect that case and allow
        // destroying the impostor while never touching the real default (repo
        // root). The path must be strictly under workspaces_dir to prevent any
        // ambiguity.
        if let Ok(root) = repo_root() {
            let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(&root);
            if flavor == maw_core::model::layout::LayoutFlavor::ConsolidatedMawDir {
                let impostor_path = flavor.workspaces_dir(&root).join(DEFAULT_WORKSPACE);
                if impostor_path.exists() {
                    // Verify it really IS under the workspaces dir (safety).
                    let ws_dir = flavor.workspaces_dir(&root);
                    if impostor_path.starts_with(&ws_dir) {
                        println!(
                            "Note: destroying impostor workspace at {} \
                             (this is NOT the real default workspace — that is the repo root).",
                            impostor_path.display()
                        );
                        return destroy_consolidated_impostor_default(
                            &root,
                            &impostor_path,
                            confirm,
                            force,
                            format,
                        );
                    }
                }
            }
        }
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
        // bn-3fhj: if the worktree dir was removed but registry/metadata
        // still tracks the workspace, `--force` should purge that residual
        // state so `ws list` stops advertising a MISSING workspace forever.
        if force && workspace_has_residual_state(&root, name) {
            return destroy_residual_state(&root, name);
        }
        println!(
            "Workspace '{name}' is already absent at {}.",
            path.display()
        );
        println!("No action needed.");
        return Ok(());
    }

    // bn-cm63: refuse to destroy a workspace that an in-flight merge has
    // frozen as a source. Without this, destroy deletes
    // `refs/manifold/head/<ws>` while the merge is mid-flight; the merge's
    // post-COMMIT `record_merge_operations` then re-bootstraps that oplog
    // head, leaking a dangling blob ref with no owning workspace. Serializing
    // here (refuse, not corrupt) keeps the ref lifecycle coherent. An
    // orphaned/stale merge-state must NOT block destroy forever — only a Live
    // merge does (mirrors prepare.rs / bn-2wyh staleness handling).
    guard_destroy_against_inflight_merge(&root, name)?;

    // bn-13rc: destroy pins recovery refs and writes a destroy record — shared
    // epoch/recovery state. Take the repo-level epoch lock for the mutation.
    // Acquired AFTER the in-flight-merge guard so a same-workspace live merge
    // still fast-fails with the "being merged" message (bn-cm63) rather than
    // blocking on the lock; unrelated concurrent mutations serialize here.
    let _epoch_lock = crate::epoch_lock::EpochLock::acquire(&root, "ws destroy")?;

    let backend = get_backend()?;
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;
    let status = backend
        .status(&ws_id)
        .map_err(|e| anyhow::anyhow!("Failed to inspect workspace state: {e}"))?;
    let base_epoch = status.base_epoch.to_epoch_id();
    let touched_count = compute_patchset(&path, &base_epoch)
        .map(|patch_set| patch_set.len())
        .map_err(|e| anyhow::anyhow!("Failed to inspect local changes before destroy: {e}"))?
        .max(status.dirty_count());

    // FP: crash after status check but before any destructive action.
    maw::fp!("FP_DESTROY_AFTER_STATUS")?;

    if touched_count > 0 && !force {
        // bn-voy5: build a structured DestroyRefusal (bn-c6l3 scaffold)
        // that carries lifecycle_state + recommended_action + bn-29fi
        // destroy-prevention cues in a single payload. Render to the
        // caller-requested format (text default, JSON via --format).
        //
        // commits_ahead is computed by opening the workspace repo and
        // counting commits between base_epoch and HEAD; non-fatal —
        // a failure here defaults to 0, which conservatively classifies
        // the workspace as DirtyUncommitted (still refuses, still
        // surfaces the safer "commit then merge" path).
        let commits_ahead = workspace_commits_ahead(&path, base_epoch.as_str()).unwrap_or(0);
        let refusal = DestroyRefusal::new(name, touched_count, commits_ahead, status.dirty_count());
        let fmt = OutputFormat::resolve(format);
        match fmt {
            OutputFormat::Json => {
                // Emit the JSON payload to stderr (matches the refusal
                // surface — `bail!` also lands on stderr via anyhow),
                // then bail with a short message. The integration test
                // (bn_c6l3_refusal_emits_machine_readable_json_under_format_flag)
                // parses the first `{...}` block out of stderr, so the
                // JSON must be valid + appear before the bail line.
                let json = refusal
                    .render_json()
                    .context("failed to serialize destroy refusal as JSON")?;
                eprintln!("{json}");
                bail!(
                    "Refusing destroy: '{name}' has {touched_count} unmerged change(s) \
                     (see JSON payload above for lifecycle_state + recommended_action)."
                );
            }
            OutputFormat::Text | OutputFormat::Pretty => {
                bail!("{}", refusal.render_text());
            }
        }
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

    let capture_result = if force {
        super::capture::capture_before_destroy(&path, name, status.base_epoch.oid())
            .map_err(|e| anyhow::anyhow!("Failed to capture workspace state before destroy: {e}"))?
    } else {
        None
    };

    // Determine final head for destroy record before we destroy
    let final_head =
        super::capture::resolve_head(&path).unwrap_or_else(|_| status.base_epoch.oid().clone());

    if let Err(e) = record_workspace_destroy_op(&root, &ws_id, &base_epoch) {
        tracing::warn!("Failed to record workspace destroy in history: {e}");
    }

    let artifact_path_result = super::destroy_record::write_destroy_record(
        &root,
        name,
        &base_epoch,
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

    // bn-1aey: capture this BEFORE deletion — once the workspace directory
    // is gone, std::env::current_dir() can itself start failing.
    let cwd_was_inside = super::cwd_is_inside(&path);

    backend
        .destroy(&ws_id)
        .map_err(|e| anyhow::anyhow!("Failed to destroy workspace: {e}"))?;

    // Clean up workspace metadata (best-effort; don't fail destroy if missing).
    let _ = metadata::delete(&root, name);

    // bn-20fp: resolve the output format once so the standalone-destroy success
    // path can emit a machine-readable object (with `cwd_destroyed`) instead of
    // always printing text. `format` was previously only consulted on the
    // refusal path.
    let fmt = OutputFormat::resolve(format);
    let text_mode = fmt != OutputFormat::Json;
    // Recovery refs pinned by this destroy (force + snapshot), for the JSON.
    let mut pinned_refs: Vec<String> = Vec::new();

    if force {
        if let Some(ref capture) = capture_result {
            pinned_refs.push(capture.pinned_ref.clone());
            if text_mode {
                let short_oid = &capture.commit_oid.as_str()[..12];
                println!("Snapshot saved: {short_oid}");
                println!("  State: abandoned-with-snapshot (lifecycle vocabulary, bn-29fi).");
                println!("  Recover (inspect):       maw ws recover {name}");
                // bn-29fi mergeback queue cue: when force-destroy left
                // committed work behind, the agent's next safe action is
                // recover-into-new-ws-then-merge. Naming the two-step
                // sequence eliminates the discovery cost that drives
                // `ws_recover_invoked` cluster turns.
                println!(
                    "  Recover + merge (full):  maw ws recover {name} --to {name}-restored \
                     && maw ws merge {name}-restored --into default --destroy"
                );
                println!("Workspace '{name}' destroyed.");
                // Emit full recovery surface contract
                super::capture::emit_recovery_surface(
                    name,
                    capture,
                    artifact_path_result.as_deref().ok(),
                    false, // no merge commit — standalone destroy
                    true,  // destroy operation succeeded
                );
            }
        } else if text_mode {
            println!("Workspace '{name}' destroyed. (nothing to snapshot)");
        }
    } else if text_mode {
        println!("Workspace '{name}' destroyed.");
    }

    if fmt == OutputFormat::Json {
        // bn-20fp (item 4): the machine-readable equivalent of the tail-visible
        // destroy-cwd warning — `cwd_destroyed` so a text-scraping orchestrator
        // that switched to JSON never loses the signal.
        let output = serde_json::json!({
            "status": "destroyed",
            "workspace": name,
            "cwd_destroyed": cwd_was_inside,
            "recovery": { "pinned_refs": pinned_refs },
            "message": format!("Workspace '{name}' destroyed."),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if cwd_was_inside {
        // bn-1aey / bn-20fp: in text mode this warning is already the final
        // line of output — keep it last so tail/grep-based scrapers see it.
        eprintln!(
            "note: your current directory was inside workspace '{name}' which was just \
             destroyed — cd back to the project root before running more commands."
        );
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
    let temp_backup = maw_core::model::layout::LayoutFlavor::detect_with_env(&root)
        .workspaces_dir(&root)
        .join(format!(".{name}-attach-backup"));
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

/// Destroy the impostor `.maw/workspaces/default` that can be created in a
/// consolidated-layout repo when the reserved-name guard was absent (bn-21qy).
///
/// The REAL default workspace is the repo root; this function only touches
/// the impostor directory which is strictly under `.maw/workspaces/`.
///
/// Safety invariant: caller must have verified `impostor_path` is under
/// `workspaces_dir` before calling this.
fn destroy_consolidated_impostor_default(
    root: &std::path::Path,
    impostor_path: &std::path::Path,
    confirm: bool,
    _force: bool,
    _format: Option<OutputFormat>,
) -> Result<()> {
    if confirm {
        use std::io::Write as _;
        println!(
            "About to remove the impostor workspace at {}",
            impostor_path.display()
        );
        println!("This will NOT touch the repo root (the real default workspace).");
        println!();
        print!("Continue? [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Remove the worktree registration via git worktree remove --force so git's
    // own worktree book-keeping is cleaned up properly.
    let remove_result = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(impostor_path)
        .current_dir(root)
        .output();

    match remove_result {
        Ok(out) if out.status.success() => {}
        _ => {
            // If git worktree remove fails (e.g. the worktree wasn't properly
            // registered), fall back to a filesystem removal + prune.
            if impostor_path.exists() {
                std::fs::remove_dir_all(impostor_path).with_context(|| {
                    format!(
                        "Failed to remove impostor directory {}",
                        impostor_path.display()
                    )
                })?;
            }
            let _ = Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(root)
                .output();
        }
    }

    // Clean up workspace metadata (best-effort).
    let _ = metadata::delete(root, DEFAULT_WORKSPACE);

    println!(
        "Removed impostor workspace 'default' at {}.",
        impostor_path.display()
    );
    println!(
        "The real default workspace (repo root at {}) is untouched.",
        root.display()
    );
    Ok(())
}

/// Check whether stale registry/metadata still references a workspace whose
/// worktree dir is gone (bn-3fhj). Returns true if any of:
///   - `.manifold/workspaces/<name>.toml` exists
///   - any `refs/manifold/.../<name>` ref exists
///   - the git worktree admin dir `<repo>/worktrees/<name>` exists
fn workspace_has_residual_state(root: &std::path::Path, name: &str) -> bool {
    let meta_path = metadata::metadata_path(root, name);
    if meta_path.exists() {
        return true;
    }
    for ref_name in manifold_refs::workspace_owned_refs(name) {
        if matches!(manifold_refs::read_ref(root, &ref_name), Ok(Some(_))) {
            return true;
        }
    }
    let worktree_admin = root.join("repo.git").join("worktrees").join(name);
    if worktree_admin.exists() {
        return true;
    }
    let worktree_admin_alt = root.join(".git").join("worktrees").join(name);
    worktree_admin_alt.exists()
}

/// Purge registry/metadata residue for a workspace whose worktree dir is
/// already gone (bn-3fhj). Used by `maw ws destroy --force <name>` when the
/// normal destroy path's `path.exists()` precondition fails.
fn destroy_residual_state(root: &std::path::Path, name: &str) -> Result<()> {
    // Run `git worktree prune` to drop the stale worktree admin dir for the
    // missing worktree. We run it from the repo root so git finds .git/repo.git.
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(root)
        .output();

    // Best-effort cleanup of refs owned by this workspace.
    for ref_name in manifold_refs::workspace_owned_refs(name) {
        let _ = manifold_refs::delete_ref(root, &ref_name);
    }

    // Best-effort cleanup of metadata.
    let _ = metadata::delete(root, name);

    println!("Workspace '{name}' was missing on disk; cleaned up registry and metadata.");
    Ok(())
}
