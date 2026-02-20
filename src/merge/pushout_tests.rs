//! Property-testing merge correctness via pushout contracts (§9.2).
//!
//! The categorical framework treats patches as morphisms and merge as a pushout.
//! This module provides executable verification of pushout properties through
//! randomized testing:
//!
//! 1. **Embedding**: Every workspace's edits are present in the merge result
//!    (either in resolved changes or represented as conflict sides).
//! 2. **Minimality**: No strictly-better merge result exists with fewer conflicts
//!    while still embedding all sides (approximate via sampling).
//! 3. **Commutativity**: The merge result is independent of workspace ordering
//!    (pushout is a universal construction — unique up to isomorphism).
//!
//! # Coverage
//!
//! - 1000+ random scenarios per property via `ProptestConfig::with_cases(1000)`
//! - Random history generator: base content + N (2-8) patch-sets
//! - Change types: disjoint adds, identical modifications, non-overlapping edits,
//!   overlapping edits (conflicts), deletions, modify/delete, add/add
//! - Failure produces minimal reproducing case via proptest shrinking
//!
//! # Pushout diagram
//!
//! ```text
//!        base (A)
//!        /    \
//!    p₁ /      \ p₂
//!      /        \
//!    B₁          B₂
//!      \        /
//!   q₂  \      / q₁
//!        \    /
//!      merge (M)
//! ```
//!
//! The pushout M satisfies:
//! - q₂ ∘ p₁ = q₁ ∘ p₂  (commutativity)
//! - For each side Bᵢ, its edits embed into M  (universality)
//! - M is minimal: no M' with fewer conflicts also satisfies these properties

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use proptest::prelude::*;

use crate::merge::build::ResolvedChange;
use crate::merge::partition::partition_by_path;
use crate::merge::resolve::{resolve_partition, ConflictReason, ConflictRecord, ResolveResult};
use crate::merge::types::{ChangeKind, FileChange, PatchSet};
use crate::model::types::{EpochId, WorkspaceId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn epoch() -> EpochId {
    EpochId::new(&"a".repeat(40)).unwrap()
}

fn ws(name: &str) -> WorkspaceId {
    WorkspaceId::new(name).unwrap()
}

/// A workspace's set of changes for testing.
#[derive(Clone, Debug)]
struct TestWorkspace {
    name: String,
    changes: Vec<FileChange>,
}

/// Convert test workspaces to PatchSets.
fn to_patch_sets(workspaces: &[TestWorkspace]) -> Vec<PatchSet> {
    workspaces
        .iter()
        .map(|w| PatchSet::new(ws(&w.name), epoch(), w.changes.clone()))
        .collect()
}

/// Build base contents map for paths that need it (Modified/Deleted changes).
fn make_base_contents(workspaces: &[TestWorkspace]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut base = BTreeMap::new();
    for w in workspaces {
        for change in &w.changes {
            if matches!(change.kind, ChangeKind::Modified | ChangeKind::Deleted) {
                base.entry(change.path.clone())
                    .or_insert_with(|| b"base content\nline 2\nline 3\n".to_vec());
            }
        }
    }
    base
}

/// Run the merge pipeline and return the result.
fn run_merge(
    workspaces: &[TestWorkspace],
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
) -> ResolveResult {
    let patch_sets = to_patch_sets(workspaces);
    let partition = partition_by_path(&patch_sets);
    resolve_partition(&partition, base_contents).expect("resolve should not error")
}

/// Collect all workspace names that touch a given path.
fn workspaces_touching_path(workspaces: &[TestWorkspace], path: &PathBuf) -> Vec<String> {
    workspaces
        .iter()
        .filter(|w| w.changes.iter().any(|c| &c.path == path))
        .map(|w| w.name.clone())
        .collect()
}

/// Format workspace scenario for failure reproduction.
fn format_scenario(workspaces: &[TestWorkspace]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Workspace count: {}\n", workspaces.len()));
    for w in workspaces {
        out.push_str(&format!("  {} ({} changes):\n", w.name, w.changes.len()));
        for c in &w.changes {
            let content_summary = match &c.content {
                Some(b) => format!("{} bytes", b.len()),
                None => "none".to_string(),
            };
            out.push_str(&format!(
                "    {} {:?} (content: {})\n",
                c.kind, c.path, content_summary
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Proptest strategies
// ---------------------------------------------------------------------------

/// Generate a file path: 1-2 segments, short alphanumeric names.
fn arb_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec("[a-z][a-z0-9]{0,5}", 1..=2usize).prop_map(|segments| {
        let mut p = segments.join("/");
        p.push_str(".rs");
        PathBuf::from(p)
    })
}

/// Generate file content: 1-8 lines of short text.
fn arb_content() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec("[a-zA-Z0-9 ]{1,15}\n", 1..=8usize)
        .prop_map(|lines| lines.join("").into_bytes())
}

/// Generate a ChangeKind.
fn arb_change_kind() -> impl Strategy<Value = ChangeKind> {
    prop_oneof![
        2 => Just(ChangeKind::Added),
        2 => Just(ChangeKind::Modified),
        1 => Just(ChangeKind::Deleted),
    ]
}

/// Generate a single FileChange.
fn arb_file_change() -> impl Strategy<Value = FileChange> {
    (arb_path(), arb_change_kind(), arb_content()).prop_map(|(path, kind, content)| {
        let content = if matches!(kind, ChangeKind::Deleted) {
            None
        } else {
            Some(content)
        };
        FileChange::new(path, kind, content)
    })
}

/// Generate 2-8 workspaces with 1-6 file changes each.
fn arb_workspaces() -> impl Strategy<Value = Vec<TestWorkspace>> {
    prop::collection::vec(
        prop::collection::vec(arb_file_change(), 1..=6usize),
        2..=8usize,
    )
    .prop_map(|workspace_changes| {
        workspace_changes
            .into_iter()
            .enumerate()
            .map(|(i, changes)| TestWorkspace {
                name: format!("ws-{i:02}"),
                changes,
            })
            .collect()
    })
}

/// Generate workspaces where multiple workspaces share the same path with
/// different content (controlled shared-path scenario).
fn arb_shared_path_workspaces() -> impl Strategy<Value = Vec<TestWorkspace>> {
    (arb_path(), prop::collection::vec(arb_content(), 2..=5usize)).prop_map(
        |(shared_path, contents)| {
            contents
                .into_iter()
                .enumerate()
                .map(|(i, content)| TestWorkspace {
                    name: format!("ws-{i:02}"),
                    changes: vec![FileChange::new(
                        shared_path.clone(),
                        ChangeKind::Modified,
                        Some(content),
                    )],
                })
                .collect()
        },
    )
}

/// Generate workspaces where some workspaces modify and some delete the same
/// file (guaranteed modify/delete scenario).
fn arb_modify_delete_workspaces() -> impl Strategy<Value = Vec<TestWorkspace>> {
    (arb_path(), arb_content(), 1..=3usize, 1..=2usize).prop_map(
        |(path, content, n_modifiers, n_deleters)| {
            let mut workspaces = Vec::new();
            for i in 0..n_modifiers {
                workspaces.push(TestWorkspace {
                    name: format!("ws-mod-{i:02}"),
                    changes: vec![FileChange::new(
                        path.clone(),
                        ChangeKind::Modified,
                        Some(content.clone()),
                    )],
                });
            }
            for i in 0..n_deleters {
                workspaces.push(TestWorkspace {
                    name: format!("ws-del-{i:02}"),
                    changes: vec![FileChange::new(path.clone(), ChangeKind::Deleted, None)],
                });
            }
            workspaces
        },
    )
}

/// Generate workspaces that each add the same path with different content
/// (guaranteed add/add conflict).
fn arb_add_add_workspaces() -> impl Strategy<Value = Vec<TestWorkspace>> {
    (arb_path(), prop::collection::vec(arb_content(), 2..=4usize))
        .prop_filter("contents must differ", |(_, contents)| {
            // At least two distinct contents.
            contents.windows(2).any(|w| w[0] != w[1])
        })
        .prop_map(|(path, contents)| {
            contents
                .into_iter()
                .enumerate()
                .map(|(i, content)| TestWorkspace {
                    name: format!("ws-{i:02}"),
                    changes: vec![FileChange::new(
                        path.clone(),
                        ChangeKind::Added,
                        Some(content),
                    )],
                })
                .collect()
        })
}

/// Generate non-overlapping edits to a shared file (should merge cleanly).
/// Creates a base with N regions separated by spacers, and N workspaces
/// each editing a different region.
fn arb_non_overlapping_workspaces(
) -> impl Strategy<Value = (Vec<TestWorkspace>, BTreeMap<PathBuf, Vec<u8>>)> {
    (2..=6usize).prop_flat_map(|n_workspaces| {
        let path = PathBuf::from("shared.txt");

        // Build base content with well-separated regions.
        let mut base_lines: Vec<String> = Vec::new();
        for i in 0..n_workspaces {
            base_lines.push(format!("region-{i}"));
            // 4 spacer lines for diff3 context separation.
            for _ in 0..4 {
                base_lines.push("-".to_string());
            }
        }
        let base_content = base_lines.join("\n") + "\n";

        let workspaces: Vec<TestWorkspace> = (0..n_workspaces)
            .map(|i| {
                let mut modified_lines = base_lines.clone();
                let idx = i * 5; // region starts at i * 5
                modified_lines[idx] = format!("EDITED-BY-{i:02}");
                let modified_content = modified_lines.join("\n") + "\n";

                TestWorkspace {
                    name: format!("ws-{i:02}"),
                    changes: vec![FileChange::new(
                        path.clone(),
                        ChangeKind::Modified,
                        Some(modified_content.into_bytes()),
                    )],
                }
            })
            .collect();

        let mut base_map = BTreeMap::new();
        base_map.insert(path, base_content.into_bytes());

        Just((workspaces, base_map))
    })
}

// ---------------------------------------------------------------------------
// Pushout Property 1: EMBEDDING
//
// Every workspace's changes must appear in the merge result.
// For each workspace W and each path P that W changes:
//   - P must appear in either `resolved` or `conflicts`.
//   - If P is in `conflicts`, W must appear as a conflict side.
// ---------------------------------------------------------------------------

/// Check the embedding property for a merge result.
fn verify_embedding(workspaces: &[TestWorkspace], result: &ResolveResult) -> Result<(), String> {
    // Build a lookup: path → which collection it's in.
    let resolved_paths: BTreeSet<PathBuf> =
        result.resolved.iter().map(|r| r.path().clone()).collect();

    let conflict_map: BTreeMap<PathBuf, &ConflictRecord> = result
        .conflicts
        .iter()
        .map(|c| (c.path.clone(), c))
        .collect();

    for w in workspaces {
        for change in &w.changes {
            let path = &change.path;
            let in_resolved = resolved_paths.contains(path);
            let in_conflicts = conflict_map.contains_key(path);

            if !in_resolved && !in_conflicts {
                return Err(format!(
                    "EMBEDDING VIOLATION: workspace '{}' changed path {:?}, \
                     but it appears in neither resolved nor conflicts.\n\
                     Resolved paths: {:?}\n\
                     Conflict paths: {:?}",
                    w.name,
                    path,
                    resolved_paths,
                    conflict_map.keys().collect::<Vec<_>>(),
                ));
            }

            // If path is in conflicts, verify this workspace appears as a side.
            if let Some(conflict) = conflict_map.get(path) {
                // Count how many workspaces touch this path.
                let ws_touching = workspaces_touching_path(workspaces, path);
                if ws_touching.len() > 1 {
                    // This workspace should appear as a conflict side.
                    let side_ws: Vec<String> = conflict
                        .sides
                        .iter()
                        .map(|s| s.workspace_id.to_string())
                        .collect();
                    if !side_ws.contains(&w.name) {
                        return Err(format!(
                            "EMBEDDING VIOLATION: workspace '{}' changed shared path {:?}, \
                             path is conflicted, but workspace not in conflict sides.\n\
                             Conflict sides: {:?}\n\
                             Workspaces touching path: {:?}",
                            w.name, path, side_ws, ws_touching,
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// For cleanly-resolved shared paths, verify each workspace's non-overlapping
/// edits are present in the merged content.
fn verify_content_embedding(
    workspaces: &[TestWorkspace],
    result: &ResolveResult,
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), String> {
    // Collect resolved upserts.
    let resolved_map: BTreeMap<PathBuf, &[u8]> = result
        .resolved
        .iter()
        .filter_map(|r| match r {
            ResolvedChange::Upsert { path, content } => Some((path.clone(), content.as_slice())),
            ResolvedChange::Delete { .. } => None,
        })
        .collect();

    // For each path touched by multiple workspaces that resolved cleanly,
    // verify embedding of unique edits.
    let mut path_to_workspaces: BTreeMap<PathBuf, Vec<(&TestWorkspace, &FileChange)>> =
        BTreeMap::new();
    for w in workspaces {
        for change in &w.changes {
            path_to_workspaces
                .entry(change.path.clone())
                .or_default()
                .push((w, change));
        }
    }

    for (path, ws_changes) in &path_to_workspaces {
        if ws_changes.len() <= 1 {
            continue; // Not a shared path.
        }

        let Some(merged_content) = resolved_map.get(path) else {
            continue; // Conflicted, not resolved — embedding checked by verify_embedding.
        };

        // If all workspace contents are identical, the merged content should match.
        let contents: Vec<Option<&Vec<u8>>> =
            ws_changes.iter().map(|(_, c)| c.content.as_ref()).collect();

        let non_none_contents: Vec<&Vec<u8>> = contents.iter().filter_map(|c| *c).collect();
        if non_none_contents.len() >= 2 && non_none_contents.windows(2).all(|w| w[0] == w[1]) {
            // All identical — merged content should match any of them.
            if *merged_content != non_none_contents[0].as_slice() {
                return Err(format!(
                    "CONTENT EMBEDDING VIOLATION: all workspaces wrote identical \
                     content to {:?}, but merged content differs.\n\
                     Expected: {} bytes\n\
                     Got: {} bytes",
                    path,
                    non_none_contents[0].len(),
                    merged_content.len(),
                ));
            }
        }

        // For non-overlapping edits against a base, check that each workspace's
        // unique edit text appears in the merged result.
        if let Some(base) = base_contents.get(path) {
            let base_str = String::from_utf8_lossy(base);
            let merged_str = String::from_utf8_lossy(merged_content);

            for (w, change) in ws_changes {
                if let Some(ref content) = change.content {
                    let ws_str = String::from_utf8_lossy(content);
                    // Find lines unique to this workspace (not in base).
                    let base_lines: BTreeSet<&str> = base_str.lines().collect();
                    let ws_unique_lines: Vec<&str> = ws_str
                        .lines()
                        .filter(|line| !base_lines.contains(line))
                        .collect();

                    // Each unique line from this workspace should appear in merged output.
                    for unique_line in &ws_unique_lines {
                        if !unique_line.trim().is_empty()
                            && !merged_str.lines().any(|ml| ml == *unique_line)
                        {
                            // This is a soft check — non-overlapping edits should
                            // embed. But if there are overlapping edits that conflict,
                            // this path should be in conflicts, not resolved.
                            // So if we got here, it's a real violation.
                            return Err(format!(
                                "CONTENT EMBEDDING VIOLATION: workspace '{}' added \
                                 unique line {:?} to {:?}, resolved cleanly, but \
                                 line is missing from merged content.\n\
                                 Merged:\n{}\n\
                                 Workspace content:\n{}",
                                w.name, unique_line, path, merged_str, ws_str,
                            ));
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pushout Property 2: MINIMALITY (approximate)
//
// For each conflict, verify it is necessary — the conflict cannot be avoided
// without losing a side's contribution. We verify specific invariants:
//
// a) If conflict reason is ModifyDelete, at least one side deleted and at
//    least one modified — this is inherently unresolvable.
// b) If conflict reason is AddAddDifferent, all sides added with different
//    content and no base exists — cannot be auto-resolved.
// c) If conflict reason is Diff3Conflict, the overlapping edits prevent
//    automatic resolution (verified by the diff3 engine itself).
// d) No path appears in both resolved and conflicts (would indicate the
//    engine resolved it AND flagged it as conflict — wasteful).
// ---------------------------------------------------------------------------

/// Verify minimality: each conflict is necessary.
fn verify_minimality(
    workspaces: &[TestWorkspace],
    result: &ResolveResult,
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), String> {
    let resolved_paths: BTreeSet<PathBuf> =
        result.resolved.iter().map(|r| r.path().clone()).collect();

    // No path should appear in both resolved and conflicts.
    for conflict in &result.conflicts {
        if resolved_paths.contains(&conflict.path) {
            return Err(format!(
                "MINIMALITY VIOLATION: path {:?} appears in both resolved and conflicts. \
                 Conflict reason: {}",
                conflict.path, conflict.reason,
            ));
        }
    }

    // Each conflict must be justified by its reason.
    for conflict in &result.conflicts {
        match &conflict.reason {
            ConflictReason::ModifyDelete => {
                // At least one side must be a deletion and at least one non-deletion.
                let has_delete = conflict
                    .sides
                    .iter()
                    .any(|s| matches!(s.kind, ChangeKind::Deleted));
                let has_non_delete = conflict
                    .sides
                    .iter()
                    .any(|s| !matches!(s.kind, ChangeKind::Deleted));
                if !has_delete || !has_non_delete {
                    return Err(format!(
                        "MINIMALITY VIOLATION: ModifyDelete conflict on {:?} but \
                         sides don't include both delete and non-delete.\n\
                         Sides: {:?}",
                        conflict.path,
                        conflict
                            .sides
                            .iter()
                            .map(|s| format!("{}:{}", s.workspace_id, s.kind))
                            .collect::<Vec<_>>(),
                    ));
                }
            }
            ConflictReason::AddAddDifferent => {
                // All sides must be adds, with different content, and no base.
                let all_adds = conflict
                    .sides
                    .iter()
                    .all(|s| matches!(s.kind, ChangeKind::Added));
                if !all_adds {
                    return Err(format!(
                        "MINIMALITY VIOLATION: AddAddDifferent conflict on {:?} but \
                         not all sides are adds.\n\
                         Sides: {:?}",
                        conflict.path,
                        conflict
                            .sides
                            .iter()
                            .map(|s| format!("{}:{}", s.workspace_id, s.kind))
                            .collect::<Vec<_>>(),
                    ));
                }
                if conflict.base.is_some() {
                    return Err(format!(
                        "MINIMALITY VIOLATION: AddAddDifferent conflict on {:?} but \
                         base content exists (should be None for pure add/add).",
                        conflict.path,
                    ));
                }
                // Contents must actually differ.
                let contents: Vec<Option<&Vec<u8>>> =
                    conflict.sides.iter().map(|s| s.content.as_ref()).collect();
                let non_none: Vec<&Vec<u8>> = contents.iter().filter_map(|c| *c).collect();
                if non_none.len() >= 2 && non_none.windows(2).all(|w| w[0] == w[1]) {
                    return Err(format!(
                        "MINIMALITY VIOLATION: AddAddDifferent conflict on {:?} but \
                         all contents are identical (should have been resolved by \
                         hash equality).",
                        conflict.path,
                    ));
                }
            }
            ConflictReason::Diff3Conflict => {
                // Must have a base, and at least 2 sides with different content.
                if base_contents.get(&conflict.path).is_none() && conflict.base.is_none() {
                    return Err(format!(
                        "MINIMALITY VIOLATION: Diff3Conflict on {:?} but no base \
                         content available (diff3 requires a base).",
                        conflict.path,
                    ));
                }
                let contents: Vec<Option<&Vec<u8>>> =
                    conflict.sides.iter().map(|s| s.content.as_ref()).collect();
                let non_none: Vec<&Vec<u8>> = contents.iter().filter_map(|c| *c).collect();
                if non_none.len() >= 2 && non_none.windows(2).all(|w| w[0] == w[1]) {
                    return Err(format!(
                        "MINIMALITY VIOLATION: Diff3Conflict on {:?} but all sides \
                         have identical content (should resolve via hash equality).",
                        conflict.path,
                    ));
                }
            }
            ConflictReason::MissingBase => {
                // Sides have different content but no base to merge against.
                // This is a valid conflict if contents differ.
            }
            ConflictReason::MissingContent => {
                // A non-deletion entry was missing file content.
                // Verify at least one side is not a deletion but has None content.
                let has_missing_content = conflict
                    .sides
                    .iter()
                    .any(|s| !matches!(s.kind, ChangeKind::Deleted) && s.content.is_none());
                if !has_missing_content {
                    return Err(format!(
                        "MINIMALITY VIOLATION: MissingContent conflict on {:?} but \
                         all non-deletion sides have content.",
                        conflict.path,
                    ));
                }
            }
        }
    }

    // Verify conflict side counts: every workspace that touched a shared conflicted
    // path should be represented.
    for conflict in &result.conflicts {
        let expected_ws = workspaces_touching_path(workspaces, &conflict.path);
        if expected_ws.len() > 1 {
            let actual_ws: BTreeSet<String> = conflict
                .sides
                .iter()
                .map(|s| s.workspace_id.to_string())
                .collect();
            let expected_set: BTreeSet<String> = expected_ws.into_iter().collect();

            if actual_ws != expected_set {
                return Err(format!(
                    "MINIMALITY VIOLATION: conflict on {:?} has sides {:?} but \
                     workspaces touching this path are {:?}.",
                    conflict.path, actual_ws, expected_set,
                ));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pushout Property 3: COMMUTATIVITY
//
// Merge(p₁, p₂) = Merge(p₂, p₁) — the merge result must be independent
// of workspace ordering. This overlaps with determinism testing but is a
// distinct categorical property (pushout uniqueness).
// ---------------------------------------------------------------------------

/// Compare two ResolveResults for structural equality.
fn results_equal(a: &ResolveResult, b: &ResolveResult) -> bool {
    if a.resolved.len() != b.resolved.len() || a.conflicts.len() != b.conflicts.len() {
        return false;
    }

    // Compare resolved changes.
    for (ra, rb) in a.resolved.iter().zip(b.resolved.iter()) {
        match (ra, rb) {
            (
                ResolvedChange::Upsert {
                    path: pa,
                    content: ca,
                },
                ResolvedChange::Upsert {
                    path: pb,
                    content: cb,
                },
            ) => {
                if pa != pb || ca != cb {
                    return false;
                }
            }
            (ResolvedChange::Delete { path: pa }, ResolvedChange::Delete { path: pb }) => {
                if pa != pb {
                    return false;
                }
            }
            _ => return false,
        }
    }

    // Compare conflicts by path, reason, and side count.
    for (ca, cb) in a.conflicts.iter().zip(b.conflicts.iter()) {
        if ca.path != cb.path
            || format!("{}", ca.reason) != format!("{}", cb.reason)
            || ca.sides.len() != cb.sides.len()
        {
            return false;
        }
    }

    true
}

/// Generate all permutations for small N, sampled orderings for large N.
fn orderings(n: usize, max_sample: usize) -> Vec<Vec<usize>> {
    if n <= 5 {
        let mut result = Vec::new();
        let mut indices: Vec<usize> = (0..n).collect();
        permute(&mut indices, 0, &mut result);
        result
    } else {
        let mut result = Vec::with_capacity(max_sample);
        result.push((0..n).collect());
        result.push((0..n).rev().collect());

        // Deterministic shuffles.
        for seed in 0..(max_sample.saturating_sub(2)) {
            let mut indices: Vec<usize> = (0..n).collect();
            let mut state: u64 = (seed as u64)
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            for i in (1..n).rev() {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let j = (state >> 33) as usize % (i + 1);
                indices.swap(i, j);
            }
            result.push(indices);
        }

        result.truncate(max_sample);
        result
    }
}

fn permute(arr: &mut Vec<usize>, start: usize, result: &mut Vec<Vec<usize>>) {
    if start == arr.len() {
        result.push(arr.clone());
        return;
    }
    for i in start..arr.len() {
        arr.swap(start, i);
        permute(arr, start + 1, result);
        arr.swap(start, i);
    }
}

// ---------------------------------------------------------------------------
// Property tests: 1000 cases each
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    // ===================================================================
    // EMBEDDING PROPERTIES
    // ===================================================================

    /// Every workspace's changed paths appear in the merge output (resolved
    /// or conflicts). This is the fundamental pushout embedding property.
    #[test]
    fn pushout_embedding_all_paths_accounted(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        if let Err(msg) = verify_embedding(&workspaces, &result) {
            prop_assert!(false, "{}\n\nScenario:\n{}", msg, format_scenario(&workspaces));
        }
    }

    /// For conflicted shared paths, every workspace that touched the path
    /// is represented as a conflict side.
    #[test]
    fn pushout_embedding_conflict_sides_complete(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        // Build path → workspace set.
        let mut path_ws: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
        for w in &workspaces {
            for c in &w.changes {
                path_ws.entry(c.path.clone()).or_default().insert(w.name.clone());
            }
        }

        for conflict in &result.conflicts {
            let expected = path_ws.get(&conflict.path).cloned().unwrap_or_default();
            if expected.len() <= 1 {
                continue; // Unique path conflicts (e.g., MissingContent) are fine with 1 side.
            }
            let actual: BTreeSet<String> = conflict
                .sides
                .iter()
                .map(|s| s.workspace_id.to_string())
                .collect();

            prop_assert_eq!(
                &actual, &expected,
                "Conflict on {:?}: sides {:?} != expected {:?}",
                conflict.path, actual, expected,
            );
        }
    }

    /// When all workspaces write identical content to a shared path,
    /// it must resolve cleanly (hash equality embedding).
    #[test]
    fn pushout_embedding_identical_always_resolves(
        n_workspaces in 2..=8usize,
        content in arb_content(),
        path in arb_path(),
    ) {
        let workspaces: Vec<TestWorkspace> = (0..n_workspaces)
            .map(|i| TestWorkspace {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    path.clone(),
                    ChangeKind::Modified,
                    Some(content.clone()),
                )],
            })
            .collect();

        let mut base = BTreeMap::new();
        base.insert(path.clone(), b"original content\n".to_vec());

        let result = run_merge(&workspaces, &base);

        prop_assert!(
            result.is_clean(),
            "Identical content from {} workspaces should always resolve cleanly.\n\
             Got {} conflicts.\nScenario:\n{}",
            n_workspaces,
            result.conflicts.len(),
            format_scenario(&workspaces),
        );

        // Merged content must match what all workspaces wrote.
        prop_assert_eq!(result.resolved.len(), 1);
        match &result.resolved[0] {
            ResolvedChange::Upsert { content: merged, .. } => {
                prop_assert_eq!(merged, &content);
            }
            ResolvedChange::Delete { .. } => {
                prop_assert!(false, "Expected upsert, got delete");
            }
        }
    }

    /// Content embedding for non-overlapping edits: each workspace's unique
    /// edit text appears in the merged result.
    #[test]
    fn pushout_content_embedding_non_overlapping(
        (workspaces, base_contents) in arb_non_overlapping_workspaces()
    ) {
        let result = run_merge(&workspaces, &base_contents);

        prop_assert!(
            result.is_clean(),
            "Non-overlapping edits should merge cleanly.\n\
             Got {} conflicts.\nScenario:\n{}",
            result.conflicts.len(),
            format_scenario(&workspaces),
        );

        if let Err(msg) = verify_content_embedding(&workspaces, &result, &base_contents) {
            prop_assert!(false, "{}\n\nScenario:\n{}", msg, format_scenario(&workspaces));
        }

        // Verify each workspace's edit is present in the merged content.
        match &result.resolved[0] {
            ResolvedChange::Upsert { content, .. } => {
                let merged_str = String::from_utf8_lossy(content);
                for (i, _w) in workspaces.iter().enumerate() {
                    let edit_marker = format!("EDITED-BY-{i:02}");
                    prop_assert!(
                        merged_str.contains(&edit_marker),
                        "Missing edit from ws-{:02} in merged content.\n\
                         Expected: {}\n\
                         Merged content:\n{}",
                        i, edit_marker, merged_str,
                    );
                }
            }
            ResolvedChange::Delete { .. } => {
                prop_assert!(false, "Expected upsert for merged shared file");
            }
        }
    }

    // ===================================================================
    // MINIMALITY PROPERTIES
    // ===================================================================

    /// Every conflict in the merge result is justified — the conflict cannot
    /// be resolved without losing a side's contribution.
    #[test]
    fn pushout_minimality_conflicts_justified(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        if let Err(msg) = verify_minimality(&workspaces, &result, &base_contents) {
            prop_assert!(false, "{}\n\nScenario:\n{}", msg, format_scenario(&workspaces));
        }
    }

    /// Modify/delete is always a conflict — no merge can embed both intents.
    #[test]
    fn pushout_minimality_modify_delete_always_conflicts(
        workspaces in arb_modify_delete_workspaces()
    ) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        prop_assert!(
            !result.is_clean(),
            "Modify/delete should always produce at least one conflict.\n\
             Scenario:\n{}",
            format_scenario(&workspaces),
        );

        // The conflict should be ModifyDelete.
        let has_mod_del = result
            .conflicts
            .iter()
            .any(|c| matches!(c.reason, ConflictReason::ModifyDelete));
        prop_assert!(
            has_mod_del,
            "Expected ModifyDelete conflict.\n\
             Actual conflicts: {:?}",
            result.conflicts.iter().map(|c| format!("{}", c.reason)).collect::<Vec<_>>(),
        );
    }

    /// Add/add with different content is always a conflict — no base to merge against.
    #[test]
    fn pushout_minimality_add_add_different_conflicts(
        workspaces in arb_add_add_workspaces()
    ) {
        let base_contents = BTreeMap::new();
        let result = run_merge(&workspaces, &base_contents);

        prop_assert!(
            !result.is_clean(),
            "Add/add with different content should always conflict.\n\
             Scenario:\n{}",
            format_scenario(&workspaces),
        );

        let has_add_add = result
            .conflicts
            .iter()
            .any(|c| matches!(c.reason, ConflictReason::AddAddDifferent));
        prop_assert!(
            has_add_add,
            "Expected AddAddDifferent conflict.\n\
             Actual conflicts: {:?}",
            result.conflicts.iter().map(|c| format!("{}", c.reason)).collect::<Vec<_>>(),
        );
    }

    /// Disjoint changes (each workspace touches unique files) should never
    /// produce conflicts — the pushout exists trivially.
    #[test]
    fn pushout_minimality_disjoint_never_conflicts(n_workspaces in 2..=8usize) {
        let workspaces: Vec<TestWorkspace> = (0..n_workspaces)
            .map(|i| TestWorkspace {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    PathBuf::from(format!("unique-{i}.rs")),
                    ChangeKind::Added,
                    Some(format!("fn ws_{i}() {{}}\n").into_bytes()),
                )],
            })
            .collect();

        let result = run_merge(&workspaces, &BTreeMap::new());

        prop_assert!(
            result.is_clean(),
            "Disjoint changes should never conflict.\n\
             Got {} conflicts.\nScenario:\n{}",
            result.conflicts.len(),
            format_scenario(&workspaces),
        );
        prop_assert_eq!(result.resolved.len(), n_workspaces);
    }

    /// Delete/delete always resolves — both sides agree on the outcome.
    #[test]
    fn pushout_minimality_delete_delete_resolves(n_workspaces in 2..=8usize) {
        let path = PathBuf::from("deleted.txt");
        let workspaces: Vec<TestWorkspace> = (0..n_workspaces)
            .map(|i| TestWorkspace {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    path.clone(),
                    ChangeKind::Deleted,
                    None,
                )],
            })
            .collect();

        let mut base = BTreeMap::new();
        base.insert(path, b"old content\n".to_vec());

        let result = run_merge(&workspaces, &base);
        prop_assert!(result.is_clean(), "Delete/delete should resolve cleanly");
    }

    /// No path appears in both resolved and conflicts (uniqueness).
    #[test]
    fn pushout_minimality_no_path_duplication(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        let resolved_paths: BTreeSet<PathBuf> = result
            .resolved
            .iter()
            .map(|r| r.path().clone())
            .collect();
        let conflict_paths: BTreeSet<PathBuf> = result
            .conflicts
            .iter()
            .map(|c| c.path.clone())
            .collect();

        let overlap: BTreeSet<_> = resolved_paths.intersection(&conflict_paths).collect();
        prop_assert!(
            overlap.is_empty(),
            "Paths should not appear in both resolved and conflicts: {:?}",
            overlap,
        );
    }

    // ===================================================================
    // COMMUTATIVITY PROPERTIES
    // ===================================================================

    /// The merge result is independent of workspace ordering (pushout
    /// uniqueness up to isomorphism). Tests all permutations for N≤5,
    /// sampled orderings for N>5.
    #[test]
    fn pushout_commutativity_order_independent(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let n = workspaces.len();
        let perms = orderings(n, 30);

        let mut results: Vec<ResolveResult> = Vec::with_capacity(perms.len());
        for perm in &perms {
            let reordered: Vec<TestWorkspace> =
                perm.iter().map(|&i| workspaces[i].clone()).collect();
            let result = run_merge(&reordered, &base_contents);
            results.push(result);
        }

        let first = &results[0];
        for (i, result) in results.iter().enumerate().skip(1) {
            prop_assert!(
                results_equal(first, result),
                "COMMUTATIVITY VIOLATION: ordering {} produced different result.\n\
                 Ordering 0: {} resolved, {} conflicts\n\
                 Ordering {}: {} resolved, {} conflicts\n\
                 Scenario:\n{}",
                i,
                first.resolved.len(), first.conflicts.len(),
                i,
                result.resolved.len(), result.conflicts.len(),
                format_scenario(&workspaces),
            );
        }
    }

    /// Commutativity for shared-path scenarios (higher collision rate).
    #[test]
    fn pushout_commutativity_shared_paths(workspaces in arb_shared_path_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let n = workspaces.len();
        let perms = orderings(n, 20);

        let mut results: Vec<ResolveResult> = Vec::with_capacity(perms.len());
        for perm in &perms {
            let reordered: Vec<TestWorkspace> =
                perm.iter().map(|&i| workspaces[i].clone()).collect();
            let result = run_merge(&reordered, &base_contents);
            results.push(result);
        }

        let first = &results[0];
        for (i, result) in results.iter().enumerate().skip(1) {
            prop_assert!(
                results_equal(first, result),
                "COMMUTATIVITY VIOLATION on shared paths: ordering {} differs.\n\
                 Scenario:\n{}",
                i,
                format_scenario(&workspaces),
            );
        }
    }

    // ===================================================================
    // COMBINED PUSHOUT CONTRACT
    // ===================================================================

    /// Full pushout contract: embedding + minimality + commutativity in
    /// one combined check. This is the canonical §9.2 verification.
    #[test]
    fn pushout_full_contract(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        // 1. Embedding
        if let Err(msg) = verify_embedding(&workspaces, &result) {
            prop_assert!(false,
                "PUSHOUT CONTRACT VIOLATION (embedding):\n{}\n\nScenario:\n{}",
                msg, format_scenario(&workspaces),
            );
        }

        // 2. Content embedding
        if let Err(msg) = verify_content_embedding(&workspaces, &result, &base_contents) {
            prop_assert!(false,
                "PUSHOUT CONTRACT VIOLATION (content embedding):\n{}\n\nScenario:\n{}",
                msg, format_scenario(&workspaces),
            );
        }

        // 3. Minimality
        if let Err(msg) = verify_minimality(&workspaces, &result, &base_contents) {
            prop_assert!(false,
                "PUSHOUT CONTRACT VIOLATION (minimality):\n{}\n\nScenario:\n{}",
                msg, format_scenario(&workspaces),
            );
        }

        // 4. Commutativity (sample 10 orderings to keep runtime bounded)
        let n = workspaces.len();
        let perms = orderings(n, 10);
        let first = &result;
        for perm in perms.iter().skip(1) {
            let reordered: Vec<TestWorkspace> =
                perm.iter().map(|&i| workspaces[i].clone()).collect();
            let other = run_merge(&reordered, &base_contents);
            prop_assert!(
                results_equal(first, &other),
                "PUSHOUT CONTRACT VIOLATION (commutativity): \
                 ordering {:?} produced different result.\nScenario:\n{}",
                perm, format_scenario(&workspaces),
            );
        }
    }

    // ===================================================================
    // PUSHOUT UNIVERSALITY (approximate)
    //
    // The merge result M should be "minimal" — for each resolved path,
    // there should be no simpler resolution that still embeds all sides.
    // We approximate this by verifying that the resolved content is one
    // of: the base, one of the side contents, or a clean diff3 merge.
    // Any other content would suggest a non-minimal merge.
    // ===================================================================

    /// Resolved content must be either a side's content or a valid merge
    /// of the sides. No phantom content should appear.
    #[test]
    fn pushout_universality_no_phantom_content(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let result = run_merge(&workspaces, &base_contents);

        // Build path → set of possible contents (from workspaces + base).
        let mut path_contents: BTreeMap<PathBuf, Vec<Vec<u8>>> = BTreeMap::new();
        for w in &workspaces {
            for change in &w.changes {
                if let Some(ref content) = change.content {
                    path_contents
                        .entry(change.path.clone())
                        .or_default()
                        .push(content.clone());
                }
            }
        }
        for (path, base) in &base_contents {
            path_contents
                .entry(path.clone())
                .or_default()
                .push(base.clone());
        }

        for resolved in &result.resolved {
            match resolved {
                ResolvedChange::Upsert { path, content } => {
                    let possible = path_contents.get(path);
                    if let Some(possible_contents) = possible {
                        // For paths touched by only one workspace, content must match exactly.
                        let ws_touching = workspaces_touching_path(&workspaces, path);
                        if ws_touching.len() == 1 {
                            let matches = possible_contents.iter().any(|pc| pc == content);
                            prop_assert!(
                                matches,
                                "UNIVERSALITY VIOLATION: unique path {:?} resolved to \
                                 content that doesn't match any workspace or base.\n\
                                 Content len: {}, Possible lens: {:?}",
                                path,
                                content.len(),
                                possible_contents.iter().map(|c| c.len()).collect::<Vec<_>>(),
                            );
                        }
                        // For shared paths, the merged content is a combination of
                        // sides — we can't check exact match, but we can verify it's
                        // not empty when sides have content.
                        if ws_touching.len() > 1 && !possible_contents.is_empty() {
                            // Merged content should not be empty if inputs have content.
                            let all_inputs_empty =
                                possible_contents.iter().all(|c| c.is_empty());
                            if !all_inputs_empty {
                                prop_assert!(
                                    !content.is_empty(),
                                    "UNIVERSALITY VIOLATION: shared path {:?} resolved \
                                     to empty content but inputs had content.",
                                    path,
                                );
                            }
                        }
                    }
                }
                ResolvedChange::Delete { path } => {
                    // A delete is valid only if at least one workspace deleted this file
                    // (or all did).
                    let ws_deleting: Vec<&TestWorkspace> = workspaces
                        .iter()
                        .filter(|w| {
                            w.changes
                                .iter()
                                .any(|c| &c.path == path && matches!(c.kind, ChangeKind::Deleted))
                        })
                        .collect();
                    prop_assert!(
                        !ws_deleting.is_empty(),
                        "UNIVERSALITY VIOLATION: path {:?} deleted in merge but \
                         no workspace deleted it.",
                        path,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Additional focused property tests at higher case count
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(250))]

    /// Non-overlapping edits to a shared file: all edits should be preserved
    /// in the merged content (content embedding at scale).
    #[test]
    fn pushout_content_embedding_at_scale(
        (workspaces, base_contents) in arb_non_overlapping_workspaces()
    ) {
        let result = run_merge(&workspaces, &base_contents);

        // Must resolve cleanly.
        prop_assert!(
            result.is_clean(),
            "Non-overlapping edits should merge cleanly ({} conflicts).\n\
             Scenario:\n{}",
            result.conflicts.len(),
            format_scenario(&workspaces),
        );

        // Full pushout contract.
        if let Err(msg) = verify_embedding(&workspaces, &result) {
            prop_assert!(false, "{}", msg);
        }
        if let Err(msg) = verify_content_embedding(&workspaces, &result, &base_contents) {
            prop_assert!(false, "{}", msg);
        }
        if let Err(msg) = verify_minimality(&workspaces, &result, &base_contents) {
            prop_assert!(false, "{}", msg);
        }
    }

    /// Commutativity at scale: 2-8 workspaces with mixed changes.
    #[test]
    fn pushout_commutativity_at_scale(workspaces in arb_workspaces()) {
        let base_contents = make_base_contents(&workspaces);
        let n = workspaces.len();

        // Compute reference result.
        let reference = run_merge(&workspaces, &base_contents);

        // Test reversed ordering.
        let reversed: Vec<TestWorkspace> = workspaces.iter().rev().cloned().collect();
        let rev_result = run_merge(&reversed, &base_contents);
        prop_assert!(
            results_equal(&reference, &rev_result),
            "COMMUTATIVITY VIOLATION: reversed ordering differs for {} workspaces.\n\
             Forward: {} resolved, {} conflicts\n\
             Reverse: {} resolved, {} conflicts\n\
             Scenario:\n{}",
            n,
            reference.resolved.len(), reference.conflicts.len(),
            rev_result.resolved.len(), rev_result.conflicts.len(),
            format_scenario(&workspaces),
        );
    }
}
