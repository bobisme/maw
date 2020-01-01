//! Level 2 Git transport: push/pull Manifold state via `refs/manifold/*`.
//!
//! This module implements §8 "Level 2: Git as Transport", enabling multi-machine
//! Manifold collaboration without bespoke servers. All Manifold metadata (op logs,
//! workspace heads, epoch pointers) is stored as Git objects and can be synced
//! via standard `git push/fetch` under the `refs/manifold/*` namespace.
//!
//! # Ref layout
//!
//! ```text
//! refs/manifold/
//! ├── epoch/current        ← current epoch OID
//! ├── head/<workspace>     ← per-workspace op log head (latest blob OID)
//! └── ws/<workspace>       ← Level 1 materialized workspace state
//! ```
//!
//! # Push
//!
//! `push_manifold_refs(root, remote)` runs:
//! ```text
//! git push <remote> 'refs/manifold/*:refs/manifold/*'
//! ```
//!
//! # Pull
//!
//! `pull_manifold_refs(root, remote)` runs a two-phase fetch:
//!
//! 1. Fetch remote refs into `refs/manifold/remote/*` staging area:
//!    ```text
//!    git fetch <remote> 'refs/manifold/*:refs/manifold/remote/*'
//!    ```
//!
//! 2. Merge each ref category:
//!    - **epoch/current**: fast-forward only; warn on divergence.
//!    - **head/<workspace>**: merge divergent chains by creating a synthetic
//!      merge operation whose `parent_ids` include both local and remote heads.
//!    - **ws/<workspace>**: fast-forward only (Level 1 refs are derived data).
//!
//! # Security
//!
//! Before applying any fetched operation, [`validate_remote_operation`] checks:
//! - The blob OID is reachable from the repo (exists in object store).
//! - The JSON deserializes to a valid [`Operation`] (schema validation).
//! - All `parent_ids` reference OIDs that exist in the object store.
//! - The `workspace_id` contains no path-traversal characters.
//!
//! # Round-trip guarantee
//!
//! For deterministic inputs, `push_manifold_refs` followed by
//! `pull_manifold_refs` on a second repo produces equivalent local state:
//! the epoch ref and all head refs match, and the op log DAGs are isomorphic.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::model::types::{GitOid, WorkspaceId};
use crate::oplog::types::{OpPayload, Operation};
use crate::oplog::write::write_operation_blob;
use crate::refs;
use crate::workspace::repo_root;

// ---------------------------------------------------------------------------
// CLI argument types
// ---------------------------------------------------------------------------

/// Arguments for `maw pull`.
#[derive(Args, Debug)]
pub struct PullArgs {
    /// Remote to pull from (default: "origin").
    #[arg(default_value = "origin")]
    pub remote: String,

    /// Pull refs/manifold/* state from the remote.
    ///
    /// Fetches all Manifold metadata (op logs, workspace heads, epoch pointer)
    /// and merges remote op log heads into the local op log DAG.
    /// Enables multi-machine Manifold collaboration without bespoke servers.
    #[arg(long)]
    pub manifold: bool,

    /// Print what would be done without modifying any refs.
    #[arg(long)]
    pub dry_run: bool,
}

/// Additional push arguments for Level 2 transport.
///
/// Attached to the existing `maw push` command via `#[command(flatten)]`.
#[derive(Args, Debug, Default)]
pub struct ManifoldPushArgs {
    /// Push refs/manifold/* state to the remote.
    ///
    /// After pushing the branch, also pushes all Manifold metadata
    /// (op logs, workspace heads, epoch pointer) to `refs/manifold/*`
    /// on the remote. Use this for multi-machine Manifold collaboration.
    #[arg(long)]
    pub manifold: bool,
}

// ---------------------------------------------------------------------------
// Push: refs/manifold/* → remote
// ---------------------------------------------------------------------------

/// Push all `refs/manifold/*` refs to `remote`.
///
/// Runs `git push <remote> 'refs/manifold/*:refs/manifold/*'`.
/// This is a force-capable push for manifest refs (op logs are append-only
/// DAGs; the remote can only gain new objects, never lose them, so
/// non-fast-forward manifold pushes are always safe to force).
///
/// # Arguments
/// * `root`   — repository root.
/// * `remote` — remote name (e.g., `"origin"`).
/// * `dry_run`— if true, print what would be done without running git push.
///
/// # Errors
/// Returns an error if `git push` fails.
pub fn push_manifold_refs(root: &Path, remote: &str, dry_run: bool) -> Result<()> {
    // Targeted refspecs that exclude the local-only staging area
    // (refs/manifold/remote/*) and separate epoch from force-push logic.
    //
    // The epoch ref is a git commit pointer — force-pushing it could regress
    // the remote epoch if the local repo hasn't pulled recent advances.
    // Push it without --force so the remote rejects a regression.
    //
    // Op log head refs (refs/manifold/head/*) and workspace state refs
    // (refs/manifold/ws/*) point to blob OIDs; git has no ancestry concept
    // for blobs, so --force is required for updates.
    let epoch_refspec = "refs/manifold/epoch/current:refs/manifold/epoch/current";
    let head_refspec = "refs/manifold/head/*:refs/manifold/head/*";
    let ws_refspec = "refs/manifold/ws/*:refs/manifold/ws/*";

    if dry_run {
        println!("[dry-run] git push {remote} '{epoch_refspec}' (no --force)");
        println!("[dry-run] git push --force {remote} '{head_refspec}' '{ws_refspec}'");
        return Ok(());
    }

    println!("Pushing refs/manifold/* to {remote}...");

    // Clean up any leftover staging refs before checking/pushing.
    cleanup_staging(root);

    // Check whether there are any refs/manifold/* refs to push.
    let refs_exist = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/manifold/"])
        .current_dir(root)
        .output()
        .context("Failed to list refs/manifold/ refs")?;

    if refs_exist.stdout.is_empty() {
        println!("  No refs/manifold/* refs found — nothing to push.");
        println!("  Hint: run `maw init` to initialize Manifold metadata.");
        return Ok(());
    }

    // Step 1: Push epoch ref without --force to prevent remote regression.
    let epoch_push = Command::new("git")
        .args(["push", remote, epoch_refspec])
        .current_dir(root)
        .output()
        .context("Failed to run git push for epoch ref")?;

    if !epoch_push.status.success() {
        let stderr = String::from_utf8_lossy(&epoch_push.stderr);
        let stderr_trimmed = stderr.trim();

        if stderr_trimmed.contains("rejected")
            || stderr_trimmed.contains("non-fast-forward")
            || stderr_trimmed.contains("fetch first")
        {
            bail!(
                "Manifold epoch push rejected (non-fast-forward).\n  \
                 Remote epoch is ahead of local — pull first:\n    \
                 maw pull --manifold {remote}\n  \
                 Then retry: maw push --manifold"
            );
        }

        // "does not match any" means the local epoch ref doesn't exist yet — OK.
        if !stderr_trimmed.contains("does not match any") && !stderr_trimmed.contains("src refspec")
        {
            bail!("git push epoch ref failed: {stderr_trimmed}");
        }
    }

    // Step 2: Push op log heads and workspace state refs with --force.
    // These refs point to blob OIDs where ancestry checks don't apply.
    let force_push = Command::new("git")
        .args(["push", "--force", remote, head_refspec, ws_refspec])
        .current_dir(root)
        .output()
        .context("Failed to run git push for manifold head/ws refs")?;

    if !force_push.status.success() {
        let stderr = String::from_utf8_lossy(&force_push.stderr);
        let stderr_trimmed = stderr.trim();

        // "does not match any" is OK — means no head/* or ws/* refs exist yet.
        if !stderr_trimmed.contains("does not match any") && !stderr_trimmed.contains("src refspec")
        {
            bail!("git push refs/manifold/head/* refs/manifold/ws/* failed: {stderr_trimmed}");
        }
    }

    // Parse and print what was pushed from both operations.
    let mut pushed_count = 0;
    for output in [&epoch_push, &force_push] {
        let stderr_out = String::from_utf8_lossy(&output.stderr);
        for line in stderr_out.lines() {
            if line.contains("refs/manifold/") {
                pushed_count += 1;
                println!("    {}", line.trim());
            }
        }
    }

    if pushed_count == 0 {
        println!("  refs/manifold/* already up to date on {remote}.");
    } else {
        println!("  Pushed {pushed_count} ref(s) to {remote}.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pull: remote → local, with op log head merging
// ---------------------------------------------------------------------------

/// Fetch and integrate `refs/manifold/*` from `remote` into the local repo.
///
/// Two-phase:
/// 1. Fetch into staging area `refs/manifold/remote/*`.
/// 2. For each ref category, merge or fast-forward into local `refs/manifold/*`.
///
/// # Arguments
/// * `root`    — repository root.
/// * `remote`  — remote name (e.g., `"origin"`).
/// * `dry_run` — if true, print what would be done without modifying refs.
///
/// # Returns
/// A [`PullSummary`] describing what was merged, fast-forwarded, or skipped.
///
/// # Errors
/// Returns an error if `git fetch` fails or if security validation of
/// remote operations fails.
pub fn pull_manifold_refs(root: &Path, remote: &str, dry_run: bool) -> Result<PullSummary> {
    let mut summary = PullSummary::default();

    // Always start from a clean staging area so stale refs from an interrupted
    // previous pull cannot be mistaken for freshly-fetched remote state.
    if !dry_run {
        cleanup_staging(root);
    }

    // Phase 1: Fetch remote manifold refs into staging area.
    fetch_into_staging(root, remote, dry_run)?;

    // Phase 2: Merge staged refs into local refs.

    // 2a: Epoch
    let epoch_result = merge_epoch_ref(root, dry_run)?;
    summary.epoch = epoch_result;

    // 2b: Workspace head refs (op log heads)
    let head_results = merge_head_refs(root, dry_run)?;
    summary.heads = head_results;

    // 2c: Level 1 workspace state refs (derived data — fast-forward only)
    let ws_results = merge_ws_refs(root, dry_run)?;
    summary.ws_state = ws_results;

    // Phase 3: Clean up staging area.
    if !dry_run {
        cleanup_staging(root);
    }

    Ok(summary)
}

/// Pull result summary.
#[derive(Debug, Default)]
pub struct PullSummary {
    /// What happened to `refs/manifold/epoch/current`.
    pub epoch: RefMergeResult,
    /// Results for each workspace head ref (`refs/manifold/head/<ws>`).
    pub heads: Vec<(String, RefMergeResult)>,
    /// Results for each workspace state ref (`refs/manifold/ws/<ws>`).
    pub ws_state: Vec<(String, RefMergeResult)>,
}

impl PullSummary {
    /// Print a human-readable summary.
    pub fn print(&self) {
        println!("Pull summary:");
        println!("  epoch: {}", self.epoch.describe());
        if self.heads.is_empty() {
            println!("  workspace heads: (none)");
        } else {
            for (name, result) in &self.heads {
                println!("  head/{name}: {}", result.describe());
            }
        }
        if !self.ws_state.is_empty() {
            for (name, result) in &self.ws_state {
                println!("  ws/{name}: {}", result.describe());
            }
        }
    }

    /// Returns true if any merge operations were created (divergent heads merged).
    pub fn has_merges(&self) -> bool {
        self.heads
            .iter()
            .any(|(_, r)| matches!(r, RefMergeResult::Merged))
    }
}

/// Outcome for a single ref integration.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum RefMergeResult {
    /// No remote ref found; local unchanged.
    #[default]
    NoRemote,
    /// No local ref; fast-forwarded to remote value.
    NewFromRemote,
    /// Local and remote were identical; no-op.
    UpToDate,
    /// Remote was an ancestor of local (local is newer); no-op.
    LocalAhead,
    /// Local was an ancestor of remote; fast-forwarded to remote.
    FastForward,
    /// Local and remote diverged; created a merge operation.
    Merged,
    /// Divergence detected but could not be auto-merged (epoch only).
    DivergenceWarned { reason: String },
    /// Security validation failed for remote ops; update skipped.
    SecurityRejected { reason: String },
}

impl RefMergeResult {
    fn describe(&self) -> String {
        match self {
            Self::NoRemote => "no remote ref (skipped)".to_string(),
            Self::NewFromRemote => "new (fast-forwarded from remote)".to_string(),
            Self::UpToDate => "up to date".to_string(),
            Self::LocalAhead => "local ahead (no-op)".to_string(),
            Self::FastForward => "fast-forwarded to remote".to_string(),
            Self::Merged => "diverged — created merge op".to_string(),
            Self::DivergenceWarned { reason } => {
                format!("WARNING: diverged — {reason}")
            }
            Self::SecurityRejected { reason } => {
                format!("SECURITY: rejected — {reason}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Fetch into staging
// ---------------------------------------------------------------------------

fn fetch_into_staging(root: &Path, remote: &str, dry_run: bool) -> Result<()> {
    // Fetch all refs/manifold/* into refs/manifold/remote/* staging area.
    // This avoids immediately overwriting local state during fetch.
    let refspec = "refs/manifold/*:refs/manifold/remote/*";

    if dry_run {
        println!("[dry-run] git fetch {remote} '{refspec}'");
        return Ok(());
    }

    println!("Fetching refs/manifold/* from {remote}...");

    let fetch = Command::new("git")
        .args(["fetch", remote, refspec])
        .current_dir(root)
        .output()
        .context("Failed to run git fetch for manifold refs")?;

    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr);
        let stderr_trimmed = stderr.trim();

        // "Couldn't find remote ref" means the remote has no manifold refs yet.
        if stderr_trimmed.contains("couldn't find remote ref")
            || stderr_trimmed.contains("no such ref was fetched")
        {
            println!("  Remote has no refs/manifold/* yet — nothing to pull.");
            println!("  Push first: maw push --manifold {remote}");
            return Ok(());
        }

        bail!("git fetch refs/manifold/* failed: {stderr_trimmed}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2a: Epoch ref merging
// ---------------------------------------------------------------------------

fn merge_epoch_ref(root: &Path, dry_run: bool) -> Result<RefMergeResult> {
    let local_ref = refs::EPOCH_CURRENT;
    let remote_ref = "refs/manifold/remote/epoch/current";

    let local_oid = refs::read_ref(root, local_ref).context("Reading local epoch")?;
    let remote_oid = refs::read_ref(root, remote_ref).context("Reading remote epoch")?;

    match (local_oid, remote_oid) {
        (_, None) => Ok(RefMergeResult::NoRemote),

        (None, Some(remote)) => {
            // No local epoch: set from remote.
            if dry_run {
                println!(
                    "[dry-run] epoch: would fast-forward to {}",
                    &remote.as_str()[..12]
                );
            } else {
                refs::write_ref(root, local_ref, &remote).context("Writing epoch from remote")?;
            }
            Ok(RefMergeResult::NewFromRemote)
        }

        (Some(local), Some(remote)) if local == remote => Ok(RefMergeResult::UpToDate),

        (Some(local), Some(remote)) => {
            // Compare ancestry.
            let relation = git_ancestry_relation(root, &local, &remote)?;
            match relation {
                AncestryRelation::LocalAheadOrEqual => Ok(RefMergeResult::LocalAhead),
                AncestryRelation::RemoteAhead => {
                    // Fast-forward: remote epoch is a descendant of local.
                    if dry_run {
                        println!(
                            "[dry-run] epoch: would fast-forward {} → {}",
                            &local.as_str()[..12],
                            &remote.as_str()[..12]
                        );
                    } else {
                        refs::write_ref(root, local_ref, &remote)
                            .context("Fast-forwarding epoch to remote")?;
                    }
                    Ok(RefMergeResult::FastForward)
                }
                AncestryRelation::Diverged => {
                    // Epoch divergence: cannot auto-merge. Warn operator.
                    let reason = format!(
                        "local epoch {} and remote epoch {} have diverged. \
                         Manual recovery needed: determine which epoch is correct, \
                         then run: git update-ref refs/manifold/epoch/current <correct-oid>",
                        &local.as_str()[..12],
                        &remote.as_str()[..12]
                    );
                    tracing::warn!("epoch divergence detected");
                    eprintln!("  {reason}");
                    Ok(RefMergeResult::DivergenceWarned { reason })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2b: Workspace head merging
// ---------------------------------------------------------------------------

fn merge_head_refs(root: &Path, dry_run: bool) -> Result<Vec<(String, RefMergeResult)>> {
    // List all remote staging heads.
    let remote_heads = list_refs_with_prefix(root, "refs/manifold/remote/head/")?;
    let mut results = Vec::new();

    for remote_ref in &remote_heads {
        // Extract workspace name: refs/manifold/remote/head/<ws> → <ws>
        let ws_name = remote_ref
            .strip_prefix("refs/manifold/remote/head/")
            .unwrap_or(remote_ref);

        // Security: validate workspace name before proceeding.
        if let Err(reason) = validate_workspace_name(ws_name) {
            results.push((
                ws_name.to_string(),
                RefMergeResult::SecurityRejected { reason },
            ));
            continue;
        }

        let local_ref = refs::workspace_head_ref(ws_name);
        let remote_oid = refs::read_ref(root, remote_ref)
            .with_context(|| format!("Reading remote head for {ws_name}"))?;

        let Some(remote_oid) = remote_oid else {
            results.push((ws_name.to_string(), RefMergeResult::NoRemote));
            continue;
        };

        // Security: validate the remote op blob before applying.
        if let Err(reason) = validate_remote_op_blob(root, &remote_oid) {
            results.push((
                ws_name.to_string(),
                RefMergeResult::SecurityRejected { reason },
            ));
            continue;
        }

        let local_oid = refs::read_ref(root, &local_ref)
            .with_context(|| format!("Reading local head for {ws_name}"))?;

        let result = match local_oid {
            None => {
                // No local head: adopt remote.
                if dry_run {
                    println!(
                        "[dry-run] head/{ws_name}: would set to {} (new from remote)",
                        &remote_oid.as_str()[..12]
                    );
                } else {
                    refs::write_ref(root, &local_ref, &remote_oid)
                        .with_context(|| format!("Setting head for {ws_name} from remote"))?;
                }
                RefMergeResult::NewFromRemote
            }

            Some(ref local) if *local == remote_oid => RefMergeResult::UpToDate,

            Some(local) => {
                let relation = oplog_ancestry_relation(root, &local, &remote_oid)?;
                match relation {
                    AncestryRelation::LocalAheadOrEqual => RefMergeResult::LocalAhead,

                    AncestryRelation::RemoteAhead => {
                        // Fast-forward: remote is a descendant of local.
                        if dry_run {
                            println!(
                                "[dry-run] head/{ws_name}: would fast-forward {} → {}",
                                &local.as_str()[..12],
                                &remote_oid.as_str()[..12]
                            );
                        } else {
                            refs::write_ref(root, &local_ref, &remote_oid)
                                .with_context(|| format!("Fast-forwarding head for {ws_name}"))?;
                        }
                        RefMergeResult::FastForward
                    }

                    AncestryRelation::Diverged => {
                        // Diverged chains: create a synthetic merge op with both as parents.
                        if dry_run {
                            println!(
                                "[dry-run] head/{ws_name}: diverged — would create merge op \
                                 (local={}, remote={})",
                                &local.as_str()[..12],
                                &remote_oid.as_str()[..12]
                            );
                        } else {
                            let ws_id = WorkspaceId::new(ws_name).map_err(|e| {
                                anyhow::anyhow!("Invalid workspace id {ws_name}: {e}")
                            })?;
                            create_transport_merge_op(
                                root,
                                &ws_id,
                                &local,
                                &remote_oid,
                                &local_ref,
                            )?;
                        }
                        RefMergeResult::Merged
                    }
                }
            }
        };

        results.push((ws_name.to_string(), result));
    }

    Ok(results)
}

/// Create a synthetic merge operation joining two divergent op log chains.
///
/// The merge op is stored as a git blob with both local and remote heads
/// as `parent_ids`. The workspace head ref is then updated to point to
/// the new merge op blob OID.
///
/// After this, the op log DAG for the workspace has the shape:
///
/// ```text
///   ... local_chain ... → local_head ─┐
///                                      ├─ merge_op ← new head
///   ... remote_chain ... → remote_head ┘
/// ```
fn create_transport_merge_op(
    root: &Path,
    ws_id: &WorkspaceId,
    local_head: &GitOid,
    remote_head: &GitOid,
    head_ref: &str,
) -> Result<()> {
    let timestamp = chrono_now_or_fallback();

    // Annotate payload records the transport merge provenance.
    let mut data = BTreeMap::new();
    data.insert(
        "local_head".to_string(),
        serde_json::Value::String(local_head.as_str().to_string()),
    );
    data.insert(
        "remote_head".to_string(),
        serde_json::Value::String(remote_head.as_str().to_string()),
    );
    data.insert(
        "merge_kind".to_string(),
        serde_json::Value::String("transport-pull".to_string()),
    );

    let merge_op = Operation {
        parent_ids: vec![local_head.clone(), remote_head.clone()],
        workspace_id: ws_id.clone(),
        timestamp,
        payload: OpPayload::Annotate {
            key: "transport-merge".to_string(),
            data,
        },
    };

    // Write the merge op as a git blob.
    let merge_oid = write_operation_blob(root, &merge_op)
        .map_err(|e| anyhow::anyhow!("Failed to write transport merge op: {e}"))?;

    // Update the head ref unconditionally (we own local now — no CAS needed
    // because this is the pull-side integration step, not a concurrent write).
    refs::write_ref(root, head_ref, &merge_oid)
        .context("Updating head ref to transport merge op")?;

    println!(
        "  Merged divergent op log chains for {} → {}",
        ws_id.as_str(),
        &merge_oid.as_str()[..12]
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2c: Level 1 workspace state refs (fast-forward only)
// ---------------------------------------------------------------------------

fn merge_ws_refs(root: &Path, dry_run: bool) -> Result<Vec<(String, RefMergeResult)>> {
    let remote_refs = list_refs_with_prefix(root, "refs/manifold/remote/ws/")?;
    let mut results = Vec::new();

    for remote_ref in &remote_refs {
        let ws_name = remote_ref
            .strip_prefix("refs/manifold/remote/ws/")
            .unwrap_or(remote_ref);

        if let Err(reason) = validate_workspace_name(ws_name) {
            results.push((
                ws_name.to_string(),
                RefMergeResult::SecurityRejected { reason },
            ));
            continue;
        }

        let local_ref = refs::workspace_state_ref(ws_name);
        let remote_oid = refs::read_ref(root, remote_ref)
            .with_context(|| format!("Reading remote ws state for {ws_name}"))?;

        let Some(remote_oid) = remote_oid else {
            results.push((ws_name.to_string(), RefMergeResult::NoRemote));
            continue;
        };

        let local_oid = refs::read_ref(root, &local_ref)
            .with_context(|| format!("Reading local ws state for {ws_name}"))?;

        let result = match local_oid {
            None => {
                if dry_run {
                    println!("[dry-run] ws/{ws_name}: would set from remote");
                } else {
                    refs::write_ref(root, &local_ref, &remote_oid)
                        .with_context(|| format!("Setting ws state for {ws_name} from remote"))?;
                }
                RefMergeResult::NewFromRemote
            }
            Some(ref local) if *local == remote_oid => RefMergeResult::UpToDate,
            Some(local) => {
                // Level 1 ws refs are derived data — fast-forward only.
                // If remote is ahead, accept it. If diverged, warn.
                let relation = git_ancestry_relation(root, &local, &remote_oid)?;
                match relation {
                    AncestryRelation::LocalAheadOrEqual => RefMergeResult::LocalAhead,
                    AncestryRelation::RemoteAhead => {
                        if dry_run {
                            println!("[dry-run] ws/{ws_name}: would fast-forward");
                        } else {
                            refs::write_ref(root, &local_ref, &remote_oid).with_context(|| {
                                format!("Fast-forwarding ws state for {ws_name}")
                            })?;
                        }
                        RefMergeResult::FastForward
                    }
                    AncestryRelation::Diverged => {
                        // Level 1 ws refs diverge when the same workspace was
                        // materialized on two machines. The local view wins;
                        // the remote may be stale. Log a warning.
                        let reason = format!(
                            "Level 1 ws/{ws_name} diverged; local wins. \
                             Remote state: {}",
                            &remote_oid.as_str()[..12]
                        );
                        RefMergeResult::DivergenceWarned { reason }
                    }
                }
            }
        };

        results.push((ws_name.to_string(), result));
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Phase 3: Cleanup staging area
// ---------------------------------------------------------------------------

fn cleanup_staging(root: &Path) {
    // Delete all refs/manifold/remote/* staging refs.
    let staging_refs = list_refs_with_prefix(root, "refs/manifold/remote/").unwrap_or_default();

    for r in &staging_refs {
        // Best-effort cleanup — ignore errors.
        let _ = refs::delete_ref(root, r);
    }
}

// ---------------------------------------------------------------------------
// Security validation
// ---------------------------------------------------------------------------

/// Validate a workspace name for security.
///
/// Accepts only names that satisfy `WorkspaceId` validation.
/// This keeps transport-layer refs aligned with workspace naming rules used
/// across the rest of the system.
///
/// # Errors
/// Returns a human-readable reason string if validation fails.
pub fn validate_workspace_name(name: &str) -> Result<(), String> {
    WorkspaceId::new(name)
        .map(|_| ())
        .map_err(|e| format!("invalid workspace name: {e}"))
}

/// Validate a remote op log blob before applying it locally.
///
/// Checks:
/// 1. The OID exists in the local object store (was transferred by git fetch).
/// 2. The blob deserializes as a valid [`Operation`] (schema validation).
/// 3. All `parent_ids` in the operation reference OIDs that exist locally.
/// 4. The `workspace_id` in the operation passes [`validate_workspace_name`].
///
/// # Errors
/// Returns a human-readable reason string if validation fails.
pub fn validate_remote_op_blob(root: &Path, oid: &GitOid) -> Result<(), String> {
    // 1. Verify the OID exists in the object store.
    let cat = Command::new("git")
        .args(["cat-file", "-e", oid.as_str()])
        .current_dir(root)
        .output()
        .map_err(|e| format!("Failed to spawn git: {e}"))?;

    if !cat.status.success() {
        return Err(format!(
            "OID {} does not exist in local object store — fetch may be incomplete",
            oid.as_str()
        ));
    }

    // 2. Deserialize to Operation for schema validation.
    let op = crate::oplog::read::read_operation(root, oid)
        .map_err(|e| format!("Failed to read/deserialize op {}: {e}", oid.as_str()))?;

    // 3. Validate workspace_id.
    validate_workspace_name(op.workspace_id.as_str())
        .map_err(|e| format!("workspace_id validation failed: {e}"))?;

    // 4. Validate parent OIDs exist in the object store.
    for parent in &op.parent_ids {
        let parent_check = Command::new("git")
            .args(["cat-file", "-e", parent.as_str()])
            .current_dir(root)
            .output()
            .map_err(|e| format!("Failed to spawn git: {e}"))?;

        if !parent_check.status.success() {
            return Err(format!(
                "parent OID {} referenced by op {} does not exist locally — \
                 op log may be incomplete or tampered",
                parent.as_str(),
                oid.as_str()
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Git ancestry helpers
// ---------------------------------------------------------------------------

/// Determine ancestry relation between two op-log heads.
///
/// Unlike Git commit refs, `refs/manifold/head/<ws>` point to operation blobs.
/// We determine ancestry by walking `parent_ids` in the operation DAG.
fn oplog_ancestry_relation(
    root: &Path,
    local: &GitOid,
    remote: &GitOid,
) -> Result<AncestryRelation> {
    if oplog_is_ancestor(root, local, remote)? {
        return Ok(AncestryRelation::RemoteAhead);
    }
    if oplog_is_ancestor(root, remote, local)? {
        return Ok(AncestryRelation::LocalAheadOrEqual);
    }
    Ok(AncestryRelation::Diverged)
}

/// Returns true if `ancestor` is reachable from `descendant` by following
/// operation `parent_ids`.
fn oplog_is_ancestor(root: &Path, ancestor: &GitOid, descendant: &GitOid) -> Result<bool> {
    let mut stack = vec![descendant.clone()];
    let mut visited = HashSet::<GitOid>::new();

    while let Some(current) = stack.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }

        if &current == ancestor {
            return Ok(true);
        }

        let op = crate::oplog::read::read_operation(root, &current)
            .with_context(|| format!("Reading op {} while checking ancestry", current.as_str()))?;

        for parent in op.parent_ids {
            if !visited.contains(&parent) {
                stack.push(parent);
            }
        }
    }

    Ok(false)
}

/// Relationship between local and remote OIDs in the git DAG.
#[derive(Debug, PartialEq, Eq)]
enum AncestryRelation {
    /// Local is an ancestor of or equal to remote (remote is newer or same).
    RemoteAhead,
    /// Remote is an ancestor of local (local is newer).
    LocalAheadOrEqual,
    /// Neither is an ancestor of the other.
    Diverged,
}

/// Determine the ancestry relationship between `local` and `remote` OIDs.
///
/// Uses `git merge-base --is-ancestor` to check ancestry in both directions.
fn git_ancestry_relation(root: &Path, local: &GitOid, remote: &GitOid) -> Result<AncestryRelation> {
    // Check: is local an ancestor of remote? (i.e., remote is ahead or equal)
    let local_is_ancestor = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            local.as_str(),
            remote.as_str(),
        ])
        .current_dir(root)
        .status()
        .context("Failed to check git ancestry (local→remote)")?;

    if local_is_ancestor.success() {
        return Ok(AncestryRelation::RemoteAhead);
    }

    // Check: is remote an ancestor of local? (i.e., local is ahead or equal)
    let remote_is_ancestor = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            remote.as_str(),
            local.as_str(),
        ])
        .current_dir(root)
        .status()
        .context("Failed to check git ancestry (remote→local)")?;

    if remote_is_ancestor.success() {
        return Ok(AncestryRelation::LocalAheadOrEqual);
    }

    Ok(AncestryRelation::Diverged)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// List all git refs under `prefix`, returning the full ref names.
fn list_refs_with_prefix(root: &Path, prefix: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", prefix])
        .current_dir(root)
        .output()
        .context("Failed to list refs")?;

    if !output.status.success() {
        return Ok(vec![]); // No refs is not an error.
    }

    let refs = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(refs)
}

/// Return the current UTC timestamp in ISO 8601 format.
///
/// Falls back to a fixed string if the system clock is unavailable.
fn chrono_now_or_fallback() -> String {
    // Use std::time to get the current Unix timestamp.
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Convert to a simple ISO 8601 format without the chrono crate.
    // Formula: days since epoch = secs / 86400, etc.
    let s = secs;
    let sec = s % 60;
    let min = (s / 60) % 60;
    let hour = (s / 3600) % 24;
    let days = s / 86400;

    // Compute year/month/day from days since epoch (1970-01-01).
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
///
/// Simplified Gregorian calendar calculation.
const fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Offset from 1970-01-01 to 0001-03-01 (the reference point for this algorithm).
    // We use the algorithm from https://www.researchgate.net/publication/316558298
    // (simplified for our purposes).
    let z = days + 719_468; // offset to 0000-03-01
    let era = z / 146_097; // 400-year era
    let doe = z - era * 146_097; // day of era
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year (Mar-based)
    let mp = (5 * doy + 2) / 153; // March-based month
    let d = doy - (153 * mp + 2) / 5 + 1; // day
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// CLI command handler
// ---------------------------------------------------------------------------

/// Run `maw pull`.
pub fn run_pull(args: &PullArgs) -> Result<()> {
    let root = repo_root()?;

    // If --manifold was not passed, default to doing a manifold pull.
    // (For now, `maw pull` only supports --manifold mode.)
    if !args.manifold {
        bail!(
            "maw pull requires the --manifold flag to pull Manifold state.\n  \
             Usage: maw pull --manifold [remote]\n  \
             Example: maw pull --manifold origin"
        );
    }

    let summary = pull_manifold_refs(&root, &args.remote, args.dry_run)?;
    summary.print();

    if summary.has_merges() {
        println!();
        println!(
            "Op log heads were merged from remote. \
             Run `maw ws status` to see the current workspace state."
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn setup_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        StdCommand::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        // Need at least one commit for git merge-base to work.
        fs::write(root.join("README.md"), "# Test\n").unwrap();
        StdCommand::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        dir
    }

    fn make_commit(root: &Path, content: &str) -> GitOid {
        let file = root.join(format!("f{}.txt", content.len()));
        fs::write(&file, content).unwrap();
        StdCommand::new("git")
            .args(["add", file.to_str().unwrap()])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", content])
            .current_dir(root)
            .output()
            .unwrap();
        let out = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        GitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    }

    fn write_blob(root: &Path, content: &[u8]) -> GitOid {
        use std::io::Write;
        let mut child = StdCommand::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(content).unwrap();
        let out = child.wait_with_output().unwrap();
        GitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    }

    // -----------------------------------------------------------------------
    // validate_workspace_name
    // -----------------------------------------------------------------------

    #[test]
    fn validate_ws_name_valid() {
        assert!(validate_workspace_name("agent-1").is_ok());
        assert!(validate_workspace_name("feature-auth").is_ok());
        assert!(validate_workspace_name("default").is_ok());
        assert!(validate_workspace_name("noble-forest").is_ok());
    }

    #[test]
    fn validate_ws_name_empty() {
        let r = validate_workspace_name("");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn validate_ws_name_path_separator() {
        assert!(validate_workspace_name("a/b").is_err());
    }

    #[test]
    fn validate_ws_name_backslash() {
        assert!(validate_workspace_name("a\\b").is_err());
    }

    #[test]
    fn validate_ws_name_null_byte() {
        assert!(validate_workspace_name("a\0b").is_err());
    }

    #[test]
    fn validate_ws_name_starts_with_dot() {
        assert!(validate_workspace_name(".hidden").is_err());
    }

    #[test]
    fn validate_ws_name_double_dot() {
        assert!(validate_workspace_name("a..b").is_err());
    }

    #[test]
    fn validate_ws_name_rejects_underscore() {
        assert!(validate_workspace_name("noble_forest").is_err());
    }

    // -----------------------------------------------------------------------
    // validate_remote_op_blob
    // -----------------------------------------------------------------------

    #[test]
    fn validate_remote_op_blob_valid() {
        let dir = setup_repo();
        let root = dir.path();
        let ws_id = WorkspaceId::new("agent-1").unwrap();

        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".to_string(),
            payload: OpPayload::Create {
                epoch: crate::model::types::EpochId::new(&"a".repeat(40)).unwrap(),
            },
        };

        let oid = write_operation_blob(root, &op).unwrap();
        assert!(validate_remote_op_blob(root, &oid).is_ok());
    }

    #[test]
    fn validate_remote_op_blob_nonexistent_oid() {
        let dir = setup_repo();
        let root = dir.path();
        let fake_oid = GitOid::new(&"f".repeat(40)).unwrap();

        let r = validate_remote_op_blob(root, &fake_oid);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn validate_remote_op_blob_invalid_workspace_name() {
        let dir = setup_repo();
        let root = dir.path();

        // Write a raw blob with a workspace_id containing path traversal.
        // We need to construct raw JSON manually.
        let bad_json = br#"{"parent_ids":[],"workspace_id":"../evil","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"destroy"}}"#;
        let oid = write_blob(root, bad_json);

        let r = validate_remote_op_blob(root, &oid);
        assert!(r.is_err());
        // Either fails schema (no valid WorkspaceId) or name validation.
        // Either way we reject it.
    }

    // -----------------------------------------------------------------------
    // git_ancestry_relation
    // -----------------------------------------------------------------------

    #[test]
    fn ancestry_local_is_ancestor_of_remote() {
        let dir = setup_repo();
        let root = dir.path();

        let c1 = make_commit(root, "first");
        let c2 = make_commit(root, "second");

        // c1 is ancestor of c2 → remote (c2) is ahead of local (c1)
        let rel = git_ancestry_relation(root, &c1, &c2).unwrap();
        assert_eq!(rel, AncestryRelation::RemoteAhead);
    }

    #[test]
    fn ancestry_remote_is_ancestor_of_local() {
        let dir = setup_repo();
        let root = dir.path();

        let c1 = make_commit(root, "first");
        let c2 = make_commit(root, "second");

        // c2 is local (ahead), c1 is remote (behind)
        let rel = git_ancestry_relation(root, &c2, &c1).unwrap();
        assert_eq!(rel, AncestryRelation::LocalAheadOrEqual);
    }

    #[test]
    fn ancestry_diverged() {
        let dir = setup_repo();
        let root = dir.path();

        let base = make_commit(root, "base");

        // Create branch A
        StdCommand::new("git")
            .args(["checkout", "-b", "branch-a"])
            .current_dir(root)
            .output()
            .unwrap();
        let c_a = make_commit(root, "branch-a-commit");

        // Go back to base and create branch B
        StdCommand::new("git")
            .args(["checkout", base.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        let c_b = make_commit(root, "branch-b-commit");

        let rel = git_ancestry_relation(root, &c_a, &c_b).unwrap();
        assert_eq!(rel, AncestryRelation::Diverged);
    }

    #[test]
    fn oplog_ancestry_remote_ahead_linear_chain() {
        let dir = setup_repo();
        let root = dir.path();
        let ws_id = WorkspaceId::new("agent-1").unwrap();

        let op1 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T10:00:00Z".to_string(),
            payload: OpPayload::Create {
                epoch: crate::model::types::EpochId::new(&"a".repeat(40)).unwrap(),
            },
        };
        let oid1 = write_operation_blob(root, &op1).unwrap();

        let op2 = Operation {
            parent_ids: vec![oid1.clone()],
            workspace_id: ws_id,
            timestamp: "2026-02-19T10:05:00Z".to_string(),
            payload: OpPayload::Describe {
                message: "next".to_string(),
            },
        };
        let oid2 = write_operation_blob(root, &op2).unwrap();

        let rel = oplog_ancestry_relation(root, &oid1, &oid2).unwrap();
        assert_eq!(rel, AncestryRelation::RemoteAhead);

        let rel = oplog_ancestry_relation(root, &oid2, &oid1).unwrap();
        assert_eq!(rel, AncestryRelation::LocalAheadOrEqual);
    }

    #[test]
    fn oplog_ancestry_diverged() {
        let dir = setup_repo();
        let root = dir.path();
        let ws_id = WorkspaceId::new("agent-1").unwrap();

        let base = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T09:00:00Z".to_string(),
            payload: OpPayload::Create {
                epoch: crate::model::types::EpochId::new(&"a".repeat(40)).unwrap(),
            },
        };
        let base_oid = write_operation_blob(root, &base).unwrap();

        let left = Operation {
            parent_ids: vec![base_oid.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T09:05:00Z".to_string(),
            payload: OpPayload::Describe {
                message: "left".to_string(),
            },
        };
        let right = Operation {
            parent_ids: vec![base_oid],
            workspace_id: ws_id,
            timestamp: "2026-02-19T09:06:00Z".to_string(),
            payload: OpPayload::Describe {
                message: "right".to_string(),
            },
        };

        let left_oid = write_operation_blob(root, &left).unwrap();
        let right_oid = write_operation_blob(root, &right).unwrap();

        let rel = oplog_ancestry_relation(root, &left_oid, &right_oid).unwrap();
        assert_eq!(rel, AncestryRelation::Diverged);
    }

    // -----------------------------------------------------------------------
    // list_refs_with_prefix
    // -----------------------------------------------------------------------

    #[test]
    fn list_refs_empty_prefix() {
        let dir = setup_repo();
        let root = dir.path();

        let refs = list_refs_with_prefix(root, "refs/manifold/").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn list_refs_with_some_refs() {
        let dir = setup_repo();
        let root = dir.path();

        let c = make_commit(root, "ref-content");
        refs::write_ref(root, "refs/manifold/head/ws-a", &c).unwrap();
        refs::write_ref(root, "refs/manifold/head/ws-b", &c).unwrap();

        let r = list_refs_with_prefix(root, "refs/manifold/head/").unwrap();
        assert_eq!(r.len(), 2);
        assert!(r.contains(&"refs/manifold/head/ws-a".to_string()));
        assert!(r.contains(&"refs/manifold/head/ws-b".to_string()));
    }

    #[test]
    fn pull_ignores_stale_staging_refs_when_remote_has_no_manifold_refs() {
        let dir = setup_repo();
        let root = dir.path();

        let remote_dir = TempDir::new().unwrap();
        StdCommand::new("git")
            .args(["init", "--bare"])
            .current_dir(remote_dir.path())
            .output()
            .unwrap();

        StdCommand::new("git")
            .args([
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ])
            .current_dir(root)
            .output()
            .unwrap();

        // Simulate stale staging refs left behind by a previous interrupted pull.
        let stale_epoch = make_commit(root, "stale-epoch");
        refs::write_ref(root, "refs/manifold/remote/epoch/current", &stale_epoch).unwrap();

        let summary = pull_manifold_refs(root, "origin", false).unwrap();
        assert_eq!(summary.epoch, RefMergeResult::NoRemote);
        assert!(
            refs::read_ref(root, "refs/manifold/remote/epoch/current")
                .unwrap()
                .is_none()
        );
    }

    // -----------------------------------------------------------------------
    // chrono_now_or_fallback
    // -----------------------------------------------------------------------

    #[test]
    fn timestamp_format() {
        let ts = chrono_now_or_fallback();
        // Should be: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "timestamp should be 20 chars: {ts}");
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {ts}");
        assert!(ts.contains('T'), "timestamp should contain T: {ts}");
        // Year should be reasonable.
        let year: u64 = ts[..4].parse().unwrap();
        assert!(year >= 2026, "year should be >= 2026: {year}");
    }

    // -----------------------------------------------------------------------
    // days_to_ymd
    // -----------------------------------------------------------------------

    #[test]
    fn days_to_ymd_epoch() {
        // Day 0 = 1970-01-01
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_leap_year() {
        // 2000-02-29 = day 11016 since epoch
        // 2000-01-01 is day 10957, + 31 (Jan) + 28 (Feb 1-28) = 11016 for Feb 29
        let (y, m, d) = days_to_ymd(11016);
        assert_eq!((y, m, d), (2000, 2, 29));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2026-02-19 — let's verify manually.
        // Days from 1970-01-01 to 2026-02-19:
        // Years 1970..2025 = 56 years.
        // Leap years in 1970..2025: 1972, 1976, 1980, 1984, 1988, 1992, 1996, 2000, 2004, 2008, 2012, 2016, 2020, 2024 = 14 leap years
        // = 56 * 365 + 14 = 20440 + 14 = 20454 (to start of 2026)
        // Jan 2026: 31 days
        // Feb 1-19: 19 days
        // Total: 20454 + 31 + 19 - 1 = 20503
        // But let's just check it doesn't panic and is reasonable:
        let (y, m, d) = days_to_ymd(20503);
        assert_eq!(y, 2026);
        assert_eq!(m, 2);
        assert_eq!(d, 19);
    }

    // -----------------------------------------------------------------------
    // create_transport_merge_op
    // -----------------------------------------------------------------------

    #[test]
    fn create_transport_merge_op_updates_head_ref() {
        use crate::oplog::types::{OpPayload, Operation};
        use crate::oplog::write::write_operation_blob;

        let dir = setup_repo();
        let root = dir.path();
        let ws_id = WorkspaceId::new("agent-1").unwrap();

        // Create two "diverged" ops (with no parents to keep it simple).
        let op_a = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T10:00:00Z".to_string(),
            payload: OpPayload::Create {
                epoch: crate::model::types::EpochId::new(&"a".repeat(40)).unwrap(),
            },
        };
        let op_b = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T11:00:00Z".to_string(),
            payload: OpPayload::Create {
                epoch: crate::model::types::EpochId::new(&"b".repeat(40)).unwrap(),
            },
        };

        let oid_a = write_operation_blob(root, &op_a).unwrap();
        let oid_b = write_operation_blob(root, &op_b).unwrap();

        let head_ref = refs::workspace_head_ref(ws_id.as_str());

        // Create merge op.
        create_transport_merge_op(root, &ws_id, &oid_a, &oid_b, &head_ref).unwrap();

        // Head ref should now point to the merge op.
        let new_head = refs::read_ref(root, &head_ref).unwrap().unwrap();

        // Read the merge op and verify its parent_ids.
        let merge_op = crate::oplog::read::read_operation(root, &new_head).unwrap();
        assert_eq!(merge_op.parent_ids.len(), 2);
        assert!(merge_op.parent_ids.contains(&oid_a));
        assert!(merge_op.parent_ids.contains(&oid_b));

        // Verify payload is the annotate transport-merge.
        match &merge_op.payload {
            OpPayload::Annotate { key, data } => {
                assert_eq!(key, "transport-merge");
                assert_eq!(
                    data.get("merge_kind").and_then(|v| v.as_str()),
                    Some("transport-pull")
                );
            }
            other => panic!("expected Annotate payload, got: {other:?}"),
        }
    }
}
