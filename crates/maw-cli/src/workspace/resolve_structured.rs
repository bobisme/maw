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

use maw_core::config::ManifoldConfig;
use maw_core::merge::materialize::looks_text;
use maw_core::merge::types::ConflictTree;
use maw_core::model::conflict::{Conflict, ConflictSide, ConflictSideMode};
use maw_core::model::types::GitOid;
use maw_git::{self as git, GitRepo};

use crate::format::OutputFormat;
use crate::workspace::sync::sanity::{PostMergeSanityConfig, SanityFailure, run_post_merge_sanity};

/// Literal workspace label used by rebase's epoch-delta seed side (see
/// `sync::rebase::promote_overlaps_to_conflicts` — the "ours" side is
/// constructed with `workspace = "epoch"`). Kept `pub(crate)` so tests and
/// documentation consumers can reference the canonical name.
#[allow(dead_code)]
pub const EPOCH_LABEL: &str = "epoch";

// ---------------------------------------------------------------------------
// Sidecar paths & I/O
// ---------------------------------------------------------------------------

/// Directory holding sidecars for `ws_name` under `root`.
fn sidecar_dir(root: &Path, ws_name: &str) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(ws_name)
}

/// Path to `conflict-tree.json` for `ws_name`.
pub fn structured_sidecar_path(root: &Path, ws_name: &str) -> PathBuf {
    sidecar_dir(root, ws_name).join("conflict-tree.json")
}

/// Path to the legacy flat sidecar.
pub fn legacy_sidecar_path(root: &Path, ws_name: &str) -> PathBuf {
    sidecar_dir(root, ws_name).join("rebase-conflicts.json")
}

/// Read and deserialize `conflict-tree.json` for `ws_name`, if present.
///
/// Returns `None` when the file is missing, unreadable, or can't be parsed as
/// a [`ConflictTree`]. Callers should fall back to the legacy marker-scan
/// path in that case — this keeps pre-gjm8 workspaces working unchanged.
pub fn read_conflict_tree_sidecar(root: &Path, ws_name: &str) -> Option<ConflictTree> {
    let path = structured_sidecar_path(root, ws_name);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<ConflictTree>(&text).ok()
}

/// Delete both structured and legacy conflict sidecars for `ws_name`.
pub fn clear_conflict_sidecars(root: &Path, ws_name: &str) -> Result<()> {
    let structured = structured_sidecar_path(root, ws_name);
    if structured.exists() {
        std::fs::remove_file(&structured)
            .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", structured.display()))?;
    }

    let legacy = legacy_sidecar_path(root, ws_name);
    if legacy.exists() {
        std::fs::remove_file(&legacy)
            .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", legacy.display()))?;
    }

    Ok(())
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
pub fn list_conflicts(
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

    println!("{} structured conflict(s) in '{workspace}':", entries.len());
    for (path, conflict) in &entries {
        let shape = conflict.variant_name();
        let sides_desc = conflict.workspaces().join(", ");
        match conflict {
            Conflict::Content { atoms, .. } => {
                if atoms.is_empty() {
                    println!("  {}  [{shape}] sides=[{sides_desc}]", path.display());
                } else {
                    println!(
                        "  {}  [{shape}] sides=[{sides_desc}] atoms={}",
                        path.display(),
                        atoms.len()
                    );
                }
            }
            // bn-heb8: when a ModifyDelete was caused by an epoch rename,
            // append the rename target so the user knows where the content went.
            Conflict::ModifyDelete {
                rename_hint: Some(new_path),
                ..
            } => {
                println!(
                    "  {}  [{shape}] sides=[{sides_desc}] (renamed to {})",
                    path.display(),
                    new_path.display()
                );
            }
            _ => {
                println!("  {}  [{shape}] sides=[{sides_desc}]", path.display());
            }
        }
    }

    println!();
    println!("To resolve:");
    println!("  maw ws resolve {workspace} --keep epoch            # keep epoch version");
    println!(
        "  maw ws resolve {workspace} --keep <ws-name>        # keep a specific workspace side"
    );
    println!(
        "  maw ws resolve {workspace} --keep both             # keep all sides (concatenated)"
    );

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
pub enum Decision {
    All(String),
    File(PathBuf, String),
}

/// Parse flat `--keep` arguments into structured decisions.
///
/// Block-level (`cf-N=NAME`) keep-specs are not supported on the structured
/// path in V1 — the structured sidecar uses atoms/paths, not cf-IDs. Such
/// specs are rejected with an error so the CLI surface is predictable.
pub fn parse_decisions(raw: &[String]) -> Result<Vec<Decision>> {
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

/// Outcome of matching `target` against a side list.
enum SideMatch<'a> {
    /// A single side matched unambiguously (exact / comma-split / prefix).
    One(&'a ConflictSide),
    /// Nothing matched.
    None,
    /// The `target` is a prefix of multiple qualified side names
    /// (e.g. `feat` matching both `feat#merge-parent-0` and
    /// `feat#merge-parent-1`). Returns every matching side so the caller
    /// can render a helpful error.
    Ambiguous(Vec<&'a ConflictSide>),
}

/// Find the `ConflictSide` whose `workspace` matches `target`.
///
/// Match order (first hit wins):
///   1. **Exact** — `s.workspace == target` or `target` appears as a
///      comma-separated component of `s.workspace`.
///   2. **Qualified prefix** (bn-2ras) — if exactly one side has
///      `s.workspace == "{target}#..."`, that side matches.
///      Multi-parent merge commits produce side names like
///      `feat#merge-parent-0` / `feat#merge-parent-1`; users typing
///      `--keep feat` expect the obvious thing to happen when only one
///      such side exists. If two or more sides share the prefix, we
///      return `Ambiguous` so the caller can print the qualified
///      alternatives.
fn match_sides<'a>(sides: &'a [ConflictSide], target: &str) -> SideMatch<'a> {
    // 1. Exact / comma-split match.
    for s in sides {
        if s.workspace == target || s.workspace.split(',').any(|p| p.trim() == target) {
            return SideMatch::One(s);
        }
    }
    // 2. Qualified-prefix match (`target#...`).
    let prefix = format!("{target}#");
    let prefix_hits: Vec<&ConflictSide> = sides
        .iter()
        .filter(|s| s.workspace.starts_with(&prefix))
        .collect();
    match prefix_hits.len() {
        0 => SideMatch::None,
        1 => SideMatch::One(prefix_hits[0]),
        _ => SideMatch::Ambiguous(prefix_hits),
    }
}

/// Convenience wrapper returning the matched side (ignoring ambiguity).
/// Ambiguous matches return `None` — callers that want the full outcome
/// should use [`match_sides`] directly so they can surface a diagnostic.
fn pick_side<'a>(sides: &'a [ConflictSide], target: &str) -> Option<&'a ConflictSide> {
    match match_sides(sides, target) {
        SideMatch::One(s) => Some(s),
        SideMatch::None | SideMatch::Ambiguous(_) => None,
    }
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
            match match_sides(sides, target) {
                SideMatch::One(side) => Ok(Some(side.content.clone())),
                SideMatch::Ambiguous(hits) => {
                    // bn-2ras: `--keep feat` but the conflict has
                    // `feat#merge-parent-0` AND `feat#merge-parent-1`. List
                    // the qualified names so the user can pick one.
                    let qualified: Vec<&str> = hits.iter().map(|s| s.workspace.as_str()).collect();
                    bail!(
                        "`--keep {target}` is ambiguous — the conflict has multiple \
                         sides whose workspace starts with `{target}#`: [{}]. \
                         Use the fully-qualified side name, e.g. `--keep \"{}\"`.",
                        qualified.join(", "),
                        qualified[0],
                    );
                }
                SideMatch::None => {
                    let available: Vec<&str> = sides.iter().map(|s| s.workspace.as_str()).collect();
                    bail!(
                        "Side '{}' not found for path. Available: [{}], plus 'both'.",
                        target,
                        available.join(", ")
                    );
                }
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
        /// bn-3mbj: how this resolution was produced. Used by the caller to
        /// print a human-readable "non-trivial resolve" note when a 3-way
        /// merge ran.
        kind: ResolveKind,
        /// bn-c5ui: populated when a driver-produced three-way merge output
        /// failed the post-merge sanity check. The file is still written (the
        /// user asked for it; non-code files legitimately trip AST checks) but
        /// the auto-commit is suppressed so nothing lands in HEAD silently.
        sanity_failure: Option<SanityFailure>,
    },
    /// Removed the file from the worktree (modify/delete → accept delete).
    Deleted,
    /// Caller asked for a side that doesn't exist / can't resolve. The
    /// carried message is logged by the caller into the skipped list.
    Skipped(#[allow(dead_code)] String),
}

/// bn-3mbj / bn-1nwn: how a single-side `--keep <ws>` resolution was produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolveKind {
    /// Plain blob-replace — legacy sidecar (no `base_content`), binary
    /// content, or N>2 sides where per-hunk merge is not applicable.
    BlobReplace,
    /// 3-way merge of (base, epoch, ws) succeeded cleanly — all hunks
    /// merged without conflicts.
    ThreeWayClean,
    /// 3-way merge had internal conflicts; we resolved them with the
    /// workspace winning (`ConflictResolution::Theirs` against epoch=ours).
    ThreeWayWsWins,
    /// bn-1nwn: `--keep epoch` via per-hunk 3-way merge — epoch wins
    /// conflicted hunks, workspace's non-overlapping edits are preserved.
    ThreeWayEpochWins,
    /// bn-1nwn: `--keep both` via per-hunk 3-way union merge — both sides'
    /// conflicting lines are included (no markers), clean regions merged.
    ThreeWayUnion,
    /// Sidecar lacked `base_content` for the picked side — fell back to
    /// blob-replace and emitted a warning.
    LegacyBlobReplaceWarned,
}

// ---------------------------------------------------------------------------
// Binary-detection heuristic — imported from maw-core (bn-1hmz)
// ---------------------------------------------------------------------------
// `looks_text` is imported from `maw_core::merge::materialize` via the
// `use` at the top of this file. A NUL byte is a strong binary signal;
// invalid UTF-8 is also treated as binary so we don't splice arbitrary bytes
// into marker-based output. Keeping the definition in one place prevents the
// multi-copy drift that was the root cause of bn-1hmz.

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

/// bn-1mn0: resolve the effective merge-base OID for a per-hunk keep path.
///
/// The base OID may live in two places depending on how the sidecar was
/// produced:
///
/// * **Normal sidecars** (written by `sync::rebase`): each `ConflictSide`
///   carries `base_content = Some(ref_old)` (set by `ConflictSide::with_base`).
/// * **Reconstructed sidecars** (from placeholder headers, bn-39i8 / bn-1mn0):
///   after the fix the sides also carry `base_content`, but we fall back to
///   `Conflict::Content.base` for pre-fix reconstructed sidecars and for any
///   legacy sidecar where only the top-level field was populated.
/// * **Keep-both path** already reads `Conflict::Content.base` directly; this
///   helper is used by `--keep epoch` and `--keep <ws>` to get the same reach.
///
/// Returns `None` when neither source has a base OID (add/add, binary with no
/// base, or a legacy sidecar from before bn-3mbj).
fn effective_base_oid<'a>(
    conflict_base: Option<&'a GitOid>,
    side: &'a ConflictSide,
) -> Option<&'a GitOid> {
    side.base_content.as_ref().or(conflict_base)
}

/// Apply a resolution for a single `(path, conflict)` and produce the output.
///
/// `sanity_cfg` is used to run the bn-c5ui post-merge sanity check on
/// driver-produced three-way merge outputs before returning. When the check
/// trips the returned `PathOutcome::Wrote.sanity_failure` is populated; the
/// caller decides whether to suppress auto-commit.
#[expect(
    clippy::too_many_lines,
    reason = "single decision dispatch covers --keep both, --keep epoch, 3-way (bn-3mbj/bn-1nwn), \
              legacy fallback, and single-side blob-replace; splitting fragments the control flow"
)]
fn apply_decision(
    repo: &dyn GitRepo,
    conflict: &Conflict,
    target: &str,
    rel_path: &Path,
    workspace: &str,
    sanity_cfg: PostMergeSanityConfig,
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
        if let Conflict::ModifyDelete { modifier, .. } = conflict {
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
                kind: ResolveKind::BlobReplace,
                sanity_failure: None,
            });
        }

        // bn-1nwn: `--keep both` on a 2-sided Content conflict with a base
        // OID uses a per-hunk 3-way union merge so that non-overlapping edits
        // from each side are preserved and conflicting hunks include both
        // sides' lines (no markers). Fall back to the legacy blob-concat for
        // N>2 sides, binary content, AddAdd (no base OID), or missing base.
        //
        // bn-1mn0: use effective_base_oid so reconstructed sidecars (where
        // only the side-level base_content was populated, or only the
        // top-level base field) are treated identically to normal sidecars.
        if let Conflict::Content {
            sides,
            base: conflict_base,
            ..
        } = conflict
            && sides.len() == 2
            && let (Some(epoch_side), Some(ws_side)) = (
                sides.iter().find(|s| s.workspace == EPOCH_LABEL),
                sides.iter().find(|s| s.workspace != EPOCH_LABEL),
            )
            && let Some(base_oid) = effective_base_oid(conflict_base.as_ref(), epoch_side)
        {
            let base_oid_git: git::GitOid = base_oid
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid base blob oid {base_oid}: {e}"))?;
            let epoch_oid_git: git::GitOid = epoch_side.content.as_str().parse().map_err(|e| {
                anyhow::anyhow!("invalid epoch blob oid {}: {e}", epoch_side.content)
            })?;
            let ws_oid_git: git::GitOid = ws_side
                .content
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid ws blob oid {}: {e}", ws_side.content))?;

            let base_bytes = repo
                .read_blob(base_oid_git)
                .map_err(|e| anyhow::anyhow!("read_blob({base_oid}) failed: {e}"))?;
            let epoch_bytes = repo
                .read_blob(epoch_oid_git)
                .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", epoch_side.content))?;
            let ws_bytes = repo
                .read_blob(ws_oid_git)
                .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", ws_side.content))?;

            // Per-hunk union only makes sense for text files.
            if looks_text(&base_bytes) && looks_text(&epoch_bytes) && looks_text(&ws_bytes) {
                let resolved = maw_git::merge::merge_text_with_style(
                    &base_bytes,
                    &epoch_bytes,
                    &ws_bytes,
                    EPOCH_LABEL,
                    "base",
                    ws_side.workspace.as_str(),
                    maw_git::merge::ConflictResolution::Union,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "--keep both union merge failed for {}: {e}",
                        rel_path.display()
                    )
                })?;
                let bytes = match resolved {
                    maw_git::merge::MergeResult::Clean(b)
                    | maw_git::merge::MergeResult::Conflict(b) => b,
                };
                // bn-c5ui: run post-merge sanity check before surfacing the
                // result. The file is still written (the user asked for it;
                // non-code files legitimately trip AST checks) but the caller
                // suppresses auto-commit when sanity_failure is Some(_).
                //
                // For `--keep both` (union mode) the size-delta check is
                // suppressed: the union intentionally includes content from
                // BOTH sides, so the output is by design larger than either
                // input alone — triggering the size-ratio formula even for
                // legitimate union merges. Only the AST check (language-aware,
                // only fires when both inputs parse cleanly but merged does
                // not) is meaningful for union outputs.
                let union_cfg = PostMergeSanityConfig {
                    size_ratio_max: f64::INFINITY,
                };
                let sanity_failure = run_post_merge_sanity(
                    rel_path,
                    &base_bytes,
                    &epoch_bytes,
                    &ws_bytes,
                    &bytes,
                    union_cfg,
                )
                .err();
                return Ok(PathOutcome::Wrote {
                    bytes,
                    // Union of multiple sides is never a valid symlink target.
                    mode: None,
                    kind: ResolveKind::ThreeWayUnion,
                    sanity_failure,
                });
            }
            // Binary or mixed text/binary — fall through to legacy concat.
        }

        // bn-1mn0: if this is a 2-sided text Content conflict with no base
        // available anywhere, emit a warning before the blob-concat so the
        // user knows the resolution may drop non-overlapping workspace edits.
        if let Conflict::Content {
            sides,
            base: conflict_base,
            ..
        } = conflict
            && sides.len() == 2
            && {
                let epoch_fallback_side = sides
                    .iter()
                    .find(|s| s.workspace == EPOCH_LABEL)
                    .unwrap_or(&sides[0]);
                effective_base_oid(conflict_base.as_ref(), epoch_fallback_side).is_none()
            }
        {
            eprintln!(
                "warning: --keep both is using legacy blob-concat semantics for {}; \
                 no merge-base OID is available so non-overlapping edits may be \
                 duplicated or dropped. Verify with: maw exec {workspace} -- git diff HEAD~1 -- {}",
                rel_path.display(),
                rel_path.display(),
            );
        }

        // Legacy blob-concat fallback: N>2 sides, AddAdd (no base), binary.
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
        // For the concat fallback, write as a regular file (concat of multiple
        // sides is never a valid symlink target). `any_side_mode` returns the
        // first hint for diagnostics but we don't apply it here.
        let _hint = any_side_mode(conflict);
        return Ok(PathOutcome::Wrote {
            bytes: buf,
            mode: None,
            kind: ResolveKind::BlobReplace,
            sanity_failure: None,
        });
    }

    // Single side. Translate `epoch` synonym through a canonical label.
    // The sidecar seeds the epoch side with `workspace == "epoch"` (see
    // `sync::rebase::promote_overlaps_to_conflicts`), so no aliasing required.
    let mode_hint = pick_single_side_mode(conflict, target);

    // bn-1nwn: `--keep epoch` should also perform a per-hunk 3-way merge
    // (epoch wins conflicted hunks) rather than a whole-blob replacement, so
    // the workspace's non-overlapping edits are preserved. We use the same
    // plumbing as the bn-3mbj `--keep <ws>` path below: base vs epoch vs ws,
    // with `ConflictResolution::Ours` (epoch=ours wins conflicts).
    //
    // Conditions for the per-hunk path (same guard as the ws path):
    //   • `Conflict::Content` with exactly 2 sides (epoch + one ws side)
    //   • a base OID is available via effective_base_oid (epoch side's
    //     base_content, or the top-level Conflict::Content.base as fallback)
    //   • all three blobs are text (no NUL / valid UTF-8)
    //
    // bn-1mn0: use effective_base_oid so that reconstructed sidecars (whose
    // sides may only have base_content set after the fix, or that fall back to
    // the top-level base field) get the same per-hunk treatment as normal
    // sidecars. If no base is available anywhere, fall through with a warning.
    if target == EPOCH_LABEL
        && let Conflict::Content {
            sides,
            base: conflict_base,
            ..
        } = conflict
        && sides.len() == 2
        && let Some(epoch_side) = sides.iter().find(|s| s.workspace == EPOCH_LABEL)
        && let Some(ws_side) = sides.iter().find(|s| s.workspace != EPOCH_LABEL)
        && let Some(base_oid) = effective_base_oid(conflict_base.as_ref(), epoch_side)
    {
        let base_oid_git: git::GitOid = base_oid
            .as_str()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid base blob oid {base_oid}: {e}"))?;
        let epoch_oid_git: git::GitOid =
            epoch_side.content.as_str().parse().map_err(|e| {
                anyhow::anyhow!("invalid epoch blob oid {}: {e}", epoch_side.content)
            })?;
        let ws_oid_git: git::GitOid = ws_side
            .content
            .as_str()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid ws blob oid {}: {e}", ws_side.content))?;

        let base_bytes = repo
            .read_blob(base_oid_git)
            .map_err(|e| anyhow::anyhow!("read_blob({base_oid}) failed: {e}"))?;
        let epoch_bytes = repo
            .read_blob(epoch_oid_git)
            .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", epoch_side.content))?;
        let ws_bytes = repo
            .read_blob(ws_oid_git)
            .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", ws_side.content))?;

        if looks_text(&base_bytes) && looks_text(&epoch_bytes) && looks_text(&ws_bytes) {
            // epoch=ours wins conflicted hunks; ws's non-overlapping edits
            // survive as clean merged body content.
            let resolved = maw_git::merge::merge_text_with_style(
                &base_bytes,
                &epoch_bytes,
                &ws_bytes,
                EPOCH_LABEL,
                "base",
                ws_side.workspace.as_str(),
                maw_git::merge::ConflictResolution::Ours,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "--keep epoch 3-way merge failed for {}: {e}",
                    rel_path.display()
                )
            })?;
            let bytes = match resolved {
                maw_git::merge::MergeResult::Clean(b)
                | maw_git::merge::MergeResult::Conflict(b) => b,
            };
            // bn-c5ui: run post-merge sanity check on driver-produced output.
            let sanity_failure = run_post_merge_sanity(
                rel_path,
                &base_bytes,
                &epoch_bytes,
                &ws_bytes,
                &bytes,
                sanity_cfg,
            )
            .err();
            return Ok(PathOutcome::Wrote {
                bytes,
                mode: mode_hint,
                kind: ResolveKind::ThreeWayEpochWins,
                sanity_failure,
            });
        }
        // Binary: fall through to blob-replace below.
    }
    // No matching per-hunk conditions (AddAdd, ModifyDelete, missing base,
    // binary, N>2) for --keep epoch — fall through to pick_single_side_oid.
    //
    // bn-1mn0: if the conflict is Content with 2 sides but NO base available
    // (neither side.base_content nor Conflict::Content.base), emit a warning
    // before the blob-replace so the user knows the resolution is lossy.
    if target == EPOCH_LABEL
        && let Conflict::Content {
            sides,
            base: conflict_base,
            ..
        } = conflict
        && sides.len() == 2
        && sides.iter().any(|s| s.workspace == EPOCH_LABEL)
        && effective_base_oid(
            conflict_base.as_ref(),
            sides
                .iter()
                .find(|s| s.workspace == EPOCH_LABEL)
                .unwrap_or(&sides[0]),
        )
        .is_none()
    {
        eprintln!(
            "warning: --keep epoch is using legacy blob-replace semantics for {}; \
             no merge-base OID is available so non-overlapping workspace edits may be \
             dropped. Verify with: maw exec {workspace} -- git diff HEAD~1 -- {}",
            rel_path.display(),
            rel_path.display(),
        );
    }

    // bn-3mbj: `--keep <ws-name>` (anything that isn't the literal `epoch`
    // label) should re-apply the workspace's intent on top of the new epoch
    // rather than wholesale replacing the file with the workspace's
    // pre-rebase blob. We only attempt the 3-way merge for `Conflict::Content`
    // (where there's a meaningful epoch side and merge base) and only when
    // a base OID is available (via effective_base_oid) AND the conflict has a
    // matching `epoch` side. If either is missing we fall back to the legacy
    // blob-replace path with a one-line stderr warning so old in-flight
    // sidecars keep resolving.
    if target != EPOCH_LABEL
        && let Conflict::Content {
            sides,
            base: conflict_base,
            ..
        } = conflict
        && let SideMatch::One(ws_side) = match_sides(sides, target)
    {
        let epoch_side = sides.iter().find(|s| s.workspace == EPOCH_LABEL);
        // bn-1mn0: use effective_base_oid so reconstructed sidecars (and any
        // legacy sidecar where only Conflict::Content.base was populated) also
        // get the 3-way path.
        if let (Some(base_oid), Some(epoch_side)) = (
            effective_base_oid(conflict_base.as_ref(), ws_side),
            epoch_side,
        ) {
            // Three-way path: read all three blobs and run merge_text. The
            // primary call uses `Diff3` so a clean merge stays clean. On
            // an internal conflict, retry with `Theirs` (workspace wins) —
            // this preserves the workspace's intent on overlapping hunks
            // while still picking up sibling-merged content elsewhere.
            let base_oid_git: git::GitOid = base_oid
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid base blob oid {base_oid}: {e}"))?;
            let epoch_oid_git: git::GitOid = epoch_side.content.as_str().parse().map_err(|e| {
                anyhow::anyhow!("invalid epoch blob oid {}: {e}", epoch_side.content)
            })?;
            let ws_oid_git: git::GitOid = ws_side
                .content
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid ws blob oid {}: {e}", ws_side.content))?;

            let base_bytes = repo
                .read_blob(base_oid_git)
                .map_err(|e| anyhow::anyhow!("read_blob({base_oid}) failed: {e}"))?;
            let epoch_bytes = repo
                .read_blob(epoch_oid_git)
                .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", epoch_side.content))?;
            let ws_bytes = repo
                .read_blob(ws_oid_git)
                .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", ws_side.content))?;

            // bn-1hmz: binary guard — if any blob fails the looks_text
            // heuristic, skip the 3-way text merge entirely and fall
            // through to the blob-replace path below (ws blob wins
            // wholesale, which is the correct semantics for --keep <ws>
            // on a binary file).  Without this guard, merge_text produces
            // a "clean" frankenstein result on binary files that happen
            // to contain 0x0A bytes, silently corrupting the file.
            if !looks_text(&base_bytes) || !looks_text(&epoch_bytes) || !looks_text(&ws_bytes) {
                // Fall through to blob-replace — the ws side wins wholesale.
                let bytes = ws_bytes;
                return Ok(PathOutcome::Wrote {
                    bytes,
                    mode: mode_hint,
                    kind: ResolveKind::BlobReplace,
                    sanity_failure: None,
                });
            }

            // Try the diff3 merge first — this is the same primitive
            // `try_clean_three_way_overlap` uses during rebase. We use
            // `epoch` as the "ours" label and the workspace name as the
            // "theirs" label for human-readable output / consistent
            // ordering with rebase's own labelling.
            let clean_attempt = maw_git::merge::merge_text(
                &base_bytes,
                &epoch_bytes,
                &ws_bytes,
                EPOCH_LABEL,
                "base",
                target,
            )
            .map_err(|e| anyhow::anyhow!("3-way merge failed for {}: {e}", rel_path.display()))?;

            match clean_attempt {
                maw_git::merge::MergeResult::Clean(bytes) => {
                    // bn-c5ui: run post-merge sanity check on driver-produced
                    // output.
                    let sanity_failure = run_post_merge_sanity(
                        rel_path,
                        &base_bytes,
                        &epoch_bytes,
                        &ws_bytes,
                        &bytes,
                        sanity_cfg,
                    )
                    .err();
                    return Ok(PathOutcome::Wrote {
                        bytes,
                        mode: mode_hint,
                        kind: ResolveKind::ThreeWayClean,
                        sanity_failure,
                    });
                }
                maw_git::merge::MergeResult::Conflict(_) => {
                    // ws-wins on internal conflicts. With epoch as `ours`
                    // and ws as `theirs`, `ConflictResolution::Theirs`
                    // resolves overlapping hunks by taking the ws side —
                    // exactly the "workspace wins" semantics the spec
                    // calls for. Non-overlapping epoch edits stay.
                    let resolved = maw_git::merge::merge_text_with_style(
                        &base_bytes,
                        &epoch_bytes,
                        &ws_bytes,
                        EPOCH_LABEL,
                        "base",
                        target,
                        maw_git::merge::ConflictResolution::Theirs,
                    )
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "ws-wins 3-way merge failed for {}: {e}",
                            rel_path.display()
                        )
                    })?;
                    let bytes = match resolved {
                        // Should not happen — `Theirs` always resolves —
                        // but defensively accept either shape and use the
                        // bytes the merger produced.
                        maw_git::merge::MergeResult::Clean(b)
                        | maw_git::merge::MergeResult::Conflict(b) => b,
                    };
                    // bn-c5ui: run post-merge sanity check on driver-produced
                    // output.
                    let sanity_failure = run_post_merge_sanity(
                        rel_path,
                        &base_bytes,
                        &epoch_bytes,
                        &ws_bytes,
                        &bytes,
                        sanity_cfg,
                    )
                    .err();
                    return Ok(PathOutcome::Wrote {
                        bytes,
                        mode: mode_hint,
                        kind: ResolveKind::ThreeWayWsWins,
                        sanity_failure,
                    });
                }
            }
        }

        // Legacy sidecar (no base OID available on the picked side or at the
        // top-level Conflict::Content.base) — fall through to blob-replace,
        // but emit a one-line warning so the user knows to verify and so
        // future debugging has a paper trail.
        // bn-1mn0: use effective_base_oid for the guard so pre-fix
        // reconstructed sidecars (sides have no base_content but the top-level
        // base field may be set) are not incorrectly emitting a warning.
        if effective_base_oid(conflict_base.as_ref(), ws_side).is_none() {
            eprintln!(
                "warning: --keep {target} is using legacy blob-replace semantics for {}; \
                 sibling content from the target branch may be dropped. Verify with: \
                 maw exec {workspace} -- git diff HEAD~1 -- {}",
                rel_path.display(),
                rel_path.display(),
            );
            let oid = ws_side.content.clone();
            let git_oid: git::GitOid = oid
                .as_str()
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid blob oid {oid}: {e}"))?;
            let bytes = repo
                .read_blob(git_oid)
                .map_err(|e| anyhow::anyhow!("read_blob({oid}) failed: {e}"))?;
            return Ok(PathOutcome::Wrote {
                bytes,
                mode: mode_hint,
                kind: ResolveKind::LegacyBlobReplaceWarned,
                sanity_failure: None,
            });
        }
    }

    if let Some(oid) = pick_single_side_oid(conflict, target)? {
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
            kind: ResolveKind::BlobReplace,
            sanity_failure: None,
        })
    } else {
        // bn-heb8: when --keep epoch on a modify/delete discards the
        // workspace's edit, print a one-liner so the edit is recoverable
        // without git archaeology. This fires for both rename and plain
        // delete cases.
        if let Conflict::ModifyDelete {
            modifier,
            rename_hint,
            ..
        } = conflict
        {
            let oid = &modifier.content;
            if let Some(new_path) = rename_hint {
                eprintln!(
                    "note: discarded workspace edit at {} \
                     (blob {oid} — recover with: git cat-file blob {oid})\n\
                     note: epoch renamed this file to {}; apply your edit there",
                    rel_path.display(),
                    new_path.display(),
                );
            } else {
                eprintln!(
                    "note: discarded workspace edit at {} \
                     (blob {oid} — recover with: git cat-file blob {oid})",
                    rel_path.display(),
                );
            }
        }
        Ok(PathOutcome::Deleted)
    }
}

/// Apply `PathOutcome` to the worktree at `ws_path.join(rel)`.
///
/// Returns `Ok((true, kind, sanity_failure))` when the worktree was updated,
/// or `Ok((false, _, None))` when the outcome was `Skipped`. The `kind` and
/// `sanity_failure` only carry meaning when `bool` is `true`; callers use
/// them to print a "non-trivial resolve" note and to suppress auto-commit when
/// a sanity failure is present (bn-c5ui).
fn apply_outcome(
    ws_path: &Path,
    rel: &Path,
    outcome: PathOutcome,
) -> Result<(bool, ResolveKind, Option<SanityFailure>)> {
    match outcome {
        PathOutcome::Wrote {
            bytes,
            mode,
            kind,
            sanity_failure,
        } => {
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
                    std::fs::remove_file(&full)
                        .map_err(|e| anyhow::anyhow!("remove {}: {e}", full.display()))?;
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
                    std::fs::write(&full, &bytes)
                        .map_err(|e| anyhow::anyhow!("write {}: {e}", full.display()))?;
                }
                return Ok((true, kind, sanity_failure));
            }

            std::fs::write(&full, &bytes)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", full.display()))?;

            // bn-mg0j: executable bit.
            #[cfg(unix)]
            if matches!(mode, Some(ConflictSideMode::BlobExecutable)) {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o755);
                std::fs::set_permissions(&full, perms)
                    .map_err(|e| anyhow::anyhow!("chmod +x {}: {e}", full.display()))?;
            }

            Ok((true, kind, sanity_failure))
        }
        PathOutcome::Deleted => {
            let full = ws_path.join(rel);
            if full.is_file() || full.is_symlink() {
                std::fs::remove_file(&full)
                    .map_err(|e| anyhow::anyhow!("remove {}: {e}", full.display()))?;
            }
            Ok((true, ResolveKind::BlobReplace, None))
        }
        PathOutcome::Skipped(_) => Ok((false, ResolveKind::BlobReplace, None)),
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
#[expect(
    clippy::too_many_lines,
    reason = "structured resolver keeps path processing and auto-commit reporting together"
)]
pub fn run_structured(
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
                    bail!("Multiple blanket --keep flags. Use one, or use PATH=NAME per-file.");
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

    // bn-c5ui: load the sanity config the same way rebase does — fail closed
    // (defaults = strict ON, ratio 1.5x) when the config file is absent or
    // unparseable. A missing config is not a licence to skip the check.
    let manifold_config = ManifoldConfig::load(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).bootstrap_config_path(root),
    )
    .unwrap_or_default();
    let sanity_cfg = PostMergeSanityConfig::from_merge(&manifold_config.merge);

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
            println!(
                r#"{{"status":"clean","workspace":"{workspace}","structured":true,"message":"No structured conflicts found."}}"#
            );
        } else {
            println!("No structured conflicts found in '{workspace}'.");
        }
        return Ok(true);
    }

    let mut resolved = Vec::<PathBuf>::new();
    let mut three_way_clean = Vec::<PathBuf>::new();
    let mut three_way_ws_wins = Vec::<PathBuf>::new();
    // bn-1nwn: track paths resolved via per-hunk epoch-wins or union merge.
    let mut three_way_epoch_wins = Vec::<PathBuf>::new();
    let mut three_way_union = Vec::<PathBuf>::new();
    let mut skipped = Vec::<(PathBuf, String)>::new();
    // bn-c5ui: paths whose driver-produced output failed the post-merge sanity
    // check. The file is still written but auto-commit is suppressed for the
    // entire invocation (partial commits are confusing; the user must review
    // every flagged file before committing).
    let mut sanity_warnings: Vec<(PathBuf, SanityFailure)> = Vec::new();

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
        match apply_decision(repo_dyn, &conflict, &target, rel, workspace, sanity_cfg) {
            Ok(outcome) => match apply_outcome(ws_path, rel, outcome)? {
                (true, kind, maybe_failure) => {
                    tree.conflicts.remove(rel);
                    resolved.push(rel.clone());
                    if let Some(failure) = maybe_failure {
                        sanity_warnings.push((rel.clone(), failure));
                    }
                    match kind {
                        ResolveKind::ThreeWayClean => three_way_clean.push(rel.clone()),
                        ResolveKind::ThreeWayWsWins => three_way_ws_wins.push(rel.clone()),
                        ResolveKind::ThreeWayEpochWins => three_way_epoch_wins.push(rel.clone()),
                        ResolveKind::ThreeWayUnion => three_way_union.push(rel.clone()),
                        ResolveKind::BlobReplace | ResolveKind::LegacyBlobReplaceWarned => {}
                    }
                }
                (false, _, _) => {
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

    // bn-c5ui: emit loud warnings for any sanity-flagged paths BEFORE the
    // auto-commit decision so agents and users see them on every non-JSON
    // invocation. The file is already written (user asked for it; non-code
    // files legitimately trip AST checks). We just refuse to auto-commit.
    for (path, failure) in &sanity_warnings {
        eprintln!(
            "WARNING: resolved file failed the post-merge sanity check ({failure}); \
             review before merging: {}",
            path.display()
        );
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
    //
    // bn-c5ui: also skip auto-commit when ANY resolved path tripped the
    // post-merge sanity check (the user must review flagged files first; a
    // partial commit of the "clean" subset would be confusing).
    let auto_committed =
        if tree.conflicts.is_empty() && !resolved.is_empty() && sanity_warnings.is_empty() {
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
            tracing::warn!("auto-commit after resolve failed in '{workspace}': {e}");
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
        let committed_field = auto_commit_msg
            .as_ref()
            .map_or_else(String::new, |sha| format!(r#","auto_committed":"{sha}""#));
        // bn-c5ui: include sanity_warnings in JSON output.
        let sanity_warnings_json: Vec<String> = sanity_warnings
            .iter()
            .map(|(p, f)| {
                format!(
                    r#"{{"path":"{}","reason":"{}"}}"#,
                    p.display(),
                    f.to_string().replace('"', "\\\"")
                )
            })
            .collect();
        let sanity_field = if sanity_warnings_json.is_empty() {
            String::new()
        } else {
            format!(r#","sanity_warnings":[{}]"#, sanity_warnings_json.join(","))
        };
        println!(
            r#"{{"status":"ok","workspace":"{workspace}","structured":true,"resolved":[{}],"conflicts_remaining":{},"skipped":[{}]{}{}}}"#,
            resolved_json.join(","),
            tree.conflicts.len(),
            skipped_json.join(","),
            committed_field,
            sanity_field,
        );
    } else {
        for p in &resolved {
            // bn-3mbj / bn-1nwn: surface how each resolution was produced so
            // agents and users know whether per-hunk semantics were applied.
            if three_way_clean.contains(p) {
                println!(
                    "  resolved: {} (3-way merge: ws intent on top of epoch)",
                    p.display()
                );
            } else if three_way_ws_wins.contains(p) {
                println!(
                    "  resolved: {} (3-way merge: ws intent on top of epoch, ws wins on overlap)",
                    p.display()
                );
            } else if three_way_epoch_wins.contains(p) {
                println!(
                    "  resolved: {} (kept epoch in conflicted hunk(s); preserved cleanly-merged changes from both sides)",
                    p.display()
                );
            } else if three_way_union.contains(p) {
                println!(
                    "  resolved: {} (kept both sides in conflicted hunk(s); preserved cleanly-merged changes from both sides)",
                    p.display()
                );
            } else {
                println!("  resolved: {}", p.display());
            }
        }
        for (p, r) in &skipped {
            eprintln!("  skipped: {} — {r}", p.display());
        }
        if resolved.is_empty() && skipped.is_empty() {
            println!("Nothing to resolve.");
        } else if tree.conflicts.is_empty() {
            if sanity_warnings.is_empty() {
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
                    None => {
                        println!(
                            "\nAll structured conflicts resolved — workspace is ready for merge."
                        );
                    }
                }
            } else {
                // bn-c5ui: at least one resolved path failed the sanity check;
                // suppress the auto-commit and tell the user exactly what to
                // run after they have reviewed the flagged files.
                let n = sanity_warnings.len();
                let noun = if n == 1 { "path" } else { "paths" };
                eprintln!(
                    "\nAuto-commit suppressed: {n} resolved {noun} failed the post-merge \
                     sanity check (see WARNING(s) above). Review the flagged file(s), then:"
                );
                eprintln!("  maw exec {workspace} -- git add --");
                for (p, _) in &sanity_warnings {
                    eprintln!("    {}", p.display());
                }
                eprintln!(
                    "  maw exec {workspace} -- git commit -m \"resolve: apply --keep decisions \
                     for {} path(s) in '{workspace}'\"",
                    resolved.len()
                );
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
// TODO(gix): we intentionally shell out to `git` here rather than going
// through the `GitRepo` trait because: (a) the workspace is a regular
// worktree with a standard HEAD; (b) `git commit` handles the index + HEAD
// update atomically and respects existing user config (author/committer,
// hooks, signing); (c) the structured-resolve path has to succeed on
// `core.symlinks=true` worktrees where staging a symlink via the gix index
// surface would need more care. The symlink/mode preservation tests
// (`resolve_preserves_symlink_mode_on_keep`, bn-mg0j) depend on this CLI
// codepath. Migrating requires either porting hook/signing config plumbing
// into maw-git or accepting that resolve auto-commits skip user hooks —
// neither is in scope for bn-15wt.
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
        format!("resolve: {} (bn-gjm8 auto-commit)", resolved[0].display())
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

    use maw_core::model::conflict::ConflictSide;
    use maw_core::model::ordering::OrderingKey;
    use maw_core::model::patch::FileId;
    use maw_core::model::types::{EpochId, WorkspaceId};

    fn epoch() -> EpochId {
        EpochId::new(&"e".repeat(40)).expect("operation should succeed")
    }
    fn oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).expect("operation should succeed")
    }
    fn ord(ws: &str) -> OrderingKey {
        OrderingKey::new(
            epoch(),
            WorkspaceId::new(ws).expect("operation should succeed"),
            1,
            1_700_000_000_000,
        )
    }
    fn side(ws: &str, c: char) -> ConflictSide {
        ConflictSide::new(ws.to_owned(), oid(c), ord(ws))
    }
    /// `side()` requires a valid `WorkspaceId` for the `OrderingKey`. Merge-side
    /// labels like `feat#merge-parent-0` are not valid workspace ids, so
    /// tests that exercise those labels build the side with a distinct
    /// ordering-key workspace (the label is what's visible in the conflict).
    fn labeled_side(label: &str, ord_ws: &str, c: char) -> ConflictSide {
        ConflictSide::new(label.to_owned(), oid(c), ord(ord_ws))
    }

    #[test]
    fn parse_decisions_rejects_cf_specs() {
        let err = parse_decisions(&["cf-0=alice".into()]).expect_err("operation should fail");
        let msg = err.to_string();
        assert!(msg.contains("cf-"), "msg={msg}");
    }

    #[test]
    fn parse_decisions_parses_all_and_file() {
        let specs = parse_decisions(&["epoch".into(), "src/main.rs=bn-abc".into()])
            .expect("operation should succeed");
        assert_eq!(specs.len(), 2);
        assert!(matches!(&specs[0], Decision::All(n) if n == "epoch"));
        assert!(matches!(
            &specs[1],
            Decision::File(p, n) if p == Path::new("src/main.rs") && n == "bn-abc"
        ));
    }

    #[test]
    fn parse_decisions_rejects_empty_side() {
        let err = parse_decisions(&["src/x=".into()]).expect_err("operation should fail");
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
        let got = pick_single_side_oid(&c, EPOCH_LABEL).expect("operation should succeed");
        assert_eq!(got, Some(oid('a')));
        let got2 = pick_single_side_oid(&c, "bn-abc").expect("operation should succeed");
        assert_eq!(got2, Some(oid('b')));
    }

    // -----------------------------------------------------------------------
    // bn-2ras — `--keep <ws>` matches `<ws>#...` sides unambiguously
    //
    // Merge-commit rebases produce side names like `feat#merge-parent-0` /
    // `feat#merge-parent-1`. Users typing `--keep feat` expect the obvious
    // thing to work when exactly one such side exists; when multiple share
    // the prefix, we error with the qualified names.
    // -----------------------------------------------------------------------

    #[test]
    fn pick_single_side_oid_matches_unambiguous_prefix() {
        let c = Conflict::Content {
            path: PathBuf::from("a.rs"),
            file_id: FileId::new(1),
            base: None,
            sides: vec![
                side(EPOCH_LABEL, 'a'),
                labeled_side("feat#merge-parent-0", "feat", 'b'),
            ],
            atoms: vec![],
        };
        // `--keep feat` → unique match on `feat#merge-parent-0`.
        let got = pick_single_side_oid(&c, "feat").expect("operation should succeed");
        assert_eq!(got, Some(oid('b')));
        // Exact match still wins when qualified name is typed.
        let got2 =
            pick_single_side_oid(&c, "feat#merge-parent-0").expect("operation should succeed");
        assert_eq!(got2, Some(oid('b')));
    }

    #[test]
    fn pick_single_side_oid_errors_on_ambiguous_prefix() {
        let c = Conflict::Content {
            path: PathBuf::from("a.rs"),
            file_id: FileId::new(1),
            base: None,
            sides: vec![
                labeled_side("feat#merge-parent-0", "feat", 'a'),
                labeled_side("feat#merge-parent-1", "feat", 'b'),
            ],
            atoms: vec![],
        };
        let err = pick_single_side_oid(&c, "feat").expect_err("operation should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("ambiguous"),
            "expected 'ambiguous' in error message, got: {msg}"
        );
        assert!(
            msg.contains("feat#merge-parent-0") && msg.contains("feat#merge-parent-1"),
            "ambiguity error should list qualified side names, got: {msg}"
        );
    }

    #[test]
    fn pick_single_side_oid_exact_match_on_ambiguous_prefix_works() {
        // When the target is the fully-qualified name, the exact-match rule
        // in `match_sides` wins — ambiguity only fires for a bare prefix.
        let c = Conflict::Content {
            path: PathBuf::from("a.rs"),
            file_id: FileId::new(1),
            base: None,
            sides: vec![
                labeled_side("feat#merge-parent-0", "feat", 'a'),
                labeled_side("feat#merge-parent-1", "feat", 'b'),
            ],
            atoms: vec![],
        };
        let got =
            pick_single_side_oid(&c, "feat#merge-parent-1").expect("operation should succeed");
        assert_eq!(got, Some(oid('b')));
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
        let err = pick_single_side_oid(&c, "nonexistent").expect_err("operation should fail");
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
            rename_hint: None,
        };
        let mod_side = pick_single_side_oid(&c, "alice").expect("operation should succeed");
        assert_eq!(mod_side, Some(oid('a')));
        let del_side = pick_single_side_oid(&c, "bob").expect("operation should succeed");
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
        let td = tempfile::tempdir().expect("operation should succeed");
        let got = read_conflict_tree_sidecar(td.path(), "no-such-ws");
        assert!(got.is_none());
    }

    #[test]
    fn read_conflict_tree_sidecar_malformed_returns_none() {
        let td = tempfile::tempdir().expect("operation should succeed");
        let dir = sidecar_dir(td.path(), "broken");
        std::fs::create_dir_all(&dir).expect("operation should succeed");
        std::fs::write(dir.join("conflict-tree.json"), "not json")
            .expect("operation should succeed");
        let got = read_conflict_tree_sidecar(td.path(), "broken");
        assert!(got.is_none());
    }

    #[test]
    fn read_conflict_tree_sidecar_roundtrip() {
        let td = tempfile::tempdir().expect("operation should succeed");
        let dir = sidecar_dir(td.path(), "ok");
        std::fs::create_dir_all(&dir).expect("operation should succeed");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/x.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/x.rs"),
                sides: vec![side(EPOCH_LABEL, 'a'), side("ws-b", 'b')],
            },
        );
        let json = serde_json::to_string_pretty(&tree).expect("operation should succeed");
        std::fs::write(dir.join("conflict-tree.json"), json).expect("operation should succeed");

        let got = read_conflict_tree_sidecar(td.path(), "ok").expect("parsed");
        assert_eq!(got, tree);
    }

    #[test]
    fn write_conflict_tree_sidecar_deletes_when_empty() {
        let td = tempfile::tempdir().expect("operation should succeed");
        let dir = sidecar_dir(td.path(), "wipe");
        std::fs::create_dir_all(&dir).expect("operation should succeed");
        let path = dir.join("conflict-tree.json");
        std::fs::write(&path, "{}").expect("operation should succeed");

        let tree = ConflictTree::new(epoch());
        assert!(tree.is_empty());
        write_conflict_tree_sidecar(td.path(), "wipe", &tree).expect("operation should succeed");
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

    /// Init a git repo inside the `ws_path` directory (so `GixRepo::open(ws_path)`
    /// finds it without ancestor discovery). Returns
    /// `(root_tempdir, root_path, ws_path, repo_handle)`.
    ///
    /// Real maw workspaces have a `.git` gitfile pointing to the common dir;
    /// for unit tests we initialize a standalone repo inside the workspace
    /// directory, which is enough for `read_blob` / `write_blob` round-trips.
    fn setup_ws_repo(ws_name: &str) -> (tempfile::TempDir, PathBuf, PathBuf, maw_git::GixRepo) {
        let td = tempfile::tempdir().expect("operation should succeed");
        let root = td.path().to_path_buf();

        let ws_path = root.join("ws").join(ws_name);
        std::fs::create_dir_all(&ws_path).expect("operation should succeed");

        std::process::Command::new("git")
            .args(["init", ws_path.to_str().expect("operation should succeed")])
            .output()
            .expect("operation should succeed");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");

        let repo = maw_git::GixRepo::open(&ws_path).expect("operation should succeed");
        (td, root, ws_path, repo)
    }

    /// Write a blob into the repo and return its `GitOid` (maw-core flavor).
    fn write_blob(repo: &maw_git::GixRepo, bytes: &[u8]) -> GitOid {
        use maw_git::GitRepo;
        let git_oid = repo.write_blob(bytes).expect("operation should succeed");
        GitOid::new(&git_oid.to_string()).expect("operation should succeed")
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
        let (rel, conflict) = make_content_conflict(
            "src/a.rs",
            b"EPOCH_CONTENT\n",
            b"WS_CONTENT\n",
            "ws-a",
            &repo,
        );
        // Write initial worktree file with marker soup
        std::fs::create_dir_all(ws_path.join("src")).expect("operation should succeed");
        std::fs::write(ws_path.join(&rel), b"<<<<<<< markers\n").expect("operation should succeed");

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
        .expect("operation should succeed");

        let after = std::fs::read(ws_path.join(&rel)).expect("operation should succeed");
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
        std::fs::write(ws_path.join(&rel), b"<<<<<<< markers\n").expect("operation should succeed");
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
        .expect("operation should succeed");

        let after = std::fs::read(ws_path.join(&rel)).expect("operation should succeed");
        assert_eq!(after, b"WS_SIDE\n");
    }

    #[test]
    fn resolve_structured_keep_both_concatenates() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-c");
        let (rel, conflict) = make_content_conflict("y.rs", b"EPOCH_A\n", b"WS_B\n", "ws-c", &repo);
        std::fs::write(ws_path.join(&rel), b"<<<<<<< markers\n").expect("operation should succeed");
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
        .expect("operation should succeed");

        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("operation should succeed");
        // Order: epoch side first (as seeded), then workspace side.
        assert!(after.contains("EPOCH_A"), "missing epoch content: {after}");
        assert!(after.contains("WS_B"), "missing workspace content: {after}");
        let epoch_pos = after.find("EPOCH_A").expect("operation should succeed");
        let ws_pos = after.find("WS_B").expect("operation should succeed");
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
        let (rel_a, c_a) = make_content_conflict("a.rs", b"EPOCH_A\n", b"WS_A\n", "ws-d", &repo);
        let (rel_b, c_b) = make_content_conflict("b.rs", b"EPOCH_B\n", b"WS_B\n", "ws-d", &repo);
        std::fs::write(ws_path.join(&rel_a), b"<<<<<<< markers\n")
            .expect("operation should succeed");
        std::fs::write(ws_path.join(&rel_b), b"<<<<<<< markers\n")
            .expect("operation should succeed");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel_a.clone(), c_a);
        tree.conflicts.insert(rel_b.clone(), c_b);

        // Persist sidecar, then only resolve `a.rs`.
        write_conflict_tree_sidecar(&root, "ws-d", &tree).expect("operation should succeed");

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
        .expect("operation should succeed");

        // Structured sidecar should now only contain b.rs.
        let after = read_conflict_tree_sidecar(&root, "ws-d").expect("sidecar still present");
        assert_eq!(after.conflicts.len(), 1);
        assert!(after.conflicts.contains_key(&rel_b));
        assert!(!after.conflicts.contains_key(&rel_a));

        // a.rs was written from epoch side.
        let a_bytes = std::fs::read(ws_path.join(&rel_a)).expect("operation should succeed");
        assert_eq!(a_bytes, b"EPOCH_A\n");
        // b.rs left untouched (still has our marker placeholder).
        let b_bytes = std::fs::read(ws_path.join(&rel_b)).expect("operation should succeed");
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
        std::fs::create_dir_all(&dir).expect("operation should succeed");
        std::fs::write(dir.join("conflict-tree.json"), "not-json")
            .expect("operation should succeed");
        assert!(read_conflict_tree_sidecar(&root, "ws-e").is_none());
    }

    #[test]
    fn resolve_structured_list_json_reports_paths() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-f");
        let (rel_a, c_a) = make_content_conflict("a.rs", b"E\n", b"W\n", "ws-f", &repo);
        let (rel_b, c_b) = make_content_conflict("b.rs", b"E\n", b"W\n", "ws-f", &repo);
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel_a.clone(), c_a);
        tree.conflicts.insert(rel_b.clone(), c_b);

        // List mode — should not touch worktree.
        std::fs::write(ws_path.join(&rel_a), b"original-a").expect("operation should succeed");
        std::fs::write(ws_path.join(&rel_b), b"original-b").expect("operation should succeed");

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
        .expect("operation should succeed");

        // Worktree content unchanged.
        assert_eq!(
            std::fs::read(ws_path.join(&rel_a)).expect("operation should succeed"),
            b"original-a"
        );
        assert_eq!(
            std::fs::read(ws_path.join(&rel_b)).expect("operation should succeed"),
            b"original-b"
        );
    }

    #[test]
    fn resolve_structured_per_atom_deferred_documented() {
        // Per-atom resolution is intentionally deferred in V1. This test
        // locks the current behavior: a `cf-N=NAME` spec on the structured
        // path is rejected with an error pointing to the atom follow-up.
        //
        // When per-atom lands, replace this test with a positive one.
        let err = parse_decisions(&["cf-0=alice".into()]).expect_err("operation should fail");
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
        std::fs::write(
            ws_path.join("x.rs"),
            b"<<<<<<< epoch (current)\nEPOCH\n=======\nWS\n>>>>>>> ws-autocommit\n",
        )
        .expect("operation should succeed");
        let add = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");
        assert!(add.success());
        let commit = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");
        assert!(commit.success());

        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel, conflict) =
            make_content_conflict("x.rs", b"EPOCH\n", b"WS\n", "ws-autocommit", &repo);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel, conflict);

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
        .expect("operation should succeed");

        // HEAD must have advanced — the resolution is committed.
        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
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
            .expect("operation should succeed");
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
        std::fs::write(ws_path.join("seed"), b"seed").expect("operation should succeed");
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");
        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel_a, c_a) =
            make_content_conflict("a.rs", b"EPOCH_A\n", b"WS_A\n", "ws-partial", &repo);
        let (rel_b, c_b) =
            make_content_conflict("b.rs", b"EPOCH_B\n", b"WS_B\n", "ws-partial", &repo);
        std::fs::write(ws_path.join(&rel_a), b"marker-a").expect("operation should succeed");
        std::fs::write(ws_path.join(&rel_b), b"marker-b").expect("operation should succeed");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel_a.clone(), c_a);
        tree.conflicts.insert(rel_b, c_b);

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
        .expect("operation should succeed");

        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
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
        std::fs::write(ws_path.join("seed"), b"seed").expect("operation should succeed");
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");

        // Build a ModifyDelete conflict with real blobs:
        //   modifier side: "MODIFIED\n"  (the *new* epoch content)
        //   deleter side: stores the *pre-delete* (base) content, which is
        //     the bug source — a naive `--keep both` concat would append
        //     this base blob's bytes under the workspace-side banner.
        let modifier_oid = {
            use maw_git::GitRepo;
            let oid = repo
                .write_blob(b"MODIFIED\n")
                .expect("operation should succeed");
            GitOid::new(&oid.to_string()).expect("operation should succeed")
        };
        let base_oid = {
            use maw_git::GitRepo;
            let oid = repo
                .write_blob(b"BASE_PREDELETE\n")
                .expect("operation should succeed");
            GitOid::new(&oid.to_string()).expect("operation should succeed")
        };
        let rel = PathBuf::from("dir/file.txt");
        let conflict = Conflict::ModifyDelete {
            path: rel.clone(),
            file_id: FileId::new(7),
            modifier: ConflictSide::new(EPOCH_LABEL.to_owned(), modifier_oid.clone(), ord("epoch")),
            deleter: ConflictSide::new(
                "ws-mod-del".to_owned(),
                base_oid, // <-- the pre-delete blob, pretending to be "their" content
                ord("ws-mod-del"),
            ),
            modified_content: modifier_oid,
            rename_hint: None,
        };
        std::fs::create_dir_all(ws_path.join("dir")).expect("operation should succeed");
        std::fs::write(ws_path.join(&rel), b"placeholder").expect("operation should succeed");

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
        .expect("operation should succeed");

        let after = std::fs::read(ws_path.join(&rel)).expect("operation should succeed");
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
        std::fs::write(ws_path.join("seed"), b"seed").expect("operation should succeed");
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&ws_path)
            .status()
            .expect("operation should succeed");

        // Build a conflict where the workspace-side is flagged as a
        // symlink whose blob content is the link target "a.txt\n" without
        // the newline (git stores symlink targets without a trailing LF).
        let ws_target_oid = {
            use maw_git::GitRepo;
            let oid = repo.write_blob(b"a.txt").expect("operation should succeed");
            GitOid::new(&oid.to_string()).expect("operation should succeed")
        };
        let epoch_oid = {
            use maw_git::GitRepo;
            let oid = repo.write_blob(b"b.txt").expect("operation should succeed");
            GitOid::new(&oid.to_string()).expect("operation should succeed")
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
        .expect("operation should succeed");

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
        .expect("operation should succeed");

        // The resolved path must be a symlink pointing at `a.txt`.
        let full = ws_path.join(&rel);
        let meta = std::fs::symlink_metadata(&full).expect("operation should succeed");
        assert!(
            meta.file_type().is_symlink(),
            "resolved path should be a symlink, got file_type={:?}",
            meta.file_type()
        );
        let target = std::fs::read_link(&full).expect("operation should succeed");
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
        std::fs::create_dir_all(sidecar.parent().expect("operation should succeed"))
            .expect("operation should succeed");
        std::fs::write(&sidecar, b"this is not valid json{{").expect("operation should succeed");

        // Sidecar exists on disk.
        assert!(sidecar.exists(), "sidecar path should exist");
        // But the reader returns None (unparseable). This is the
        // precise combination `resolve::run` uses to fire the bn-24tl
        // "cannot resolve — regenerate" bail instead of silently
        // falling through to the legacy stripper.
        assert!(read_conflict_tree_sidecar(&root, "ws-corrupt").is_none());
    }

    // -----------------------------------------------------------------------
    // bn-1nwn — per-hunk resolution for --keep epoch / --keep <ws> / --keep both
    //
    // The bug: `--keep epoch` was doing a whole-blob replacement (losing the
    // workspace's non-overlapping edits); `--keep both` was doing a naive
    // blob-concat (losing the per-hunk merge). Both should use the 3-way
    // text merge plumbing that bn-3mbj already uses for `--keep <ws>`.
    //
    // Setup for all three tests:
    //   base.txt  = "line1\nshared\nline3\n"
    //   epoch.txt = "LINE1\nshared\nline3\n"    (epoch changed line1)
    //   ws.txt    = "line1\nshared\nLINE3\n"    (ws changed line3 — disjoint)
    //
    // Conflict: epoch and ws both changed different lines — the 3-way merge
    // of (base, epoch, ws) is clean (no overlap), so all three resolution
    // modes should produce a marker-free result that combines both edits.
    // -----------------------------------------------------------------------

    /// Build an epoch-vs-workspace `Content` conflict whose sides carry
    /// `base_content` (so the per-hunk 3-way path is available).
    fn make_content_conflict_with_base(
        path: &str,
        base_bytes: &[u8],
        epoch_bytes: &[u8],
        ws_bytes: &[u8],
        ws_name: &str,
        repo: &maw_git::GixRepo,
    ) -> (PathBuf, Conflict) {
        let base_oid = write_blob(repo, base_bytes);
        let epoch_oid = write_blob(repo, epoch_bytes);
        let ws_oid = write_blob(repo, ws_bytes);
        let p = PathBuf::from(path);
        (
            p.clone(),
            Conflict::Content {
                path: p,
                file_id: FileId::new(1),
                base: Some(base_oid.clone()),
                sides: vec![
                    ConflictSide::with_base(
                        EPOCH_LABEL.to_owned(),
                        epoch_oid,
                        ord("epoch"),
                        Some(base_oid.clone()),
                    ),
                    ConflictSide::with_base(
                        ws_name.to_owned(),
                        ws_oid,
                        ord(ws_name),
                        Some(base_oid),
                    ),
                ],
                atoms: vec![],
            },
        )
    }

    /// Helper: seed an initial commit in `ws_path` so `auto_commit_resolution`
    /// can call `git commit` without "no commits yet" failures.
    fn seed_initial_commit(ws_path: &std::path::Path) {
        std::fs::write(ws_path.join("seed"), b"seed").expect("seed write");
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(ws_path)
            .status()
            .expect("git add seed");
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(ws_path)
            .status()
            .expect("git commit seed");
    }

    // -----------------------------------------------------------------------
    // bn-1nwn test 1: --keep epoch preserves workspace's disjoint edit
    // -----------------------------------------------------------------------

    /// `--keep epoch` on a conflict where epoch changed line A and the ws
    /// changed (non-overlapping) line B: after resolution the file should
    /// contain epoch's version of A AND the workspace's version of B.
    ///
    /// Without the fix the resolver wrote the epoch's whole blob (line B
    /// silently reverted to base).
    #[test]
    fn resolve_keep_epoch_preserves_disjoint_ws_edit() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-1nwn-epoch");
        seed_initial_commit(&ws_path);

        // base: line1 + line3 both at "original"
        // epoch: changed line1 to "EPOCH_LINE1"
        // ws: changed line3 to "WS_LINE3" (disjoint)
        let base = b"original_line1\nshared\noriginal_line3\n";
        let epoch_content = b"EPOCH_LINE1\nshared\noriginal_line3\n";
        let ws_content = b"original_line1\nshared\nWS_LINE3\n";

        // Write the conflict-state placeholder (the rendered marker file that
        // `maw ws sync` would have written).
        let rel = PathBuf::from("f.txt");
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch (current)\nEPOCH_LINE1\nshared\noriginal_line3\n\
              ======= base\noriginal_line1\nshared\noriginal_line3\n\
              >>>>>>> ws-1nwn-epoch\noriginal_line1\nshared\nWS_LINE3\n",
        )
        .expect("write placeholder");

        let (rel2, conflict) = make_content_conflict_with_base(
            "f.txt",
            base,
            epoch_content,
            ws_content,
            "ws-1nwn-epoch",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-1nwn-epoch",
            &ws_path,
            &[],
            &["epoch".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("resolve should succeed");

        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("read resolved file");

        // Epoch's version of line1 must be present.
        assert!(
            after.contains("EPOCH_LINE1"),
            "epoch side of conflict must be present, got:\n{after}"
        );
        // Workspace's disjoint edit (line3) must be preserved.
        assert!(
            after.contains("WS_LINE3"),
            "workspace's disjoint edit must be preserved after --keep epoch, got:\n{after}"
        );
        // No conflict markers.
        assert!(
            !after.contains("<<<<<<<"),
            "resolved file must not contain conflict markers, got:\n{after}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-1nwn test 2: --keep <ws> preserves epoch's disjoint edit
    // -----------------------------------------------------------------------

    /// `--keep <ws>` on a conflict where the workspace changed line A and
    /// epoch changed (non-overlapping) line B: after resolution the file
    /// should contain the workspace's version of A AND epoch's version of B.
    ///
    /// This is what bn-3mbj already fixed; the test locks the behavior in
    /// alongside the new bn-1nwn tests so any regression is immediately
    /// visible.
    #[test]
    fn resolve_keep_ws_preserves_disjoint_epoch_edit() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-1nwn-ws");
        seed_initial_commit(&ws_path);

        // base: both lines at "original"
        // epoch: changed line3 to "EPOCH_LINE3" (disjoint from ws conflict)
        // ws: changed line1 to "WS_LINE1"
        let base = b"original_line1\nshared\noriginal_line3\n";
        let epoch_content = b"original_line1\nshared\nEPOCH_LINE3\n";
        let ws_content = b"WS_LINE1\nshared\noriginal_line3\n";

        let rel = PathBuf::from("g.txt");
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch\noriginal_line1\nshared\nEPOCH_LINE3\n\
              =======\nWS_LINE1\nshared\noriginal_line3\n>>>>>>> ws-1nwn-ws\n",
        )
        .expect("write placeholder");

        let (rel2, conflict) = make_content_conflict_with_base(
            "g.txt",
            base,
            epoch_content,
            ws_content,
            "ws-1nwn-ws",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-1nwn-ws",
            &ws_path,
            &[],
            &["ws-1nwn-ws".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("resolve should succeed");

        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("read resolved file");

        // Workspace's version of line1 must be present.
        assert!(
            after.contains("WS_LINE1"),
            "ws side of conflict must be present, got:\n{after}"
        );
        // Epoch's disjoint edit (line3) must be preserved.
        assert!(
            after.contains("EPOCH_LINE3"),
            "epoch's disjoint edit must be preserved after --keep ws, got:\n{after}"
        );
        // No conflict markers.
        assert!(
            !after.contains("<<<<<<<"),
            "resolved file must not contain conflict markers, got:\n{after}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-1nwn test 3: --keep both preserves both sides in conflict region
    //                  AND preserves each side's clean edits
    // -----------------------------------------------------------------------

    /// `--keep both` on a conflict with disjoint edits: after resolution the
    /// file should contain both sides' edits everywhere.
    ///
    /// Without the fix `--keep both` did a blob-concat (epoch-blob + ws-blob),
    /// losing the merged clean regions and producing duplicated context.
    #[test]
    fn resolve_keep_both_preserves_both_sides() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-1nwn-both");
        seed_initial_commit(&ws_path);

        // base: original on both lines
        // epoch: changed line1 to "EPOCH_LINE1"
        // ws: changed line3 to "WS_LINE3" — disjoint
        // Expected (union merge, all clean): EPOCH_LINE1 + shared + WS_LINE3
        let base = b"original_line1\nshared\noriginal_line3\n";
        let epoch_content = b"EPOCH_LINE1\nshared\noriginal_line3\n";
        let ws_content = b"original_line1\nshared\nWS_LINE3\n";

        let rel = PathBuf::from("h.txt");
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch\nEPOCH_LINE1\nshared\noriginal_line3\n\
              =======\noriginal_line1\nshared\nWS_LINE3\n>>>>>>> ws-1nwn-both\n",
        )
        .expect("write placeholder");

        let (rel2, conflict) = make_content_conflict_with_base(
            "h.txt",
            base,
            epoch_content,
            ws_content,
            "ws-1nwn-both",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-1nwn-both",
            &ws_path,
            &[],
            &["both".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("resolve should succeed");

        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("read resolved file");

        // Both edits must appear (union merge preserves both sides of clean
        // hunks; with disjoint edits neither side conflicts so the result
        // is the same as a clean merge).
        assert!(
            after.contains("EPOCH_LINE1"),
            "--keep both must include epoch's edit, got:\n{after}"
        );
        assert!(
            after.contains("WS_LINE3"),
            "--keep both must include ws's edit, got:\n{after}"
        );
        // No conflict markers.
        assert!(
            !after.contains("<<<<<<<"),
            "resolved file must not contain conflict markers, got:\n{after}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-1nwn test 4: --keep epoch with overlapping conflict, epoch wins
    // -----------------------------------------------------------------------

    /// `--keep epoch` where both sides changed the same line (true conflict):
    /// after resolution the file contains epoch's version of the conflicted
    /// line and no markers.
    #[test]
    fn resolve_keep_epoch_wins_true_conflict() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-1nwn-true-conflict");
        seed_initial_commit(&ws_path);

        // base: shared line
        // epoch + ws: both changed the same line (true conflict)
        let base = b"contested_line\n";
        let epoch_content = b"EPOCH_VERSION\n";
        let ws_content = b"WS_VERSION\n";

        let rel = PathBuf::from("conflict.txt");
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch\nEPOCH_VERSION\n=======\nWS_VERSION\n>>>>>>> ws-1nwn-true-conflict\n",
        )
        .expect("write placeholder");

        let (rel2, conflict) = make_content_conflict_with_base(
            "conflict.txt",
            base,
            epoch_content,
            ws_content,
            "ws-1nwn-true-conflict",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-1nwn-true-conflict",
            &ws_path,
            &[],
            &["epoch".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("resolve should succeed");

        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("read resolved file");

        // Epoch wins the conflict.
        assert!(
            after.contains("EPOCH_VERSION"),
            "epoch must win the conflict, got:\n{after}"
        );
        assert!(
            !after.contains("WS_VERSION"),
            "ws version must not appear when epoch wins, got:\n{after}"
        );
        // No conflict markers.
        assert!(
            !after.contains("<<<<<<<"),
            "resolved file must not contain conflict markers, got:\n{after}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-1nwn test 5: --keep epoch falls back to blob-replace for binary
    // -----------------------------------------------------------------------

    /// When any side is binary (NUL byte), `--keep epoch` falls back to the
    /// whole-blob replacement path (per-hunk text merge is meaningless for
    /// binary files).
    #[test]
    fn resolve_keep_epoch_binary_falls_back_to_blob_replace() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-1nwn-binary");
        seed_initial_commit(&ws_path);

        // Epoch side contains a NUL → binary.
        let base = b"normal text\n";
        let epoch_content = b"binary\x00content\n";
        let ws_content = b"workspace version\n";

        let rel = PathBuf::from("bin.dat");
        std::fs::write(ws_path.join(&rel), b"placeholder\n").expect("write placeholder");

        let (rel2, conflict) = make_content_conflict_with_base(
            "bin.dat",
            base,
            epoch_content,
            ws_content,
            "ws-1nwn-binary",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-1nwn-binary",
            &ws_path,
            &[],
            &["epoch".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("binary fallback should not fail");

        let after = std::fs::read(ws_path.join(&rel)).expect("read resolved file");

        // Binary fallback: epoch's whole blob is written.
        assert_eq!(
            after, b"binary\x00content\n",
            "binary fallback should write epoch's whole blob"
        );
    }

    // -----------------------------------------------------------------------
    // bn-1hmz test: --keep <ws> falls back to blob-replace for binary
    // -----------------------------------------------------------------------

    /// When any side is binary (NUL byte), `--keep <ws>` must fall back to the
    /// whole-blob replacement path (the ws side wins wholesale), exactly like
    /// `--keep epoch` and `--keep both` do.
    ///
    /// Before bn-1hmz the `--keep <ws>` Theirs path had NO `looks_text` guard,
    /// so a binary file with embedded 0x0A bytes would get a frankenstein
    /// result from `merge_text`, and the output line would incorrectly claim
    /// "(3-way merge: ws intent on top of epoch, ws wins on overlap)".
    ///
    /// After the fix: result bytes must be byte-identical to the ws side blob,
    /// and the resolve kind must be `BlobReplace` (not `ThreeWayClean` /
    /// `ThreeWayWsWins`).
    #[test]
    fn resolve_keep_ws_binary_falls_back_to_blob_replace() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-1hmz-binary");
        seed_initial_commit(&ws_path);

        // Three binary blobs — each side changes a different "section"
        // separated by 0x0A (so the text driver would see "disjoint lines"
        // and produce a "clean" merge if not guarded).
        let base: &[u8] = b"HDR\x00aaaa\nMID\x00bbbb\nEND\x00cccc\n";
        let epoch_content: &[u8] = b"HDR\x00XXXX\nMID\x00bbbb\nEND\x00cccc\n";
        let ws_content: &[u8] = b"HDR\x00aaaa\nMID\x00bbbb\nEND\x00ZZZZ\n";

        let rel = PathBuf::from("data.bin");
        std::fs::write(ws_path.join(&rel), b"placeholder\n").expect("write placeholder");

        let (rel2, conflict) = make_content_conflict_with_base(
            "data.bin",
            base,
            epoch_content,
            ws_content,
            "ws-1hmz-binary",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-1hmz-binary",
            &ws_path,
            &[],
            &["ws-1hmz-binary".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("binary blob-replace for --keep <ws> should not fail");

        let after = std::fs::read(ws_path.join(&rel)).expect("read resolved file");

        // Binary fallback: ws's whole blob is written — byte-identical,
        // never a frankenstein mix of epoch + ws bytes.
        assert_eq!(
            after,
            ws_content,
            "bn-1hmz: --keep <ws> on a binary conflict must write the ws blob wholesale; \
             got (hex): {}",
            after
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );

        // Specifically must NOT contain both sides' distinguishing bytes.
        // b"HDR\x00XXXX" and b"END\x00ZZZZ" are each 8 bytes.
        let has_epoch_region = after.windows(8).any(|w| w == b"HDR\x00XXXX");
        let has_ws_region = after.windows(8).any(|w| w == b"END\x00ZZZZ");
        assert!(
            !has_epoch_region,
            "bn-1hmz: epoch's binary region must not appear in --keep <ws> result"
        );
        assert!(
            has_ws_region,
            "bn-1hmz: ws's binary region must appear in --keep <ws> result"
        );
    }

    // -----------------------------------------------------------------------
    // bn-c5ui — post-merge sanity check before auto-commit
    //
    // The incident: `maw ws resolve --keep both` (gix Union) produced a merged
    // .rs file that failed to parse (unclosed delimiter), but the resolver
    // auto-committed it and printed success anyway. The fix: run
    // `run_post_merge_sanity` on driver-produced merge outputs before
    // auto-committing; suppress the commit and print a loud WARNING when the
    // check fires.
    //
    // Tests:
    //   (a) incident shape: union merge trips the sanity check → file IS
    //       written, auto-commit is SKIPPED, warning is emitted
    //   (b) clean per-hunk resolution still auto-commits (no regression)
    //   (c) non-.rs file (.txt) union resolution does NOT warn (language-aware
    //       check skips unsupported extensions; size check passes for
    //       reasonable content)
    // -----------------------------------------------------------------------

    /// Write a `ManifoldConfig` file at the V2 bootstrap path
    /// (`root/.manifold/config.toml`) with a near-zero `size_ratio_max` so the
    /// size-delta sanity check fires for ANY non-trivial merge output. Used to
    /// simulate the "sanity check fires" condition reliably without needing to
    /// find exact content that breaks the merge algorithm.
    fn write_tight_sanity_config(root: &std::path::Path) {
        let config_dir = root.join(".manifold");
        std::fs::create_dir_all(&config_dir).expect("create .manifold dir");
        // 0.0001 ratio: any merged output > 0.01% of expected size trips it.
        std::fs::write(
            config_dir.join("config.toml"),
            b"[merge]\npost_rebase_size_ratio_max = 0.0001\n",
        )
        .expect("write tight sanity config");
    }

    /// (a) Incident-shape test: a per-hunk 3-way resolve of two .rs EOF
    /// appends trips the post-merge sanity check → the resolved file IS written
    /// (bytes on disk), the auto-commit is SKIPPED (HEAD unchanged), and a
    /// WARNING message is printed.
    ///
    /// We simulate the "check fires" condition by writing a near-zero
    /// `post_rebase_size_ratio_max` to the bootstrap config and using
    /// `--keep ws-c5ui-sanity` (`ThreeWayWsWins` path) so the loaded config is
    /// respected. The artificially tight threshold (0.0001x) ensures the
    /// size-delta check trips for any non-trivial merge output — exercising the
    /// same auto-commit-suppression code path that fires when the AST check
    /// catches a genuinely broken union output.
    ///
    /// Note: `--keep both` (the original incident command) disables the
    /// size-delta check for union outputs because a union is intentionally
    /// larger than either input alone. The AST check still guards union; this
    /// test exercises the full `sanity_cfg` path via `--keep <ws>`.
    #[test]
    fn c5ui_sanity_failure_skips_autocommit_and_writes_file() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-c5ui-sanity");
        seed_initial_commit(&ws_path);

        // Write a tight sanity config so the size-delta check fires for any
        // non-trivial merge output on the --keep <ws> path.
        write_tight_sanity_config(&root);

        // Seed a conflict: both sides append different test functions at EOF
        // of a .rs file (the incident shape).
        let base = b"fn existing() {}\n";
        let epoch_content = b"fn existing() {}\n\nfn test_epoch() {\n    assert!(1 == 1);\n}\n";
        let ws_content = b"fn existing() {}\n\nfn test_ws() {\n    assert!(2 == 2);\n}\n";

        let rel = PathBuf::from("tests/scenario.rs");
        std::fs::create_dir_all(ws_path.join("tests")).expect("create tests dir");
        // Write marker-content placeholder (what the workspace HEAD would contain
        // after rebase with conflicts).
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch\nfn existing() {}\n=======\nfn existing() {}\n>>>>>>> ws-c5ui-sanity\n",
        )
        .expect("write placeholder");

        // Stage + commit the placeholder so HEAD exists with the marker content
        // (mirrors real post-rebase state).
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("git add placeholder");
        std::process::Command::new("git")
            .args(["commit", "-m", "post-rebase markers"])
            .current_dir(&ws_path)
            .status()
            .expect("git commit placeholder");

        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("rev-parse HEAD before");
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel2, conflict) = make_content_conflict_with_base(
            "tests/scenario.rs",
            base,
            epoch_content,
            ws_content,
            "ws-c5ui-sanity",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        // Run resolve --keep <ws> so the loaded sanity_cfg (tight 0.0001 ratio)
        // is applied to the ThreeWayWsWins output, tripping the size check.
        run_structured(
            &root,
            "ws-c5ui-sanity",
            &ws_path,
            &[],
            &["ws-c5ui-sanity".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("resolve should not error (sanity failure is non-fatal)");

        // (i) The resolved file MUST be on disk (user asked for it).
        let after_bytes = std::fs::read(ws_path.join(&rel))
            .expect("resolved file must be written even when sanity check fires");
        assert!(
            !after_bytes.is_empty(),
            "resolved file must be non-empty; sanity failure should not delete output"
        );
        // The ws side's content should be present (ws wins on conflicts).
        let after_str = String::from_utf8_lossy(&after_bytes);
        assert!(
            after_str.contains("existing"),
            "resolved file must contain content from the inputs; got:\n{after_str}"
        );

        // (ii) HEAD must NOT have advanced — auto-commit is suppressed.
        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("rev-parse HEAD after");
        let head_after_sha = String::from_utf8_lossy(&head_after.stdout)
            .trim()
            .to_owned();
        assert_eq!(
            head_before_sha, head_after_sha,
            "bn-c5ui: auto-commit must be SKIPPED when sanity check fires; \
             HEAD must not advance"
        );
    }

    /// (b) Clean per-hunk union resolution (no sanity failure under the default
    /// config) still auto-commits. Verifies that the sanity machinery does not
    /// regress the happy path.
    #[test]
    fn c5ui_clean_union_still_autocommits() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-c5ui-clean");
        seed_initial_commit(&ws_path);
        // No tight config — use defaults (size_ratio_max = 1.5).

        // Simple disjoint content: epoch and ws add different lines to a
        // two-line base. The union merge is clean and well within size bounds.
        let base = b"line_base\n";
        let epoch_content = b"line_base\nline_epoch\n";
        let ws_content = b"line_base\nline_ws\n";

        let rel = PathBuf::from("changes.txt");
        std::fs::write(
            ws_path.join(&rel),
            b"<<<<<<< epoch\nline_base\n=======\nline_base\n>>>>>>> ws-c5ui-clean\n",
        )
        .expect("write placeholder");
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "markers"])
            .current_dir(&ws_path)
            .status()
            .expect("git commit");

        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("rev-parse HEAD before");
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel2, conflict) = make_content_conflict_with_base(
            "changes.txt",
            base,
            epoch_content,
            ws_content,
            "ws-c5ui-clean",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-c5ui-clean",
            &ws_path,
            &[],
            &["both".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("clean resolve should succeed");

        // HEAD must have ADVANCED — the resolution was committed.
        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("rev-parse HEAD after");
        let head_after_sha = String::from_utf8_lossy(&head_after.stdout)
            .trim()
            .to_owned();
        assert_ne!(
            head_before_sha, head_after_sha,
            "bn-c5ui: clean union resolve must still auto-commit; HEAD should advance"
        );

        // The resolved file must contain content from both sides.
        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("read resolved file");
        assert!(after.contains("line_epoch"), "epoch side missing: {after}");
        assert!(after.contains("line_ws"), "ws side missing: {after}");
        assert!(!after.contains("<<<<<<<"), "unexpected markers: {after}");
    }

    /// (c) Non-.rs file (.txt) union resolution does not warn. The
    /// language-aware AST check returns `Ok(())` immediately for `.txt` (no
    /// tree-sitter grammar), and the size-delta check passes for reasonable
    /// content under the default 1.5x ratio. Verifies that the sanity
    /// machinery respects the "lenient on unsupported file types" contract.
    #[test]
    fn c5ui_non_rs_txt_union_no_sanity_warning() {
        let (_td, root, ws_path, repo) = setup_ws_repo("ws-c5ui-txt");
        seed_initial_commit(&ws_path);
        // Default config — no tight threshold.

        // .txt file: both sides append a log line.
        let base = b"header\n";
        let epoch_content = b"header\nepoch_entry\n";
        let ws_content = b"header\nws_entry\n";

        let rel = PathBuf::from("log.txt");
        std::fs::write(ws_path.join(&rel), b"<<<<<<\nheader\n>>>>>>>\n")
            .expect("write placeholder");
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_path)
            .status()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "markers"])
            .current_dir(&ws_path)
            .status()
            .expect("git commit");

        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("rev-parse HEAD before");
        let head_before_sha = String::from_utf8_lossy(&head_before.stdout)
            .trim()
            .to_owned();

        let (rel2, conflict) = make_content_conflict_with_base(
            "log.txt",
            base,
            epoch_content,
            ws_content,
            "ws-c5ui-txt",
            &repo,
        );
        assert_eq!(rel, rel2);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(rel.clone(), conflict);

        run_structured(
            &root,
            "ws-c5ui-txt",
            &ws_path,
            &[],
            &["both".into()],
            false,
            OutputFormat::Text,
            tree,
        )
        .expect("txt union resolve should succeed without warnings");

        // HEAD must have advanced — no sanity warning, so auto-commit fires.
        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws_path)
            .output()
            .expect("rev-parse HEAD after");
        let head_after_sha = String::from_utf8_lossy(&head_after.stdout)
            .trim()
            .to_owned();
        assert_ne!(
            head_before_sha, head_after_sha,
            "bn-c5ui: .txt union resolve should auto-commit (no AST check for .txt; \
             size check passes under default 1.5x ratio)"
        );

        // The file must contain content from both sides.
        let after = std::fs::read_to_string(ws_path.join(&rel)).expect("read resolved file");
        assert!(after.contains("epoch_entry"), "epoch missing: {after}");
        assert!(after.contains("ws_entry"), "ws missing: {after}");
    }
}
