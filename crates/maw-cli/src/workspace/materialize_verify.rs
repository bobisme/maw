//! Post-materialization `worktree == HEAD` verification + auto-repair (bn-3gba).
//!
//! # Why this exists
//!
//! Field report 3 (bn-p3m9, continuum 2026-08-11): two freshly created
//! workspaces materialized with **HEAD and index correct at the trunk tip**
//! but a working tree carrying byte-exact *stale-epoch* blobs on exactly one
//! recent commit's path set. `git status` showed them as ordinary local
//! modifications, so the workers either had to notice unexplained reverts or
//! they would commit them straight back onto trunk.
//!
//! This module is the **mechanism-independent** defense: it converts the whole
//! class from silent corruption into a caught, repaired, and *recorded* event.
//!
//! # The mechanism that was subsequently found
//!
//! `merge`'s FF-absorb sibling materialization
//! ([`super::merge`]'s `sync_ff_paths_in_worktree`, driven from
//! `reconcile_epoch_with_branch`) applies the **global** `epoch..branch` delta
//! to every sibling and then moves that sibling's HEAD and index to the
//! absorbed tip. A sibling whose *own* base epoch is OLDER than the global
//! epoch therefore keeps stale blobs on every path the global delta does not
//! cover, while its HEAD and index say it is at the tip — byte-for-byte the
//! field-report signature. `MaterializeOp::FfAbsorbFastForward` is that call
//! site, and the repair here completes the materialization from HEAD.
//!
//! The defense is still written mechanism-independently (all four
//! clean-at-commit sites are instrumented), because a fix at that one call site
//! cannot prove no other path produces the same state.
//!
//! # The contract this asserts
//!
//! Some maw operations promise "the workspace ends **clean at a known
//! commit**". After such an operation the working tree must be byte-identical
//! to the HEAD tree. When it is not:
//!
//! 1. a loud `WARNING` naming the divergent paths goes to **stderr**,
//! 2. the pre-repair bytes are pinned to a recovery ref
//!    (`refs/manifold/recovery/<ws>/materialize-<ts>`) — the Prime Invariant is
//!    unconditional, so maw never overwrites bytes it has not first made
//!    recoverable, *even bytes it believes are wrong*. In this class those
//!    bytes are also the only forensic evidence of the (still unreproduced)
//!    mechanism. **If the capture fails, the repair is skipped** — divergence
//!    left in place and reported beats content destroyed without a snapshot,
//! 3. every divergent path that exists in HEAD is re-materialized from HEAD,
//! 4. the event is recorded in the workspace oplog (an `Annotate` op) and as a
//!    JSON artifact under
//!    `.maw/manifold/artifacts/ws/<name>/materialize-repair/<ts>.json`, so a
//!    future field report carries the evidence instead of a guess.
//!
//! # Where it is (and is NOT) called
//!
//! Called from the four sites whose contract is clean-at-commit:
//!
//! | Site | Call |
//! |------|------|
//! | `maw ws create` | `create::create_with_output` after `backend.create` |
//! | `maw ws sync` fast-forward | `sync::checks::sync_worktree_to_epoch_inner` after `checkout_detach` |
//! | FF-absorb sibling FF/replay | `merge::reconcile_epoch_with_branch` (clean siblings only) |
//! | post-merge sibling auto-rebase | `sync::auto_rebase::rebase_one_sibling` on `RebasedClean` |
//!
//! Deliberately **NOT** called where dirty state is preserved on purpose —
//! asserting there would make the repair itself a Prime-Invariant violation:
//!
//! * snapshot / preserve-and-replay paths (`working_copy::snapshot_working_copy`
//!   and the trunk dirty preserve/replay) — dirty state is the *input*;
//! * `ws sync` when it **replays local commits** (the rebase path) — the
//!   workspace can legitimately end conflicted with marker bytes on disk;
//! * conflicted workspaces after a conflicting rebase (`RebasedWithConflicts*`)
//!   — conflict-as-data is a first-class state, not divergence;
//! * FF-absorb siblings classified `FastForward { dirty: true }` — the safety
//!   predicate *proved* the dirty paths are disjoint from the FF range and the
//!   edits are preserved on purpose;
//! * `RebasedCleanRefsOnly` (worktree update deliberately skipped) — the
//!   worktree is *expected* to lag HEAD there.
//!
//! # How divergence is detected: TREES, not `git status`
//!
//! The detector hashes the working tree into a real git tree and diffs it
//! against `HEAD^{tree}` — see [`divergent_paths_by_tree`]. It deliberately
//! does **not** trust any status-shaped query, because this corruption class
//! can hide from all of them: `git status`, `git diff HEAD` and gix's
//! `status_head_to_worktree` all short-circuit on the index **stat cache**, and
//! the corrupter rewrites the index and HEAD *after* writing the stale bytes
//! (`set_head_detached` + index realignment), which can leave an index entry
//! carrying the CORRECT blob OID stamped with the STALE file's stat data. The
//! file is then "clean" to every stat-based check while being genuinely wrong.
//! `materialize_verify_survives_index_stat_cache_refresh` is the regression
//! test that pins this.
//!
//! `status_head_to_worktree()` remains only as a degraded fallback for when the
//! `git` plumbing itself fails (bn-pfh7: it must be the HEAD→worktree variant,
//! never gix's plain index→worktree `status()`).
//!
//! # Scope of "divergent"
//!
//! Only `M` (content differs), `D` (in HEAD, missing on disk) and `T`
//! (typechange) entries count. An `A` entry names a path in the worktree that
//! is **not in HEAD**, so "repair" would mean *deleting* it — exactly the
//! destructive move the Prime Invariant forbids, and a guaranteed false
//! positive on ordinary untracked scratch (and on the artifact
//! `ws create --template` writes). Renames are decomposed (`--no-renames`) into
//! a caught `D` plus an ignored `A`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use maw_git::{FileStatus, GitRepo as _, StatusEntry};
use serde::{Deserialize, Serialize};

use maw_core::model::types::WorkspaceId;
use maw_core::oplog::read::read_head;
use maw_core::oplog::types::{OpPayload, Operation};

use super::oplog_runtime::append_operation_with_runtime_checkpoint;

/// Maximum number of divergent paths printed verbatim in the WARNING before
/// the "...and N more" summary line. Mirrors `sync::checks`' dirty-path cap.
const MAX_PATHS_SHOWN: usize = 20;

/// Which clean-at-commit operation is being verified. Used verbatim in the
/// WARNING, the artifact, and the oplog annotation so a field report can say
/// *which* materialization path diverged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializeOp {
    /// `maw ws create` — a fresh worktree at the base epoch.
    Create,
    /// `maw ws sync` fast-forward path (no local commits to replay).
    SyncFastForward,
    /// FF-absorb fast-forwarded a **clean** sibling onto the absorbed tip.
    FfAbsorbFastForward,
    /// FF-absorb replayed a committed-ahead (clean) sibling onto the tip.
    FfAbsorbReplay,
    /// Post-merge sibling auto-rebase that ended clean with the worktree
    /// synced.
    AutoRebase,
}

impl MaterializeOp {
    /// Stable identifier used in messages, the artifact, and the oplog.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Create => "ws create",
            Self::SyncFastForward => "ws sync (fast-forward)",
            Self::FfAbsorbFastForward => "merge FF-absorb (sibling fast-forward)",
            Self::FfAbsorbReplay => "merge FF-absorb (sibling replay)",
            Self::AutoRebase => "merge auto-rebase (sibling)",
        }
    }

    /// The command an operator can run to re-drive this operation by hand.
    const fn manual_hint(self) -> &'static str {
        match self {
            Self::Create => "maw ws destroy <ws> && maw ws create <ws>",
            Self::SyncFastForward
            | Self::AutoRebase
            | Self::FfAbsorbFastForward
            | Self::FfAbsorbReplay => "maw ws sync <ws>",
        }
    }
}

/// One divergent path and what the repair managed to do with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergentPath {
    /// Workspace-relative path.
    pub path: String,
    /// `M` (content differs), `D` (missing from worktree), or `R` (renamed).
    pub status: String,
    /// Whether the path was re-materialized from HEAD.
    pub repaired: bool,
    /// Why the repair could not run, when `repaired == false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The recorded outcome of one post-materialization verification that found
/// divergence. Written verbatim to the artifact file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeRepairRecord {
    /// Schema version — additive changes bump the minor semantics only.
    pub schema_version: u32,
    /// Workspace whose worktree diverged.
    pub workspace: String,
    /// Which clean-at-commit operation produced the divergence.
    pub operation: MaterializeOp,
    /// Human label of `operation` (so the artifact reads without a decoder).
    pub operation_label: String,
    /// HEAD OID the worktree was verified against.
    pub head: String,
    /// ISO-8601 UTC timestamp of the verification.
    pub timestamp: String,
    /// Every divergent path with its repair outcome.
    pub paths: Vec<DivergentPath>,
    /// Recovery ref holding the pre-repair bytes, when the snapshot succeeded.
    /// `None` means the capture failed — in which case NOTHING was repaired
    /// (fail-safe: never overwrite content that was not first preserved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_ref: Option<String>,
    /// Commit OID the `preserved_ref` points at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_oid: Option<String>,
    /// Count of paths successfully re-materialized from HEAD.
    pub repaired_count: usize,
    /// Paths still divergent after the repair (empty on a full recovery).
    pub residual_paths: Vec<String>,
    /// maw version that produced this record.
    pub tool_version: String,
}

/// Directory holding this workspace's materialization-repair artifacts.
///
/// Mirrors the destroy-record convention
/// (`.maw/manifold/artifacts/ws/<ws>/destroy/`): one JSON file per event, so
/// concurrent writers never lose an earlier record to a read-modify-write.
#[must_use]
pub fn artifact_dir(root: &Path, ws_name: &str) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("materialize-repair")
}

/// Classify a status entry for the verifier.
///
/// Returns `Some(letter)` for the tracked-divergence statuses the repair can
/// act on and `None` for everything the repair must not touch (see the module
/// docs: repairing an `Added`/`Untracked` path means deleting it).
const fn divergence_letter(status: FileStatus) -> Option<&'static str> {
    match status {
        FileStatus::Modified => Some("M"),
        FileStatus::Deleted => Some("D"),
        FileStatus::Renamed => Some("R"),
        FileStatus::Added | FileStatus::Untracked => None,
    }
}

/// Filter a raw `status_head_to_worktree` set down to the entries that count
/// as post-materialization divergence.
///
/// Pure — unit-tested directly (the scoping decision is the load-bearing part
/// of this module: a false positive here would make the repair destroy real
/// work).
#[must_use]
pub fn divergent_entries(entries: &[StatusEntry]) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = entries
        .iter()
        .filter_map(|e| divergence_letter(e.status).map(|l| (e.path.clone(), l)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Compute the divergent path set by **comparing trees**, not by asking git
/// whether the worktree is dirty.
///
/// # Why a tree comparison and not `git status` (bn-3gba / bn-p3m9)
///
/// Every status-shaped check — `git status --porcelain`, `git diff HEAD`, and
/// gix's [`maw_git::GitRepo::status_head_to_worktree`] — trusts the index stat
/// cache: an index entry whose `(size, mtime, ctime, ino, …)` matches the file
/// on disk is declared unmodified **without reading the file**. The bn-p3m9
/// corrupter writes stale bytes and *then* rewrites the index and HEAD
/// (`set_head_detached` + index realignment), so the index can end up holding
/// the CORRECT blob OID stamped with the STALE file's stat data. The
/// divergence is then invisible to every stat-based check while the working
/// tree is genuinely wrong. Detecting the class therefore requires hashing the
/// bytes.
///
/// The technique is the cheapest way to force git to do exactly that:
///
/// 1. seed a throwaway index from `HEAD` (`read-tree` writes entries with
///    ZEROED stat data, so nothing can be trusted-as-clean),
/// 2. `git add -A` against it — with no usable stat cache git must re-hash
///    every file in the worktree,
/// 3. `write-tree` that index and `diff-tree` it against `HEAD^{tree}`.
///
/// The caller's real index, HEAD, and stash list are untouched (same
/// alternate-index discipline as [`super::capture::capture_before_clean`]).
///
/// Cost is one content hash of the worktree: ~100 ms for maw's own 1.4k-file
/// / 36 MB checkout, and `.gitignore`d trees (e.g. `target/`) are pruned, not
/// walked. That is the price of a detector this class cannot hide from.
///
/// Returns `(path, letter)` pairs for `M`(odified), `D`(eleted) and
/// `T`(ypechange) entries only. `A` entries are paths present in the worktree
/// but NOT in HEAD (untracked scratch) — "repairing" one means deleting it, so
/// they are never divergence. Renames are disabled (`--no-renames`) so a rename
/// decomposes into a caught `D` plus an ignored `A`.
fn divergent_paths_by_tree(ws_path: &Path) -> Result<Vec<(String, &'static str)>, String> {
    let temp = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let index = temp.path().join("index");

    let run = |args: &[&str]| -> Result<String, String> {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .current_dir(ws_path)
            .output()
            .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
        if !out.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        String::from_utf8(out.stdout).map_err(|e| format!("git {}: {e}", args.join(" ")))
    };

    run(&["read-tree", "HEAD"])?;
    run(&["add", "-A"])?;
    let worktree_tree = run(&["write-tree"])?.trim().to_owned();

    let raw = run(&[
        "diff-tree",
        "-r",
        "--no-renames",
        "--name-status",
        "-z",
        "HEAD^{tree}",
        &worktree_tree,
    ])?;

    // `-z` output is NUL-separated `status\0path\0status\0path…` — robust
    // against paths with spaces, quotes or newlines (which the default
    // c-quoting would mangle).
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut out: Vec<(String, &'static str)> = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let letter = match status.chars().next() {
            Some('M') => "M",
            Some('D') => "D",
            Some('T') => "T",
            // 'A' = not in HEAD (untracked scratch); anything else is not a
            // shape this repair can act on.
            _ => continue,
        };
        out.push((path.to_owned(), letter));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Render the WARNING body (path list, capped). Pure — unit-tested.
#[must_use]
pub fn format_divergent_paths(paths: &[DivergentPath]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = paths
        .iter()
        .take(MAX_PATHS_SHOWN)
        .map(|p| format!("  {} {}", p.status, p.path))
        .collect();
    if paths.len() > MAX_PATHS_SHOWN {
        lines.push(format!("  ...and {} more", paths.len() - MAX_PATHS_SHOWN));
    }
    lines.join("\n")
}

/// Verify that `ws_path`'s worktree matches its HEAD tree, and repair it if
/// not.
///
/// Returns `Some(record)` **only** when divergence was found (so the caller
/// can surface it); `None` on the clean fast path and on any plumbing failure.
///
/// This never returns an error and never aborts the caller's operation: a
/// verifier that can break `ws create` is worse than the bug it guards. Every
/// failure is logged via `tracing` and the operation continues.
///
/// `root` is the repo root; `ws_name` the workspace; `ws_path` its worktree.
#[expect(
    clippy::too_many_lines,
    reason = "one ordered, fail-safe sequence: status -> preserve -> repair -> re-verify -> record; splitting it hides the ordering the Prime Invariant depends on"
)]
pub fn verify_clean_materialization(
    root: &Path,
    ws_name: &str,
    ws_path: &Path,
    op: MaterializeOp,
) -> Option<MaterializeRepairRecord> {
    if !ws_path.exists() {
        return None;
    }

    let repo = match maw_git::GixRepo::open(ws_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "post-materialization verify: failed to open workspace repo"
            );
            return None;
        }
    };

    // AUTHORITATIVE detection: hash the worktree into a tree and diff it
    // against HEAD's tree. A stat-cache-based check (`git status`,
    // `status_head_to_worktree`) can be MASKED by this very corruption class —
    // see `divergent_paths_by_tree` for why. Only if the git plumbing itself
    // fails do we fall back to the status query (bn-pfh7: HEAD→worktree, never
    // the plain index→worktree `status()`), so a broken `git` degrades the
    // detector instead of disabling it.
    let divergent = match divergent_paths_by_tree(ws_path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "post-materialization verify: tree comparison failed; \
                 falling back to the (stat-cache-fallible) status query"
            );
            match repo.status_head_to_worktree() {
                Ok(entries) => divergent_entries(&entries),
                Err(e) => {
                    tracing::warn!(
                        workspace = %ws_name,
                        error = %e,
                        "post-materialization verify: status fallback also failed"
                    );
                    return None;
                }
            }
        }
    };
    if divergent.is_empty() {
        // The overwhelmingly common path: no output, no artifact, no oplog
        // entry, no recovery ref.
        return None;
    }

    let Ok(Some(head_oid)) = repo.rev_parse_opt("HEAD") else {
        tracing::warn!(
            workspace = %ws_name,
            "post-materialization verify: divergence detected but HEAD is unreadable; \
             skipping repair"
        );
        return None;
    };

    // ---- Preserve BEFORE repairing (Prime Invariant, fail-safe). ----
    //
    // The divergent bytes are, by the operation's contract, not user work — but
    // maw never overwrites bytes it has not first made recoverable, and in this
    // class those bytes are the only forensic evidence of the mechanism. A
    // capture failure DISABLES the repair: leaving the divergence in place and
    // reporting it loudly is strictly safer than destroying unpreserved content.
    let rel_paths: Vec<String> = divergent.iter().map(|(p, _)| p.clone()).collect();
    let (preserved_ref, preserved_oid, preserve_error) =
        match super::capture::capture_before_materialize_repair(ws_path, ws_name, &rel_paths) {
            Ok(Some(c)) => (
                Some(c.pinned_ref.clone()),
                Some(c.commit_oid.as_str().to_owned()),
                None,
            ),
            // `paths` is non-empty here, so `Ok(None)` is unreachable in
            // practice; treat it like a failed capture rather than assuming.
            Ok(None) => (None, None, Some("capture produced no snapshot".to_owned())),
            Err(e) => {
                tracing::warn!(
                    workspace = %ws_name,
                    error = %e,
                    "post-materialization verify: pre-repair capture failed; skipping repair"
                );
                (None, None, Some(e.to_string()))
            }
        };

    // ---- Repair: re-materialize every divergent path from HEAD. ----
    let mut paths: Vec<DivergentPath> = Vec::with_capacity(divergent.len());
    for (rel, letter) in &divergent {
        let (repaired, detail) = preserve_error.as_deref().map_or_else(
            || repair_one_path(&repo, ws_path, head_oid, rel),
            |reason| {
                (
                    false,
                    Some(format!(
                        "repair skipped: pre-repair capture failed: {reason}"
                    )),
                )
            },
        );
        paths.push(DivergentPath {
            path: rel.clone(),
            status: (*letter).to_string(),
            repaired,
            detail,
        });
    }

    // Realign the index with HEAD. The contract is clean-at-commit, so the
    // index must equal the HEAD tree; if the divergence also poisoned the
    // index, leaving it alone would keep the workspace dirty after the file
    // repair. Same primitive FF-absorb uses after its HEAD rewrite.
    if preserve_error.is_none()
        && let Err(e) = repo.unstage_all()
    {
        tracing::warn!(
            workspace = %ws_name,
            error = %e,
            "post-materialization verify: index realignment failed"
        );
    }

    // ---- Re-verify to report what (if anything) survived the repair. ----
    //
    // Same tree comparison as the detection pass — critically, the repair just
    // rewrote the index (`unstage_all`), so a stat-based re-check here would be
    // the MOST likely place to be masked and report a false "all clean".
    let residual_paths: Vec<String> = divergent_paths_by_tree(ws_path)
        .unwrap_or_default()
        .into_iter()
        .map(|(p, _)| p)
        .collect();

    let repaired_count = paths.iter().filter(|p| p.repaired).count();
    let record = MaterializeRepairRecord {
        schema_version: 1,
        workspace: ws_name.to_owned(),
        operation: op,
        operation_label: op.label().to_owned(),
        head: head_oid.to_string(),
        timestamp: super::now_timestamp_iso8601_precise(),
        paths,
        preserved_ref,
        preserved_oid,
        repaired_count,
        residual_paths,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let artifact = write_artifact(root, ws_name, &record);
    record_oplog_annotation(root, ws_name, &record, artifact.as_deref());
    print_warning(&record, artifact.as_deref());

    Some(record)
}

/// Re-materialize one path from `head_oid` into the worktree, preserving the
/// git entry mode. Returns `(repaired, detail)`.
fn repair_one_path(
    repo: &maw_git::GixRepo,
    ws_path: &Path,
    head_oid: maw_git::GitOid,
    rel: &str,
) -> (bool, Option<String>) {
    let full = ws_path.join(rel);
    match repo.read_blob_at_path(head_oid, rel) {
        Ok(Some((mode, _oid, content))) => {
            if let Some(parent) = full.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return (false, Some(format!("mkdir failed: {e}")));
            }
            match materialize_blob_with_mode(&full, mode, &content) {
                Ok(()) => (true, None),
                Err(e) => (false, Some(format!("write failed: {e}"))),
            }
        }
        // Not a blob in HEAD (deleted there, or a tree/gitlink). Repair would
        // mean removing a worktree path that HEAD does not describe — out of
        // scope (see the module docs), so report it and leave it alone.
        Ok(None) => (
            false,
            Some("path is not a blob in HEAD; left untouched".to_owned()),
        ),
        Err(e) => (false, Some(format!("read blob at HEAD failed: {e}"))),
    }
}

/// Materialize one tree blob into the worktree **preserving its git mode**.
///
/// Moved here from `merge.rs` (bn-3gba) so the FF-absorb materializer and the
/// post-materialization repair share one implementation. A plain `fs::write`
/// (a) drops the executable bit for `100755` entries and (b) turns a `120000`
/// symlink entry into a regular file whose contents are the raw link target —
/// the symlink-corruption class already fixed for `stash_apply`.
///
/// # Errors
///
/// Returns the underlying I/O error if the write, `symlink(2)`, permission
/// change, or the removal of a conflicting entry at `full` fails.
#[cfg(unix)]
pub fn materialize_blob_with_mode(
    full: &Path,
    mode: maw_git::EntryMode,
    content: &[u8],
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    match mode {
        maw_git::EntryMode::Link => {
            let target = std::str::from_utf8(content)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            // symlink(2) fails with EEXIST if anything is already there
            // (regular file from the old buggy write, or a stale link).
            if full.symlink_metadata().is_ok() {
                std::fs::remove_file(full)?;
            }
            std::os::unix::fs::symlink(target, full)
        }
        maw_git::EntryMode::Blob | maw_git::EntryMode::BlobExecutable => {
            // If the destination is currently a symlink, `fs::write` would
            // follow it and clobber the link's target file. Replace it.
            if full
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                std::fs::remove_file(full)?;
            }
            std::fs::write(full, content)?;
            let bits = if mode == maw_git::EntryMode::BlobExecutable {
                0o755
            } else {
                0o644
            };
            std::fs::set_permissions(full, std::fs::Permissions::from_mode(bits))
        }
        // Not file paths in a name-status / FF-diff set; nothing to write.
        maw_git::EntryMode::Tree | maw_git::EntryMode::Commit => Ok(()),
    }
}

/// Non-unix fallback: plain content write (no mode bits to preserve).
///
/// # Errors
///
/// Returns the underlying I/O error if the write fails.
#[cfg(not(unix))]
pub fn materialize_blob_with_mode(
    full: &Path,
    _mode: maw_git::EntryMode,
    content: &[u8],
) -> std::io::Result<()> {
    std::fs::write(full, content)
}

/// Write the record as `<artifact_dir>/<timestamp>.json`. Returns the path on
/// success. Best-effort — a failed artifact write never fails the operation.
fn write_artifact(root: &Path, ws_name: &str, record: &MaterializeRepairRecord) -> Option<PathBuf> {
    let dir = artifact_dir(root, ws_name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            workspace = %ws_name,
            error = %e,
            "post-materialization verify: failed to create artifact dir"
        );
        return None;
    }
    // Colons are legal on the filesystems maw targets but awkward in shell
    // paths; the destroy records use the same dashed form.
    let stamp = record.timestamp.replace([':', '.'], "-");
    let path = dir.join(format!("{stamp}.json"));
    let json = match serde_json::to_string_pretty(record) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "post-materialization verify: artifact serialize failed");
            return None;
        }
    };
    match std::fs::write(&path, json) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "post-materialization verify: artifact write failed"
            );
            None
        }
    }
}

/// Append an `Annotate` op so `maw ws history <ws>` shows the repair.
///
/// `Annotate` (rather than a new `OpPayload` variant) keeps the oplog schema
/// additive: an older maw reading a newer log still deserializes the entry.
fn record_oplog_annotation(
    root: &Path,
    ws_name: &str,
    record: &MaterializeRepairRecord,
    artifact: Option<&Path>,
) {
    let Ok(ws_id) = WorkspaceId::new(ws_name) else {
        return;
    };
    let previous_head = match read_head(root, &ws_id) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "post-materialization verify: oplog head unreadable; annotation skipped"
            );
            return;
        }
    };

    let mut data: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    data.insert("bone".to_owned(), "bn-3gba".into());
    data.insert(
        "operation".to_owned(),
        record.operation_label.clone().into(),
    );
    data.insert("head".to_owned(), record.head.clone().into());
    data.insert(
        "divergent_paths".to_owned(),
        serde_json::Value::from(
            record
                .paths
                .iter()
                .map(|p| serde_json::Value::from(p.path.clone()))
                .collect::<Vec<_>>(),
        ),
    );
    data.insert("repaired_count".to_owned(), record.repaired_count.into());
    if let Some(r) = record.preserved_ref.clone() {
        data.insert("preserved_ref".to_owned(), r.into());
    }
    if let Some(o) = record.preserved_oid.clone() {
        data.insert("preserved_oid".to_owned(), o.into());
    }
    data.insert(
        "residual_paths".to_owned(),
        serde_json::Value::from(
            record
                .residual_paths
                .iter()
                .map(|p| serde_json::Value::from(p.clone()))
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(path) = artifact {
        data.insert("artifact".to_owned(), path.display().to_string().into());
    }

    let op = Operation {
        parent_ids: previous_head.iter().cloned().collect(),
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Annotate {
            key: "materialize-repair".to_owned(),
            data,
        },
    };

    if let Err(e) =
        append_operation_with_runtime_checkpoint(root, &ws_id, &op, previous_head.as_ref())
    {
        tracing::warn!(
            workspace = %ws_name,
            error = %e,
            "post-materialization verify: oplog annotation append failed"
        );
    }
}

/// Print the loud operator-facing WARNING.
///
/// **stderr only**: `maw ws sync --format json` and `maw ws merge --format
/// json` must keep stdout parseable, and every hook site can be reached from a
/// JSON-output command.
fn print_warning(record: &MaterializeRepairRecord, artifact: Option<&Path>) {
    let ws = &record.workspace;
    let n = record.paths.len();
    eprintln!(
        "WARNING: workspace '{ws}' did not materialize cleanly after {} — \
         {n} tracked path(s) differed from HEAD ({}).",
        record.operation_label,
        &record.head[..12.min(record.head.len())],
    );
    eprintln!(
        "  This is the bn-p3m9 corruption class: HEAD/index correct, working tree stale. \
         maw repaired it from HEAD; no committed work was at risk."
    );
    eprintln!("{}", format_divergent_paths(&record.paths));
    if let Some(pinned) = record.preserved_ref.as_deref() {
        eprintln!("  Pre-repair bytes pinned at: {pinned}");
        eprintln!("  Inspect them with: git show {pinned}:<path>");
    } else {
        eprintln!(
            "  NO pre-repair snapshot could be taken, so NOTHING was repaired (fail-safe: maw \
             never overwrites bytes it has not first preserved)."
        );
    }
    eprintln!(
        "  Repaired {} of {n} path(s) from HEAD.",
        record.repaired_count
    );
    if record.residual_paths.is_empty() {
        eprintln!("  Workspace now matches HEAD.");
    } else {
        eprintln!(
            "  STILL DIVERGENT after repair ({}): {}",
            record.residual_paths.len(),
            record.residual_paths.join(", ")
        );
        eprintln!("  To fix by hand: {}", record.operation.manual_hint());
    }
    if let Some(path) = artifact {
        eprintln!("  Evidence: {}", path.display());
    }
    eprintln!("  Please report this with the evidence file: it is an OPEN maw bug (bn-p3m9).");
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(path: &str, status: FileStatus) -> StatusEntry {
        StatusEntry {
            path: path.to_owned(),
            status,
        }
    }

    /// The load-bearing scoping decision: only paths that exist in HEAD are
    /// considered divergent, because "repair" means "check out from HEAD".
    #[test]
    fn untracked_and_added_are_never_divergent() {
        let entries = vec![
            entry("scratch.txt", FileStatus::Untracked),
            entry("staged-new.rs", FileStatus::Added),
        ];
        assert!(
            divergent_entries(&entries).is_empty(),
            "repairing an Added/Untracked path would DELETE it — Prime Invariant violation"
        );
    }

    /// Modified / Deleted / Renamed are the bn-p3m9 signature and are all
    /// reported with their git status letter.
    #[test]
    fn modified_deleted_renamed_are_divergent() {
        let entries = vec![
            entry("src/lib.rs", FileStatus::Modified),
            entry("docs/gone.md", FileStatus::Deleted),
            entry("old/name.rs", FileStatus::Renamed),
        ];
        let got = divergent_entries(&entries);
        assert_eq!(
            got,
            vec![
                ("docs/gone.md".to_owned(), "D"),
                ("old/name.rs".to_owned(), "R"),
                ("src/lib.rs".to_owned(), "M"),
            ]
        );
    }

    /// A mixed set keeps only the tracked-divergence half.
    #[test]
    fn mixed_set_keeps_only_tracked_divergence() {
        let entries = vec![
            entry("target/debug/x", FileStatus::Untracked),
            entry("Cargo.lock", FileStatus::Modified),
            entry("notes/tmp.md", FileStatus::Untracked),
        ];
        assert_eq!(
            divergent_entries(&entries),
            vec![("Cargo.lock".to_owned(), "M")]
        );
    }

    /// An empty status set is the (overwhelmingly common) clean fast path.
    #[test]
    fn clean_status_yields_no_divergence() {
        assert!(divergent_entries(&[]).is_empty());
    }

    #[test]
    fn format_paths_is_empty_for_empty_input() {
        assert_eq!(format_divergent_paths(&[]), "");
    }

    #[test]
    fn format_paths_caps_and_summarises() {
        let paths: Vec<DivergentPath> = (0..MAX_PATHS_SHOWN + 3)
            .map(|i| DivergentPath {
                path: format!("f{i}.txt"),
                status: "M".to_owned(),
                repaired: true,
                detail: None,
            })
            .collect();
        let rendered = format_divergent_paths(&paths);
        assert_eq!(rendered.lines().count(), MAX_PATHS_SHOWN + 1);
        assert!(rendered.ends_with("  ...and 3 more"), "{rendered}");
    }

    /// Every op label is distinct and non-empty — the artifact and the WARNING
    /// must name which materialization path diverged.
    #[test]
    fn op_labels_are_distinct() {
        let ops = [
            MaterializeOp::Create,
            MaterializeOp::SyncFastForward,
            MaterializeOp::FfAbsorbFastForward,
            MaterializeOp::FfAbsorbReplay,
            MaterializeOp::AutoRebase,
        ];
        let mut labels: Vec<&str> = ops.iter().map(|o| o.label()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), before, "op labels must be distinct");
        assert!(labels.iter().all(|l| !l.is_empty()));
        assert!(ops.iter().all(|o| !o.manual_hint().is_empty()));
    }

    /// The artifact record round-trips through JSON (field reports read it).
    #[test]
    fn record_round_trips_through_json() {
        let record = MaterializeRepairRecord {
            schema_version: 1,
            workspace: "bn-3huh7".to_owned(),
            operation: MaterializeOp::Create,
            operation_label: MaterializeOp::Create.label().to_owned(),
            head: "ea85bf6ea85bf6ea85bf6ea85bf6ea85bf6ea85b".to_owned(),
            timestamp: "2026-08-11T20:23:19.000000000Z".to_owned(),
            paths: vec![DivergentPath {
                path: "AGENTS.md".to_owned(),
                status: "M".to_owned(),
                repaired: true,
                detail: None,
            }],
            preserved_ref: Some("refs/manifold/recovery/bn-3huh7/materialize-x".to_owned()),
            preserved_oid: Some("c1258c6c1258c6c1258c6c1258c6c1258c6c1258".to_owned()),
            repaired_count: 1,
            residual_paths: Vec::new(),
            tool_version: "test".to_owned(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: MaterializeRepairRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
        assert!(
            json.contains("\"operation\":\"create\""),
            "operation must serialize kebab-case: {json}"
        );
        assert!(
            json.contains("preserved_ref"),
            "the recovery pin must be in the artifact: {json}"
        );
    }

    /// A record whose capture failed omits the pin fields entirely (they are
    /// `skip_serializing_if = None`) and still round-trips — this is the
    /// fail-safe shape a reader must be able to distinguish from a repaired one.
    #[test]
    fn record_without_pin_round_trips_and_omits_fields() {
        let record = MaterializeRepairRecord {
            schema_version: 1,
            workspace: "ws".to_owned(),
            operation: MaterializeOp::SyncFastForward,
            operation_label: MaterializeOp::SyncFastForward.label().to_owned(),
            head: "0".repeat(40),
            timestamp: "2026-08-12T00:00:00.000000000Z".to_owned(),
            paths: vec![DivergentPath {
                path: "a.txt".to_owned(),
                status: "M".to_owned(),
                repaired: false,
                detail: Some("repair skipped: pre-repair capture failed: boom".to_owned()),
            }],
            preserved_ref: None,
            preserved_oid: None,
            repaired_count: 0,
            residual_paths: vec!["a.txt".to_owned()],
            tool_version: "test".to_owned(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("preserved_ref"), "{json}");
        let back: MaterializeRepairRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn artifact_dir_is_under_manifold_artifacts_ws() {
        let dir = artifact_dir(Path::new("/repo"), "alice");
        let s = dir.display().to_string();
        assert!(s.contains("artifacts"), "{s}");
        assert!(s.ends_with("ws/alice/materialize-repair"), "{s}");
    }
}
