//! Structured conflict resolution — consumes `conflict-tree.json`.
//!
//! After rebase, a workspace may have two conflict sidecars:
//!
//! * `.manifold/artifacts/ws/<name>/rebase-conflicts.json` — legacy flat
//!   schema read by the marker-scanning resolver in [`super::resolve`].
//! * `.manifold/artifacts/ws/<name>/conflict-tree.json` — the structured
//!   [`ConflictTree`] written by `maw-core::merge::materialize`.
//!
//! When the structured sidecar is present, this module takes over. It walks
//! `ConflictTree.conflicts`, applies a user-specified `--keep` decision to
//! each entry (or to a single path via `PATH=NAME`), reads the chosen side's
//! blob content via [`GitRepo::read_blob`], overwrites the worktree file, and
//! rewrites (or deletes) the sidecar to reflect the remaining state.
//!
//! V1 scope:
//! * `--keep epoch` — pick the side whose `workspace == "epoch"` (see
//!   [`crates/maw-cli/src/workspace/sync/rebase.rs`] `promote_overlaps_to_conflicts`
//!   which seeds that literal label).
//! * `--keep <ws-name>` — pick the side whose `workspace == <ws-name>`.
//! * `--keep both` — concatenate all sides in their sidecar-declared order,
//!   separated by a newline when needed. No markers emitted.
//! * `PATH=NAME` forms — resolve one path only.
//!
//! Per-atom resolution (`--atom <id>`) is **deferred**. A file-wide `--keep`
//! covers every atom in a `Conflict::Content`; partial atom resolution will
//! land in a follow-up bone — the structured sidecar already has enough
//! information to support it.
//!
//! When the sidecar is absent or unparseable, callers fall back to the legacy
//! marker-scanning path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use maw_core::merge::types::ConflictTree;
use maw_core::model::conflict::{Conflict, ConflictSide, ConflictSideMode};
use maw_core::model::types::GitOid;
use maw_git::{self as git, GitRepo};

use crate::format::OutputFormat;

/// Literal workspace label used by rebase's epoch-delta seed side (see
/// `sync::rebase::promote_overlaps_to_conflicts` — the "ours" side is
/// constructed with `workspace = "epoch"`). Kept `pub(crate)` so tests and
/// documentation consumers can reference the canonical name.
#[allow(dead_code)]
pub(crate) const EPOCH_LABEL: &str = "epoch";

// ---------------------------------------------------------------------------
// Sidecar paths & I/O
// ---------------------------------------------------------------------------

/// Directory holding sidecars for `ws_name` under `root`.
fn sidecar_dir(root: &Path, ws_name: &str) -> PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws_name)
}

/// Path to `conflict-tree.json` for `ws_name`.
pub(crate) fn structured_sidecar_path(root: &Path, ws_name: &str) -> PathBuf {
    sidecar_dir(root, ws_name).join("conflict-tree.json")
}

/// Path to the legacy flat sidecar.
pub(crate) fn legacy_sidecar_path(root: &Path, ws_name: &str) -> PathBuf {
    sidecar_dir(root, ws_name).join("rebase-conflicts.json")
}

/// Read and deserialize `conflict-tree.json` for `ws_name`, if present.
///
/// Returns `None` when the file is missing, unreadable, or can't be parsed as
/// a [`ConflictTree`]. Callers should fall back to the legacy marker-scan
/// path in that case — this keeps pre-gjm8 workspaces working unchanged.
pub(crate) fn read_conflict_tree_sidecar(root: &Path, ws_name: &str) -> Option<ConflictTree> {
    let path = structured_sidecar_path(root, ws_name);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<ConflictTree>(&text).ok()
}

/// Write an updated `ConflictTree` back to the sidecar. If the tree has no
/// remaining conflicts (and no remaining clean entries), delete the file.
fn write_conflict_tree_sidecar(root: &Path, ws_name: &str, tree: &ConflictTree) -> Result<()> {
    let path = structured_sidecar_path(root, ws_name);
    if tree.conflicts.is_empty() && tree.clean.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", path.display()))?;
        }
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(tree)?;
    std::fs::write(&path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// --list over the structured sidecar
// ---------------------------------------------------------------------------

/// Render a structured `--list` view, matching the shape of the legacy text
/// and JSON output but driven by `ConflictTree.conflicts`.
///
/// Only paths in `filter_paths` (if non-empty) are shown; otherwise all.
pub(crate) fn list_conflicts(
    tree: &ConflictTree,
    workspace: &str,
    filter_paths: &[String],
    format: OutputFormat,
) -> Result<()> {
    let filter: Option<std::collections::HashSet<PathBuf>> = if filter_paths.is_empty() {
        None
    } else {
        Some(filter_paths.iter().map(PathBuf::from).collect())
    };

    // Gather candidates respecting filter.
    let entries: Vec<(&PathBuf, &Conflict)> = tree
        .conflicts
        .iter()
        .filter(|(p, _)| filter.as_ref().is_none_or(|f| f.contains(*p)))
        .collect();

    if format == OutputFormat::Json {
        let items: Vec<String> = entries
            .iter()
            .map(|(path, conflict)| {
                let shape = conflict.variant_name();
                let side_count = conflict.side_count();
                let workspaces: Vec<String> = conflict
                    .workspaces()
                    .iter()
                    .map(|w| format!("\"{}\"", w.replace('"', "\\\"")))
                    .collect();
                let atom_count = match conflict {
                    Conflict::Content { atoms, .. } => atoms.len(),
                    _ => 0,
                };
                format!(
                    r#"{{"path":"{}","shape":"{}","sides":{},"atoms":{},"workspaces":[{}]}}"#,
                    path.display(),
                    shape,
                    side_count,
                    atom_count,
                    workspaces.join(","),
                )
            })
            .collect();
        println!(
            r#"{{"workspace":"{workspace}","conflict_count":{},"structured":true,"conflicts":[{}]}}"#,
            entries.len(),
            items.join(","),
        );
        return Ok(());
    }

    if entries.is_empty() {
        println!("No structured conflicts in '{workspace}'.");
        return Ok(());
    }

    println!(
        "{} structured conflict(s) in '{workspace}':",
        entries.len()
    );
    for (path, conflict) in &entries {
        let shape = conflict.variant_name();
        let sides_desc = conflict.workspaces().join(", ");
        match conflict {
            Conflict::Content { atoms, .. } => {
                if atoms.is_empty() {
                    println!(
                        "  {}  [{shape}] sides=[{sides_desc}]",
                        path.display()
                    );
                } else {
                    println!(
                        "  {}  [{shape}] sides=[{sides_desc}] atoms={}",
                        path.display(),
                        atoms.len()
                    );
                }
            }
            _ => {
                println!(
                    "  {}  [{shape}] sides=[{sides_desc}]",
                    path.display()
                );
            }
        }
    }

    println!();
    println!("To resolve:");
    println!("  maw ws resolve {workspace} --keep epoch            # keep epoch version");
    println!("  maw ws resolve {workspace} --keep <ws-name>        # keep a specific workspace side");
    println!("  maw ws resolve {workspace} --keep both             # keep all sides (concatenated)");

    Ok(())
}

// ---------------------------------------------------------------------------
// Keep-spec resolution
// ---------------------------------------------------------------------------

/// Per-path decision parsed from the flat `--keep` strings.
///
/// * `All(name)` — applies to every path with a matching side.
/// * `File(path, name)` — applies only to `path`.
#[derive(Debug)]
pub(crate) enum Decision {
    All(String),
    File(PathBuf, String),
}

/// Parse flat `--keep` arguments into structured decisions.
///
/// Block-level (`cf-N=NAME`) keep-specs are not supported on the structured
/// path in V1 — the structured sidecar uses atoms/paths, not cf-IDs. Such
/// specs are rejected with an error so the CLI surface is predictable.
pub(crate) fn parse_decisions(raw: &[String]) -> Result<Vec<Decision>> {
    let mut out = Vec::new();
    for s in raw {
        if let Some((left, right)) = s.split_once('=') {
            let left = left.trim();
            let right = right.trim();
            if right.is_empty() {
                bail!("Invalid --keep '{s}': empty side name after '='");
            }
            if left.starts_with("cf-") {
                bail!(
                    "Per-block `cf-N=NAME` keep-specs are not supported with the structured \
                     sidecar. Use `--keep <ws-name>` or `PATH=<ws-name>`. Per-atom resolution \
                     is tracked as a follow-up."
                );
            }
            out.push(Decision::File(PathBuf::from(left), right.to_owned()));
        } else {
            out.push(Decision::All(s.clone()));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Side picking
// ---------------------------------------------------------------------------

/// Find the `ConflictSide` whose `workspace` matches `target`.
///
/// Uses the same looseness as the legacy path: an exact match wins; otherwise
/// we try comma-separated multi-workspace labels (e.g. `"ws-a, ws-b"`).
fn pick_side<'a>(sides: &'a [ConflictSide], target: &str) -> Option<&'a ConflictSide> {
    for s in sides {
        if s.workspace == target {
            return Some(s);
        }
        if s.workspace.split(',').any(|p| p.trim() == target) {
            return Some(s);
        }
    }
    None
}

/// Return all side OIDs for `--keep both`, in declared order.
fn all_sides(conflict: &Conflict) -> Vec<GitOid> {
    match conflict {
        Conflict::Content { sides, .. } | Conflict::AddAdd { sides, .. } => {
            sides.iter().map(|s| s.content.clone()).collect()
        }
        Conflict::ModifyDelete {
            modifier, deleter, ..
        } => vec![modifier.content.clone(), deleter.content.clone()],
        Conflict::DivergentRename { destinations, .. } => destinations
            .iter()
            .map(|(_, s)| s.content.clone())
            .collect(),
    }
}

/// Pick a single side's blob OID for a `Conflict`.
///
/// * For `Content` / `AddAdd`: searches `sides`.
/// * For `ModifyDelete`: the `deleter` has no real content, so picking it
///   signals "accept the delete" (returns `None`).
/// * For `DivergentRename`: V1 cannot express a single-path resolution.
fn pick_single_side_oid(conflict: &Conflict, target: &str) -> Result<Option<GitOid>> {
    match conflict {
        Conflict::Content { sides, .. } | Conflict::AddAdd { sides, .. } => {
            if let Some(side) = pick_side(sides, target) {
                Ok(Some(side.content.clone()))
            } else {
                let available: Vec<&str> =
                    sides.iter().map(|s| s.workspace.as_str()).collect();
                bail!(
                    "Side '{}' not found for path. Available: [{}], plus 'both'.",
                    target,
                    available.join(", ")
                );
            }
        }
        Conflict::ModifyDelete {
            modifier, deleter, ..
        } => {
            if modifier.workspace == target
                || modifier.workspace.split(',').any(|p| p.trim() == target)
            {
                Ok(Some(modifier.content.clone()))
            } else if deleter.workspace == target
                || deleter.workspace.split(',').any(|p| p.trim() == target)
            {
                // Deleter side: caller interprets `None` as "delete the path".
                Ok(None)
            } else {
                bail!(
                    "Side '{}' not found. Available: [{}, {}] (modifier, deleter).",
                    target,
                    modifier.workspace,
                    deleter.workspace
                );
            }
        }
        Conflict::DivergentRename { .. } => {
            bail!(
                "DivergentRename conflicts cannot be resolved via --keep in V1. Resolve by \
                 manually renaming the file in the worktree and committing."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution dispatch
// ---------------------------------------------------------------------------

/// Outcome of applying `--keep` to a single path.
enum PathOutcome {
    /// Wrote `bytes` to the worktree file. The optional `mode` hint tells
    /// the worktree applier whether to re-establish a symlink (`Link`) or
    /// executable bit rather than write a plain regular file (bn-mg0j).
    Wrote {
        bytes: Vec<u8>,
        mode: Option<ConflictSideMode>,
    },
    /// Removed the file from the worktree (modify/delete → accept delete).
    Deleted,
    /// Caller asked for a side that doesn't exist / can't resolve. The
    /// carried message is logged by the caller into the skipped list.
    Skipped(#[allow(dead_code)] String),
}

/// Scan all sides of a conflict and return the first mode hint found.
///
/// bn-mg0j: `--keep both` concatenates bytes so no single side mode wins —
/// but if any side was recorded as a symlink, the resulting concat is not a
/// valid symlink target anyway. We pick the first non-None hint so that a
/// single-side-mode conflict (common case) still re-applies correctly.
fn any_side_mode(conflict: &Conflict) -> Option<ConflictSideMode> {
    match conflict {
        Conflict::Content { sides, .. } | Conflict::AddAdd { sides, .. } => {
            sides.iter().find_map(|s| s.mode)
        }
        Conflict::ModifyDelete {
            modifier, deleter, ..
        } => modifier.mode.or(deleter.mode),
        Conflict::DivergentRename { destinations, .. } => {
            destinations.iter().find_map(|(_, s)| s.mode)
        }
    }
}

/// Pick a single side by name and return its mode hint, matching the same
/// looseness as `pick_single_side_oid`.
fn pick_single_side_mode(conflict: &Conflict, target: &str) -> Option<ConflictSideMode> {
    match conflict {
        Conflict::Content { sides, .. } | Conflict::AddAdd { sides, .. } => {
            pick_side(sides, target).and_then(|s| s.mode)
        }
        Conflict::ModifyDelete {
            modifier, deleter, ..
        } => {
            if modifier.workspace == target
                || modifier.workspace.split(',').any(|p| p.trim() == target)
            {
                modifier.mode
            } else if deleter.workspace == target
                || deleter.workspace.split(',').any(|p| p.trim() == target)
            {
                deleter.mode
            } else {
                None
            }
        }
        Conflict::DivergentRename { destinations, .. } => destinations
            .iter()
            .find(|(_, s)| s.workspace == target)
            .and_then(|(_, s)| s.mode),
    }
}

/// Apply a resolution for a single `(path, conflict)` and produce the output.
fn apply_decision(
    repo: &dyn GitRepo,
    conflict: &Conflict,
    target: &str,
) -> Result<PathOutcome> {
    if target == "both" {
        // bn-2pry: ModifyDelete has no meaningful "both" — the deleter side
        // carries the *pre-delete* blob OID (the base content) in its
        // `content` field, so a naive concat would silently resurrect the
        // base bytes under the "keep both" banner. Treat `--keep both` on
        // a modify/delete as an alias for keeping only the modifier's
        // content (the deletion is effectively declined). Document the
        // choice in the skipped reason carried back to the caller so the
        // CLI can surface it.
        if let Conflict::ModifyDelete {
            modifier, ..
        } = conflict
        {
            let git_oid: git::GitOid = modifier
                .content
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid blob oid {}: {e}", modifier.content))?;
            let bytes = repo
                .read_blob(git_oid)
                .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", modifier.content))?;
            return Ok(PathOutcome::Wrote {
                bytes,
                mode: modifier.mode,
            });
        }

        let oids = all_sides(conflict);
        if oids.is_empty() {
            return Ok(PathOutcome::Skipped("no sides to concatenate".into()));
        }
        let mut buf: Vec<u8> = Vec::new();
        for (i, oid) in oids.iter().enumerate() {
            let git_oid: git::GitOid = oid
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid blob oid {oid}: {e}"))?;
            let bytes = repo
                .read_blob(git_oid)
                .map_err(|e| anyhow::anyhow!("read_blob({oid}) failed: {e}"))?;
            if i > 0 && !buf.ends_with(b"\n") {
                buf.push(b'\n');
            }
            buf.extend_from_slice(&bytes);
        }
        // For `both`, write the concatenation as a regular file unless every
        // side was a symlink to the same target (degenerate case we don't
        // special-case in V1). `any_side_mode` returns the first hint for
        // diagnostics.
        let _hint = any_side_mode(conflict);
        return Ok(PathOutcome::Wrote {
            bytes: buf,
            // Concat of multiple sides is never a valid symlink target. Fall
            // back to a regular file write.
            mode: None,
        });
    }

    // Single side. Translate `epoch` synonym through a canonical label.
    // The sidecar seeds the epoch side with `workspace == "epoch"` (see
    // `sync::rebase::promote_overlaps_to_conflicts`), so no aliasing required.
    let mode_hint = pick_single_side_mode(conflict, target);
    match pick_single_side_oid(conflict, target)? {
        Some(oid) => {
            let git_oid: git::GitOid = oid
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid blob oid {oid}: {e}"))?;
            let bytes = repo
                .read_blob(git_oid)
                .map_err(|e| anyhow::anyhow!("read_blob({oid}) failed: {e}"))?;
            Ok(PathOutcome::Wrote {
                bytes,
                mode: mode_hint,
            })
        }
        None => Ok(PathOutcome::Deleted),
    }
}

/// Apply `PathOutcome` to the worktree at `ws_path.join(rel)`.
fn apply_outcome(ws_path: &Path, rel: &Path, outcome: PathOutcome) -> Result<bool> {
    match outcome {
        PathOutcome::Wrote { bytes, mode } => {
            let full = ws_path.join(rel);
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir)?;
            }

            // bn-mg0j: if the side carried a `Link` mode hint, re-create the
            // path as a symlink whose target is the blob's content, rather
            // than writing the target bytes as a regular file. Without this,
            // a resolved symlink conflict ended up as a 100644 regular file
            // containing the target path as content.
            if matches!(mode, Some(ConflictSideMode::Link)) {
                // Remove any existing entry so the symlink create succeeds.
                if full.is_file() || full.is_symlink() {
                    std::fs::remove_file(&full).map_err(|e| {
                        anyhow::anyhow!("remove {}: {e}", full.display())
                    })?;
                }
                let target = std::str::from_utf8(&bytes).map_err(|e| {
                    anyhow::anyhow!(
                        "symlink target at {} is not valid UTF-8: {e}",
                        full.display()
                    )
                })?;
                // Strip a single trailing newline if present — git symlink
                // blobs typically store the target without a trailing LF,
                // but we're defensive here in case the side was captured
                // from text-mode tooling.
                let target = target.strip_suffix('\n').unwrap_or(target);
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(target, &full).map_err(|e| {
                        anyhow::anyhow!("symlink {} -> {target}: {e}", full.display())
                    })?;
                }
                #[cfg(not(unix))]
                {
                    // No meaningful symlink story on non-unix for maw; fall
                    // through to a regular file write so we at least don't
                    // lose data.
                    std::fs::write(&full, &bytes).map_err(|e| {
                        anyhow::anyhow!("write {}: {e}", full.display())
                    })?;
                }
                return Ok(true);
            }

            std::fs::write(&full, &bytes)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", full.display()))?;

            // bn-mg0j: executable bit.
            #[cfg(unix)]
            if matches!(mode, Some(ConflictSideMode::BlobExecutable)) {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o755);
                std::fs::set_permissions(&full, perms).map_err(|e| {
                    anyhow::anyhow!("chmod +x {}: {e}", full.display())
                })?;
            }

            Ok(true)
        }
        PathOutcome::Deleted => {
            let full = ws_path.join(rel);
            if full.is_file() || full.is_symlink() {
                std::fs::remove_file(&full)
                    .map_err(|e| anyhow::anyhow!("remove {}: {e}", full.display()))?;
            }
            Ok(true)
        }
        PathOutcome::Skipped(_) => Ok(false),
    }
}

/// Main entry point. Called by `super::resolve::run` when the structured
/// sidecar is present.
///
/// Returns `Ok(true)` on normal completion (the caller must NOT fall back to
/// the legacy path). Returns `Ok(false)` only when the caller should still
/// fall back — currently never, but kept as a signal channel for future
/// additions.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_structured(
    root: &Path,
    workspace: &str,
    ws_path: &Path,
    paths: &[String],
    keep: &[String],
    list: bool,
    format: OutputFormat,
    mut tree: ConflictTree,
) -> Result<bool> {
    if list {
        list_conflicts(&tree, workspace, paths, format)?;
        return Ok(true);
    }

    if keep.is_empty() {
        bail!(
            "Must specify --keep or --list.\n\
             \n  Examples:\n\
             \n    maw ws resolve {workspace} --keep epoch              # keep epoch side\n\
             \n    maw ws resolve {workspace} --keep <ws-name>          # keep specific workspace\n\
             \n    maw ws resolve {workspace} --keep both               # keep all sides\n\
             \n    maw ws resolve {workspace} --keep PATH=<name>        # resolve one file\n\
             \n    maw ws resolve {workspace} --list                    # list conflicts"
        );
    }

    let decisions = parse_decisions(keep)?;
    let mut all_side: Option<String> = None;
    let mut file_sides: BTreeMap<PathBuf, String> = BTreeMap::new();
    for d in decisions {
        match d {
            Decision::All(n) => {
                if all_side.is_some() {
                    bail!(
                        "Multiple blanket --keep flags. Use one, or use PATH=NAME per-file."
                    );
                }
                all_side = Some(n);
            }
            Decision::File(p, n) => {
                file_sides.insert(p, n);
            }
        }
    }

    // Open repo at ws_path — matches what sync::rebase does.
    let repo = git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("Failed to open git repo at {}: {e}", ws_path.display()))?;
    let repo_dyn: &dyn GitRepo = &repo;

    // Determine the set of paths to process.
    let target_paths: Vec<PathBuf> = if !file_sides.is_empty() && all_side.is_none() {
        file_sides.keys().cloned().collect()
    } else if !paths.is_empty() {
        paths.iter().map(PathBuf::from).collect()
    } else {
        tree.conflicts.keys().cloned().collect()
    };

    if target_paths.is_empty() {
        if format == OutputFormat::Json {
            println!(r#"{{"status":"clean","workspace":"{workspace}","structured":true,"message":"No structured conflicts found."}}"#);
        } else {
            println!("No structured conflicts found in '{workspace}'.");
        }
        return Ok(true);
    }

    let mut resolved = Vec::<PathBuf>::new();
    let mut skipped = Vec::<(PathBuf, String)>::new();

    // Iterate over a snapshot of target paths; mutate `tree` as we go.
    for rel in &target_paths {
        let Some(conflict) = tree.conflicts.get(rel).cloned() else {
            skipped.push((rel.clone(), "not in structured sidecar".into()));
            continue;
        };
        let Some(target) = file_sides.get(rel).cloned().or_else(|| all_side.clone()) else {
            skipped.push((rel.clone(), "no --keep side chosen for path".into()));
            continue;
        };
        match apply_decision(repo_dyn, &conflict, &target) {
            Ok(outcome) => match apply_outcome(ws_path, rel, outcome)? {
                true => {
                    tree.conflicts.remove(rel);
                    resolved.push(rel.clone());
                }
                false => {
                    skipped.push((rel.clone(), "decision produced no output".into()));
                }
            },
            Err(e) => {
                skipped.push((rel.clone(), e.to_string()));
            }
        }
    }

    // Persist updated sidecar (or delete if tree is fully empty).
    write_conflict_tree_sidecar(root, workspace, &tree)?;

    // If the sidecar now has no more conflicts, also sweep the legacy
    // sidecar so later `find_conflicted_files` runs don't see stale state.
    if tree.conflicts.is_empty() {
        let legacy = legacy_sidecar_path(root, workspace);
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }
    }

    // bn-2cc1: when the sidecar is now empty AND the invocation actually
    // resolved something, auto-commit the resolution so that the workspace
    // is *truly* ready for merge. Previously the resolver wrote bytes to
    // the worktree but left the HEAD tree containing the pre-resolution
    // marker blobs, causing the merge-time marker gate to refuse to ship
    // what looked like a clean workspace.
    //
    // Skip auto-commit when nothing resolved in this invocation (e.g. a
    // --list or a no-op re-run) and when there are still remaining
    // conflicts — partial resolution should not silently create commits
    // while more work is pending.
    let auto_committed = if tree.conflicts.is_empty() && !resolved.is_empty() {
        auto_commit_resolution(ws_path, workspace, &resolved)
    } else {
        Ok(None)
    };
    let auto_commit_msg: Option<String> = match auto_committed {
        Ok(m) => m,
        Err(e) => {
            // Non-fatal — the resolution wrote to the worktree; a failed
            // auto-commit just means the user has to `git commit` manually.
            // Surface the reason so agents can react.
            tracing::warn!(
                "auto-commit after resolve failed in '{workspace}': {e}"
            );
            None
        }
    };

    // Reporting
    if format == OutputFormat::Json {
        let resolved_json: Vec<String> = resolved
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect();
        let skipped_json: Vec<String> = skipped
            .iter()
            .map(|(p, r)| {
                format!(
                    r#"{{"path":"{}","reason":"{}"}}"#,
                    p.display(),
                    r.replace('"', "\\\"")
                )
            })
            .collect();
        let committed_field = match &auto_commit_msg {
            Some(sha) => format!(r#","auto_committed":"{sha}""#),
            None => String::new(),
        };
        println!(
            r#"{{"status":"ok","workspace":"{workspace}","structured":true,"resolved":[{}],"conflicts_remaining":{},"skipped":[{}]{}}}"#,
            resolved_json.join(","),
            tree.conflicts.len(),
            skipped_json.join(","),
            committed_field,
        );
    } else {
        for p in &resolved {
            println!("  resolved: {}", p.display());
        }
        for (p, r) in &skipped {
            eprintln!("  skipped: {} — {r}", p.display());
        }
        if resolved.is_empty() && skipped.is_empty() {
            println!("Nothing to resolve.");
        } else if tree.conflicts.is_empty() {
            match &auto_commit_msg {
                Some(sha) => println!(
                    "\nAll structured conflicts resolved and committed ({}). \
                     Workspace is ready for merge.",
                    &sha[..sha.len().min(12)]
                ),
                None if !resolved.is_empty() => println!(
                    "\nAll structured conflicts resolved — workspace is ready for merge. \
                     (auto-commit skipped: run `maw exec {workspace} -- git commit` if needed)"
                ),
                None => println!(
                    "\nAll structured conflicts resolved — workspace is ready for merge."
                ),
            }
        } else {
            let total_original = tree.conflicts.len() + resolved.len();
            println!(
                "\n{} of {} conflict(s) resolved, {} remaining. \
                 Run `maw ws resolve {workspace} --list` to continue, \
                 or `maw exec {workspace} -- git commit -m ...` to save progress.",
                resolved.len(),
                total_original,
                tree.conflicts.len(),
            );
        }
    }

    Ok(true)
}

/// Stage and commit the resolved files after a full structured resolve.
///
/// Returns `Ok(Some(sha))` when a commit was created, `Ok(None)` when the
/// worktree turned out to be clean (nothing to commit — legitimate race
/// with an external actor), or an error when `git add` / `git commit`
/// actually failed.
///
/// We intentionally shell out to `git` here rather than going through the
/// `GitRepo` trait because: (a) the workspace is a regular worktree with a
/// standard HEAD; (b) `git commit` handles the index + HEAD update
/// atomically and respects existing user config (author/committer, hooks,
/// signing); (c) the structured-resolve path has to succeed on
/// `core.symlinks=true` worktrees where staging a symlink via the gix index
/// surface would need more care.
fn auto_commit_resolution(
    ws_path: &Path,
    workspace: &str,
    resolved: &[PathBuf],
) -> Result<Option<String>> {
    use std::process::Command;

    // Stage only the paths we actually touched. This is narrower than
    // `git add -A` and avoids pulling in unrelated worktree edits the user
    // might have made alongside the conflict resolution.
    let mut add = Command::new("git");
    add.arg("add").arg("--");
    for p in resolved {
        add.arg(p);
    }
    let add_out = add
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("git add failed to spawn: {e}"))?;
    if !add_out.status.success() {
        bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&add_out.stderr).trim()
        );
    }

    // If there's nothing staged after the add (e.g. the resolve happened to
    // write the exact same bytes that were already committed), bail out
    // cleanly with `Ok(None)` rather than creating an empty commit.
    let staged = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(ws_path)
        .status()
        .map_err(|e| anyhow::anyhow!("git diff --cached failed: {e}"))?;
    if staged.success() {
        // No staged changes — nothing to commit.
        return Ok(None);
    }

    let msg = if resolved.len() == 1 {
        format!(
            "resolve: {} (bn-gjm8 auto-commit)",
            resolved[0].display()
        )
    } else {
        format!(
            "resolve: apply structured --keep decisions for {} path(s) in '{workspace}' (bn-gjm8 auto-commit)",
            resolved.len()
        )
    };

    let commit_out = Command::new("git")
        .args(["commit", "-m", &msg])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("git commit failed to spawn: {e}"))?;
    if !commit_out.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit_out.stderr).trim()
        );
    }

    // Read the new HEAD SHA for reporting.
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("git rev-parse HEAD failed: {e}"))?;
    if !head.status.success() {
        // Commit succeeded but rev-parse failed — report a stub.
        return Ok(Some(String::new()));
    }
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    Ok(Some(sha))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use maw_core::model::conflict::{ConflictSide};
    use maw_core::model::ordering::OrderingKey;
    use maw_core::model::patch::FileId;
    use maw_core::model::types::{EpochId, WorkspaceId};

    fn epoch() -> EpochId {
        EpochId::new(&"e".repeat(40)).unwrap()
    }
    fn oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }
    fn ord(ws: &str) -> OrderingKey {
        OrderingKey::new(epoch(), WorkspaceId::new(ws).unwrap(), 1, 1_700_000_000_000)
    }
    fn side(ws: &str, c: char) -> ConflictSide {
        ConflictSide::new(ws.to_owned(), oid(c), ord(ws))
    }

    #[test]
    fn parse_decisions_rejects_cf_specs() {
        let err = parse_decisions(&["cf-0=alice".into()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cf-"), "msg={msg}");
    }

    #[test]
    fn parse_decisions_parses_all_and_file() {
        let specs =
            parse_decisions(&["epoch".into(), "src/main.rs=bn-abc".into()]).unwrap();
        assert_eq!(specs.len(), 2);
        assert!(matches!(&specs[0], Decision::All(n) if n == "epoch"));
        assert!(matches!(
            &specs[1],
            Decision::File(p, n) if p == Path::new("src/main.rs") && n == "bn-abc"
        ));
    }

    #[test]
    fn parse_decisions_rejects_empty_side() {
        let err = parse_decisions(&["src/x=".into()]).unwrap_err();
        assert!(err.to_string().contains("empty side"));
    }

    #[test]
    fn pick_single_side_oid_content_picks_by_name() {
        let c = Conflict::Content {
            path: PathBuf::from("a.rs"),
            file_id: FileId::new(1),
            base: Some(oid('0')),
            sides: vec![side(EPOCH_LABEL, 'a'), side("bn-abc", 'b')],
            atoms: vec![],
        };
        let got = pick_single_side_oid(&c, EPOCH_LABEL).unwrap();
        assert_eq!(got, Some(oid('a')));
        let got2 = pick_single_side_oid(&c, "bn-abc").unwrap();
        assert_eq!(got2, Some(oid('b')));
    }

    #[test]
    fn pick_single_side_oid_unknown_name_errors() {
        let c = Conflict::Content {
            path: PathBuf::from("a.rs"),
            file_id: FileId::new(1),
            base: None,
            sides: vec![side("alice", 'a'), side("bob", 'b')],
            atoms: vec![],
        };
        let err = pick_single_side_oid(&c, "nonexistent").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nonexistent"));
        assert!(msg.contains("alice"));
        assert!(msg.contains("bob"));
    }

    #[test]
    fn pick_single_side_modify_delete_deleter_returns_none() {
        let c = Conflict::ModifyDelete {
            path: PathBuf::from("x"),
            file_id: FileId::new(2),
            modifier: side("alice", 'a'),
            deleter: side("bob", 'b'),
            modified_content: oid('a'),
        };
        let mod_side = pick_single_side_oid(&c, "alice").unwrap();
        assert_eq!(mod_side, Some(oid('a')));
        let del_side = pick_single_side_oid(&c, "bob").unwrap();
        assert!(del_side.is_none(), "deleter side should return None");
    }

    #[test]
    fn all_sides_content_returns_every_side() {
        let c = Conflict::Content {
            path: PathBuf::from("a.rs"),
            file_id: FileId::new(1),
            base: Some(oid('0')),
            sides: vec![side("a", 'a'), side("b", 'b'), side("c", 'c')],
            atoms: vec![],
        };
        let oids = all_sides(&c);
        assert_eq!(oids, vec![oid('a'), oid('b'), oid('c')]);
    }

    #[test]
    fn read_conflict_tree_sidecar_missing_returns_none() {
        let td = tempfile::tempdir().unwrap();
        let got = read_conflict_tree_sidecar(td.path(), "no-such-ws");
        assert!(got.is_none());
    }

    #[test]
    fn read_conflict_tree_sidecar_malformed_returns_none() {
        let td = tempfile::tempdir().unwrap();
        let dir = sidecar_dir(td.path(), "broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("conflict-tree.json"), "not json").unwrap();
        let got = read_conflict_tree_sidecar(td.path(), "broken");
        assert!(got.is_none());
    }

    #[test]
    fn read_conflict_tree_sidecar_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let dir = sidecar_dir(td.path(), "ok");
        std::fs::create_dir_all(&dir).unwrap();

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/x.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/x.rs"),
                sides: vec![side(EPOCH_LABEL, 'a'), side("ws-b", 'b')],
            },
        );
        let json = serde_json::to_string_pretty(&tree).unwrap();
        std::fs::write(dir.join("conflict-tree.json"), json).unwrap();

        let got = read_conflict_tree_sidecar(td.path(), "ok").expect("parsed");
        assert_eq!(got, tree);
    }

    #[test]
    fn write_conflict_tree_sidecar_deletes_when_empty() {
        let td = tempfile::tempdir().unwrap();
        let dir = sidecar_dir(td.path(), "wipe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conflict-tree.json");
        std::fs::write(&path, "{}").unwrap();

        let tree = ConflictTree::new(epoch());
        assert!(tree.is_empty());
        write_conflict_tree_sidecar(td.path(), "wipe", &tree).unwrap();
        assert!(!path.exists(), "empty tree should have removed sidecar");
    }

    // -----------------------------------------------------------------------
    // End-to-end tests using a real git repo
    //
    // These set up a tmp git repo that looks workspace-shaped:
    //   <root>/         — bare-ish container (just `git init` + a `ws/` dir)
    //   <root>/ws/<w>/  — the workspace worktree we pass as `ws_path`
    //
    // The workspace itself is the same repo (so `GixRepo::open(ws_path)` walks
    // up and finds `<root>/.git`). That's enough for `read_blob` to work — we
    // only need OID lookups, not HEAD/worktree semantics.
    // -----------------------------------------------------------------------

    /// Init a git repo inside the ws_path directory (so `GixRepo::open(ws_path)`
    /// finds it without ancestor discovery). Returns
    /// `(root_tempdir, root_path, ws_path, repo_handle)`.
    ///
    /// Real maw workspaces have a `.git` gitfile pointing to the common dir;
    /// for unit tests we initialize a standalone repo inside the workspace
    /// directory, which is enough for `read_blob` / `write_blob` round-trips.
    fn setup_ws_repo(ws_name: &str) -> (tempfile::TempDir, PathBuf, PathBuf, maw_git::GixRepo) {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        let ws_path = root.join("ws").join(ws_name);
        std::fs::create_dir_all(&ws_path).unwrap();

        std::process::Command::new("git")
            .args(["init", ws_path.to_str().unwrap()])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&ws_path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&ws_path)
            .output()
            .unwrap();

        let repo = maw_git::GixRepo::open(&ws_path).unwrap();
        (td, root, ws_path, repo)
    }

    /// Write a blob into the repo and return its `GitOid` (maw-core flavor).
    fn write_blob(repo: &maw_git::GixRepo, bytes: &[u8]) -> GitOid {
        use maw_git::GitRepo;
        let git_oid = repo.write_blob(bytes).unwrap();
        GitOid::new(&git_oid.to_string()).unwrap()
    }

    /// Build an epoch-vs-workspace content conflict with real blobs.
    fn make_content_conflict(
        path: &str,
        epoch_bytes: &[u8],
        ws_bytes: &[u8],
        ws_name: &str,
        repo: &maw_git::GixRepo,
    ) -> (PathBuf, Conflict) {
        let epoch_oid = write_blob(repo, epoch_bytes);
        let ws_oid = write_blob(repo, ws_bytes);
        let p = PathBuf::from(path);
        (
            p.clone(),
            Conflict::Content {
                path: p,
                file_id: FileId::new(1),
                base: None,
                sides: vec![
                    ConflictSide::new(EPOCH_LABEL.to_owned(), epoch_oid, ord("ws")),
                    ConflictSide::new(ws_name.to_owned(), ws_oid, ord(ws_name)),
                ],
                atoms: vec![],
            },
        )
    }

    #[test]
    fn resolve_structured_keep_epoch_applies_epoch_side() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-a");
        let (rel, conflict) =
            make_content_conflict("src/a.rs", b"EPOCH_CONTENT\n", b"WS_CONTENT\n", "ws-a", &repo);
        // Write initial worktree file with marker soup
        std::fs::create_dir_all(ws_path.join("src")).unwrap();
        std::fs::write(ws_path.join(&rel), b"<<<<<<< markers\n").unwrap();

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-a",
            &ws_path,
            &[],
            &["epoch".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        let after = std::fs::read(ws_path.join(&rel)).unwrap();
        assert_eq!(after, b"EPOCH_CONTENT\n");
        // Sidecar should be gone (tree empty).
        let sp = structured_sidecar_path(&root, "ws-a");
        assert!(!sp.exists(), "sidecar should be removed when empty");
    }

    #[test]
    fn resolve_structured_keep_workspace_applies_ws_side() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-b");
        let (rel, conflict) =
            make_content_conflict("x.rs", b"EPOCH\n", b"WS_SIDE\n", "ws-b", &repo);
        std::fs::write(ws_path.join(&rel), b"<<<<<<< markers\n").unwrap();
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-b",
            &ws_path,
            &[],
            &["ws-b".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        let after = std::fs::read(ws_path.join(&rel)).unwrap();
        assert_eq!(after, b"WS_SIDE\n");
    }

    #[test]
    fn resolve_structured_keep_both_concatenates() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-c");
        let (rel, conflict) =
            make_content_conflict("y.rs", b"EPOCH_A\n", b"WS_B\n", "ws-c", &repo);
        std::fs::write(ws_path.join(&rel), b"<<<<<<< markers\n").unwrap();
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-c",
            &ws_path,
            &[],
            &["both".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        let after = std::fs::read_to_string(ws_path.join(&rel)).unwrap();
        // Order: epoch side first (as seeded), then workspace side.
        assert!(after.contains("EPOCH_A"), "missing epoch content: {after}");
        assert!(after.contains("WS_B"), "missing workspace content: {after}");
        let epoch_pos = after.find("EPOCH_A").unwrap();
        let ws_pos = after.find("WS_B").unwrap();
        assert!(
            epoch_pos < ws_pos,
            "epoch side should come first, got: {after}"
        );
        // No conflict markers should appear.
        assert!(!after.contains("<<<<<<<"), "unexpected marker: {after}");
    }

    #[test]
    fn resolve_structured_updates_sidecar_after_keep() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-d");
        let (rel_a, c_a) =
            make_content_conflict("a.rs", b"EPOCH_A\n", b"WS_A\n", "ws-d", &repo);
        let (rel_b, c_b) =
            make_content_conflict("b.rs", b"EPOCH_B\n", b"WS_B\n", "ws-d", &repo);
        std::fs::write(ws_path.join(&rel_a), b"<<<<<<< markers\n").unwrap();
        std::fs::write(ws_path.join(&rel_b), b"<<<<<<< markers\n").unwrap();

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel_a.clone(), c_a);
        tree.conflicts.insert(rel_b.clone(), c_b.clone());

        // Persist sidecar, then only resolve `a.rs`.
        write_conflict_tree_sidecar(&root, "ws-d", &tree).unwrap();

        run_structured(
            &root,
            "ws-d",
            &ws_path,
            &[rel_a.display().to_string()],
            &[format!("{}=epoch", rel_a.display())],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        // Structured sidecar should now only contain b.rs.
        let after = read_conflict_tree_sidecar(&root, "ws-d").expect("sidecar still present");
        assert_eq!(after.conflicts.len(), 1);
        assert!(after.conflicts.contains_key(&rel_b));
        assert!(!after.conflicts.contains_key(&rel_a));

        // a.rs was written from epoch side.
        let a_bytes = std::fs::read(ws_path.join(&rel_a)).unwrap();
        assert_eq!(a_bytes, b"EPOCH_A\n");
        // b.rs left untouched (still has our marker placeholder).
        let b_bytes = std::fs::read(ws_path.join(&rel_b)).unwrap();
        assert_eq!(b_bytes, b"<<<<<<< markers\n");
    }

    #[test]
    fn resolve_falls_back_to_legacy_when_structured_sidecar_missing() {
        // Verified at the dispatch level: `read_conflict_tree_sidecar` returns
        // None if the file doesn't exist, so `resolve::run` skips the
        // structured branch. Here we just assert the reader's None behavior
        // on a fully-shaped workspace to lock the invariant in place.
        let (_td, root, _ws_path, _repo) = setup_ws_repo("ws-e");
        // No conflict-tree.json written.
        let got = read_conflict_tree_sidecar(&root, "ws-e");
        assert!(
            got.is_none(),
            "missing sidecar must trigger legacy fallback"
        );

        // And once a malformed sidecar is present the reader still returns
        // None (i.e. falls back to legacy rather than raising).
        let dir = sidecar_dir(&root, "ws-e");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("conflict-tree.json"), "not-json").unwrap();
        assert!(read_conflict_tree_sidecar(&root, "ws-e").is_none());
    }

    #[test]
    fn resolve_structured_list_json_reports_paths() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-f");
        let (rel_a, c_a) =
            make_content_conflict("a.rs", b"E\n", b"W\n", "ws-f", &repo);
        let (rel_b, c_b) =
            make_content_conflict("b.rs", b"E\n", b"W\n", "ws-f", &repo);
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel_a.clone(), c_a);
        tree.conflicts.insert(rel_b.clone(), c_b);

        // List mode — should not touch worktree.
        std::fs::write(ws_path.join(&rel_a), b"original-a").unwrap();
        std::fs::write(ws_path.join(&rel_b), b"original-b").unwrap();

        run_structured(
            &root,
            "ws-f",
            &ws_path,
            &[],
            &[],
            true,
            OutputFormat::Json,
            tree,
        )
        .unwrap();

        // Worktree content unchanged.
        assert_eq!(std::fs::read(ws_path.join(&rel_a)).unwrap(), b"original-a");
        assert_eq!(std::fs::read(ws_path.join(&rel_b)).unwrap(), b"original-b");
    }

    #[test]
    fn resolve_structured_per_atom_deferred_documented() {
        // Per-atom resolution is intentionally deferred in V1. This test
        // locks the current behavior: a `cf-N=NAME` spec on the structured
        // path is rejected with an error pointing to the atom follow-up.
        //
        // When per-atom lands, replace this test with a positive one.
        let err = parse_decisions(&["cf-0=alice".into()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Per-block") || msg.contains("cf-"),
            "expected cf-N rejection, got: {msg}"
        );
        assert!(
            msg.contains("follow-up") || msg.contains("structured"),
            "error should hint at deferred atom support, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-2cc1 — auto-commit when full resolution clears the sidecar
    // -----------------------------------------------------------------------

    /// After `--keep` resolves the last remaining conflict, the resolver
    /// should auto-commit the resolution so HEAD is clean and the merge-time
    /// marker gate doesn't see the pre-resolve blobs.
    #[test]
    fn resolve_keep_auto_commits_when_fully_resolved() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-autocommit");

        // Seed an initial commit containing a file with marker bytes so
        // HEAD reflects an unresolved state (same shape as post-rebase).
        std::fs::write(ws_path.join("x.rs"), b"<<<<<<< epoch (current)\nEPOCH\n=======\nWS\n>>>>>>> ws-autocommit\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .unwrap();
        assert!(add.success());
        let commit = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&ws_path)
            .status()
            .unwrap();
        assert!(commit.success());

        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .unwrap();
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel, conflict) =
            make_content_conflict("x.rs", b"EPOCH\n", b"WS\n", "ws-autocommit", &repo);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-autocommit",
            &ws_path,
            &[],
            &["epoch".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        // HEAD must have advanced — the resolution is committed.
        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .unwrap();
        let head_after_sha = String::from_utf8_lossy(&head_after.stdout)
            .trim()
            .to_owned();
        assert_ne!(
            head_before_sha, head_after_sha,
            "auto-commit should advance HEAD"
        );

        // The committed tree should no longer contain `<<<<<<<` — the new
        // HEAD reflects the resolved content, not the marker blob.
        let show = std::process::Command::new("git")
            .args(["show", "HEAD:x.rs"])
            .current_dir(&ws_path)
            .output()
            .unwrap();
        let body = String::from_utf8_lossy(&show.stdout);
        assert!(
            !body.contains("<<<<<<<"),
            "auto-commit should produce a HEAD tree without markers, got:\n{body}"
        );
        assert!(body.contains("EPOCH"));
    }

    /// When the resolve doesn't fully clear the tree, nothing gets
    /// auto-committed (partial progress is left for the user to review).
    #[test]
    fn resolve_keep_does_not_autocommit_on_partial_resolution() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-partial");

        // Make an initial commit so HEAD exists (avoids "No commits yet").
        std::fs::write(ws_path.join("seed"), b"seed").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&ws_path)
            .status()
            .unwrap();
        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .unwrap();
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel_a, c_a) =
            make_content_conflict("a.rs", b"EPOCH_A\n", b"WS_A\n", "ws-partial", &repo);
        let (rel_b, c_b) =
            make_content_conflict("b.rs", b"EPOCH_B\n", b"WS_B\n", "ws-partial", &repo);
        std::fs::write(ws_path.join(&rel_a), b"marker-a").unwrap();
        std::fs::write(ws_path.join(&rel_b), b"marker-b").unwrap();

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel_a.clone(), c_a);
        tree.conflicts.insert(rel_b.clone(), c_b);

        // Resolve only a.rs — b.rs stays conflicted. Auto-commit must NOT
        // fire in this case.
        run_structured(
            &root,
            "ws-partial",
            &ws_path,
            &[rel_a.display().to_string()],
            &[format!("{}=epoch", rel_a.display())],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .unwrap();
        let head_after_sha = String::from_utf8_lossy(&head_after.stdout)
            .trim()
            .to_owned();
        assert_eq!(
            head_before_sha, head_after_sha,
            "partial resolve must not advance HEAD"
        );
    }

    // -----------------------------------------------------------------------
    // bn-2pry — --keep both on modify/delete does not resurrect base content
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_keep_both_modify_delete_does_not_resurrect_base() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-mod-del");

        // Seed an initial commit so auto-commit can run.
        std::fs::write(ws_path.join("seed"), b"seed").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&ws_path)
            .status()
            .unwrap();

        // Build a ModifyDelete conflict with real blobs:
        //   modifier side: "MODIFIED\n"  (the *new* epoch content)
        //   deleter side: stores the *pre-delete* (base) content, which is
        //     the bug source — a naive `--keep both` concat would append
        //     this base blob's bytes under the workspace-side banner.
        let modifier_oid = {
            use maw_git::GitRepo;
            let oid = repo.write_blob(b"MODIFIED\n").unwrap();
            GitOid::new(&oid.to_string()).unwrap()
        };
        let base_oid = {
            use maw_git::GitRepo;
            let oid = repo.write_blob(b"BASE_PREDELETE\n").unwrap();
            GitOid::new(&oid.to_string()).unwrap()
        };
        let rel = PathBuf::from("dir/file.txt");
        let conflict = Conflict::ModifyDelete {
            path: rel.clone(),
            file_id: FileId::new(7),
            modifier: ConflictSide::new(
                EPOCH_LABEL.to_owned(),
                modifier_oid.clone(),
                ord("epoch"),
            ),
            deleter: ConflictSide::new(
                "ws-mod-del".to_owned(),
                base_oid.clone(), // <-- the pre-delete blob, pretending to be "their" content
                ord("ws-mod-del"),
            ),
            modified_content: modifier_oid,
        };
        std::fs::create_dir_all(ws_path.join("dir")).unwrap();
        std::fs::write(ws_path.join(&rel), b"placeholder").unwrap();

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-mod-del",
            &ws_path,
            &[],
            &["both".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        let after = std::fs::read(ws_path.join(&rel)).unwrap();
        // The BASE_PREDELETE content must NOT appear — that would be silent
        // data resurrection.
        let after_str = String::from_utf8_lossy(&after);
        assert!(
            !after_str.contains("BASE_PREDELETE"),
            "--keep both on modify/delete resurrected base content: {after_str}"
        );
        assert!(
            after_str.contains("MODIFIED"),
            "--keep both on modify/delete should keep modifier side: {after_str}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-mg0j — symlink mode preservation on `--keep`
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn resolve_preserves_symlink_mode_on_keep() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-sym");

        // Seed an initial commit.
        std::fs::write(ws_path.join("seed"), b"seed").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&ws_path)
            .status()
            .unwrap();

        // Build a conflict where the workspace-side is flagged as a
        // symlink whose blob content is the link target "a.txt\n" without
        // the newline (git stores symlink targets without a trailing LF).
        let ws_target_oid = {
            use maw_git::GitRepo;
            let oid = repo.write_blob(b"a.txt").unwrap();
            GitOid::new(&oid.to_string()).unwrap()
        };
        let epoch_oid = {
            use maw_git::GitRepo;
            let oid = repo.write_blob(b"b.txt").unwrap();
            GitOid::new(&oid.to_string()).unwrap()
        };
        let rel = PathBuf::from("link");
        let conflict = Conflict::AddAdd {
            path: rel.clone(),
            sides: vec![
                ConflictSide::with_mode(
                    EPOCH_LABEL.to_owned(),
                    epoch_oid,
                    ord("epoch"),
                    Some(ConflictSideMode::Link),
                ),
                ConflictSide::with_mode(
                    "ws-sym".to_owned(),
                    ws_target_oid,
                    ord("ws-sym"),
                    Some(ConflictSideMode::Link),
                ),
            ],
        };
        // Placeholder worktree file (what materialize left behind).
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch\nb.txt\n=======\na.txt\n>>>>>>> ws-sym\n",
        )
        .unwrap();

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-sym",
            &ws_path,
            &[],
            &["ws-sym".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .unwrap();

        // The resolved path must be a symlink pointing at `a.txt`.
        let full = ws_path.join(&rel);
        let meta = std::fs::symlink_metadata(&full).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "resolved path should be a symlink, got file_type={:?}",
            meta.file_type()
        );
        let target = std::fs::read_link(&full).unwrap();
        assert_eq!(target, Path::new("a.txt"));
    }

    // -----------------------------------------------------------------------
    // bn-24tl — corrupt sidecar produces a clear error rather than falling
    // back to the (broken-for-placeholder-content) legacy stripper
    // -----------------------------------------------------------------------

    /// When a structured sidecar *exists* but is malformed JSON,
    /// `read_conflict_tree_sidecar` returns `None` — matching the contract
    /// on a missing sidecar. The `resolve::run` dispatcher distinguishes
    /// these two cases by `Path::exists()` and bails with a clear message
    /// on the malformed-but-present case (bn-24tl).
    ///
    /// This test locks in the reader's behavior; the dispatcher-level
    /// bail is additionally covered by the integration test for
    /// `resolve::run` against a real repo layout.
    #[test]
    fn read_conflict_tree_sidecar_malformed_triggers_bn_24tl_branch() {
        let (_td, root, _ws_path, _repo) = setup_ws_repo("ws-corrupt");
        let sidecar = structured_sidecar_path(&root, "ws-corrupt");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"this is not valid json{{").unwrap();

        // Sidecar exists on disk.
        assert!(sidecar.exists(), "sidecar path should exist");
        // But the reader returns None (unparseable). This is the
        // precise combination `resolve::run` uses to fire the bn-24tl
        // "cannot resolve — regenerate" bail instead of silently
        // falling through to the legacy stripper.
        assert!(read_conflict_tree_sidecar(&root, "ws-corrupt").is_none());
    }
}
