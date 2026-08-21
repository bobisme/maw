//! Pre-destroy capture helper for workspace safety.
//!
//! Before a workspace is destroyed (via `maw ws destroy` or post-merge
//! `--destroy`), this module captures the workspace's dirty state as a
//! detached git commit and pins it under `refs/manifold/recovery/` so
//! the data survives garbage collection.
//!
//! # Design
//!
//! - **Workspace-owned**: orchestration lives in the workspace layer so both
//!   destroy paths (standalone and post-merge) stay consistent.
//! - **Git-first**: uses git commands directly for capture. A non-git backend
//!   would need its own capture implementation (TODO if ever needed).
//! - **Fail-safe**: if capture fails on a dirty workspace, the caller should
//!   abort the destructive delete rather than proceeding without a safety net.
//!
//! # Capture Modes
//!
//! - `WorktreeCapture`: workspace has uncommitted changes (staged, unstaged,
//!   or untracked files). A detached commit is created from the full worktree
//!   state.
//! - `HeadOnly`: workspace has no dirty files but is ahead of its base epoch
//!   (committed-only changes). The final HEAD is pinned as the recovery ref.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;
use serde::Serialize;
use tracing::instrument;

use maw_core::model::types::GitOid;
use maw_core::refs;

// ---------------------------------------------------------------------------
// Recovery ref prefix
// ---------------------------------------------------------------------------

/// Ref namespace for recovery pins.
///
/// Format: `refs/manifold/recovery/<workspace-name>/<timestamp>`
pub const RECOVERY_PREFIX: &str = "refs/manifold/recovery/";

/// Build the recovery ref name for a workspace capture.
#[must_use]
pub fn recovery_ref(workspace_name: &str, timestamp: &str) -> String {
    // Sanitize timestamp for ref name (colons → dashes)
    let safe_ts = timestamp.replace(':', "-");
    format!("{RECOVERY_PREFIX}{workspace_name}/{safe_ts}")
}

/// Build the recovery ref name for a `maw ws clean` capture (bn-auu5).
///
/// Distinct `clean-<timestamp>` component so clean snapshots are
/// self-describing in `git for-each-ref refs/manifold/recovery/` and in
/// `maw ws recover` listings, and never collide with destroy captures.
#[must_use]
pub fn clean_recovery_ref(workspace_name: &str, timestamp: &str) -> String {
    let safe_ts = timestamp.replace(':', "-");
    format!("{RECOVERY_PREFIX}{workspace_name}/clean-{safe_ts}")
}

/// Build the recovery ref name for a post-materialization repair capture
/// (bn-3gba).
///
/// Distinct `materialize-<timestamp>` component so a divergence snapshot is
/// self-describing next to `clean-*` and destroy captures.
#[must_use]
pub fn materialize_recovery_ref(workspace_name: &str, timestamp: &str) -> String {
    let safe_ts = timestamp.replace(':', "-");
    format!("{RECOVERY_PREFIX}{workspace_name}/materialize-{safe_ts}")
}

// ---------------------------------------------------------------------------
// Capture result types
// ---------------------------------------------------------------------------

/// The mode in which the workspace state was captured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Full worktree capture — workspace had uncommitted changes.
    WorktreeCapture,
    /// Head-only pin — workspace was clean but ahead of epoch.
    HeadOnly,
}

/// Metadata returned from a successful capture.
#[derive(Clone, Debug, Serialize)]
pub struct CaptureResult {
    /// The git OID of the captured commit.
    pub commit_oid: GitOid,
    /// The pinned ref path (under `refs/manifold/recovery/`).
    pub pinned_ref: String,
    /// List of dirty paths that were captured (empty for `HeadOnly`).
    pub dirty_paths: Vec<String>,
    /// How the capture was performed.
    pub mode: CaptureMode,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Capture the current workspace state before destruction.
///
/// Returns `Ok(None)` if the workspace is clean *and* at the base epoch
/// (nothing to preserve). Returns `Ok(Some(result))` if state was captured.
///
/// # Fail-safe
///
/// If the workspace has dirty files and capture fails, this returns `Err`.
/// The caller **must not** proceed with destruction on error — doing so
/// would lose data.
///
/// # Arguments
///
/// * `ws_path` — absolute path to the workspace directory
/// * `ws_name` — workspace name (used for the recovery ref)
/// * `base_epoch` — the workspace's base epoch OID (to detect committed-ahead)
#[instrument(skip_all, fields(workspace = ws_name))]
pub fn capture_before_destroy(
    ws_path: &Path,
    ws_name: &str,
    base_epoch: &GitOid,
) -> Result<Option<CaptureResult>> {
    // Step 1: detect dirty state
    let dirty_paths = list_dirty_paths(ws_path)?;

    // Step 1b (bn-2k9e): `list_dirty_paths` — like `git status`, `git diff
    // HEAD` and every other status-shaped query — trusts the index stat cache.
    // A tracked file whose on-disk bytes disagree with HEAD while its
    // `(size, mtime[, ctime])` still match its index entry is reported CLEAN
    // and never reaches the snapshot, so destroy removes the only copy of
    // those bytes. Hash-compare the worktree against HEAD to find them.
    let hidden = hidden_divergent_paths(ws_path, &dirty_paths)?;
    if let Some(ref err) = hidden.unverified {
        // The hash detector could not run, so "clean" is unproven. Fail CLOSED:
        // `hidden.rehash` was widened to every tracked file, and the capture
        // below re-hashes all of them. It still returns `None` when nothing
        // actually differed, so a genuinely clean workspace pins nothing.
        tracing::warn!(
            workspace = %ws_name,
            error = %err,
            "pre-destroy hash comparison failed; force-rehashing every tracked file"
        );
    }

    if dirty_paths.is_empty() && hidden.paths.is_empty() {
        if hidden.unverified.is_some()
            && let Some(result) = capture_dirty_worktree(ws_path, ws_name, &[], &hidden.rehash)?
        {
            return Ok(Some(result));
        }
        // No dirty files — check if HEAD is ahead of base epoch
        let head_oid = resolve_head(ws_path)?;
        if head_oid.as_str() == base_epoch.as_str() {
            // Workspace is clean and at epoch — nothing to capture
            tracing::debug!("workspace is clean and at epoch, nothing to capture");
            return Ok(None);
        }
        // HEAD is ahead of epoch but no dirty files — pin HEAD
        return pin_head_only(ws_path, ws_name, &head_oid);
    }

    if !hidden.paths.is_empty() {
        warn_hidden_divergence(ws_name, &hidden.paths);
    }

    // Step 2: capture dirty worktree as a detached commit. The hidden paths
    // join the reported dirty set AND are force-rehashed, so the stash tree
    // holds their real bytes rather than the HEAD blob the forged stat cache
    // would otherwise stage.
    let mut all_paths = dirty_paths;
    all_paths.extend(hidden.paths.iter().cloned());
    all_paths.sort();
    all_paths.dedup();
    capture_dirty_worktree(ws_path, ws_name, &all_paths, &hidden.rehash)
}

/// Snapshot stat-cache-masked stale bytes before a **non-`--force`** destroy
/// removes the worktree (bn-2k9e).
///
/// `maw ws destroy <ws>` without `--force` only proceeds when the workspace is
/// judged untouched — and every measure feeding that judgement
/// (`compute_patchset`'s `git diff <epoch>`, gix's `status_head_to_worktree`)
/// trusts the index stat cache. A file whose bytes were replaced while its
/// `(size, mtime)` were preserved is therefore destroyed with no capture at
/// all, reachable from no `refs/manifold/recovery/<ws>/*` ref: a Prime
/// Invariant violation, and the same blind spot bn-154g closed for `ws sync`'s
/// fast-forward checkout.
///
/// Returns `Ok(None)` — pinning nothing, printing nothing — when the hash
/// comparison proves the worktree matches HEAD, so an ordinary clean destroy is
/// byte-for-byte what it was before this guard existed.
///
/// # Fail-safe (Prime Invariant)
///
/// Returns `Err` when the comparison could not be made or the snapshot could
/// not be pinned. Callers **must** abort the destroy on `Err`: the worktree is
/// about to be deleted and maw cannot show that its bytes are recoverable.
#[instrument(skip_all, fields(workspace = ws_name))]
pub fn capture_hidden_stale_before_destroy(
    ws_path: &Path,
    ws_name: &str,
) -> Result<Option<CaptureResult>> {
    let hidden = hidden_divergent_paths(ws_path, &[])?;
    if let Some(err) = hidden.unverified {
        bail!(
            "cannot verify that workspace '{ws_name}' matches HEAD before destroying it: {err}\n  \
             Refusing rather than deleting bytes that were never proven recoverable.\n  \
             Snapshot the whole worktree first: maw ws destroy {ws_name} --force"
        );
    }
    if hidden.paths.is_empty() {
        return Ok(None);
    }
    warn_hidden_divergence(ws_name, &hidden.paths);
    capture_paths_to_ref(ws_path, ws_name, &hidden.paths, RefKind::Destroy)
}

/// Tracked paths whose on-disk bytes disagree with HEAD but which every
/// status-shaped query reports CLEAN (bn-2k9e).
struct HiddenDivergence {
    /// Paths the hash comparison PROVED divergent, minus the ones the caller
    /// already captures. Empty when the detector could not run.
    paths: Vec<String>,
    /// Paths whose index stat entry must be discarded so the capture re-hashes
    /// them from disk. Equals `paths` normally; every tracked file when the
    /// detector failed (fail-closed widening).
    rehash: Vec<String>,
    /// `Some(error)` when the hash detector itself could not run, so "clean" is
    /// unproven.
    unverified: Option<String>,
}

/// Hash-compare the worktree against HEAD and return the divergent paths the
/// caller is not already capturing.
///
/// Reuses bn-154g's detector (`materialize_verify::divergent_paths_by_tree`)
/// verbatim — a throwaway index seeded from `HEAD` (whose entries carry ZEROED
/// stat data, so nothing can be trusted-as-clean), `git add -A` against it, and
/// a `diff-tree` against `HEAD^{tree}`. That forces git to re-hash every file,
/// which is the only thing this corruption class cannot hide from, and it goes
/// through git's clean filters, so an LFS-smudged worktree file whose pointer
/// is what HEAD stores does NOT count as divergence.
///
/// Deliberately does NOT use `materialize_verify::detect_divergence`: its
/// fallback is the status query, which is exactly what the mask defeats.
///
/// Paths missing from disk are dropped — a deletion cannot be stat-masked
/// (the stat fails, so git sees it), `git add -f` on one would fail, and it is
/// already in the caller's status-visible set.
fn hidden_divergent_paths(ws_path: &Path, already_captured: &[String]) -> Result<HiddenDivergence> {
    let excluded: BTreeSet<&str> = already_captured.iter().map(String::as_str).collect();
    match super::materialize_verify::divergent_paths_by_tree(ws_path) {
        Ok(divergent) => {
            let paths: Vec<String> = divergent
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| !excluded.contains(p.as_str()))
                .filter(|p| ws_path.join(p).symlink_metadata().is_ok())
                .collect();
            Ok(HiddenDivergence {
                rehash: paths.clone(),
                paths,
                unverified: None,
            })
        }
        Err(e) => {
            let rehash: Vec<String> = tracked_files(ws_path)?
                .into_iter()
                .filter(|p| !excluded.contains(p.as_str()))
                .filter(|p| ws_path.join(p).symlink_metadata().is_ok())
                .collect();
            Ok(HiddenDivergence {
                paths: Vec::new(),
                rehash,
                unverified: Some(e),
            })
        }
    }
}

/// Every tracked path in the workspace (`git ls-files -z`).
///
/// Only used for the fail-closed widening when the hash detector is unusable;
/// an error here means git plumbing is broken in this worktree and the caller
/// aborts rather than deleting unverifiable bytes.
fn tracked_files(ws_path: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(ws_path)
        .output()
        .context("failed to list tracked files for the pre-destroy hash comparison")?;
    if !out.status.success() {
        bail!(
            "git ls-files failed during the pre-destroy hash comparison: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Announce stat-cache-masked stale bytes that are being snapshotted before the
/// workspace is removed. Loud on purpose: the workspace looked clean, so nothing
/// else in the destroy output would hint that bytes were about to be lost.
fn warn_hidden_divergence(ws_name: &str, paths: &[String]) {
    const MAX_SHOWN: usize = 5;
    eprintln!(
        "WARNING: workspace '{ws_name}' holds working-tree bytes that disagree with HEAD but that \
         every status query reports as CLEAN (stat-cache-masked, bn-2k9e)."
    );
    eprintln!("  Snapshotting them into this workspace's recovery snapshot before destroy:");
    for p in paths.iter().take(MAX_SHOWN) {
        eprintln!("    {p}");
    }
    if paths.len() > MAX_SHOWN {
        eprintln!("    ...and {} more", paths.len() - MAX_SHOWN);
    }
    eprintln!("  Inspect after destroy: maw ws recover {ws_name} --show <path>");
}

/// Drop the (possibly forged) index stat entries for `paths` so the `git add`
/// that follows is forced to re-hash them from disk (bn-2k9e).
///
/// `git add` — with or without `-f` — consults the same stat cache the mask
/// forged, so it stages the HEAD blob for a masked file. Removing the entry
/// first (`update-index --force-remove`) leaves `git add -f` no cache to
/// consult and it must read the file. Filters still apply, so an LFS-tracked
/// path is re-cleaned to its pointer exactly as a normal `git add` would.
///
/// # Fail-safe (Prime Invariant)
///
/// Returns `Err` if either step fails; the caller aborts the capture (and
/// therefore the destroy) rather than snapshotting bytes it cannot vouch for.
fn force_rehash_paths(ws_path: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    // Chunked so a workspace with tens of thousands of tracked files cannot
    // overrun the argv limit during the fail-closed widening.
    for chunk in paths.chunks(256) {
        let mut remove = Command::new("git");
        remove.args(["update-index", "--force-remove", "--"]);
        for p in chunk {
            remove.arg(p);
        }
        let out = remove
            .current_dir(ws_path)
            .output()
            .context("failed to run git update-index --force-remove during capture")?;
        if !out.status.success() {
            bail!(
                "git update-index --force-remove failed during capture: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let mut add = Command::new("git");
        add.args(["add", "-f", "--"]);
        for p in chunk {
            add.arg(p);
        }
        let out = add
            .current_dir(ws_path)
            .output()
            .context("failed to run git add -f during capture")?;
        if !out.status.success() {
            bail!(
                "git add -f failed during capture: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Capture the exact set of files a `maw ws clean` is about to delete, as a
/// detached commit pinned under `refs/manifold/recovery/<ws>/clean-<ts>`
/// (bn-auu5).
///
/// The paths are force-staged (`git add -f`) in a temporary index so that
/// gitignore'd files removed via `--ignored` are captured too — `git add -A`
/// alone would skip them and break the recovery guarantee. `git stash create`
/// then builds a commit whose tree contains those blobs without moving HEAD,
/// the stash list, or the caller's real index.
///
/// # Fail-safe (Prime Invariant)
///
/// Returns `Err` if staging or stash creation fails, or if `git stash create`
/// declines to produce a commit despite non-empty input. Callers **must** abort
/// the deletion on `Err` — nothing must be removed without a recovery point.
///
/// Returns `Ok(None)` only when `paths` is empty (caller treats as "nothing to
/// clean").
#[instrument(skip_all, fields(workspace = ws_name, paths = paths.len()))]
pub fn capture_before_clean(
    ws_path: &Path,
    ws_name: &str,
    paths: &[String],
) -> Result<Option<CaptureResult>> {
    capture_paths_to_ref(ws_path, ws_name, paths, RefKind::Clean)
}

/// Capture the exact bytes at `paths` **before** the post-materialization
/// verifier re-materializes them from HEAD (bn-3gba).
///
/// Same alternate-index technique as [`capture_before_clean`], pinned under
/// `refs/manifold/recovery/<ws>/materialize-<ts>` instead. The divergent
/// content is *by contract* not user work (the operation promised a clean
/// worktree at a known commit), but the Prime Invariant is unconditional: maw
/// never overwrites bytes it has not first made recoverable — **even wrong
/// bytes**. In the bn-p3m9 class those bytes are the only forensic evidence of
/// the (still unreproduced) mechanism, so pinning them is also what makes the
/// next field report actionable.
///
/// # Fail-safe (Prime Invariant)
///
/// Returns `Err` when the snapshot could not be produced. The caller **must
/// not** repair on `Err` — it must leave the divergence in place and report it.
///
/// Returns `Ok(None)` only when `paths` is empty.
#[instrument(skip_all, fields(workspace = ws_name, paths = paths.len()))]
pub fn capture_before_materialize_repair(
    ws_path: &Path,
    ws_name: &str,
    paths: &[String],
) -> Result<Option<CaptureResult>> {
    capture_paths_to_ref(ws_path, ws_name, paths, RefKind::Materialize)
}

/// Which recovery-ref namespace an explicit-path capture pins into.
#[derive(Clone, Copy)]
enum RefKind {
    /// `maw ws clean` pre-deletion snapshot (bn-auu5).
    Clean,
    /// Post-materialization pre-repair snapshot (bn-3gba).
    Materialize,
    /// Pre-destroy snapshot of stat-cache-masked stale bytes on a workspace
    /// that every status query calls clean (bn-2k9e). Pins into the ordinary
    /// destroy namespace (`refs/manifold/recovery/<ws>/<ts>`) on purpose: the
    /// destroy record then points at it, so `maw ws recover <ws> --show <path>`
    /// and `--restore-file` resolve the masked bytes through the normal front
    /// door instead of needing a second, special-cased lookup.
    Destroy,
}

impl RefKind {
    fn ref_name(self, ws_name: &str, timestamp: &str) -> String {
        match self {
            Self::Clean => clean_recovery_ref(ws_name, timestamp),
            Self::Materialize => materialize_recovery_ref(ws_name, timestamp),
            Self::Destroy => recovery_ref(ws_name, timestamp),
        }
    }

    const fn empty_snapshot_error(self) -> &'static str {
        match self {
            Self::Clean => "clean aborted to avoid data loss: files were selected for removal",
            Self::Materialize => {
                "post-materialization repair aborted to avoid data loss: divergent paths were \
                 detected"
            }
            Self::Destroy => {
                "destroy aborted to avoid data loss: the worktree holds tracked bytes that \
                 disagree with HEAD"
            }
        }
    }
}

/// Shared body of [`capture_before_clean`] / [`capture_before_materialize_repair`].
///
/// The paths are force-staged (`git add -f`) in a temporary index seeded from
/// `HEAD`, so gitignore'd files are captured too — `git add -A` alone would
/// skip them and break the recovery guarantee. `git stash create` then builds a
/// commit whose tree contains those blobs without moving HEAD, the stash list,
/// or the caller's real index.
fn capture_paths_to_ref(
    ws_path: &Path,
    ws_name: &str,
    paths: &[String],
    kind: RefKind,
) -> Result<Option<CaptureResult>> {
    if paths.is_empty() {
        return Ok(None);
    }

    // Use an alternate index initialized from HEAD. Staging in the real index
    // and resetting it afterward destroys any staged work the caller already
    // had, even though these captures promise not to touch tracked state.
    let temp_dir = tempfile::tempdir().context("failed to create capture temp directory")?;
    let temp_index = temp_dir.path().join("index");
    initialize_temporary_index(ws_path, &temp_index)?;
    stage_paths_force(ws_path, paths, &temp_index)?;

    let stash_result = stash_create_with_index(ws_path, &temp_index)?;
    let Some(stash_oid_str) = stash_result else {
        return Err(anyhow::anyhow!(
            "{} but `git stash create` produced no snapshot commit (paths = {paths:?})",
            kind.empty_snapshot_error()
        ));
    };

    let commit_oid =
        GitOid::new(&stash_oid_str).map_err(|e| anyhow::anyhow!("invalid stash OID: {e}"))?;

    // FP: crash after tree/commit creation but before ref pinning.
    maw::fp!("FP_CLEAN_CAPTURE_BEFORE_PIN")?;

    let timestamp = super::now_timestamp_iso8601_precise();
    let ref_name = kind.ref_name(ws_name, &timestamp);
    let repo_root = repo_root_from_worktree(ws_path)?;
    refs::write_ref(&repo_root, &ref_name, &commit_oid)
        .map_err(|e| anyhow::anyhow!("failed to pin recovery ref: {e}"))?;

    tracing::info!(
        ref_name = %ref_name,
        oid = %commit_oid,
        path_count = paths.len(),
        "captured explicit-path recovery snapshot"
    );

    Ok(Some(CaptureResult {
        commit_oid,
        pinned_ref: ref_name,
        dirty_paths: paths.to_vec(),
        mode: CaptureMode::WorktreeCapture,
    }))
}

/// Initialize an alternate index from `HEAD`, leaving the workspace's real
/// index untouched.
fn initialize_temporary_index(ws_path: &Path, index_path: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["read-tree", "HEAD"])
        .env("GIT_INDEX_FILE", index_path)
        .current_dir(ws_path)
        .output()
        .context("failed to initialize temporary index for clean capture")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git read-tree failed during clean capture: {}",
            stderr.trim()
        );
    }
    Ok(())
}

/// Force-stage an explicit set of paths (`git add -f -- <paths>`) in an
/// alternate index, so ignored files are included without changing the
/// caller's staging state.
fn stage_paths_force(ws_path: &Path, paths: &[String], index_path: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("add").arg("-f").arg("--");
    for p in paths {
        cmd.arg(p);
    }
    let out = cmd
        .env("GIT_INDEX_FILE", index_path)
        .current_dir(ws_path)
        .output()
        .context("failed to run git add -f for clean capture")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git add -f failed during clean capture: {}", stderr.trim());
    }
    Ok(())
}

/// Create a detached stash commit using `index_path` instead of the caller's
/// real index. `git stash create` prints the commit OID and does not update the
/// stash list.
fn stash_create_with_index(ws_path: &Path, index_path: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(["stash", "create"])
        .env("GIT_INDEX_FILE", index_path)
        .current_dir(ws_path)
        .output()
        .context("failed to run git stash create for clean capture")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git stash create failed during clean capture: {}",
            stderr.trim()
        );
    }
    let oid = String::from_utf8(out.stdout).context("git stash create returned a non-UTF-8 OID")?;
    let oid = oid.trim();
    Ok((!oid.is_empty()).then(|| oid.to_owned()))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// List all dirty paths in the workspace (staged + unstaged + untracked).
///
/// Must be HEAD→worktree, not index→worktree: this feeds the recovery
/// snapshot taken before a workspace is destroyed, so a staged-but-not-
/// re-edited file dropped here would be permanently lost on destroy
/// (Prime Invariant: no work is ever lost).
fn list_dirty_paths(ws_path: &Path) -> Result<Vec<String>> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let entries = repo
        .status_head_to_worktree()
        .map_err(|e| anyhow::anyhow!("git status failed: {e}"))?;

    let paths: Vec<String> = entries.into_iter().map(|entry| entry.path).collect();

    Ok(paths)
}

/// Resolve HEAD to a full OID.
pub fn resolve_head(ws_path: &Path) -> Result<GitOid> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let git_oid = repo
        .rev_parse("HEAD")
        .map_err(|e| anyhow::anyhow!("failed to resolve HEAD: {e}"))?;
    let oid_str = git_oid.to_string();
    GitOid::new(&oid_str).map_err(|e| anyhow::anyhow!("invalid HEAD OID: {e}"))
}

/// Pin HEAD (committed-only, no dirty files) under a recovery ref.
fn pin_head_only(
    ws_path: &Path,
    ws_name: &str,
    head_oid: &GitOid,
) -> Result<Option<CaptureResult>> {
    let timestamp = super::now_timestamp_iso8601_precise();
    let ref_name = recovery_ref(ws_name, &timestamp);

    // Pin the ref in the repo (use the repo root, not the worktree)
    let repo_root = repo_root_from_worktree(ws_path)?;
    refs::write_ref(&repo_root, &ref_name, head_oid)
        .map_err(|e| anyhow::anyhow!("failed to pin recovery ref: {e}"))?;

    tracing::info!(
        ref_name = %ref_name,
        oid = %head_oid,
        "pinned head-only recovery ref"
    );

    Ok(Some(CaptureResult {
        commit_oid: head_oid.clone(),
        pinned_ref: ref_name,
        dirty_paths: Vec::new(),
        mode: CaptureMode::HeadOnly,
    }))
}

/// Capture the dirty worktree as a detached commit and pin it.
///
/// Uses `git add -A` + `git stash create` to build a commit object that
/// includes all tracked changes plus untracked files, without moving HEAD
/// or altering the index/stash-list.
///
/// `force_rehash` (bn-2k9e) names paths whose index stat entry must be
/// discarded before staging, because `git add -A` would otherwise trust a
/// forged stat cache and stage the HEAD blob instead of the bytes on disk.
fn capture_dirty_worktree(
    ws_path: &Path,
    ws_name: &str,
    dirty_paths: &[String],
    force_rehash: &[String],
) -> Result<Option<CaptureResult>> {
    // bn-2k9e: must run BEFORE `stage_all_for_capture`, so the `git add -A`
    // there sees entries that already carry the real (re-hashed) content.
    if let Err(e) = force_rehash_paths(ws_path, force_rehash) {
        warn_on_reset_failure(ws_path, "capture-force-rehash-failure");
        return Err(e);
    }

    // `git stash create` produces a merge commit that captures the current
    // index + worktree state as a detached object. It does NOT modify
    // HEAD, the index, or the stash list — perfect for our pre-destroy
    // capture.
    //
    // However, `git stash create` only captures tracked files + staged
    // changes. Untracked files are missed unless we stage them first.
    // We use `git add -A` to stage everything, then `git stash create`
    // to build the commit, then unstage to restore the index.
    //
    // TODO(gix): the `git add -A` step still shells to git because gix has
    // no equivalent that walks the worktree, hashes new content, stages
    // modifications/deletions, and handles embedded repo directories. Once
    // that primitive exists, replace `stage_all_for_capture` too.

    // Stage all files (including untracked). Embedded git directories can make
    // `git add -A` fail with "does not have a commit checked out". Retry while
    // excluding those paths so normal files are still captured.
    let excluded_paths = stage_all_for_capture(ws_path)?;

    // Create a stash commit (does not modify HEAD or stash list)
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let stash_result = repo.stash_create().map_err(|e| {
        // Restore index state before bailing.
        warn_on_reset_failure(ws_path, "capture-stash-create-failure");
        anyhow::anyhow!("git stash create failed during capture: {e}")
    })?;

    // Paths that remained "capturable" after embedded-repo exclusions — these
    // are the paths we'd actually expect the stash to preserve.
    let capturable_dirty_paths: Vec<String> = dirty_paths
        .iter()
        .filter(|path| {
            !excluded_paths
                .iter()
                .any(|excluded| path_is_under_excluded(path, excluded))
        })
        .cloned()
        .collect();

    let Some(stash_git_oid) = stash_result else {
        // `stash_create` returned None. Two legitimate shapes:
        //
        // 1. All dirty paths were uncapturable embedded git dirs that
        //    `stage_all_for_capture` excluded. After the excludes, there
        //    is genuinely no capturable content — returning `Ok(None)`
        //    is correct (matches "clean at epoch" semantics from the
        //    caller's perspective).
        //
        // 2. At least one dirty path WAS capturable, and yet
        //    `stash_create` still refused. This is the silent-data-loss
        //    scenario guarded by bn-3mpx — return `Err` so callers
        //    abort rather than proceed over user work.
        //
        warn_on_reset_failure(ws_path, "capture-stash-create-none");
        if capturable_dirty_paths.is_empty() {
            tracing::debug!(
                excluded_paths = ?excluded_paths,
                "stash_create returned None; all dirty paths were uncapturable \
                 embedded repos, treating as no-op capture"
            );
            return Ok(None);
        }
        tracing::warn!(
            dirty_paths = ?dirty_paths,
            capturable = ?capturable_dirty_paths,
            excluded = ?excluded_paths,
            "stash_create returned None despite capturable dirty paths"
        );
        return Err(anyhow::anyhow!(
            "capture aborted to avoid silent data loss: capturable dirty paths \
             were detected but `git stash create` refused to produce a stash \
             commit (capturable = {capturable_dirty_paths:?})"
        ));
    };

    let stash_oid_str = stash_git_oid.to_string();
    let commit_oid =
        GitOid::new(&stash_oid_str).map_err(|e| anyhow::anyhow!("invalid stash OID: {e}"))?;

    // Restore the index to its pre-add state (don't leave staged changes
    // behind — the workspace is about to be destroyed, but be clean anyway).
    warn_on_reset_failure(ws_path, "capture-restore-index");

    let captured_dirty_paths = capturable_dirty_paths;

    // FP: crash after stash/tree creation but before ref pinning.
    // A crash here means the commit object exists but is unreachable (no ref).
    maw::fp!("FP_CAPTURE_BEFORE_PIN")?;

    // Pin the commit under a recovery ref
    let timestamp = super::now_timestamp_iso8601_precise();
    let ref_name = recovery_ref(ws_name, &timestamp);

    let repo_root = repo_root_from_worktree(ws_path)?;
    refs::write_ref(&repo_root, &ref_name, &commit_oid)
        .map_err(|e| anyhow::anyhow!("failed to pin recovery ref: {e}"))?;

    tracing::info!(
        ref_name = %ref_name,
        oid = %commit_oid,
        dirty_count = captured_dirty_paths.len(),
        skipped_uncapturable_count = excluded_paths.len(),
        "captured dirty worktree state"
    );

    Ok(Some(CaptureResult {
        commit_oid,
        pinned_ref: ref_name,
        dirty_paths: captured_dirty_paths,
        mode: CaptureMode::WorktreeCapture,
    }))
}

/// Reset the index to HEAD and surface any failure via `tracing::warn!` rather
/// than silently swallowing it (bn-2blj). We're always in a cleanup path here
/// so the caller continues regardless, but a visible warning is needed to
/// debug cases where the index is left dirty for the next operation.
fn warn_on_reset_failure(ws_path: &Path, context: &str) {
    let repo = match maw_git::GixRepo::open(ws_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                context = %context,
                ws_path = %ws_path.display(),
                error = %e,
                "failed to open repo during capture cleanup"
            );
            return;
        }
    };
    if let Err(e) = repo.unstage_all() {
        tracing::warn!(
            context = %context,
            ws_path = %ws_path.display(),
            error = %e,
            "unstage_all failed during capture cleanup — index may be dirty"
        );
    }
}

// TODO(gix): replace `git add -A` and its `:(exclude)` retry below with a
// maw-git primitive. gix currently has no equivalent that walks the worktree,
// hashes new content, stages modifications/deletions, and handles embedded
// repo directories. Until that primitive exists, we shell to git.
fn stage_all_for_capture(ws_path: &Path) -> Result<Vec<String>> {
    let add_output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git add -A")?;

    if add_output.status.success() {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&add_output.stderr);
    let excluded_paths = parse_uncapturable_embedded_repo_paths(&stderr);
    if excluded_paths.is_empty() {
        bail!("git add -A failed during capture: {}", stderr.trim());
    }

    let mut retry = Command::new("git");
    retry.arg("add").arg("-A").arg("--").arg(".");
    for excluded in &excluded_paths {
        retry.arg(format!(":(exclude){excluded}"));
    }

    let retry_output = retry
        .current_dir(ws_path)
        .output()
        .context("failed to retry git add -A with path exclusions")?;

    if !retry_output.status.success() {
        let retry_stderr = String::from_utf8_lossy(&retry_output.stderr);
        bail!(
            "git add -A retry failed during capture: {}",
            retry_stderr.trim()
        );
    }

    tracing::warn!(
        skipped_uncapturable_paths = ?excluded_paths,
        "capture skipped embedded git directories without checked-out commits"
    );

    Ok(excluded_paths)
}

pub(super) fn parse_uncapturable_embedded_repo_paths(stderr: &str) -> Vec<String> {
    const PREFIX: &str = "error: '";
    const SUFFIX: &str = "' does not have a commit checked out";

    let mut paths: Vec<String> = stderr
        .lines()
        .filter_map(|line| {
            line.strip_prefix(PREFIX)
                .and_then(|tail| tail.strip_suffix(SUFFIX))
                .map(ToOwned::to_owned)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn path_is_under_excluded(path: &str, excluded: &str) -> bool {
    let p = path.trim_end_matches('/');
    let e = excluded.trim_end_matches('/');
    p == e || p.starts_with(&format!("{e}/"))
}

/// Resolve the repo root from a worktree path.
///
/// Uses gix's `common_dir()` to find the shared git directory, then derives
/// the repo root from it.
fn repo_root_from_worktree(ws_path: &Path) -> Result<std::path::PathBuf> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let common_dir = std::fs::canonicalize(repo.common_dir()).with_context(|| {
        format!(
            "failed to canonicalize git common dir for {}",
            ws_path.display()
        )
    })?;
    let mut root = common_dir
        .parent()
        .context("cannot determine repo root from git common dir")?
        .to_path_buf();

    // Support nested common-dir layouts like <root>/.manifold/git
    if root.file_name().is_some_and(|name| name == ".manifold") {
        root = root
            .parent()
            .context("cannot determine repo root from nested common dir")?
            .to_path_buf();
    }

    // Validate that the derived root looks like a real maw repo root.
    // Without this check, a non-standard layout silently returns a wrong
    // path and recovery refs get pinned under an unrelated repo's
    // namespace (bn-2bow).
    // Layout-aware: accept either v2 markers (ws/ or .manifold/) or
    // consolidated markers (.maw/manifold/).
    let has_ws_dir = root.join("ws").is_dir();
    let has_manifold_dir = root.join(".manifold").is_dir();
    let has_consolidated = root.join(".maw").join("manifold").is_dir();
    if !has_ws_dir && !has_manifold_dir && !has_consolidated {
        bail!(
            "derived repo root {} does not contain `ws/`, `.manifold/`, or `.maw/manifold/` — \
             refusing to pin recovery ref under an unrecognized layout",
            root.display()
        );
    }

    Ok(root)
}

// ---------------------------------------------------------------------------
// Recovery surface output
// ---------------------------------------------------------------------------

/// Emit the full recovery output contract to stderr.
///
/// All 5 required fields:
/// 1. Operation result (success/failure)
/// 2. Whether COMMIT succeeded
/// 3. Snapshot ref + oid
/// 4. Artifact path
/// 5. Executable recovery command
///
/// This function is the single source of truth for recovery surface output.
/// All code paths that create recovery snapshots MUST call this to ensure
/// agents can parse and act on the output consistently.
pub fn emit_recovery_surface(
    workspace_name: &str,
    capture: &CaptureResult,
    artifact_path: Option<&std::path::Path>,
    commit_succeeded: bool,
    operation_succeeded: bool,
) {
    let status = if operation_succeeded {
        "success"
    } else {
        "failure"
    };
    let commit_status = if commit_succeeded { "yes" } else { "no" };
    let mode_label = match capture.mode {
        CaptureMode::WorktreeCapture => "worktree-snapshot",
        CaptureMode::HeadOnly => "head-only",
    };

    eprintln!("RECOVERY_SURFACE for '{workspace_name}':");
    eprintln!("  result:       {status}");
    eprintln!("  commit:       {commit_status}");
    eprintln!("  snapshot_ref: {}", capture.pinned_ref);
    eprintln!("  snapshot_oid: {}", capture.commit_oid);
    eprintln!("  capture_mode: {mode_label}");
    if let Some(path) = artifact_path {
        eprintln!("  artifact:     {}", path.display());
    } else {
        eprintln!("  artifact:     (none)");
    }
    eprintln!("  recover_cmd:  maw ws recover {workspace_name}");
}

/// Emit a structured recovery failure notice when capture itself fails.
///
/// Emits the same field names as [`emit_recovery_surface`] but with
/// `(capture failed)` placeholders so agents can still parse the output
/// structure consistently.
pub fn emit_recovery_surface_failed(
    workspace_name: &str,
    error: &dyn std::fmt::Display,
    commit_succeeded: bool,
) {
    let commit_status = if commit_succeeded { "yes" } else { "no" };

    eprintln!("RECOVERY_SURFACE for '{workspace_name}':");
    eprintln!("  result:       failure");
    eprintln!("  commit:       {commit_status}");
    eprintln!("  snapshot_ref: (capture failed)");
    eprintln!("  snapshot_oid: (capture failed)");
    eprintln!("  capture_mode: (capture failed)");
    eprintln!("  artifact:     (none)");
    eprintln!("  recover_cmd:  git -C <workspace-path> stash list");
    eprintln!("  error:        {error}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create a fresh git repo with one commit. Returns (tempdir, repo root, HEAD OID).
    fn setup_repo() -> (TempDir, std::path::PathBuf, GitOid) {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");

        // Create a `.manifold/` directory so `repo_root_from_worktree`'s
        // layout validation (bn-2bow) recognizes this as a maw repo root.
        fs::create_dir_all(root.join(".manifold")).expect("operation should succeed");

        fs::write(root.join("README.md"), "# Test\n").expect("operation should succeed");
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        let oid_str = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let oid = GitOid::new(&oid_str).expect("operation should succeed");

        (dir, root, oid)
    }

    // -----------------------------------------------------------------------
    // recovery_ref formatting
    // -----------------------------------------------------------------------

    /// Regression test for bn-2bow: a git repo that looks nothing like a
    /// maw layout (no `ws/`, no `.manifold/`) must be rejected by
    /// `repo_root_from_worktree` rather than silently returning a wrong path.
    #[test]
    fn repo_root_validation_rejects_non_maw_layout() {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");

        // No .manifold/ or ws/ dir → should error
        let result = repo_root_from_worktree(&root);
        assert!(
            result.is_err(),
            "expected error when layout lacks ws/ and .manifold/, got {result:?}"
        );
        let err = format!("{}", result.expect_err("operation should fail"));
        assert!(
            err.contains("does not contain `ws/`")
                && err.contains(".manifold/")
                && err.contains(".maw/manifold/"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn repo_root_validation_accepts_manifold_dir() {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        fs::create_dir_all(root.join(".manifold")).expect("operation should succeed");

        let result = repo_root_from_worktree(&root).expect("operation should succeed");
        assert_eq!(result, root);
    }

    #[test]
    fn repo_root_validation_accepts_ws_dir() {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        fs::create_dir_all(root.join("ws")).expect("operation should succeed");

        let result = repo_root_from_worktree(&root).expect("operation should succeed");
        assert_eq!(result, root);
    }

    #[test]
    fn recovery_ref_format() {
        let r = recovery_ref("alice", "2025-01-15T10:30:00Z");
        assert_eq!(r, "refs/manifold/recovery/alice/2025-01-15T10-30-00Z");
    }

    #[test]
    fn clean_recovery_ref_format() {
        let r = clean_recovery_ref("alice", "2025-01-15T10:30:00Z");
        assert_eq!(r, "refs/manifold/recovery/alice/clean-2025-01-15T10-30-00Z");
        assert!(!r.contains(':'), "colons should be sanitized: {r}");
    }

    // -----------------------------------------------------------------------
    // bn-auu5: capture_before_clean
    // -----------------------------------------------------------------------

    #[test]
    fn capture_before_clean_empty_paths_is_none() {
        let (_dir, root, _oid) = setup_repo();
        let result = capture_before_clean(&root, "feat", &[]).expect("ok");
        assert!(result.is_none(), "no paths → nothing to capture");
    }

    #[test]
    fn capture_before_clean_pins_clean_ref_and_captures_bytes() {
        let (_dir, root, _oid) = setup_repo();

        // One plain untracked file and one gitignore'd file.
        fs::write(root.join(".gitignore"), "*.ign\n").expect("write gitignore");
        Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&root)
            .output()
            .expect("add gitignore");
        Command::new("git")
            .args(["commit", "-m", "gitignore"])
            .current_dir(&root)
            .output()
            .expect("commit");

        fs::write(root.join("scratch.tmp"), "junk-bytes\n").expect("write scratch");
        fs::write(root.join("build.ign"), "ignored-bytes\n").expect("write ignored");

        let paths = vec!["scratch.tmp".to_string(), "build.ign".to_string()];
        let capture = capture_before_clean(&root, "feat", &paths)
            .expect("ok")
            .expect("some capture");

        assert_eq!(capture.mode, CaptureMode::WorktreeCapture);
        assert!(
            capture
                .pinned_ref
                .starts_with("refs/manifold/recovery/feat/clean-"),
            "ref should use the clean- prefix: {}",
            capture.pinned_ref
        );

        // The pinned ref resolves and the snapshot tree contains BOTH files,
        // including the gitignore'd one (force-staged).
        let ref_oid = refs::read_ref(&root, &capture.pinned_ref).expect("read ref");
        assert_eq!(ref_oid, Some(capture.commit_oid.clone()));

        let tree = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", capture.commit_oid.as_str()])
            .current_dir(&root)
            .output()
            .expect("ls-tree");
        let files = String::from_utf8_lossy(&tree.stdout);
        assert!(
            files.contains("scratch.tmp"),
            "snapshot must contain scratch.tmp: {files}"
        );
        assert!(
            files.contains("build.ign"),
            "snapshot must contain the gitignore'd file (force-staged): {files}"
        );

        // Round-trip the exact bytes of the removed file from the snapshot.
        let show = Command::new("git")
            .args(["show", &format!("{}:scratch.tmp", capture.commit_oid)])
            .current_dir(&root)
            .output()
            .expect("git show");
        assert_eq!(String::from_utf8_lossy(&show.stdout), "junk-bytes\n");

        // The index must be restored (no staged leftovers from the capture).
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&root)
            .output()
            .expect("diff cached");
        assert!(
            staged.stdout.is_empty(),
            "capture must leave the index clean, staged: {}",
            String::from_utf8_lossy(&staged.stdout)
        );
    }

    #[test]
    fn capture_before_clean_preserves_existing_staged_changes() {
        let (_dir, root, _oid) = setup_repo();

        fs::write(root.join("tracked.txt"), "base\n").expect("write tracked file");
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&root)
            .output()
            .expect("add tracked file");
        Command::new("git")
            .args(["commit", "-m", "add tracked file"])
            .current_dir(&root)
            .output()
            .expect("commit tracked file");

        fs::write(root.join("tracked.txt"), "staged user work\n").expect("modify tracked file");
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&root)
            .output()
            .expect("stage user work");
        fs::write(root.join("scratch.tmp"), "scratch\n").expect("write scratch file");

        let before = Command::new("git")
            .args(["diff", "--cached", "--binary"])
            .current_dir(&root)
            .output()
            .expect("read staged diff before capture")
            .stdout;

        capture_before_clean(&root, "feat", &["scratch.tmp".to_string()])
            .expect("capture succeeds")
            .expect("snapshot created");

        let after = Command::new("git")
            .args(["diff", "--cached", "--binary"])
            .current_dir(&root)
            .output()
            .expect("read staged diff after capture")
            .stdout;
        assert_eq!(
            after, before,
            "clean capture must preserve the caller's index byte-for-byte"
        );
    }

    #[test]
    fn recovery_ref_sanitizes_colons() {
        let r = recovery_ref("ws-1", "2025-01-15T10:30:45Z");
        assert!(!r.contains(':'), "colons should be replaced: {r}");
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — clean workspace at epoch
    // -----------------------------------------------------------------------

    #[test]
    fn capture_clean_at_epoch_returns_none() {
        let (_dir, root, head_oid) = setup_repo();
        let result =
            capture_before_destroy(&root, "test-ws", &head_oid).expect("operation should succeed");
        assert!(
            result.is_none(),
            "clean workspace at epoch should return None"
        );
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — dirty workspace
    // -----------------------------------------------------------------------

    #[test]
    fn capture_dirty_workspace_returns_some() {
        let (_dir, root, head_oid) = setup_repo();

        // Create a dirty file
        fs::write(root.join("dirty.txt"), "dirty content\n").expect("operation should succeed");

        let result = capture_before_destroy(&root, "test-ws", &head_oid)
            .expect("operation should succeed")
            .expect("dirty workspace should return Some");

        assert_eq!(result.mode, CaptureMode::WorktreeCapture);
        assert!(!result.dirty_paths.is_empty());
        assert!(result.dirty_paths.iter().any(|p| p == "dirty.txt"));
        assert!(
            result
                .pinned_ref
                .starts_with("refs/manifold/recovery/test-ws/")
        );

        // Verify the pinned ref exists and resolves
        let ref_oid = refs::read_ref(&root, &result.pinned_ref).expect("operation should succeed");
        assert_eq!(ref_oid, Some(result.commit_oid));
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — untracked files
    // -----------------------------------------------------------------------

    #[test]
    fn capture_untracked_files() {
        let (_dir, root, head_oid) = setup_repo();

        // Create an untracked file (never git-added)
        fs::write(root.join("new-file.txt"), "brand new\n").expect("operation should succeed");

        let result = capture_before_destroy(&root, "test-ws", &head_oid)
            .expect("operation should succeed")
            .expect("untracked files should be captured");

        assert_eq!(result.mode, CaptureMode::WorktreeCapture);
        assert!(result.dirty_paths.iter().any(|p| p == "new-file.txt"));

        // Verify the captured commit contains the file
        let output = Command::new("git")
            .args(["show", &format!("{}:new-file.txt", result.commit_oid)])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        // git stash create uses a merge commit structure; the worktree
        // content is in the third parent's tree. Access via the commit's
        // tree directly.
        let tree_output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", result.commit_oid.as_str()])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        let tree_files = String::from_utf8_lossy(&tree_output.stdout);
        // The stash commit's tree should include the new file
        // (via the index parent or worktree parent)
        assert!(
            tree_files.contains("new-file.txt") || output.status.success(),
            "captured commit should contain untracked file"
        );
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — committed-ahead (head_only mode)
    // -----------------------------------------------------------------------

    #[test]
    fn capture_committed_ahead_pins_head() {
        let (_dir, root, base_oid) = setup_repo();

        // Make a second commit (workspace is now ahead of base epoch)
        fs::write(root.join("feature.txt"), "new feature\n").expect("operation should succeed");
        Command::new("git")
            .args(["add", "feature.txt"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "add feature"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");

        let result = capture_before_destroy(&root, "test-ws", &base_oid)
            .expect("operation should succeed")
            .expect("committed-ahead workspace should return Some");

        assert_eq!(result.mode, CaptureMode::HeadOnly);
        assert!(result.dirty_paths.is_empty());

        // The captured OID should be the current HEAD, not the base epoch
        let current_head = resolve_head(&root).expect("operation should succeed");
        assert_eq!(result.commit_oid, current_head);
        assert_ne!(result.commit_oid.as_str(), base_oid.as_str());

        // Recovery ref should exist
        let ref_oid = refs::read_ref(&root, &result.pinned_ref).expect("operation should succeed");
        assert_eq!(ref_oid, Some(result.commit_oid));
    }

    // -----------------------------------------------------------------------
    // list_dirty_paths
    // -----------------------------------------------------------------------

    #[test]
    fn list_dirty_paths_empty_when_clean() {
        let (_dir, root, _oid) = setup_repo();
        let paths = list_dirty_paths(&root).expect("operation should succeed");
        assert!(paths.is_empty());
    }

    #[test]
    fn list_dirty_paths_detects_modified() {
        let (_dir, root, _oid) = setup_repo();
        fs::write(root.join("README.md"), "# Modified\n").expect("operation should succeed");
        let paths = list_dirty_paths(&root).expect("operation should succeed");
        assert!(paths.contains(&"README.md".to_string()));
    }

    #[test]
    fn list_dirty_paths_detects_untracked() {
        let (_dir, root, _oid) = setup_repo();
        fs::write(root.join("untracked.txt"), "hi\n").expect("operation should succeed");
        let paths = list_dirty_paths(&root).expect("operation should succeed");
        assert!(paths.contains(&"untracked.txt".to_string()));
    }

    #[test]
    fn parse_uncapturable_embedded_repo_paths_extracts_paths() {
        let stderr = "error: '.tmp/sub/' does not have a commit checked out\nerror: unable to index file '.tmp/sub/'\nfatal: adding files failed\n";
        let paths = parse_uncapturable_embedded_repo_paths(stderr);
        assert_eq!(paths, vec![".tmp/sub/".to_string()]);
    }

    #[test]
    fn capture_dirty_workspace_skips_uncapturable_embedded_repo() {
        let (_dir, root, head_oid) = setup_repo();

        fs::create_dir_all(root.join(".tmp/sub")).expect("operation should succeed");
        Command::new("git")
            .args(["init"])
            .current_dir(root.join(".tmp/sub"))
            .output()
            .expect("operation should succeed");
        fs::write(root.join(".tmp/sub/file.txt"), "nested\n").expect("operation should succeed");
        fs::write(root.join("capturable.txt"), "capturable\n").expect("operation should succeed");

        let result = capture_before_destroy(&root, "test-ws", &head_oid)
            .expect("operation should succeed")
            .expect("capture should succeed with fallback exclusions");

        assert_eq!(result.mode, CaptureMode::WorktreeCapture);
        assert!(result.dirty_paths.iter().any(|p| p == "capturable.txt"));
        assert!(!result.dirty_paths.iter().any(|p| p.starts_with(".tmp/sub")));

        let tree_output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", result.commit_oid.as_str()])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        let tree_files = String::from_utf8_lossy(&tree_output.stdout);
        assert!(tree_files.contains("capturable.txt"));
    }

    // -----------------------------------------------------------------------
    // bn-2k9e: stat-cache-masked stale bytes must reach the destroy snapshot
    // -----------------------------------------------------------------------

    /// Plant stale bytes on `rel` while keeping `(size, mtime)` — and, via
    /// `core.trustCTime=false`, ctime — matching the index entry, so every
    /// status-shaped query reports the file CLEAN.
    ///
    /// Same recipe as `tests/dst_production_tier.rs`'s
    /// `corrupt_worktree_stat_masked` and `tests/sync_ff_hidden_divergence_
    /// bn_154g.rs`. `core.trustCTime=false` is load-bearing: gix compares
    /// `ctime.secs` whenever `trust_ctime` is on, independently of
    /// `core.checkStat`, and writing the stale bytes always bumps ctime.
    ///
    /// Returns whether the mask actually took (asserted by callers — a fixture
    /// that fails to mask would make these tests vacuously green).
    fn plant_masked_stale(root: &std::path::Path, rel: &str, stale: &str) -> bool {
        use std::fs::FileTimes;
        use std::time::{Duration, SystemTime};

        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git")
        };
        git(&["config", "core.checkStat", "minimal"]);
        git(&["config", "core.trustctime", "false"]);

        let abs = root.join(rel);
        assert_eq!(
            fs::read_to_string(&abs).expect("read victim").len(),
            stale.len(),
            "the stale payload must be the SAME length as the committed bytes"
        );

        let backdated = SystemTime::now() - Duration::from_mins(10);
        let times = FileTimes::new()
            .set_accessed(backdated)
            .set_modified(backdated);
        let backdate = || {
            fs::File::options()
                .write(true)
                .open(&abs)
                .expect("open victim")
                .set_times(times)
                .expect("back-date victim");
        };

        backdate();
        git(&["reset", "--mixed", "HEAD"]);
        git(&["update-index", "--refresh"]);
        fs::write(&abs, stale).expect("write stale bytes");
        backdate();

        let clean = || {
            String::from_utf8_lossy(&git(&["status", "--porcelain"]).stdout)
                .trim()
                .is_empty()
        };
        clean() && clean()
    }

    fn snapshot_blob(root: &std::path::Path, oid: &str, rel: &str) -> String {
        let out = Command::new("git")
            .args(["show", &format!("{oid}:{rel}")])
            .current_dir(root)
            .output()
            .expect("git show");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The core of bn-2k9e: a workspace every status query calls CLEAN, whose
    /// worktree secretly disagrees with HEAD, must still produce a snapshot —
    /// and that snapshot must hold the on-disk bytes, not HEAD's.
    #[test]
    fn capture_before_destroy_snapshots_stat_masked_stale_bytes() {
        let (_dir, root, head_oid) = setup_repo();

        assert!(
            plant_masked_stale(&root, "README.md", "# Stle\n"),
            "fixture failed to mask the divergence; this test would be vacuous"
        );
        assert!(
            list_dirty_paths(&root).expect("status").is_empty(),
            "the mask must make the status query report CLEAN"
        );

        let capture = capture_before_destroy(&root, "test-ws", &head_oid)
            .expect("capture must succeed")
            .expect("masked divergence must produce a snapshot");

        assert_eq!(capture.mode, CaptureMode::WorktreeCapture);
        assert!(
            capture.dirty_paths.iter().any(|p| p == "README.md"),
            "the masked path must be reported: {:?}",
            capture.dirty_paths
        );
        assert_eq!(
            snapshot_blob(&root, capture.commit_oid.as_str(), "README.md"),
            "# Stle\n",
            "the snapshot must hold the MASKED bytes, not HEAD's"
        );
    }

    /// The non-`--force` entry point: same guarantee, pinned into the ordinary
    /// destroy namespace so the destroy record can point at it.
    #[test]
    fn capture_hidden_stale_pins_into_the_destroy_namespace() {
        let (_dir, root, _oid) = setup_repo();

        assert!(plant_masked_stale(&root, "README.md", "# Stle\n"));

        let capture = capture_hidden_stale_before_destroy(&root, "test-ws")
            .expect("capture must succeed")
            .expect("masked divergence must produce a snapshot");

        assert_eq!(capture.dirty_paths, vec!["README.md".to_string()]);
        assert!(
            capture
                .pinned_ref
                .starts_with("refs/manifold/recovery/test-ws/")
                && !capture.pinned_ref.contains("/materialize-")
                && !capture.pinned_ref.contains("/clean-"),
            "must pin into the destroy namespace: {}",
            capture.pinned_ref
        );
        assert_eq!(
            snapshot_blob(&root, capture.commit_oid.as_str(), "README.md"),
            "# Stle\n"
        );
    }

    /// The negative control for the non-force path: a genuinely clean worktree
    /// must pin nothing. A guard that fired on every destroy would leave a ref
    /// per workspace behind and bury the signal it exists to raise.
    #[test]
    fn capture_hidden_stale_on_a_clean_workspace_pins_nothing() {
        let (_dir, root, _oid) = setup_repo();

        assert!(
            capture_hidden_stale_before_destroy(&root, "test-ws")
                .expect("capture must succeed")
                .is_none(),
            "a clean workspace must produce no snapshot"
        );
    }

    /// An LFS-tracked path whose HEAD blob is a *pointer* while the worktree
    /// holds the smudged bytes must NOT read as divergence — `git add` re-runs
    /// the configured clean filter, so the comparison is filter-aware by
    /// construction (bn-1ero's replay-compare lesson at this site).
    ///
    /// Skipped when `git-lfs` is not installed: without the clean filter the
    /// worktree bytes genuinely differ from HEAD, and pinning them would be the
    /// correct — if noisy — answer.
    #[test]
    fn lfs_pointer_in_head_vs_smudged_worktree_is_not_divergence() {
        if Command::new("git")
            .args(["config", "--get", "filter.lfs.clean"])
            .output()
            .is_ok_and(|o| !o.status.success())
        {
            eprintln!("skipping: git-lfs clean filter not configured");
            return;
        }
        let (_dir, root, _oid) = setup_repo();

        fs::write(
            root.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("write attrs");
        fs::write(root.join("big.bin"), "REAL-LFS-CONTENT\n").expect("write lfs file");
        for args in [vec!["add", "-A"], vec!["commit", "-m", "lfs"]] {
            Command::new("git")
                .args(&args)
                .current_dir(&root)
                .output()
                .expect("git");
        }
        // HEAD must hold the pointer while the worktree holds the real bytes.
        assert!(
            snapshot_blob(&root, "HEAD", "big.bin").starts_with("version https://git-lfs"),
            "fixture: HEAD must hold an LFS pointer"
        );
        assert_eq!(
            fs::read_to_string(root.join("big.bin")).expect("read"),
            "REAL-LFS-CONTENT\n"
        );

        let hidden = hidden_divergent_paths(&root, &[]).expect("detector must run");
        assert!(hidden.unverified.is_none(), "{:?}", hidden.unverified);
        assert!(
            !hidden.paths.iter().any(|p| p == "big.bin"),
            "a smudged LFS file must not count as divergence: {:?}",
            hidden.paths
        );
    }

    /// Paths the caller already captures are not re-reported, so a visibly
    /// dirty file never produces a second, duplicate pin.
    #[test]
    fn hidden_divergent_paths_excludes_already_captured_paths() {
        let (_dir, root, _oid) = setup_repo();
        fs::write(root.join("README.md"), "visibly edited\n").expect("write");

        let all = hidden_divergent_paths(&root, &[]).expect("detector");
        assert!(all.paths.iter().any(|p| p == "README.md"));

        let excluded = hidden_divergent_paths(&root, &["README.md".to_string()]).expect("detector");
        assert!(
            !excluded.paths.iter().any(|p| p == "README.md"),
            "{:?}",
            excluded.paths
        );
    }
}
