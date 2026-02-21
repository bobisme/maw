//! Property tests for merge engine determinism.
//!
//! The merge pipeline (partition → resolve → build) must be deterministic:
//! the same set of workspace patch-sets must always produce the same
//! `ResolveResult` regardless of the order workspaces appear in the input,
//! and the same resolved changes must produce the same git tree OID.
//!
//! Uses proptest to generate random merge scenarios and verify that all
//! permutations of workspace ordering yield identical output.
//! Minimum 100 scenarios per property test.
//!
//! # Coverage
//!
//! - **Workspace counts**: 2, 3, 5, and 10 workspaces tested explicitly
//! - **Change types**: disjoint, overlapping (diff3-resolvable), conflicting,
//!   identical, delete/delete, modify/delete, add/add
//! - **End-to-end**: full pipeline through `build_merge_commit` with real git
//!   repos in /tmp, verifying identical commit OIDs across orderings
//! - **100+ random scenarios**: via proptest with `ProptestConfig::with_cases(100)`

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use proptest::prelude::*;

use crate::merge::build::{ResolvedChange, build_merge_commit};
use crate::merge::partition::partition_by_path;
use crate::merge::resolve::{ConflictRecord, ResolveResult, resolve_partition};
use crate::merge::types::{ChangeKind, FileChange, PatchSet};
use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fixed epoch OID for all tests (content doesn't matter for partition/resolve
/// at the unit level — resolve just needs `base_contents` map).
fn epoch() -> EpochId {
    EpochId::new(&"a".repeat(40)).unwrap()
}

fn ws(name: &str) -> WorkspaceId {
    WorkspaceId::new(name).unwrap()
}

/// Canonical representation of a `ResolvedChange` for comparison.
fn canon_resolved(r: &ResolvedChange) -> (PathBuf, bool, Option<Vec<u8>>) {
    match r {
        ResolvedChange::Upsert { path, content } => (path.clone(), true, Some(content.clone())),
        ResolvedChange::Delete { path } => (path.clone(), false, None),
    }
}

fn canon_conflict(c: &ConflictRecord) -> (PathBuf, String, usize) {
    (c.path.clone(), format!("{}", c.reason), c.sides.len())
}

/// Compare two `ResolveResults` for equality.
/// Both resolved and conflicts vectors are already sorted by path in the
/// implementation, so direct comparison works.
fn results_equal(a: &ResolveResult, b: &ResolveResult) -> bool {
    if a.resolved.len() != b.resolved.len() || a.conflicts.len() != b.conflicts.len() {
        return false;
    }

    let a_res: Vec<_> = a.resolved.iter().map(canon_resolved).collect();
    let b_res: Vec<_> = b.resolved.iter().map(canon_resolved).collect();
    if a_res != b_res {
        return false;
    }

    let a_con: Vec<_> = a.conflicts.iter().map(canon_conflict).collect();
    let b_con: Vec<_> = b.conflicts.iter().map(canon_conflict).collect();
    a_con == b_con
}

// ---------------------------------------------------------------------------
// Proptest strategies
// ---------------------------------------------------------------------------

/// Generate a valid file path (1-3 segments, alphanumeric + .rs suffix).
fn arb_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec("[a-z][a-z0-9]{0,7}", 1..=3usize).prop_map(|segments| {
        let mut p = segments.join("/");
        p.push_str(".rs");
        PathBuf::from(p)
    })
}

/// Generate file content: 1-10 lines of short text.
fn arb_content() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec("[a-zA-Z0-9 ]{1,20}\n", 1..=10usize)
        .prop_map(|lines| lines.join("").into_bytes())
}

/// Generate a `ChangeKind`.
fn arb_change_kind() -> impl Strategy<Value = ChangeKind> {
    prop_oneof![
        Just(ChangeKind::Added),
        Just(ChangeKind::Modified),
        Just(ChangeKind::Deleted),
    ]
}

/// Generate a single `FileChange`.
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

/// A workspace scenario: workspace name + list of file changes.
#[derive(Clone, Debug)]
struct WorkspaceScenario {
    name: String,
    changes: Vec<FileChange>,
}

/// Generate 2-5 workspaces, each with 1-8 file changes.
fn arb_scenario() -> impl Strategy<Value = Vec<WorkspaceScenario>> {
    prop::collection::vec(
        prop::collection::vec(arb_file_change(), 1..=8usize),
        2..=5usize,
    )
    .prop_map(|workspace_changes| {
        workspace_changes
            .into_iter()
            .enumerate()
            .map(|(i, changes)| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes,
            })
            .collect()
    })
}

/// Generate base contents for paths that appear as Modified or Deleted in any
/// workspace. This ensures resolve has base content available for diff3.
fn make_base_contents(scenarios: &[WorkspaceScenario]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut base = BTreeMap::new();
    for ws_scenario in scenarios {
        for change in &ws_scenario.changes {
            if matches!(change.kind, ChangeKind::Modified | ChangeKind::Deleted) {
                base.entry(change.path.clone())
                    .or_insert_with(|| b"base content\nline 2\nline 3\n".to_vec());
            }
        }
    }
    base
}

/// Convert scenarios to `PatchSets` in the given order.
fn to_patch_sets(scenarios: &[WorkspaceScenario]) -> Vec<PatchSet> {
    scenarios
        .iter()
        .map(|s| PatchSet::new(ws(&s.name), epoch(), s.changes.clone()))
        .collect()
}

/// Generate all permutations of indices [0..n).
/// For n<=5 this is at most 120 permutations — well within budget.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let mut result = Vec::new();
    let mut indices: Vec<usize> = (0..n).collect();
    permute(&mut indices, 0, &mut result);
    result
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

/// Generate orderings for determinism testing.
///
/// For n<=5 (n!<=120): returns all permutations.
/// For n>5: returns a deterministic sample of `sample_count` orderings
/// covering identity, reverse, and shuffled patterns. Uses a seeded
/// shuffle for reproducibility.
fn sampled_orderings(n: usize, sample_count: usize) -> Vec<Vec<usize>> {
    if n <= 5 {
        return permutations(n);
    }

    let mut result = Vec::with_capacity(sample_count);

    // Always include identity and reverse.
    result.push((0..n).collect());
    result.push((0..n).rev().collect());

    // Evens first, then odds.
    let mut evens_first: Vec<usize> = (0..n).filter(|x| x % 2 == 0).collect();
    evens_first.extend((0..n).filter(|x| x % 2 != 0));
    result.push(evens_first);

    // Odds first, then evens.
    let mut odds_first: Vec<usize> = (0..n).filter(|x| x % 2 != 0).collect();
    odds_first.extend((0..n).filter(|x| x % 2 == 0));
    result.push(odds_first);

    // Generate deterministic shuffles using a simple LCG seeded from index.
    // This ensures reproducibility without depending on external RNG.
    for seed in 0..(sample_count.saturating_sub(result.len())) {
        let mut indices: Vec<usize> = (0..n).collect();
        // Fisher-Yates shuffle with deterministic LCG.
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

    result.truncate(sample_count);
    result
}

/// Generate 2-10 workspaces, each with 1-8 file changes.
/// Used for large-N determinism testing.
fn arb_large_scenario() -> impl Strategy<Value = Vec<WorkspaceScenario>> {
    prop::collection::vec(
        prop::collection::vec(arb_file_change(), 1..=8usize),
        2..=10usize,
    )
    .prop_map(|workspace_changes| {
        workspace_changes
            .into_iter()
            .enumerate()
            .map(|(i, changes)| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes,
            })
            .collect()
    })
}

/// Format a scenario for reproduction on failure.
fn format_repro(scenarios: &[WorkspaceScenario], perm: &[usize]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Workspace count: {}\n", scenarios.len()));
    out.push_str(&format!("Failing permutation: {perm:?}\n"));
    out.push_str("Workspaces:\n");
    for s in scenarios {
        out.push_str(&format!("  {} ({} changes):\n", s.name, s.changes.len()));
        for c in &s.changes {
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
// Git repo helpers for end-to-end tests
// ---------------------------------------------------------------------------

/// Run a git command in a directory, panicking on failure.
fn run_git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Get a git OID by rev-parsing.
fn git_rev_parse(root: &Path, rev: &str) -> GitOid {
    let out = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "rev-parse {rev} failed");
    GitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
}

/// Get the tree OID for a commit.
fn git_tree_oid(root: &Path, commit: &str) -> GitOid {
    git_rev_parse(root, &format!("{commit}^{{tree}}"))
}

/// Set up a fresh git repo with identity configured and an initial commit.
/// Returns (`TempDir`, `EpochId`).
fn setup_git_repo() -> (tempfile::TempDir, EpochId) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    run_git(root, &["init"]);
    run_git(root, &["config", "user.name", "Test"]);
    run_git(root, &["config", "user.email", "test@test.com"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("README.md"), "# Test\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-m", "initial"]);

    let oid = git_rev_parse(root, "HEAD");
    let epoch = EpochId::new(oid.as_str()).unwrap();
    (dir, epoch)
}

/// Set up a git repo with multiple base files for richer merge scenarios.
/// Creates files with spaced-out regions so diff3 can resolve non-overlapping edits.
fn setup_git_repo_with_base_files(
    file_count: usize,
    regions_per_file: usize,
) -> (tempfile::TempDir, EpochId, Vec<(PathBuf, String)>) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    run_git(root, &["init"]);
    run_git(root, &["config", "user.name", "Test"]);
    run_git(root, &["config", "user.email", "test@test.com"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);

    let mut base_files = Vec::new();
    for f in 0..file_count {
        let path = PathBuf::from(format!("src/file-{f:02}.txt"));
        let mut lines = Vec::new();
        for r in 0..regions_per_file {
            lines.push(format!("region-{r}-of-file-{f}"));
            // 4 spacer lines for diff3 context separation
            for _ in 0..4 {
                lines.push("-".to_string());
            }
        }
        let content = lines.join("\n") + "\n";
        let full_path = root.join(&path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, &content).unwrap();
        base_files.push((path, content));
    }

    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "base: add files"]);

    let oid = git_rev_parse(root, "HEAD");
    let epoch = EpochId::new(oid.as_str()).unwrap();
    (dir, epoch, base_files)
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Core determinism property: partition → resolve produces identical
    /// results regardless of workspace ordering in the input.
    #[test]
    fn merge_is_order_independent(scenarios in arb_scenario()) {
        let base_contents = make_base_contents(&scenarios);
        let n = scenarios.len();
        let perms = permutations(n);

        // Run partition → resolve for each permutation.
        let mut results: Vec<ResolveResult> = Vec::with_capacity(perms.len());
        for perm in &perms {
            let reordered: Vec<WorkspaceScenario> =
                perm.iter().map(|&i| scenarios[i].clone()).collect();
            let patch_sets = to_patch_sets(&reordered);
            let partition = partition_by_path(&patch_sets);
            let result = resolve_partition(&partition, &base_contents)
                .expect("resolve should not error");
            results.push(result);
        }

        // All results must be identical to the first.
        let first = &results[0];
        for (i, result) in results.iter().enumerate().skip(1) {
            prop_assert!(
                results_equal(first, result),
                "Permutation {} produced different result than permutation 0.\n\
                 Perm 0: {} resolved, {} conflicts\n\
                 Perm {}: {} resolved, {} conflicts",
                i,
                first.resolved.len(), first.conflicts.len(),
                i,
                result.resolved.len(), result.conflicts.len(),
            );
        }
    }

    /// Determinism for partition alone: unique/shared counts and contents
    /// must be identical regardless of input ordering.
    #[test]
    fn partition_counts_are_order_independent(scenarios in arb_scenario()) {
        let n = scenarios.len();
        let perms = permutations(n);

        let mut counts: Vec<(usize, usize, usize)> = Vec::new();
        for perm in &perms {
            let reordered: Vec<WorkspaceScenario> =
                perm.iter().map(|&i| scenarios[i].clone()).collect();
            let patch_sets = to_patch_sets(&reordered);
            let partition = partition_by_path(&patch_sets);
            counts.push((
                partition.unique_count(),
                partition.shared_count(),
                partition.total_path_count(),
            ));
        }

        let first = counts[0];
        for (i, &c) in counts.iter().enumerate().skip(1) {
            prop_assert_eq!(
                first, c,
                "Permutation {} gave different partition counts than permutation 0",
                i,
            );
        }
    }

    /// Partition paths must always be sorted lexicographically.
    #[test]
    fn partition_paths_always_sorted(scenarios in arb_scenario()) {
        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);

        // Check unique paths are sorted.
        let unique_paths: Vec<_> = partition.unique.iter().map(|(p, _)| p).collect();
        for w in unique_paths.windows(2) {
            prop_assert!(
                w[0] <= w[1],
                "Unique paths not sorted: {:?} > {:?}",
                w[0], w[1],
            );
        }

        // Check shared paths are sorted.
        let shared_paths: Vec<_> = partition.shared.iter().map(|(p, _)| p).collect();
        for w in shared_paths.windows(2) {
            prop_assert!(
                w[0] <= w[1],
                "Shared paths not sorted: {:?} > {:?}",
                w[0], w[1],
            );
        }

        // Within shared entries, workspace IDs should be sorted.
        for (path, entries) in &partition.shared {
            let ws_ids: Vec<_> = entries.iter().map(|e| e.workspace_id.as_str()).collect();
            for w in ws_ids.windows(2) {
                prop_assert!(
                    w[0] <= w[1],
                    "Workspace IDs not sorted for path {:?}: {:?} > {:?}",
                    path, w[0], w[1],
                );
            }
        }
    }

    /// Resolved output paths must always be sorted.
    #[test]
    fn resolve_paths_always_sorted(scenarios in arb_scenario()) {
        let base_contents = make_base_contents(&scenarios);
        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);
        let result = resolve_partition(&partition, &base_contents)
            .expect("resolve should not error");

        // Resolved paths sorted.
        let res_paths: Vec<_> = result.resolved.iter().map(super::build::ResolvedChange::path).collect();
        for w in res_paths.windows(2) {
            prop_assert!(
                w[0] <= w[1],
                "Resolved paths not sorted: {:?} > {:?}",
                w[0], w[1],
            );
        }

        // Conflict paths sorted.
        let con_paths: Vec<_> = result.conflicts.iter().map(|c| &c.path).collect();
        for w in con_paths.windows(2) {
            prop_assert!(
                w[0] <= w[1],
                "Conflict paths not sorted: {:?} > {:?}",
                w[0], w[1],
            );
        }
    }

    /// Every path from every workspace must appear in either resolved or conflicts.
    #[test]
    fn all_paths_accounted_for(scenarios in arb_scenario()) {
        let base_contents = make_base_contents(&scenarios);
        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);
        let result = resolve_partition(&partition, &base_contents)
            .expect("resolve should not error");

        // Collect all input paths (deduplicated).
        let mut input_paths: std::collections::BTreeSet<PathBuf> =
            std::collections::BTreeSet::new();
        for s in &scenarios {
            for change in &s.changes {
                input_paths.insert(change.path.clone());
            }
        }

        // Collect all output paths.
        let mut output_paths: std::collections::BTreeSet<PathBuf> =
            std::collections::BTreeSet::new();
        for r in &result.resolved {
            output_paths.insert(r.path().clone());
        }
        for c in &result.conflicts {
            output_paths.insert(c.path.clone());
        }

        prop_assert_eq!(
            input_paths, output_paths,
            "Input paths and output paths should match",
        );
    }

    /// Hash-equality property: if all workspaces write the same content to a
    /// path, it should resolve (not conflict), regardless of the number of
    /// workspaces.
    #[test]
    fn identical_changes_always_resolve(
        n_workspaces in 2..=5usize,
        content in arb_content(),
    ) {
        let scenarios: Vec<WorkspaceScenario> = (0..n_workspaces)
            .map(|i| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    PathBuf::from("same.txt"),
                    ChangeKind::Modified,
                    Some(content.clone()),
                )],
            })
            .collect();

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("same.txt"), b"original\n".to_vec());

        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);

        // Should be 0 unique, 1 shared.
        prop_assert_eq!(partition.unique_count(), 0);
        prop_assert_eq!(partition.shared_count(), 1);

        let result =
            resolve_partition(&partition, &base).expect("resolve should not error");

        prop_assert!(
            result.is_clean(),
            "Identical changes should resolve cleanly, got {} conflicts",
            result.conflicts.len(),
        );
        prop_assert_eq!(result.resolved.len(), 1);

        // Content should match what all workspaces wrote.
        match &result.resolved[0] {
            ResolvedChange::Upsert {
                content: resolved_content,
                ..
            } => {
                prop_assert_eq!(resolved_content, &content);
            }
            ResolvedChange::Delete { .. } => {
                prop_assert!(false, "Expected upsert, got delete");
            }
        }
    }

    /// Disjoint changes (each workspace touches unique files) should always
    /// merge cleanly with zero conflicts.
    #[test]
    fn disjoint_changes_never_conflict(n_workspaces in 2..=5usize) {
        let scenarios: Vec<WorkspaceScenario> = (0..n_workspaces)
            .map(|i| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    PathBuf::from(format!("unique-{i}.rs")),
                    ChangeKind::Added,
                    Some(format!("fn ws_{i}() {{}}\n").into_bytes()),
                )],
            })
            .collect();

        let base = BTreeMap::new();
        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);

        prop_assert_eq!(partition.unique_count(), n_workspaces);
        prop_assert_eq!(partition.shared_count(), 0);

        let result =
            resolve_partition(&partition, &base).expect("resolve should not error");

        prop_assert!(result.is_clean());
        prop_assert_eq!(result.resolved.len(), n_workspaces);
    }

    /// Delete/delete should always resolve cleanly regardless of workspace count.
    #[test]
    fn delete_delete_always_resolves(n_workspaces in 2..=5usize) {
        let scenarios: Vec<WorkspaceScenario> = (0..n_workspaces)
            .map(|i| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    PathBuf::from("deleted.txt"),
                    ChangeKind::Deleted,
                    None,
                )],
            })
            .collect();

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("deleted.txt"), b"old content\n".to_vec());

        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);
        let result =
            resolve_partition(&partition, &base).expect("resolve should not error");

        prop_assert!(result.is_clean());
        prop_assert_eq!(result.resolved.len(), 1);
        match &result.resolved[0] {
            ResolvedChange::Delete { path } => {
                prop_assert_eq!(path, &PathBuf::from("deleted.txt"));
            }
            _ => prop_assert!(false, "Expected delete"),
        }
    }

    /// Modify/delete must always produce a conflict.
    #[test]
    fn modify_delete_always_conflicts(content in arb_content()) {
        let scenarios = vec![
            WorkspaceScenario {
                name: "ws-00".to_string(),
                changes: vec![FileChange::new(
                    PathBuf::from("clash.txt"),
                    ChangeKind::Modified,
                    Some(content),
                )],
            },
            WorkspaceScenario {
                name: "ws-01".to_string(),
                changes: vec![FileChange::new(
                    PathBuf::from("clash.txt"),
                    ChangeKind::Deleted,
                    None,
                )],
            },
        ];

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("clash.txt"), b"original\n".to_vec());

        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);
        let result =
            resolve_partition(&partition, &base).expect("resolve should not error");

        prop_assert!(!result.is_clean());
        prop_assert_eq!(result.conflicts.len(), 1);
        prop_assert_eq!(
            format!("{}", result.conflicts[0].reason),
            "modify/delete conflict",
        );
    }
}

// ---------------------------------------------------------------------------
// Large-N property tests (2-10 workspaces with sampled orderings)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Core determinism property at scale: partition → resolve produces
    /// identical results for 2-10 workspaces using sampled orderings.
    /// For N<=5, tests all N! permutations. For N>5, tests 50 sampled orderings.
    #[test]
    fn merge_is_order_independent_large_n(scenarios in arb_large_scenario()) {
        let base_contents = make_base_contents(&scenarios);
        let n = scenarios.len();
        let orderings = sampled_orderings(n, 50);

        let mut results: Vec<ResolveResult> = Vec::with_capacity(orderings.len());
        for ordering in &orderings {
            let reordered: Vec<WorkspaceScenario> =
                ordering.iter().map(|&i| scenarios[i].clone()).collect();
            let patch_sets = to_patch_sets(&reordered);
            let partition = partition_by_path(&patch_sets);
            let result = resolve_partition(&partition, &base_contents)
                .expect("resolve should not error");
            results.push(result);
        }

        let first = &results[0];
        for (i, result) in results.iter().enumerate().skip(1) {
            prop_assert!(
                results_equal(first, result),
                "Ordering {} (of {}) produced different result for {} workspaces.\n\
                 Ordering 0: {} resolved, {} conflicts\n\
                 Ordering {}: {} resolved, {} conflicts\n\
                 Reproduction info:\n{}",
                i, orderings.len(), n,
                first.resolved.len(), first.conflicts.len(),
                i,
                result.resolved.len(), result.conflicts.len(),
                format_repro(&scenarios, &orderings[i]),
            );
        }
    }

    /// Hash-equality property at scale: identical changes from 2-10
    /// workspaces always resolve cleanly.
    #[test]
    fn identical_changes_always_resolve_large_n(
        n_workspaces in 2..=10usize,
        content in arb_content(),
    ) {
        let scenarios: Vec<WorkspaceScenario> = (0..n_workspaces)
            .map(|i| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    PathBuf::from("same.txt"),
                    ChangeKind::Modified,
                    Some(content.clone()),
                )],
            })
            .collect();

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("same.txt"), b"original\n".to_vec());

        // Test with sampled orderings.
        let orderings = sampled_orderings(n_workspaces, 20);
        for ordering in &orderings {
            let reordered: Vec<WorkspaceScenario> =
                ordering.iter().map(|&i| scenarios[i].clone()).collect();
            let patch_sets = to_patch_sets(&reordered);
            let partition = partition_by_path(&patch_sets);
            let result = resolve_partition(&partition, &base)
                .expect("resolve should not error");

            prop_assert!(
                result.is_clean(),
                "Identical changes from {} workspaces should resolve cleanly, \
                 got {} conflicts (ordering: {:?})",
                n_workspaces, result.conflicts.len(), ordering,
            );
            prop_assert_eq!(result.resolved.len(), 1);
        }
    }

    /// Disjoint changes from 2-10 workspaces never conflict, regardless of order.
    #[test]
    fn disjoint_changes_never_conflict_large_n(n_workspaces in 2..=10usize) {
        let scenarios: Vec<WorkspaceScenario> = (0..n_workspaces)
            .map(|i| WorkspaceScenario {
                name: format!("ws-{i:02}"),
                changes: vec![FileChange::new(
                    PathBuf::from(format!("unique-{i}.rs")),
                    ChangeKind::Added,
                    Some(format!("fn ws_{i}() {{}}\n").into_bytes()),
                )],
            })
            .collect();

        let base = BTreeMap::new();
        let orderings = sampled_orderings(n_workspaces, 20);

        for ordering in &orderings {
            let reordered: Vec<WorkspaceScenario> =
                ordering.iter().map(|&i| scenarios[i].clone()).collect();
            let patch_sets = to_patch_sets(&reordered);
            let partition = partition_by_path(&patch_sets);
            let result = resolve_partition(&partition, &base)
                .expect("resolve should not error");

            prop_assert!(result.is_clean());
            prop_assert_eq!(result.resolved.len(), n_workspaces);
        }
    }
}

// ---------------------------------------------------------------------------
// End-to-end determinism tests (real git repos in /tmp)
// ---------------------------------------------------------------------------

/// Run the full merge pipeline (partition → resolve → build) and return
/// the commit OID. This exercises the complete code path including git
/// object creation.
fn run_full_merge(
    root: &Path,
    epoch: &EpochId,
    scenarios: &[WorkspaceScenario],
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
) -> GitOid {
    let patch_sets = to_patch_sets(scenarios);
    let partition = partition_by_path(&patch_sets);
    let result = resolve_partition(&partition, base_contents).expect("resolve should not error");

    // Only build if we have resolved changes (skip conflicts for this test).
    let ws_ids: Vec<WorkspaceId> = scenarios.iter().map(|s| ws(&s.name)).collect();

    build_merge_commit(root, epoch, &ws_ids, &result.resolved, None).expect("build should succeed")
}

/// End-to-end determinism: 2 workspaces with disjoint file additions.
/// Different workspace orderings must produce the same commit OID.
#[test]
fn e2e_determinism_2_workspaces_disjoint() {
    let (dir, epoch) = setup_git_repo();
    let root = dir.path();

    let scenarios = vec![
        WorkspaceScenario {
            name: "ws-00".to_string(),
            changes: vec![FileChange::new(
                PathBuf::from("alpha.rs"),
                ChangeKind::Added,
                Some(b"fn alpha() {}\n".to_vec()),
            )],
        },
        WorkspaceScenario {
            name: "ws-01".to_string(),
            changes: vec![FileChange::new(
                PathBuf::from("beta.rs"),
                ChangeKind::Added,
                Some(b"fn beta() {}\n".to_vec()),
            )],
        },
    ];

    let base = BTreeMap::new();

    let oid_forward = run_full_merge(root, &epoch, &scenarios, &base);
    let oid_reverse = run_full_merge(
        root,
        &epoch,
        &[scenarios[1].clone(), scenarios[0].clone()],
        &base,
    );

    assert_eq!(
        oid_forward, oid_reverse,
        "2-workspace disjoint merge must produce identical commit OID regardless of ordering"
    );
}

/// End-to-end determinism: 3 workspaces with non-overlapping edits to the same file.
#[test]
fn e2e_determinism_3_workspaces_shared_file() {
    let (dir, epoch, base_files) = setup_git_repo_with_base_files(1, 3);
    let root = dir.path();

    let (path, base_content) = &base_files[0];
    let base_lines: Vec<&str> = base_content.lines().collect();

    // Each workspace modifies a different region.
    let mut scenarios = Vec::new();
    for i in 0..3 {
        let mut lines: Vec<String> = base_lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let region_start = i * 5;
        lines[region_start] = format!("EDITED-BY-WS-{i}");
        let content = lines.join("\n") + "\n";

        scenarios.push(WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![FileChange::new(
                path.clone(),
                ChangeKind::Modified,
                Some(content.into_bytes()),
            )],
        });
    }

    let mut base = BTreeMap::new();
    base.insert(path.clone(), base_content.as_bytes().to_vec());

    // Test all 6 permutations of 3 workspaces.
    let perms = permutations(3);
    let reference_oid = run_full_merge(root, &epoch, &scenarios, &base);

    for (i, perm) in perms.iter().enumerate().skip(1) {
        let reordered: Vec<WorkspaceScenario> =
            perm.iter().map(|&j| scenarios[j].clone()).collect();
        let oid = run_full_merge(root, &epoch, &reordered, &base);
        assert_eq!(
            reference_oid, oid,
            "3-workspace permutation {i} ({perm:?}) produced different commit OID"
        );
    }
}

/// End-to-end determinism: 5 workspaces with a mix of adds, modifies, and deletes.
#[test]
fn e2e_determinism_5_workspaces_mixed() {
    let (dir, epoch, base_files) = setup_git_repo_with_base_files(2, 5);
    let root = dir.path();

    let (path0, base0) = &base_files[0];
    let (path1, _base1) = &base_files[1];
    let base0_lines: Vec<&str> = base0.lines().collect();

    let scenarios = vec![
        // ws-00: adds a new file
        WorkspaceScenario {
            name: "ws-00".to_string(),
            changes: vec![FileChange::new(
                PathBuf::from("src/new.rs"),
                ChangeKind::Added,
                Some(b"pub fn new_func() {}\n".to_vec()),
            )],
        },
        // ws-01: modifies region 0 of file-00
        WorkspaceScenario {
            name: "ws-01".to_string(),
            changes: vec![FileChange::new(
                path0.clone(),
                ChangeKind::Modified,
                Some({
                    let mut lines: Vec<String> = base0_lines
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect();
                    lines[0] = "MODIFIED-BY-WS-01".to_string();
                    (lines.join("\n") + "\n").into_bytes()
                }),
            )],
        },
        // ws-02: modifies region 2 of file-00
        WorkspaceScenario {
            name: "ws-02".to_string(),
            changes: vec![FileChange::new(
                path0.clone(),
                ChangeKind::Modified,
                Some({
                    let mut lines: Vec<String> = base0_lines
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect();
                    lines[10] = "MODIFIED-BY-WS-02".to_string();
                    (lines.join("\n") + "\n").into_bytes()
                }),
            )],
        },
        // ws-03: deletes file-01
        WorkspaceScenario {
            name: "ws-03".to_string(),
            changes: vec![FileChange::new(path1.clone(), ChangeKind::Deleted, None)],
        },
        // ws-04: adds another new file
        WorkspaceScenario {
            name: "ws-04".to_string(),
            changes: vec![FileChange::new(
                PathBuf::from("docs/guide.md"),
                ChangeKind::Added,
                Some(b"# Guide\nSome documentation.\n".to_vec()),
            )],
        },
    ];

    let mut base = BTreeMap::new();
    for (p, c) in &base_files {
        base.insert(p.clone(), c.as_bytes().to_vec());
    }

    // Test all 120 permutations of 5 workspaces.
    let perms = permutations(5);
    let reference_oid = run_full_merge(root, &epoch, &scenarios, &base);

    for (i, perm) in perms.iter().enumerate().skip(1) {
        let reordered: Vec<WorkspaceScenario> =
            perm.iter().map(|&j| scenarios[j].clone()).collect();
        let oid = run_full_merge(root, &epoch, &reordered, &base);
        assert_eq!(
            reference_oid, oid,
            "5-workspace permutation {i} ({perm:?}) produced different commit OID"
        );
    }
}

/// End-to-end determinism: 10 workspaces each editing a different region of
/// the same file. Uses sampled orderings (50 of 10! = 3,628,800 total).
#[test]
fn e2e_determinism_10_workspaces_same_file() {
    let (dir, epoch, base_files) = setup_git_repo_with_base_files(1, 10);
    let root = dir.path();

    let (path, base_content) = &base_files[0];
    let base_lines: Vec<&str> = base_content.lines().collect();

    // Each of 10 workspaces modifies a different region.
    let mut scenarios = Vec::new();
    for i in 0..10 {
        let mut lines: Vec<String> = base_lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let region_start = i * 5;
        lines[region_start] = format!("EDITED-BY-WS-{i:02}");
        let content = lines.join("\n") + "\n";

        scenarios.push(WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![FileChange::new(
                path.clone(),
                ChangeKind::Modified,
                Some(content.into_bytes()),
            )],
        });
    }

    let mut base = BTreeMap::new();
    base.insert(path.clone(), base_content.as_bytes().to_vec());

    // Sample 50 orderings of 10 workspaces.
    let orderings = sampled_orderings(10, 50);
    let reference_oid = run_full_merge(root, &epoch, &scenarios, &base);

    // Verify the merged result contains all 10 edits.
    {
        let patch_sets = to_patch_sets(&scenarios);
        let partition = partition_by_path(&patch_sets);
        let result = resolve_partition(&partition, &base).expect("resolve should succeed");
        assert!(
            result.is_clean(),
            "10 non-overlapping edits should merge cleanly"
        );
        match &result.resolved[0] {
            ResolvedChange::Upsert { content, .. } => {
                let text = String::from_utf8_lossy(content);
                for i in 0..10 {
                    assert!(
                        text.contains(&format!("EDITED-BY-WS-{i:02}")),
                        "Missing edit from ws-{i:02} in merged result"
                    );
                }
            }
            _ => panic!("Expected upsert for merged file"),
        }
    }

    for (i, ordering) in orderings.iter().enumerate().skip(1) {
        let reordered: Vec<WorkspaceScenario> =
            ordering.iter().map(|&j| scenarios[j].clone()).collect();
        let oid = run_full_merge(root, &epoch, &reordered, &base);
        assert_eq!(
            reference_oid, oid,
            "10-workspace ordering {i} ({ordering:?}) produced different commit OID.\n\
             Reference OID: {reference_oid}\n\
             Got OID: {oid}"
        );
    }
}

/// End-to-end determinism: 10 workspaces with disjoint file changes.
/// Each workspace adds a unique file — no shared paths.
#[test]
fn e2e_determinism_10_workspaces_disjoint() {
    let (dir, epoch) = setup_git_repo();
    let root = dir.path();

    let scenarios: Vec<WorkspaceScenario> = (0..10)
        .map(|i| WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![FileChange::new(
                PathBuf::from(format!("src/module-{i:02}.rs")),
                ChangeKind::Added,
                Some(format!("pub mod module_{i:02} {{\n    pub fn run() {{}}\n}}\n").into_bytes()),
            )],
        })
        .collect();

    let base = BTreeMap::new();
    let orderings = sampled_orderings(10, 50);
    let reference_oid = run_full_merge(root, &epoch, &scenarios, &base);

    for (i, ordering) in orderings.iter().enumerate().skip(1) {
        let reordered: Vec<WorkspaceScenario> =
            ordering.iter().map(|&j| scenarios[j].clone()).collect();
        let oid = run_full_merge(root, &epoch, &reordered, &base);
        assert_eq!(
            reference_oid, oid,
            "10-workspace disjoint ordering {i} produced different commit OID"
        );
    }
}

/// End-to-end determinism: verify that identical content from multiple
/// workspaces produces the same tree as a single workspace with that content.
#[test]
fn e2e_identical_changes_collapse() {
    let (dir, epoch) = setup_git_repo();
    let root = dir.path();

    let content = b"pub fn shared() { println!(\"hello\"); }\n".to_vec();

    // 5 workspaces all adding the same file with the same content.
    let scenarios: Vec<WorkspaceScenario> = (0..5)
        .map(|i| WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![FileChange::new(
                PathBuf::from("shared.rs"),
                ChangeKind::Added,
                Some(content.clone()),
            )],
        })
        .collect();

    let base = BTreeMap::new();

    // All orderings should produce the same tree.
    let perms = permutations(5);
    let reference_oid = run_full_merge(root, &epoch, &scenarios, &base);
    let reference_tree = git_tree_oid(root, reference_oid.as_str());

    for (i, perm) in perms.iter().enumerate().skip(1) {
        let reordered: Vec<WorkspaceScenario> =
            perm.iter().map(|&j| scenarios[j].clone()).collect();
        let oid = run_full_merge(root, &epoch, &reordered, &base);
        let tree = git_tree_oid(root, oid.as_str());
        assert_eq!(
            reference_tree, tree,
            "Identical-content permutation {i} produced different tree OID"
        );
    }
}

/// End-to-end determinism: conflicts are reported identically regardless of ordering.
/// When workspaces conflict, the conflict set must be the same for all orderings.
#[test]
fn e2e_conflicts_deterministic_across_orderings() {
    let (dir, _epoch, base_files) = setup_git_repo_with_base_files(1, 1);
    let root = dir.path();
    let (path, base_content) = &base_files[0];
    let _ = root; // not used for build (conflicts skip build)

    // 3 workspaces: ws-00 modifies, ws-01 deletes, ws-02 modifies differently.
    // This produces modify/delete conflict for ws-00 vs ws-01.
    let scenarios = [
        WorkspaceScenario {
            name: "ws-00".to_string(),
            changes: vec![FileChange::new(
                path.clone(),
                ChangeKind::Modified,
                Some(b"completely new content A\n".to_vec()),
            )],
        },
        WorkspaceScenario {
            name: "ws-01".to_string(),
            changes: vec![FileChange::new(path.clone(), ChangeKind::Deleted, None)],
        },
        WorkspaceScenario {
            name: "ws-02".to_string(),
            changes: vec![FileChange::new(
                path.clone(),
                ChangeKind::Modified,
                Some(b"completely new content B\n".to_vec()),
            )],
        },
    ];

    let mut base = BTreeMap::new();
    base.insert(path.clone(), base_content.as_bytes().to_vec());

    let perms = permutations(3);
    let mut results: Vec<ResolveResult> = Vec::new();

    for perm in &perms {
        let reordered: Vec<WorkspaceScenario> =
            perm.iter().map(|&j| scenarios[j].clone()).collect();
        let patch_sets = to_patch_sets(&reordered);
        let partition = partition_by_path(&patch_sets);
        let result = resolve_partition(&partition, &base).expect("resolve should succeed");
        results.push(result);
    }

    // All results must have the same conflict structure.
    let first = &results[0];
    assert!(!first.is_clean(), "Should have conflicts (modify/delete)");
    for (i, result) in results.iter().enumerate().skip(1) {
        assert!(
            results_equal(first, result),
            "Conflict result for permutation {i} ({:?}) differs from permutation 0.\n\
             Perm 0: {} resolved, {} conflicts\n\
             Perm {i}: {} resolved, {} conflicts",
            perms[i],
            first.resolved.len(),
            first.conflicts.len(),
            result.resolved.len(),
            result.conflicts.len(),
        );
    }
}

// ---------------------------------------------------------------------------
// Focused deterministic tests (non-proptest, specific edge cases)
// ---------------------------------------------------------------------------

/// Test with 10 workspaces all modifying different regions of the same file.
/// Verifies the K-way sequential diff3 is deterministic across orderings.
#[test]
fn k10_non_overlapping_edits_deterministic() {
    // Build a base file with 10 distinct regions separated by spacer lines.
    let mut base_lines: Vec<String> = Vec::new();
    for i in 0..10 {
        base_lines.push(format!("region-{i}"));
        // 4 spacer lines between regions for diff3 context separation
        for _ in 0..4 {
            base_lines.push("-".to_string());
        }
    }
    let base_text = base_lines.join("\n") + "\n";

    // Each workspace modifies exactly one region.
    let mut scenarios: Vec<WorkspaceScenario> = Vec::new();
    for i in 0..10 {
        let mut modified_lines = base_lines.clone();
        let idx = i * 5; // each region starts at i*5
        modified_lines[idx] = format!("MODIFIED-{i}");
        let modified_text = modified_lines.join("\n") + "\n";

        scenarios.push(WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![FileChange::new(
                PathBuf::from("big.txt"),
                ChangeKind::Modified,
                Some(modified_text.into_bytes()),
            )],
        });
    }

    let mut base = BTreeMap::new();
    base.insert(PathBuf::from("big.txt"), base_text.into_bytes());

    // Get result for original order.
    let patch_sets = to_patch_sets(&scenarios);
    let partition = partition_by_path(&patch_sets);
    let reference = resolve_partition(&partition, &base).expect("resolve should succeed");
    assert!(
        reference.is_clean(),
        "K=10 non-overlapping should merge cleanly"
    );

    // Check result content: all 10 regions should be modified.
    match &reference.resolved[0] {
        ResolvedChange::Upsert { content, .. } => {
            let text = String::from_utf8_lossy(content);
            for i in 0..10 {
                assert!(
                    text.contains(&format!("MODIFIED-{i}")),
                    "Missing MODIFIED-{i} in merged result"
                );
            }
        }
        _ => panic!("Expected upsert"),
    }

    // Verify a sample of reversed and shuffled orderings produce the same result.
    let orderings: Vec<Vec<usize>> = vec![
        (0..10).rev().collect(),            // reversed
        vec![5, 3, 8, 1, 9, 0, 7, 2, 6, 4], // shuffled
        vec![9, 0, 8, 1, 7, 2, 6, 3, 5, 4], // another shuffle
        vec![0, 2, 4, 6, 8, 1, 3, 5, 7, 9], // evens first
    ];

    for (idx, order) in orderings.iter().enumerate() {
        let reordered: Vec<WorkspaceScenario> =
            order.iter().map(|&i| scenarios[i].clone()).collect();
        let ps = to_patch_sets(&reordered);
        let part = partition_by_path(&ps);
        let result = resolve_partition(&part, &base).expect("resolve should succeed");
        assert!(
            results_equal(&reference, &result),
            "Ordering {idx} produced different result"
        );
    }
}

/// Empty patch sets should be handled gracefully.
#[test]
fn empty_patch_sets_produce_empty_result() {
    let scenarios: Vec<WorkspaceScenario> = (0..3)
        .map(|i| WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![],
        })
        .collect();

    let base = BTreeMap::new();
    let patch_sets = to_patch_sets(&scenarios);
    let partition = partition_by_path(&patch_sets);

    assert_eq!(partition.unique_count(), 0);
    assert_eq!(partition.shared_count(), 0);

    let result = resolve_partition(&partition, &base).expect("resolve should succeed");
    assert!(result.is_clean());
    assert_eq!(result.resolved.len(), 0);
}

/// Single workspace always produces unique paths only.
#[test]
fn single_workspace_all_unique() {
    let scenarios = vec![WorkspaceScenario {
        name: "ws-00".to_string(),
        changes: vec![
            FileChange::new(
                PathBuf::from("a.rs"),
                ChangeKind::Added,
                Some(b"a\n".to_vec()),
            ),
            FileChange::new(
                PathBuf::from("b.rs"),
                ChangeKind::Modified,
                Some(b"b\n".to_vec()),
            ),
            FileChange::new(PathBuf::from("c.rs"), ChangeKind::Deleted, None),
        ],
    }];

    let mut base = BTreeMap::new();
    base.insert(PathBuf::from("b.rs"), b"old-b\n".to_vec());
    base.insert(PathBuf::from("c.rs"), b"old-c\n".to_vec());

    let patch_sets = to_patch_sets(&scenarios);
    let partition = partition_by_path(&patch_sets);

    assert_eq!(partition.unique_count(), 3);
    assert_eq!(partition.shared_count(), 0);

    let result = resolve_partition(&partition, &base).expect("resolve should succeed");
    assert!(result.is_clean());
    assert_eq!(result.resolved.len(), 3);
}

/// Add/add with identical content across N workspaces should resolve cleanly.
#[test]
fn add_add_identical_resolves() {
    let content = b"fn hello() {}\n".to_vec();
    let scenarios: Vec<WorkspaceScenario> = (0..4)
        .map(|i| WorkspaceScenario {
            name: format!("ws-{i:02}"),
            changes: vec![FileChange::new(
                PathBuf::from("new.rs"),
                ChangeKind::Added,
                Some(content.clone()),
            )],
        })
        .collect();

    let base = BTreeMap::new();
    let patch_sets = to_patch_sets(&scenarios);
    let partition = partition_by_path(&patch_sets);
    let result = resolve_partition(&partition, &base).expect("resolve should succeed");

    assert!(result.is_clean(), "Identical add/add should resolve");
    assert_eq!(result.resolved.len(), 1);
    match &result.resolved[0] {
        ResolvedChange::Upsert {
            content: resolved, ..
        } => assert_eq!(resolved, &content),
        _ => panic!("Expected upsert"),
    }
}

/// Mixed scenario: some disjoint, some shared identical, some conflicting.
/// The total paths in resolved+conflicts must equal total unique input paths.
#[test]
fn mixed_scenario_path_accounting() {
    let scenarios = vec![
        WorkspaceScenario {
            name: "ws-00".to_string(),
            changes: vec![
                FileChange::new(
                    PathBuf::from("only-a.rs"),
                    ChangeKind::Added,
                    Some(b"a\n".to_vec()),
                ),
                FileChange::new(
                    PathBuf::from("shared.rs"),
                    ChangeKind::Modified,
                    Some(b"same\n".to_vec()),
                ),
                FileChange::new(
                    PathBuf::from("conflict.rs"),
                    ChangeKind::Modified,
                    Some(b"version-a\n".to_vec()),
                ),
            ],
        },
        WorkspaceScenario {
            name: "ws-01".to_string(),
            changes: vec![
                FileChange::new(
                    PathBuf::from("only-b.rs"),
                    ChangeKind::Added,
                    Some(b"b\n".to_vec()),
                ),
                FileChange::new(
                    PathBuf::from("shared.rs"),
                    ChangeKind::Modified,
                    Some(b"same\n".to_vec()),
                ),
                FileChange::new(PathBuf::from("conflict.rs"), ChangeKind::Deleted, None),
            ],
        },
    ];

    let mut base = BTreeMap::new();
    base.insert(PathBuf::from("shared.rs"), b"old\n".to_vec());
    base.insert(PathBuf::from("conflict.rs"), b"old\n".to_vec());

    let patch_sets = to_patch_sets(&scenarios);
    let partition = partition_by_path(&patch_sets);
    let result = resolve_partition(&partition, &base).expect("resolve should succeed");

    // 4 unique paths total: only-a, only-b, shared, conflict
    let total_output = result.resolved.len() + result.conflicts.len();
    assert_eq!(total_output, 4, "All 4 paths must appear in output");

    // shared.rs should resolve (identical content)
    assert!(
        result
            .resolved
            .iter()
            .any(|r| r.path() == &PathBuf::from("shared.rs")),
        "shared.rs should be resolved"
    );

    // conflict.rs should conflict (modify/delete)
    assert!(
        result
            .conflicts
            .iter()
            .any(|c| c.path == PathBuf::from("conflict.rs")),
        "conflict.rs should be a conflict"
    );
}
