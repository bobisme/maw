//! Garbage-collect stale `refs/manifold/head/*` and `refs/manifold/recovery/*` refs.
//!
//! Over the lifetime of a project, agent workspaces are created and destroyed.
//! The head refs (`refs/manifold/head/<name>`) and recovery refs
//! (`refs/manifold/recovery/<name>/*`) for destroyed workspaces accumulate
//! indefinitely. This module provides a GC mechanism to clean them up.
//!
//! # Head refs
//!
//! A head ref is considered stale if the corresponding workspace directory
//! (`ws/<name>/`) no longer exists.
//!
//! # Recovery refs
//!
//! Recovery refs are deleted if they are older than a configurable threshold
//! (default: 30 days), based on when the recovery pin was *created*: the
//! timestamp embedded in the ref name (`refs/manifold/recovery/<ws>/[<kind>-]<ts>`),
//! else the destroy record that claims the ref, else (legacy refs only) the
//! committer time of the pinned commit. The pinned commit's own age is not
//! the pin's age: `destroy --force` of a clean workspace pins an existing,
//! possibly months-old commit (bn-3maj).
//!
//! # Safety policy (bn-wxg28)
//!
//! - Pins of workspaces that still exist (the default workspace's dirty-trunk
//!   pins `recovery/default/*`, `materialize-*` pins of a live agent
//!   workspace, ...) are skipped unless the caller passes `include_live`
//!   (`--include-live`). Such a pin can be the only copy of displaced edits.
//! - A sweep that would drop anything while `older_than_days == 0`, or that
//!   would drop a pin younger than [`YOUNG_PIN_SECS`] or a pin of a live
//!   workspace, refuses without `force` and lists what it would drop
//!   ([`GcRefused`]). `dry_run` never refuses; it lists the same refs.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use maw_git::GitRepo as _;

use maw_core::refs;

use crate::workspace::capture::RECOVERY_PREFIX;
use crate::workspace::destroy_record;

/// Result of a ref GC pass.
#[derive(Debug, Default)]
pub struct RefGcReport {
    /// Number of stale head refs deleted.
    pub head_refs_deleted: usize,
    /// Number of old recovery snapshots (recovery refs) deleted.
    pub recovery_refs_deleted: usize,
    /// Number of recovery snapshots kept (newer than the age threshold).
    pub recovery_refs_kept: usize,
    /// Names of stale head refs that were deleted (workspace names).
    pub stale_head_names: Vec<String>,
    /// Recovery ref names that were deleted.
    pub deleted_recovery_refs: Vec<String>,
    /// Number of destroy records pruned in lockstep with their recovery refs
    /// (or because their recovery ref was already gone). See [`run`].
    pub destroy_records_deleted: usize,
    /// `(workspace, record filename)` of every destroy record pruned.
    pub deleted_destroy_records: Vec<(String, String)>,
    /// Details of every recovery ref deleted (or, in a dry run, that would
    /// be), in the same order as `deleted_recovery_refs`.
    pub dropped_pins: Vec<PinInfo>,
    /// Recovery refs of still-existing workspaces that were kept because
    /// `include_live` was off (bn-wxg28). Counted in `recovery_refs_kept`.
    pub skipped_live_pins: Vec<PinInfo>,
    /// Why this sweep needs `--force` (empty when it does not). Set on a dry
    /// run so the preview can say so; a real run without `force` fails with
    /// [`GcRefused`] instead.
    pub force_reasons: Vec<String>,
}

/// Pins younger than this (seconds) are "young": dropping one needs `--force`.
pub const YOUNG_PIN_SECS: u64 = 86_400;

/// One recovery ref considered by the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinInfo {
    /// Full ref name (`refs/manifold/recovery/<ws>/<leaf>`).
    pub ref_name: String,
    /// Workspace the pin belongs to.
    pub workspace: String,
    /// Whether that workspace currently exists.
    pub live: bool,
    /// Pin age in seconds, when known.
    pub age_secs: Option<u64>,
}

impl PinInfo {
    fn is_young(&self) -> bool {
        self.age_secs.is_some_and(|a| a < YOUNG_PIN_SECS)
    }

    /// `<ref>  (workspace <ws>[, LIVE], age <age>)` for listings.
    #[must_use]
    pub fn describe(&self) -> String {
        let age = self
            .age_secs
            .map_or_else(|| "unknown".to_string(), format_age);
        let live = if self.live { ", LIVE workspace" } else { "" };
        format!(
            "{}  (workspace {}{live}, pin age {age})",
            self.ref_name, self.workspace
        )
    }
}

/// Options for the recovery-snapshot sweep ([`run_with`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryGcOptions {
    /// Drop pins created at least this many days ago.
    pub older_than_days: u64,
    /// Report only; never delete, never refuse.
    pub dry_run: bool,
    /// Also consider pins of workspaces that still exist.
    pub include_live: bool,
    /// Allow a risky drop (see the module docs).
    pub force: bool,
}

/// A recovery-snapshot sweep refused because it needs `--force` (bn-wxg28).
#[derive(Debug, Clone)]
pub struct GcRefused {
    /// Pins the sweep would have dropped.
    pub would_drop: Vec<PinInfo>,
    /// Why `--force` is required.
    pub reasons: Vec<String>,
    /// The options of the refused run (to print the exact next commands).
    pub opts: RecoveryGcOptions,
}

impl std::fmt::Display for GcRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "gc --recovery-snapshots refused: it would drop {} recovery snapshot(s) and needs \
             --force because:",
            self.would_drop.len()
        )?;
        for r in &self.reasons {
            writeln!(f, "  - {r}")?;
        }
        writeln!(f, "Snapshots it would drop (nothing was deleted):")?;
        for p in &self.would_drop {
            writeln!(f, "  {}", p.describe())?;
        }
        writeln!(
            f,
            "A recovery snapshot can be the only copy of destroyed or displaced work."
        )?;
        writeln!(f, "  Inspect: maw ws recover")?;
        writeln!(f, "  Preview: {}", gc_command(&self.opts, true, false))?;
        write!(
            f,
            "  To drop them anyway: {}",
            gc_command(&self.opts, false, true)
        )
    }
}

impl std::error::Error for GcRefused {}

/// The `maw gc --recovery-snapshots ...` command line for `opts`.
fn gc_command(opts: &RecoveryGcOptions, dry_run: bool, force: bool) -> String {
    let mut cmd = String::from("maw gc --recovery-snapshots");
    if opts.older_than_days != 30 {
        cmd.push_str(" --older-than ");
        cmd.push_str(&opts.older_than_days.to_string());
    }
    if opts.include_live {
        cmd.push_str(" --include-live");
    }
    if dry_run {
        cmd.push_str(" --dry-run");
    }
    if force {
        cmd.push_str(" --force");
    }
    cmd
}

/// Human-readable age: `45s`, `12m`, `5h`, `3d`.
fn format_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Whether workspace `ws` currently exists. The default workspace is the repo
/// root in the consolidated layout (no `.maw/workspaces/default/`), so it is
/// resolved through `default_target_path`, never `workspace_path` alone.
fn workspace_is_live(root: &Path, ws: &str, default_names: &[String]) -> bool {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    if flavor.workspace_path(root, ws).exists() {
        return true;
    }
    default_names.iter().any(|d| d == ws) && flavor.default_target_path(root, ws).exists()
}

/// Names that denote the default workspace: `default`, plus the configured
/// default workspace name when `.maw.toml` is readable.
fn default_workspace_names(root: &Path) -> Vec<String> {
    let mut names = vec!["default".to_string()];
    if let Ok(cfg) = crate::workspace::MawConfig::load(root) {
        let d = cfg.default_workspace().to_string();
        if !names.contains(&d) {
            names.push(d);
        }
    }
    names
}

/// Why dropping `drops` needs `--force` (empty = safe without it).
fn force_reasons(drops: &[PinInfo], older_than_days: u64) -> Vec<String> {
    let mut reasons = Vec::new();
    if drops.is_empty() {
        return reasons;
    }
    if older_than_days == 0 {
        reasons.push("--older-than 0 drops every recovery snapshot, however new".to_string());
    }
    let young = drops.iter().filter(|p| p.is_young()).count();
    if young > 0 {
        reasons.push(format!(
            "{young} snapshot(s) were pinned less than 1 day ago"
        ));
    }
    let live = drops.iter().filter(|p| p.live).count();
    if live > 0 {
        reasons.push(format!(
            "{live} snapshot(s) belong to workspaces that still exist (--include-live)"
        ));
    }
    reasons
}

/// Count stale head refs (refs for workspaces that no longer exist).
///
/// Used by `maw doctor` to report stale refs without deleting them.
/// # Errors
///
/// Returns an error if stale refs cannot be inspected.
pub fn count_stale_head_refs(root: &Path) -> Result<usize> {
    let repo =
        maw_git::GixRepo::open(root).map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;
    let head_refs = repo
        .list_refs(refs::HEAD_PREFIX)
        .map_err(|e| anyhow::anyhow!("list_refs failed: {e}"))?;

    let mut count = 0;
    for (ref_name, _oid) in &head_refs {
        let ws_name = ref_name
            .as_str()
            .strip_prefix(refs::HEAD_PREFIX)
            .unwrap_or("");
        if ws_name.is_empty() {
            continue;
        }
        let ws_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
            .workspace_path(root, ws_name);
        if !ws_dir.exists() {
            count += 1;
        }
    }
    Ok(count)
}

/// Workspace names that an in-flight, non-terminal, *live* merge has frozen
/// as sources. A head ref for such a workspace must NOT be pruned: the
/// running merge legitimately owns the oplog head and will append to it
/// post-COMMIT. Deleting it here would re-introduce the bn-cm63 race from
/// the GC side. Orphaned/indeterminate merge-state does NOT protect a head
/// ref (the merge will never complete), so its dangling refs are still
/// reclaimed — that is the whole point of self-healing GC.
fn live_merge_source_names(root: &Path) -> std::collections::HashSet<String> {
    use maw_core::merge_state::{DEFAULT_STALE_AFTER_SECS, MergeStateFile, Staleness};

    let mut names = std::collections::HashSet::new();
    let state_path = MergeStateFile::default_path(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
    );
    let Ok(state) = MergeStateFile::read(&state_path) else {
        return names;
    };
    if state.phase.is_terminal() {
        return names;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if matches!(
        state.staleness(now, DEFAULT_STALE_AFTER_SECS),
        Staleness::Live
    ) {
        for s in &state.sources {
            names.insert(s.as_str().to_string());
        }
    }
    names
}

/// bn-3ppf lock audit: every deleting caller (`maw gc`, with or without
/// `--recovery-snapshots`) holds the repo epoch lock (main.rs); `--dry-run`
/// is lock-free but read-only.
///
/// Prune dangling oplog head refs: `refs/manifold/head/<name>` (and the
/// other refs owned by that workspace) when `ws/<name>/` no longer exists
/// and the workspace is not a source of a *live* in-flight merge.
///
/// Extracted so plain `maw gc` can self-heal leaked head refs (bn-cm63)
/// without also running the recovery-ref age sweep that only `maw gc --recovery-snapshots`
/// should perform.
fn prune_dangling_head_refs(
    repo: &maw_git::GixRepo,
    root: &Path,
    dry_run: bool,
    report: &mut RefGcReport,
) -> Result<()> {
    let head_refs = repo
        .list_refs(refs::HEAD_PREFIX)
        .map_err(|e| anyhow::anyhow!("list_refs failed for head refs: {e}"))?;

    let protected = live_merge_source_names(root);

    for (ref_name, _oid) in &head_refs {
        let ws_name = ref_name
            .as_str()
            .strip_prefix(refs::HEAD_PREFIX)
            .unwrap_or("");
        if ws_name.is_empty() {
            continue;
        }
        let ws_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
            .workspace_path(root, ws_name);
        if ws_dir.exists() {
            continue;
        }
        if protected.contains(ws_name) {
            // A live merge owns this oplog head right now. Skip it; it is
            // not dangling — it will be reclaimed on a later GC once the
            // merge (and any subsequent destroy) settles.
            continue;
        }
        report.stale_head_names.push(ws_name.to_string());
        if !dry_run {
            // Delete every ref owned by this (gone) workspace. Iterates
            // the single source of truth in `workspace_owned_refs` so a
            // new ref kind is a one-line change there (bn-3kcp). The
            // head ref we discovered via list_refs is one of the entries
            // in that set — delete_ref is idempotent so re-deleting it
            // is harmless.
            for owned in refs::workspace_owned_refs(ws_name) {
                let _ = refs::delete_ref(root, &owned);
            }
        }
        report.head_refs_deleted += 1;
    }
    Ok(())
}

/// Prune only dangling oplog head refs (no recovery-ref sweep).
///
/// This is what plain `maw gc` runs so the documented cleanup path actually
/// clears the `maw doctor` "stale head refs" warning, and so already-leaked
/// or legacy dangling head refs self-heal (bn-cm63). `maw gc --recovery-snapshots` still
/// additionally sweeps old recovery refs via [`run`].
///
/// # Errors
///
/// Returns an error if the repository cannot be opened or refs cannot be
/// listed.
pub fn run_head_refs_only(root: &Path, dry_run: bool) -> Result<RefGcReport> {
    let repo =
        maw_git::GixRepo::open(root).map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;
    let mut report = RefGcReport::default();
    prune_dangling_head_refs(&repo, root, dry_run, &mut report)?;
    Ok(report)
}

/// CLI entry point for plain `maw gc`'s head-ref self-heal pass (bn-cm63).
///
/// Prints a concise summary only when something was (or would be) cleaned,
/// so the common no-op case stays quiet and does not clutter `maw gc`
/// output.
#[allow(clippy::missing_errors_doc)]
pub fn run_head_refs_cli(root: &Path, dry_run: bool) -> Result<()> {
    let report = run_head_refs_only(root, dry_run)?;
    if report.head_refs_deleted == 0 {
        return Ok(());
    }
    if dry_run {
        println!(
            "Would prune {} dangling head ref(s) for non-existent workspaces:",
            report.head_refs_deleted
        );
        for name in &report.stale_head_names {
            println!("  refs/manifold/head/{name}");
        }
        println!("To apply: maw gc");
    } else {
        println!(
            "Pruned {} dangling head ref(s) for non-existent workspaces.",
            report.head_refs_deleted
        );
    }
    Ok(())
}

/// Run ref GC with the legacy library defaults: live-workspace pins are
/// skipped and no `--force` gate applies (callers that want the CLI's safety
/// policy use [`run_with`]).
///
/// See [`run_with`] for what is deleted.
#[allow(clippy::missing_errors_doc)]
pub fn run(root: &Path, older_than_days: u64, dry_run: bool) -> Result<RefGcReport> {
    run_with(
        root,
        &RecoveryGcOptions {
            older_than_days,
            dry_run,
            include_live: false,
            force: true,
        },
    )
}

/// Run ref GC: delete stale head refs and old recovery refs, and keep destroy
/// records coherent with the recovery refs they claim (bn-3uou).
///
/// - Head refs are deleted if `ws/<name>/` does not exist.
/// - Recovery refs are deleted if the pin was created more than
///   `older_than_days` days ago (default: 30). Pin creation time comes from
///   the ref-name timestamp, else the claiming destroy record, else the
///   pinned commit's committer time (bn-3maj).
/// - Pins of workspaces that still exist are kept unless `include_live`
///   (bn-wxg28).
/// - If the drop set is non-empty and `older_than_days == 0`, or it contains a
///   pin younger than [`YOUNG_PIN_SECS`] or a live workspace's pin, a
///   non-dry run without `force` deletes NOTHING and fails with [`GcRefused`]
///   (bn-wxg28).
/// - Destroy records (the `maw ws recover` audit trail under
///   `.maw/manifold/artifacts/ws/<name>/destroy/`) are pruned in lockstep so
///   the system never lands in the incoherent "record claims a snapshot whose
///   recovery ref was swept" state that a later `git gc --prune` would turn
///   into a dangling pointer. A record is pruned when, for a workspace that no
///   longer exists, either its recovery ref is being swept in this same pass,
///   or its recovery ref is already gone (a prior sweep / manual delete) and
///   the record itself is older than `older_than_days`. Records for
///   still-existing workspaces, and `none`-mode records that never pinned a
///   snapshot, are never touched.
///
/// If `dry_run` is true, nothing is deleted but the report shows what would be.
#[allow(clippy::missing_errors_doc)]
pub fn run_with(root: &Path, opts: &RecoveryGcOptions) -> Result<RefGcReport> {
    let RecoveryGcOptions {
        older_than_days,
        dry_run,
        include_live,
        force,
    } = *opts;
    let repo =
        maw_git::GixRepo::open(root).map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;

    let mut report = RefGcReport::default();

    // --- Recovery refs: plan (nothing is deleted until the force gate) ---
    let recovery_prefix = "refs/manifold/recovery/";
    let recovery_refs = repo
        .list_refs(recovery_prefix)
        .map_err(|e| anyhow::anyhow!("list_refs failed for recovery refs: {e}"))?;

    // Every recovery ref that currently exists, captured before deletion so the
    // record-pruning pass can tell "kept (recent pin)" from "already gone".
    let existing_recovery_refs: HashSet<String> = recovery_refs
        .iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX epoch")?
        .as_secs();
    let cutoff = now.saturating_sub(older_than_days.saturating_mul(86_400));

    // bn-3maj: age each pin by when the PIN was created, not by the pinned
    // commit's committer time. A `destroy --force` of a clean workspace pins
    // its existing (possibly months-old) HEAD; aging by commit time would let
    // the next sweep delete a recovery point created minutes ago.
    // Evidence, in order: timestamp in the ref name (every production
    // writer embeds one), then the destroy record claiming the ref, then —
    // last resort for legacy/hand-made refs — the commit time. With no
    // evidence at all the ref is kept.
    let record_claim_times = destroy_record_claim_times(root)?;
    let default_names = default_workspace_names(root);

    for (ref_name, oid) in &recovery_refs {
        let name = ref_name.as_str();
        let workspace = name
            .strip_prefix(recovery_prefix)
            .and_then(|rest| rest.rsplit_once('/'))
            .map_or("", |(ws, _)| ws)
            .to_string();
        // bn-wxg28: a pin whose workspace cannot be parsed is treated as
        // live (fail closed: never swept by default).
        let live = workspace.is_empty() || workspace_is_live(root, &workspace, &default_names);
        let pin_ts = pin_created_at_from_ref_name(name)
            .or_else(|| record_claim_times.get(name).copied())
            .or_else(|| get_commit_timestamp(&repo, *oid));
        let info = PinInfo {
            ref_name: name.to_string(),
            workspace,
            live,
            age_secs: pin_ts.map(|ts| now.saturating_sub(ts)),
        };
        match pin_ts {
            Some(ts) if ts <= cutoff => {
                if live && !include_live {
                    report.recovery_refs_kept += 1;
                    report.skipped_live_pins.push(info);
                } else {
                    report.dropped_pins.push(info);
                }
            }
            Some(_) | None => {
                // Recent enough or unknown pin age — keep conservatively.
                report.recovery_refs_kept += 1;
            }
        }
    }

    // --- bn-wxg28 force gate: refuse BEFORE deleting anything ---
    report.force_reasons = force_reasons(&report.dropped_pins, older_than_days);
    if !dry_run && !force && !report.force_reasons.is_empty() {
        return Err(anyhow::Error::new(GcRefused {
            would_drop: report.dropped_pins,
            reasons: report.force_reasons,
            opts: *opts,
        }));
    }

    // --- Head refs ---
    prune_dangling_head_refs(&repo, root, dry_run, &mut report)?;

    // --- Recovery refs: delete ---
    for info in &report.dropped_pins {
        if !dry_run {
            refs::delete_ref(root, &info.ref_name).map_err(|e| {
                anyhow::anyhow!("failed to delete recovery ref {}: {e}", info.ref_name)
            })?;
        }
        report.deleted_recovery_refs.push(info.ref_name.clone());
        report.recovery_refs_deleted += 1;
    }

    // --- Destroy records (coherence with recovery refs) ---
    let swept_recovery_refs: HashSet<String> =
        report.deleted_recovery_refs.iter().cloned().collect();
    prune_desynced_destroy_records(
        root,
        &existing_recovery_refs,
        &swept_recovery_refs,
        cutoff,
        dry_run,
        &mut report,
    )?;

    Ok(report)
}

/// Prune destroy records so they stay coherent with recovery refs.
///
/// Driven by two ref-name sets from the recovery-ref pass:
/// - `existing_recovery_refs`: every recovery ref that existed at the start of
///   this GC (before any deletion).
/// - `swept_recovery_refs`: the subset being deleted in this pass.
///
/// For each destroyed workspace (directory gone) and each of its records that
/// claims a recovery ref:
/// - claimed ref is being swept now → prune the record in lockstep;
/// - claimed ref still exists and is not being swept → keep (recent pin);
/// - claimed ref is already gone → the record is desynced; prune it when it is
///   older than the cutoff.
///
/// `none`-mode records (no snapshot pinned) and records for still-existing
/// workspaces are never touched.
fn prune_desynced_destroy_records(
    root: &Path,
    existing_recovery_refs: &HashSet<String>,
    swept_recovery_refs: &HashSet<String>,
    cutoff: u64,
    dry_run: bool,
    report: &mut RefGcReport,
) -> Result<()> {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);

    for ws in destroy_record::list_destroyed_workspaces(root)? {
        // Never touch records for a workspace that currently exists.
        if flavor.workspace_path(root, &ws).exists() {
            continue;
        }
        for filename in destroy_record::list_record_files(root, &ws)? {
            let Ok(record) = destroy_record::read_record(root, &ws, &filename) else {
                continue;
            };
            let Some(claimed) = record.recovery_ref() else {
                // `none`-mode record: no snapshot, pure audit trail. Leave it.
                continue;
            };
            let prune = if swept_recovery_refs.contains(claimed) {
                // Ref is being swept in this pass — prune the record too so no
                // unpinned-but-claimed state is ever created.
                true
            } else if existing_recovery_refs.contains(claimed) {
                // Ref still pinned and newer than the cutoff — keep both.
                false
            } else {
                // Ref already gone (prior sweep / manual delete): the record is
                // desynced. Age-gate its removal by the record's own timestamp.
                record
                    .destroyed_at_epoch_secs()
                    .is_some_and(|ts| ts <= cutoff)
            };
            if prune {
                if !dry_run {
                    destroy_record::remove_record(root, &ws, &filename)?;
                }
                report.destroy_records_deleted += 1;
                report.deleted_destroy_records.push((ws.clone(), filename));
            }
        }
    }
    Ok(())
}

/// Creation time (unix seconds) of a recovery pin, parsed from its ref name.
///
/// Every production writer names pins
/// `refs/manifold/recovery/<ws>/[<kind>-]<YYYY-MM-DD>T<HH-MM-SS>[.<frac>]Z`
/// (the ISO-8601 capture time with `:` replaced by `-`; `<kind>` is e.g.
/// `clean`, `materialize`, `invariant`). Returns `None` for any other shape
/// (legacy or hand-made names), in which case the caller falls back to other
/// evidence of pin age.
fn pin_created_at_from_ref_name(ref_name: &str) -> Option<u64> {
    let rest = ref_name.strip_prefix(RECOVERY_PREFIX)?;
    let (_ws, leaf) = rest.rsplit_once('/')?;
    let bytes = leaf.as_bytes();
    // The timestamp is either the whole leaf or follows a `<kind>-` prefix.
    (0..bytes.len())
        .filter(|&i| i == 0 || bytes[i - 1] == b'-')
        .find_map(|i| parse_ref_safe_timestamp(&leaf[i..]))
}

/// Parse `YYYY-MM-DDTHH-MM-SS[.digits]Z` (a ref-safe ISO-8601 UTC timestamp).
fn parse_ref_safe_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[13] != b'-' || b[16] != b'-' {
        return None;
    }
    let tail = s.get(19..)?;
    let tail_ok = tail == "Z"
        || tail
            .strip_prefix('.')
            .and_then(|t| t.strip_suffix('Z'))
            .is_some_and(|frac| !frac.is_empty() && frac.bytes().all(|c| c.is_ascii_digit()));
    if !tail_ok {
        return None;
    }
    let iso = format!("{}:{}:{}", &s[..13], &s[14..16], &s[17..]);
    destroy_record::parse_iso8601_utc_secs(&iso)
}

/// Map each recovery ref claimed by a destroy record to the (latest) time a
/// record claiming it was written. Used as the fallback pin-creation time for
/// recovery refs whose names carry no parseable timestamp.
fn destroy_record_claim_times(root: &Path) -> Result<HashMap<String, u64>> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for ws in destroy_record::list_destroyed_workspaces(root)? {
        for filename in destroy_record::list_record_files(root, &ws)? {
            let Ok(record) = destroy_record::read_record(root, &ws, &filename) else {
                continue;
            };
            let (Some(claimed), Some(ts)) =
                (record.recovery_ref(), record.destroyed_at_epoch_secs())
            else {
                continue;
            };
            let slot = out.entry(claimed.to_string()).or_insert(ts);
            *slot = (*slot).max(ts);
        }
    }
    Ok(out)
}

/// Get the commit timestamp (committer date as unix epoch seconds) for a given OID.
///
/// Returns `None` if the commit cannot be read or the timestamp is negative
/// (which we treat as "missing"). Replaces `git log -1 --format=%ct <oid>`.
fn get_commit_timestamp(repo: &maw_git::GixRepo, oid: maw_git::GitOid) -> Option<u64> {
    let info = repo.read_commit(oid).ok()?;
    u64::try_from(info.committer_time).ok()
}

/// Fail with [`GcRefused`] if running `opts` would need `--force` (bn-wxg28).
///
/// Read-only. `maw gc` calls this before ANY phase (epoch GC included)
/// deletes something, so a refused run is a no-op.
#[allow(clippy::missing_errors_doc)]
pub fn check_force_gate(root: &Path, opts: &RecoveryGcOptions) -> Result<()> {
    if opts.dry_run || opts.force {
        return Ok(());
    }
    let preview = run_with(
        root,
        &RecoveryGcOptions {
            dry_run: true,
            ..*opts
        },
    )?;
    if preview.force_reasons.is_empty() {
        return Ok(());
    }
    Err(anyhow::Error::new(GcRefused {
        would_drop: preview.dropped_pins,
        reasons: preview.force_reasons,
        opts: *opts,
    }))
}

/// CLI entry point for `maw gc --recovery-snapshots`.
#[allow(clippy::missing_errors_doc)]
pub fn run_cli(root: &Path, opts: &RecoveryGcOptions) -> Result<()> {
    let report = run_with(root, opts)?;
    let older_than_days = opts.older_than_days;
    let dry_run = opts.dry_run;

    let live_note = || {
        if !report.skipped_live_pins.is_empty() {
            println!(
                "Kept {} recovery snapshot(s) of workspaces that still exist \
                 (to include them: {}):",
                report.skipped_live_pins.len(),
                gc_command(
                    &RecoveryGcOptions {
                        include_live: true,
                        dry_run: true,
                        ..*opts
                    },
                    true,
                    false
                )
            );
            for p in &report.skipped_live_pins {
                println!("  {}", p.describe());
            }
        }
    };

    if report.head_refs_deleted == 0
        && report.recovery_refs_deleted == 0
        && report.destroy_records_deleted == 0
    {
        println!("No stale refs found. Nothing to clean up.");
        live_note();
        return Ok(());
    }

    if dry_run {
        println!("Ref GC preview (dry run, nothing deleted):");
        if !report.stale_head_names.is_empty() {
            println!(
                "  Would delete {} stale head ref(s):",
                report.head_refs_deleted
            );
            for name in &report.stale_head_names {
                println!("    refs/manifold/head/{name}");
            }
        }
        if !report.dropped_pins.is_empty() {
            println!(
                "  Would delete {} recovery snapshot(s) older than {older_than_days} day(s) \
                 ({} kept):",
                report.recovery_refs_deleted, report.recovery_refs_kept
            );
            for p in &report.dropped_pins {
                println!("    {}", p.describe());
            }
        }
        if !report.deleted_destroy_records.is_empty() {
            println!(
                "  Would prune {} destroy record(s) whose recovery snapshot is (or is being) \
                 removed:",
                report.destroy_records_deleted
            );
            for (ws, file) in &report.deleted_destroy_records {
                println!("    {ws}/{file}");
            }
        }
        live_note();
        if report.force_reasons.is_empty() {
            println!("To apply: {}", gc_command(opts, false, false));
        } else {
            println!("Applying this needs --force because:");
            for r in &report.force_reasons {
                println!("  - {r}");
            }
            println!("Inspect first: maw ws recover");
            println!("To apply: {}", gc_command(opts, false, true));
        }
    } else {
        // Recovery refs (the snapshot pins) and destroy records (the
        // `maw ws recover` audit trail) are pruned together so they never
        // disagree — this is what lets `maw doctor`'s abandoned-with-snapshot
        // count actually drop after a GC.
        println!(
            "Pruned {} stale head ref(s); removed {} recovery snapshot(s) and {} destroy \
             record(s) older than {older_than_days} day(s) ({} snapshot(s) kept).",
            report.head_refs_deleted,
            report.recovery_refs_deleted,
            report.destroy_records_deleted,
            report.recovery_refs_kept
        );
        for p in &report.dropped_pins {
            println!("  removed {}", p.describe());
        }
        live_note();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn setup_repo() -> (TempDir, String) {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        fs::write(root.join("README.md"), "# test\n").expect("operation should succeed");
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        let oid = String::from_utf8(out.stdout)
            .expect("operation should succeed")
            .trim()
            .to_string();

        // Create ws/ directory structure
        fs::create_dir_all(root.join("ws")).expect("operation should succeed");

        (dir, oid)
    }

    #[test]
    fn ref_gc_handles_extreme_age_threshold_without_overflow() {
        let (dir, _) = setup_repo();
        let root = dir.path();

        let report = run(root, u64::MAX, true).expect("ref gc should not overflow");
        assert_eq!(report.head_refs_deleted, 0);
        assert_eq!(report.recovery_refs_deleted, 0);
    }

    #[test]
    fn no_stale_refs_is_noop() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 0);
        assert_eq!(report.recovery_refs_deleted, 0);
    }

    #[test]
    fn stale_head_ref_deleted_when_workspace_gone() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Create a head ref for a workspace that does not exist
        refs::write_ref(
            root,
            &refs::workspace_head_ref("gone-agent"),
            &maw_core::model::types::GitOid::new(&oid).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        // Verify the ref exists
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
                .expect("operation should succeed")
                .is_some()
        );

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["gone-agent"]);

        // Ref should be gone
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
                .expect("operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn head_ref_kept_when_workspace_exists() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Create workspace directory
        fs::create_dir_all(root.join("ws/active-agent")).expect("operation should succeed");

        // Create a head ref for the workspace
        refs::write_ref(
            root,
            &refs::workspace_head_ref("active-agent"),
            &maw_core::model::types::GitOid::new(&oid).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 0);

        // Ref should still exist
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("active-agent"))
                .expect("operation should succeed")
                .is_some()
        );
    }

    #[test]
    fn dry_run_does_not_delete() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        refs::write_ref(
            root,
            &refs::workspace_head_ref("gone-agent"),
            &maw_core::model::types::GitOid::new(&oid).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        let report = run(root, 30, true).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 1);

        // Ref should still exist because it was a dry run
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
                .expect("operation should succeed")
                .is_some()
        );
    }

    #[test]
    fn count_stale_head_refs_returns_correct_count() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("operation should succeed");

        // Two stale refs
        refs::write_ref(root, &refs::workspace_head_ref("stale-1"), &git_oid)
            .expect("operation should succeed");
        refs::write_ref(root, &refs::workspace_head_ref("stale-2"), &git_oid)
            .expect("operation should succeed");

        // One active ref (workspace exists)
        fs::create_dir_all(root.join("ws/active")).expect("operation should succeed");
        refs::write_ref(root, &refs::workspace_head_ref("active"), &git_oid)
            .expect("operation should succeed");

        let count = count_stale_head_refs(root).expect("operation should succeed");
        assert_eq!(count, 2);
    }

    #[test]
    fn old_recovery_ref_deleted() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("operation should succeed");

        // Create a recovery ref. The commit is from "just now", so with
        // older_than_days=0 it should be deleted.
        let recovery_ref = "refs/manifold/recovery/gone-ws/20250101-000000";
        refs::write_ref(root, recovery_ref, &git_oid).expect("operation should succeed");

        let report = run(root, 0, false).expect("operation should succeed");
        assert_eq!(report.recovery_refs_deleted, 1);

        // Ref should be gone
        assert!(
            refs::read_ref(root, recovery_ref)
                .expect("operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn recent_recovery_ref_kept() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("operation should succeed");

        // Create a recovery ref. The commit is from "just now", so with
        // older_than_days=30 it should be kept.
        let recovery_ref = "refs/manifold/recovery/some-ws/20260301-000000";
        refs::write_ref(root, recovery_ref, &git_oid).expect("operation should succeed");

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.recovery_refs_deleted, 0);

        // Ref should still exist
        assert!(
            refs::read_ref(root, recovery_ref)
                .expect("operation should succeed")
                .is_some()
        );
    }

    // --- bn-cm63: plain `maw gc` head-ref self-heal + live-merge guard ---

    /// Write a `.manifold/merge-state.json` owned by *this* process (so
    /// `staleness` classifies it `Live`) listing `source` as a frozen
    /// source at the `validate` phase.
    fn write_live_merge_state(root: &Path, source: &str) {
        use maw_core::merge_state::{MergePhase, MergeStateFile};
        use maw_core::model::types::{EpochId, WorkspaceId};

        let manifold = root.join(".manifold");
        fs::create_dir_all(&manifold).expect("create .manifold");
        let epoch = EpochId::new(&"a".repeat(40)).expect("epoch");
        let mut state =
            MergeStateFile::new(vec![WorkspaceId::new(source).expect("ws id")], epoch, 0);
        state.stamp_owner(); // pid == our pid -> Liveness::Alive -> Live
        state
            .advance(MergePhase::Build, 1)
            .and_then(|()| state.advance(MergePhase::Validate, 2))
            .expect("advance to validate");
        state
            .write_atomic(&MergeStateFile::default_path(&manifold))
            .expect("write merge-state");
    }

    #[test]
    fn plain_gc_prunes_dangling_head_ref() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");

        refs::write_ref(root, &refs::workspace_head_ref("ghost"), &git_oid).expect("write ref");

        // Plain gc path: head refs only, no recovery sweep.
        let report = run_head_refs_only(root, false).expect("run head refs");
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["ghost"]);
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("ghost"))
                .expect("read")
                .is_none(),
            "plain gc must prune the dangling head ref"
        );
    }

    #[test]
    fn live_merge_source_head_ref_is_protected_from_gc() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");

        // A head ref whose workspace dir is gone, but a LIVE merge (owned by
        // this process) has it frozen as a source. It must NOT be pruned —
        // pruning it would re-introduce the bn-cm63 race from the GC side.
        refs::write_ref(root, &refs::workspace_head_ref("inflight"), &git_oid).expect("write ref");
        write_live_merge_state(root, "inflight");

        let report = run_head_refs_only(root, false).expect("run head refs");
        assert_eq!(
            report.head_refs_deleted, 0,
            "a live merge's source head ref must be protected from GC"
        );
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("inflight"))
                .expect("read")
                .is_some(),
            "live-merge source head ref must survive gc"
        );
    }

    #[test]
    fn non_source_dangling_head_ref_pruned_even_with_live_merge() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");

        // Live merge for "inflight"; a *different* workspace "ghost" is gone
        // and is NOT a source — it must still be pruned.
        refs::write_ref(root, &refs::workspace_head_ref("ghost"), &git_oid).expect("write ref");
        write_live_merge_state(root, "inflight");

        let report = run_head_refs_only(root, false).expect("run head refs");
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["ghost"]);
    }

    // --- bn-3uou: destroy-record coherence with recovery refs ---

    use crate::workspace::capture::{CaptureMode, CaptureResult};
    use crate::workspace::destroy_record::{self, DestroyReason, DestroyRecord, RecordCaptureMode};

    /// Create a destroyed-workspace pair (recovery ref + matching destroy
    /// record) pinned at `oid`. The record is written "now" via the real
    /// writer so its `snapshot_ref` is exactly the ref we created.
    fn seed_destroyed_with_ref(root: &Path, ws: &str, oid: &str, ref_ts: &str) -> String {
        let git_oid = maw_core::model::types::GitOid::new(oid).expect("oid");
        let ref_name = format!("refs/manifold/recovery/{ws}/{ref_ts}");
        refs::write_ref(root, &ref_name, &git_oid).expect("write recovery ref");
        let capture = CaptureResult {
            commit_oid: git_oid.clone(),
            pinned_ref: ref_name.clone(),
            dirty_paths: vec!["draft.txt".to_string()],
            mode: CaptureMode::WorktreeCapture,
        };
        let base = maw_core::model::types::EpochId::new(&"a".repeat(40)).expect("epoch");
        destroy_record::write_destroy_record(
            root,
            ws,
            &base,
            &git_oid,
            Some(&capture),
            DestroyReason::Destroy,
        )
        .expect("write destroy record");
        ref_name
    }

    /// Write a destroy record whose claimed recovery ref does NOT exist (the
    /// desynced / already-swept state), with a caller-chosen `destroyed_at`.
    fn seed_orphaned_record(root: &Path, ws: &str, destroyed_at: &str) {
        let rec = DestroyRecord {
            workspace_id: ws.to_string(),
            destroyed_at: destroyed_at.to_string(),
            final_head: "b".repeat(40),
            final_head_ref: None,
            snapshot_oid: Some("c".repeat(40)),
            snapshot_ref: Some(format!("refs/manifold/recovery/{ws}/gone-forever")),
            capture_mode: RecordCaptureMode::DirtySnapshot,
            dirty_files: vec![],
            base_epoch: "a".repeat(40),
            destroy_reason: DestroyReason::Destroy,
            tool_version: "test".to_string(),
        };
        let dir = destroy_record::destroy_dir(root, ws);
        fs::create_dir_all(&dir).expect("create destroy dir");
        let fname = format!("{}.json", destroyed_at.replace(':', "-"));
        fs::write(
            dir.join(&fname),
            serde_json::to_string_pretty(&rec).expect("serialize record"),
        )
        .expect("write record file");
    }

    #[test]
    fn gc_prunes_record_when_recovery_ref_is_swept() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let ref_name = seed_destroyed_with_ref(root, "alice", &oid, "20260101-000000");

        // older_than 0 → the (now-dated) commit is at/older than the cutoff,
        // so the ref is swept AND its record pruned in lockstep.
        let report = run(root, 0, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert_eq!(report.destroy_records_deleted, 1);

        assert!(
            refs::read_ref(root, &ref_name).expect("read ref").is_none(),
            "recovery ref must be swept"
        );
        assert!(
            destroy_record::list_record_files(root, "alice")
                .expect("list")
                .is_empty(),
            "destroy record must be pruned in lockstep with its ref"
        );
    }

    #[test]
    fn gc_keeps_record_when_recovery_ref_is_recent() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        seed_destroyed_with_ref(root, "bob", &oid, "20260101-000000");

        // older_than 30 → the just-created commit is newer than the cutoff,
        // so both the ref and its record are kept.
        let report = run(root, 30, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 0);
        assert_eq!(report.destroy_records_deleted, 0);
        assert_eq!(
            destroy_record::list_record_files(root, "bob")
                .expect("list")
                .len(),
            1,
            "recent record must be kept"
        );
    }

    #[test]
    fn gc_does_not_touch_records_for_live_workspace() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        // A LIVE workspace directory exists AND has an orphaned old record.
        fs::create_dir_all(root.join("ws/carol")).expect("mk ws dir");
        seed_orphaned_record(root, "carol", "2020-01-01T00:00:00.000000000Z");

        let report = run(root, 0, false).expect("run gc");
        assert_eq!(
            report.destroy_records_deleted, 0,
            "records for a still-existing workspace must never be pruned"
        );
        assert_eq!(
            destroy_record::list_record_files(root, "carol")
                .expect("list")
                .len(),
            1
        );
    }

    #[test]
    fn gc_prunes_old_orphaned_record_but_keeps_recent_one() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        // Two destroyed (dir-gone) workspaces, each with a record whose
        // recovery ref is already gone (the Defect-B residue). One is old,
        // one is fresh.
        seed_orphaned_record(root, "old-ws", "2020-01-01T00:00:00.000000000Z");
        seed_orphaned_record(
            root,
            "fresh-ws",
            &crate::workspace::now_timestamp_iso8601_precise(),
        );

        let report = run(root, 30, false).expect("run gc");
        assert_eq!(
            report.destroy_records_deleted, 1,
            "only the old orphaned record should be age-gated for pruning"
        );
        assert!(
            destroy_record::list_record_files(root, "old-ws")
                .expect("list")
                .is_empty(),
            "old orphaned record pruned"
        );
        assert_eq!(
            destroy_record::list_record_files(root, "fresh-ws")
                .expect("list")
                .len(),
            1,
            "fresh orphaned record kept (age gate protects it)"
        );
    }

    #[test]
    fn gc_dry_run_reports_but_does_not_prune_records() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        seed_destroyed_with_ref(root, "dave", &oid, "20260101-000000");

        let report = run(root, 0, true).expect("dry run");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert_eq!(report.destroy_records_deleted, 1);
        // Nothing actually removed.
        assert_eq!(
            destroy_record::list_record_files(root, "dave")
                .expect("list")
                .len(),
            1,
            "dry run must not delete records"
        );
    }

    // --- bn-3maj: recovery pins are aged by pin creation time ---

    /// Commit a new file with both author and committer dates set to
    /// `days_ago` days in the past. Returns the new commit's OID.
    fn commit_backdated(root: &Path, days_ago: u64) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs()
            - days_ago * 86_400;
        let date = format!("@{secs} +0000");
        fs::write(root.join("old.txt"), "old work\n").expect("write");
        let add = Command::new("git")
            .args(["add", "old.txt"])
            .current_dir(root)
            .output()
            .expect("git add");
        assert!(add.status.success());
        let commit = Command::new("git")
            .args(["commit", "-m", "old work"])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(root)
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "{commit:?}");
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("rev-parse");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    #[test]
    fn fresh_pin_of_old_commit_is_kept() {
        // Acceptance (bn-3maj): `destroy --force` of a clean workspace pins
        // its (possibly months-old) HEAD commit. The pin was created just
        // now, so a 30-day GC must keep it even though the commit is 60
        // days old.
        let (dir, _) = setup_repo();
        let root = dir.path();
        let old = commit_backdated(root, 60);
        let git_oid = maw_core::model::types::GitOid::new(&old).expect("oid");

        let ref_name = crate::workspace::capture::recovery_ref(
            "clean-ws",
            &crate::workspace::now_timestamp_iso8601_precise(),
        );
        refs::write_ref(root, &ref_name, &git_oid).expect("write ref");

        let report = run(root, 30, false).expect("run gc");
        assert_eq!(
            report.recovery_refs_deleted, 0,
            "a pin created just now must not be swept because its commit is old"
        );
        assert_eq!(report.recovery_refs_kept, 1);
        assert!(refs::read_ref(root, &ref_name).expect("read").is_some());
    }

    #[test]
    fn fresh_prefixed_pins_of_old_commit_are_kept() {
        // clean-/materialize-/invariant- pins embed the same timestamp after
        // a kind prefix.
        let (dir, _) = setup_repo();
        let root = dir.path();
        let old = commit_backdated(root, 60);
        let git_oid = maw_core::model::types::GitOid::new(&old).expect("oid");
        let ts = crate::workspace::now_timestamp_iso8601_precise();
        let names = [
            crate::workspace::capture::clean_recovery_ref("w", &ts),
            crate::workspace::capture::materialize_recovery_ref("w", &ts),
            format!(
                "refs/manifold/recovery/w/invariant-{}",
                ts.replace(':', "-")
            ),
        ];
        for n in &names {
            refs::write_ref(root, n, &git_oid).expect("write ref");
        }
        let report = run(root, 30, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 0);
        assert_eq!(report.recovery_refs_kept, 3);
    }

    #[test]
    fn old_pin_name_is_swept_even_if_commit_is_fresh() {
        // Pin age, not commit age, decides: a pin created 60 days ago of a
        // commit dated "now" is past a 30-day threshold.
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");
        let ref_name = "refs/manifold/recovery/w/2020-01-01T00-00-00.000000000Z";
        refs::write_ref(root, ref_name, &git_oid).expect("write ref");
        let report = run(root, 30, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert!(refs::read_ref(root, ref_name).expect("read").is_none());
    }

    #[test]
    fn unparseable_pin_name_falls_back_to_destroy_record_time() {
        // Ref name has no parseable timestamp; the destroy record that claims
        // it was written just now, so the pin is fresh even though the
        // commit is 60 days old.
        let (dir, _) = setup_repo();
        let root = dir.path();
        let old = commit_backdated(root, 60);
        let ref_name = seed_destroyed_with_ref(root, "legacy", &old, "20250101-000000");
        let report = run(root, 30, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 0);
        assert_eq!(report.destroy_records_deleted, 0);
        assert!(refs::read_ref(root, &ref_name).expect("read").is_some());
    }

    #[test]
    fn pin_created_at_parses_known_shapes() {
        assert_eq!(
            pin_created_at_from_ref_name("refs/manifold/recovery/a/1970-01-02T00-00-01.5Z"),
            Some(86_401)
        );
        assert_eq!(
            pin_created_at_from_ref_name("refs/manifold/recovery/a/clean-1970-01-01T00-01-00Z"),
            Some(60)
        );
        assert_eq!(
            pin_created_at_from_ref_name("refs/manifold/recovery/a/20250101-000000"),
            None
        );
        // Multi-byte char straddling byte 19 must not panic.
        assert_eq!(
            pin_created_at_from_ref_name("refs/manifold/recovery/a/1970-01-01T00-00-0\u{e9}Z"),
            None
        );
        assert_eq!(
            pin_created_at_from_ref_name("refs/manifold/recovery/a/dst-3"),
            None
        );
        assert_eq!(pin_created_at_from_ref_name("refs/heads/main"), None);
    }

    // --- bn-wxg28: live-workspace pins + force gate ---

    const OLD_TS: &str = "2020-01-01T00-00-00.000000000Z";

    fn opts(older_than_days: u64, include_live: bool, force: bool) -> RecoveryGcOptions {
        RecoveryGcOptions {
            older_than_days,
            dry_run: false,
            include_live,
            force,
        }
    }

    fn write_pin(root: &Path, oid: &str, ws: &str, leaf: &str) -> String {
        let git_oid = maw_core::model::types::GitOid::new(oid).expect("oid");
        let name = format!("refs/manifold/recovery/{ws}/{leaf}");
        refs::write_ref(root, &name, &git_oid).expect("write pin");
        name
    }

    fn refused(err: &anyhow::Error) -> &GcRefused {
        err.downcast_ref::<GcRefused>()
            .unwrap_or_else(|| panic!("expected GcRefused, got: {err:#}"))
    }

    #[test]
    fn old_pin_of_live_workspace_is_kept_by_default() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        fs::create_dir_all(root.join("ws/alive")).expect("mk ws");
        let pin = write_pin(root, &oid, "alive", &format!("materialize-{OLD_TS}"));

        // Old enough to sweep, no --force needed for an old pin, but it
        // belongs to a live workspace: kept.
        let report = run_with(root, &opts(30, false, false)).expect("gc");
        assert_eq!(report.recovery_refs_deleted, 0);
        assert_eq!(report.skipped_live_pins.len(), 1);
        assert_eq!(report.skipped_live_pins[0].ref_name, pin);
        assert!(refs::read_ref(root, &pin).expect("read").is_some());
    }

    #[test]
    fn include_live_without_force_refuses_and_deletes_nothing() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        fs::create_dir_all(root.join("ws/alive")).expect("mk ws");
        let pin = write_pin(root, &oid, "alive", OLD_TS);
        // A dangling head ref: a refused run must not prune it either.
        refs::write_ref(
            root,
            &refs::workspace_head_ref("ghost"),
            &maw_core::model::types::GitOid::new(&oid).expect("oid"),
        )
        .expect("head ref");

        let err = run_with(root, &opts(30, true, false)).expect_err("must refuse");
        let r = refused(&err);
        assert_eq!(r.would_drop.len(), 1);
        assert!(r.would_drop[0].live);
        let msg = err.to_string();
        assert!(msg.contains(&pin), "{msg}");
        assert!(msg.contains("workspace alive, LIVE workspace"), "{msg}");
        assert!(
            msg.contains("maw gc --recovery-snapshots --include-live --force"),
            "{msg}"
        );
        assert!(refs::read_ref(root, &pin).expect("read").is_some());
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("ghost"))
                .expect("read")
                .is_some(),
            "a refused gc must delete nothing"
        );
    }

    #[test]
    fn include_live_with_force_drops_live_pin() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        fs::create_dir_all(root.join("ws/alive")).expect("mk ws");
        let pin = write_pin(root, &oid, "alive", OLD_TS);
        let report = run_with(root, &opts(30, true, true)).expect("gc");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert!(refs::read_ref(root, &pin).expect("read").is_none());
    }

    #[test]
    fn older_than_zero_without_force_refuses_even_for_old_pins() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let pin = write_pin(root, &oid, "gone", OLD_TS);
        let err = run_with(root, &opts(0, false, false)).expect_err("must refuse");
        let r = refused(&err);
        assert!(
            r.reasons.iter().any(|x| x.contains("--older-than 0")),
            "{:?}",
            r.reasons
        );
        assert!(err.to_string().contains("--older-than 0 --force"), "{err}");
        assert!(refs::read_ref(root, &pin).expect("read").is_some());
    }

    #[test]
    fn young_destroyed_pin_refuses_without_force_and_keeps_record() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let ts = crate::workspace::now_timestamp_iso8601_precise().replace(':', "-");
        let pin = seed_destroyed_with_ref(root, "fresh", &oid, &ts);

        let err = run_with(root, &opts(0, false, false)).expect_err("must refuse");
        let r = refused(&err);
        assert_eq!(r.would_drop.len(), 1);
        assert!(r.would_drop[0].is_young());
        assert!(
            r.reasons.iter().any(|x| x.contains("less than 1 day")),
            "{:?}",
            r.reasons
        );
        assert!(refs::read_ref(root, &pin).expect("read").is_some());
        assert_eq!(
            destroy_record::list_record_files(root, "fresh")
                .expect("list")
                .len(),
            1,
            "refused gc must keep the destroy record"
        );

        // With --force it goes, record in lockstep.
        let report = run_with(root, &opts(0, false, true)).expect("forced gc");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert_eq!(report.destroy_records_deleted, 1);
        assert!(refs::read_ref(root, &pin).expect("read").is_none());
    }

    #[test]
    fn young_pin_reason_is_independent_of_older_than() {
        let young = PinInfo {
            ref_name: "refs/manifold/recovery/w/x".into(),
            workspace: "w".into(),
            live: false,
            age_secs: Some(YOUNG_PIN_SECS - 1),
        };
        let old = PinInfo {
            age_secs: Some(YOUNG_PIN_SECS),
            ..young.clone()
        };
        assert_eq!(force_reasons(&[young], 7).len(), 1);
        assert!(force_reasons(&[old.clone()], 7).is_empty());
        assert!(
            force_reasons(&[], 0).is_empty(),
            "nothing to drop, nothing to refuse"
        );
        let live = PinInfo { live: true, ..old };
        assert_eq!(force_reasons(&[live], 7).len(), 1);
    }

    #[test]
    fn old_destroyed_pin_is_collected_without_force() {
        // Negative control: the everyday sweep of an old, destroyed
        // workspace's pin needs no --force.
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let pin = seed_destroyed_with_ref(root, "gone", &oid, OLD_TS);
        let report = run_with(root, &opts(30, false, false)).expect("gc");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert_eq!(report.destroy_records_deleted, 1);
        assert!(report.force_reasons.is_empty());
        assert!(refs::read_ref(root, &pin).expect("read").is_none());
    }

    #[test]
    fn dry_run_never_refuses_but_reports_force_reasons() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let pin = write_pin(root, &oid, "gone", OLD_TS);
        let report = run_with(
            root,
            &RecoveryGcOptions {
                dry_run: true,
                ..opts(0, false, false)
            },
        )
        .expect("dry run");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert!(!report.force_reasons.is_empty());
        assert!(refs::read_ref(root, &pin).expect("read").is_some());
    }

    #[test]
    fn default_workspace_is_live_at_consolidated_root() {
        // Consolidated layout: the default workspace is the repo root and has
        // no `.maw/workspaces/default/` directory.
        let dir = TempDir::new().expect("tmp");
        let root = dir.path();
        fs::create_dir_all(root.join(".maw/manifold")).expect("mk");
        let names = vec!["default".to_string()];
        assert!(workspace_is_live(root, "default", &names));
        assert!(!workspace_is_live(root, "gone", &names));
    }
}
