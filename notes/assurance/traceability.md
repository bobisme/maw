# Assurance Traceability Map

Canonical doc: `notes/assurance-plan.md`.

Status: Phase 4 planning artifact
Purpose: link every formal artifact to its implementation counterpart

This document maps Stateright model actions, Kani proof harnesses, and G1-G6
guarantees to source files, DST scenarios, and CI gates. It is the single
reference for answering "where is this property checked, and what CI gate
enforces it?"

File paths are relative to the repository source root (i.e., `ws/default/`
in the v2 bare repo layout).

---

## Table 1: Stateright Actions -> Source -> DST -> CI

Actions are defined in `notes/assurance-plan.md` section 11 (Stateright model
specification). The `src/assurance/model.rs` module is a Phase 4 deliverable
that will encode these actions using actual maw types from `src/merge_state.rs`.

| Stateright Action | Source File:Function | Invariant Check | DST Scenario | CI Gate |
|---|---|---|---|---|
| `Prepare` | `src/merge/prepare.rs:run_prepare_phase` | I-G1.1 (frozen inputs preserve committed reachability) | `tests/crash_recovery.rs:crash_in_prepare_aborts_and_preserves_workspace_files` | `formal-check`, `dst-fast` |
| `Build` | `src/merge/build_phase.rs:run_build_phase` | I-G1.1 (candidate commit preserves epoch content) | `tests/crash_recovery.rs:crash_in_build_aborts_and_preserves_workspace_files`, `tests/crash_recovery.rs:crash_in_build_no_data_loss_with_candidate_oid` | `formal-check`, `dst-fast` |
| `ValidatePass` | `src/merge/validate.rs:run_validate_phase` | I-G1.1 (validation does not mutate refs) | `tests/crash_recovery.rs:crash_in_validate_state_file_persists_for_retry` | `formal-check`, `dst-fast` |
| `ValidateFail` | `src/merge/validate.rs:run_validate_phase` (returns `Blocked` / `Quarantine`) | I-G4.1 (failed validation blocks commit) | `tests/crash_recovery.rs:crash_in_validate_state_file_persists_for_retry` | `formal-check`, `dst-fast` |
| `CommitEpoch` | `src/merge/commit.rs:run_commit_phase` (line 139: `refs::advance_epoch`) | I-G3.1, I-G3.2 (partial commit recoverable) | `tests/crash_recovery.rs:crash_in_commit_state_file_persists_for_ref_check`, `tests/concurrent_safety.rs:concurrent_agents_100_scenarios_no_corruption_or_data_loss` | `formal-check`, `dst-fast` |
| `CommitBranch` | `src/merge/commit.rs:run_commit_phase` (line 145: `refs::write_ref_cas`) | I-G3.1, I-G3.2 (atomic two-ref commit) | `tests/crash_recovery.rs:crash_in_commit_state_file_persists_for_ref_check` | `formal-check`, `dst-fast` |
| `Cleanup` | `src/merge_state.rs:run_cleanup_phase`, `src/workspace/merge.rs:update_default_workspace` (line 2982) | I-G3.1 (cleanup failure does not undo commit), I-G2.1 (rewrite capture gate), I-G4.1 (destroy gate) | `tests/crash_recovery.rs:crash_in_cleanup_state_file_deleted_and_idempotent`, `tests/crash_recovery.rs:crash_in_cleanup_is_fully_idempotent` | `formal-check`, `dst-fast` |
| `Abort` | `src/merge_state.rs:recover_from_merge_state` (pre-commit abort path) | I-G1.1 (abort preserves all pre-state), I-G2.1 (workspace files preserved) | `tests/crash_recovery.rs:crash_in_prepare_aborts_and_preserves_workspace_files`, `tests/crash_recovery.rs:crash_in_build_aborts_and_preserves_workspace_files` | `formal-check`, `dst-fast` |
| `Crash` | Injected by DST harness (failpoint injection) | All I-G*.* (crash at any point must not violate safety) | `tests/crash_recovery.rs` (all 16 test functions), `tests/concurrent_safety.rs:concurrent_agents_100_scenarios_no_corruption_or_data_loss` | `dst-fast`, `dst-nightly` |
| `Recover` | `src/merge/commit.rs:recover_partial_commit`, `src/merge_state.rs:recover_from_merge_state` | I-G3.2 (partial commit finalized or reported), I-G1.1 (recovery preserves reachability) | `tests/crash_recovery.rs:recovery_idempotent_for_pre_commit_phases`, `tests/crash_recovery.rs:recovery_idempotent_for_post_prepare_phases` | `formal-check`, `dst-fast` |

---

## Table 2: Kani Proof Harnesses -> Source -> CI

Harnesses are defined in `src/merge/kani_proofs.rs`. Each is gated behind
`#[cfg(kani)]` and runs via `cargo kani`. They verify merge algebra properties
using bounded symbolic inputs.

| Kani Harness | Source Property | CI Gate |
|---|---|---|
| `permutation_determinism_2ws_1change` | Merge result is independent of workspace ordering (2 workspaces, 1 change each). Exercises `src/merge/partition.rs:partition_by_path` and `src/merge/resolve.rs:resolve_partition`. | `formal-check` |
| `permutation_determinism_3ws_1change` | Merge result is independent of workspace ordering (3 workspaces, 1 change each, all 6 permutations). Exercises `src/merge/partition.rs:partition_by_path` and `src/merge/resolve.rs:resolve_partition`. | `formal-check` |
| `idempotence_2ws_identical_changes` | Identical modifications from 2 workspaces resolve cleanly to the same content as a single workspace. Exercises hash-equality resolution in `src/merge/resolve.rs:resolve_partition`. | `formal-check` |
| `idempotence_3ws_identical_adds` | Identical adds from 3 workspaces produce a single clean upsert. Exercises add/add hash-equality in `src/merge/resolve.rs:resolve_partition`. | `formal-check` |
| `idempotence_delete_delete_resolves` | N workspaces (2-3) all deleting the same file resolve to a single clean delete. Exercises delete/delete resolution in `src/merge/resolve.rs:resolve_partition`. | `formal-check` |
| `conflict_monotonicity_no_silent_drops_2ws` | Every input path appears in either resolved or conflicts (no silent drops). Exercises `src/merge/resolve.rs:resolve_partition` output completeness. | `formal-check` |
| `conflict_monotonicity_no_path_duplication_2ws` | No path appears in both resolved and conflicts. Exercises `src/merge/resolve.rs:resolve_partition` output partitioning. | `formal-check` |
| `conflict_monotonicity_modify_delete_always_conflicts` | Modify/delete on the same path always produces a conflict with exactly 2 sides. Exercises `src/merge/resolve.rs:resolve_partition` conflict detection. | `formal-check` |
| `conflict_monotonicity_add_add_different_conflicts` | Add/add with different content always produces a conflict. Exercises `src/merge/resolve.rs:resolve_partition` divergence detection. | `formal-check` |
| `disjoint_changes_never_conflict` | Two workspaces with disjoint file paths never conflict. Exercises `src/merge/partition.rs:partition_by_path` unique/shared classification. | `formal-check` |
| `partition_paths_sorted` | Partition output paths (both unique and shared) are always lexicographically sorted. Exercises `src/merge/partition.rs:partition_by_path` ordering. | `formal-check` |
| `partition_path_accounting` | Unique + shared path counts equal the total distinct input paths. Exercises `src/merge/partition.rs:partition_by_path` accounting. | `formal-check` |
| `resolve_output_paths_sorted` | Resolved and conflict paths in output are lexicographically sorted. Exercises `src/merge/resolve.rs:resolve_partition` output ordering. | `formal-check` |
| `patch_set_sorts_by_path` | `PatchSet::new` always sorts changes by path regardless of input order. Exercises `src/merge/types.rs:PatchSet::new` construction invariant. | `formal-check` |
| `empty_patch_sets_produce_empty_result` | Empty patch sets (1-3 workspaces with no changes) produce an empty, clean result. Exercises `src/merge/resolve.rs:resolve_partition` base case. | `formal-check` |

---

## Table 3: Guarantees -> Invariants -> Tests -> CI

Guarantees are defined in `notes/assurance-plan.md` section 4. Invariant
predicates are specified in `notes/assurance/invariants.md`. Oracle functions
are specified in `notes/assurance/invariants.md` section 4 (not yet implemented
in code; Phase 2/4 deliverable).

| Guarantee | Invariant ID | Oracle Function | DST Test | CI Gate |
|---|---|---|---|---|
| G1: Committed no-loss | I-G1.1 (durable reachability) | `check_g1_reachability(pre, post)` | `DST-G1-001` (not yet implemented); existing: `tests/concurrent_safety.rs:concurrent_agents_100_scenarios_no_corruption_or_data_loss`, `tests/concurrent_safety.rs:high_load_five_agents_100_files_total_no_data_loss` | `dst-fast`, `dst-nightly` |
| G1: Committed no-loss | I-G1.2 (rewrite pin-before-risk) | `check_g1_reachability(pre, post)` | `DST-G1-001` (not yet implemented); existing: `tests/concurrent_safety.rs:concurrent_agents_100_scenarios_no_corruption_or_data_loss` | `dst-fast`, `dst-nightly` |
| G2: Rewrite no-loss | I-G2.1 (capture-or-proof gate) | `check_g2_rewrite_preservation(pre, post, workspace)` | `DST-G2-001` (not yet implemented) | `dst-fast`, `dst-nightly` |
| G2: Rewrite no-loss | I-G2.2 (replay/rollback safety) | `check_g2_rewrite_preservation(pre, post, workspace)` | `DST-G2-001` (not yet implemented) | `dst-fast`, `dst-nightly` |
| G2: Rewrite no-loss | I-G2.3 (untracked preservation) | `check_g2_rewrite_preservation(pre, post, workspace)` | `DST-G2-001` (not yet implemented) | `dst-fast`, `dst-nightly` |
| G3: Post-COMMIT monotonicity | I-G3.1 (commit success monotonic) | `check_g3_commit_monotonicity(pre, post)` | `DST-G3-001` (not yet implemented); existing: `tests/crash_recovery.rs:crash_in_commit_state_file_persists_for_ref_check`, `tests/crash_recovery.rs:crash_in_cleanup_state_file_deleted_and_idempotent` | `dst-fast`, `dst-nightly`, `formal-check` |
| G3: Post-COMMIT monotonicity | I-G3.2 (partial commit recoverable) | `check_g3_commit_monotonicity(pre, post)` | `DST-G3-001` (not yet implemented); existing: `src/merge/commit.rs` inline tests (`recovery_finalizes_when_only_epoch_moved`, `recovery_reports_already_committed_when_both_refs_new`, `recovery_reports_not_committed_when_both_refs_old`) | `dst-fast`, `dst-nightly`, `formal-check` |
| G4: Destructive gate | I-G4.1 (destroy refuses on unknown safety) | `check_g4_destructive_gate(pre, post, workspace)` | `DST-G4-001` (not yet implemented) | `dst-fast`, `dst-nightly` |
| G4: Destructive gate | I-G4.2 (no best-effort destructive fallback) | `check_g4_destructive_gate(pre, post, workspace)` | `DST-G4-001` (not yet implemented) | `dst-fast`, `dst-nightly` |
| G5: Discoverable recovery | I-G5.1 (recovery surface presence) | `check_g5_discoverability(output, post)` | N/A (integration tests only); existing: `tests/destroy_recover.rs` (11 tests) | `contract-drift` |
| G5: Discoverable recovery | I-G5.2 (executable next step) | `check_g5_discoverability(output, post)` | N/A (integration tests only); existing: `tests/destroy_recover.rs` (11 tests) | `contract-drift` |
| G6: Searchable recovery | I-G6.1 (search coverage) | `check_g6_searchability(repo_state, query_cases)` | N/A (unit + integration tests only); existing: `src/workspace/recover.rs` inline tests (10 tests) | `contract-drift` |
| G6: Searchable recovery | I-G6.2 (provenanced chunk output) | `check_g6_searchability(repo_state, query_cases)` | N/A (unit tests only); existing: `src/workspace/recover.rs` inline tests | `contract-drift` |
| G6: Searchable recovery | I-G6.3 (deterministic truncation/order) | `check_g6_searchability(repo_state, query_cases)` | N/A (unit tests only); existing: `src/workspace/recover.rs` inline tests | `contract-drift` |

---

## Cross-reference: Source File -> Formal Artifact

This section provides a reverse lookup: given a source file, which formal
artifacts (Stateright actions, Kani proofs, guarantees) exercise it.

| Source File | Function(s) | Stateright Actions | Kani Harnesses | Guarantees |
|---|---|---|---|---|
| `src/merge/prepare.rs` | `run_prepare_phase`, `run_prepare_phase_with_epoch` | `Prepare` | -- | G1 (I-G1.1) |
| `src/merge/build_phase.rs` | `run_build_phase`, `run_build_phase_with_inputs` | `Build` | -- | G1 (I-G1.1) |
| `src/merge/validate.rs` | `run_validate_phase`, `write_validation_artifact` | `ValidatePass`, `ValidateFail` | -- | G1 (I-G1.1), G4 (I-G4.1) |
| `src/merge/commit.rs` | `run_commit_phase`, `recover_partial_commit` | `CommitEpoch`, `CommitBranch`, `Recover` | -- | G3 (I-G3.1, I-G3.2) |
| `src/merge_state.rs` | `run_cleanup_phase`, `recover_from_merge_state` | `Cleanup`, `Abort`, `Recover` | -- | G3 (I-G3.1) |
| `src/workspace/merge.rs` | `merge` (line 1941), `update_default_workspace` (line 2982), `handle_post_merge_destroy` (line 3069) | `Cleanup` (via orchestration) | -- | G2 (I-G2.1), G4 (I-G4.1, I-G4.2) |
| `src/workspace/capture.rs` | `capture_before_destroy` | -- | -- | G1 (I-G1.2), G2 (I-G2.1, I-G2.3), G4 (I-G4.1) |
| `src/workspace/recover.rs` | `recover` (search/show/restore) | -- | -- | G5 (I-G5.1, I-G5.2), G6 (I-G6.1, I-G6.2, I-G6.3) |
| `src/merge/partition.rs` | `partition_by_path` | `Build` (sub-step) | `partition_paths_sorted`, `partition_path_accounting`, `disjoint_changes_never_conflict` | -- |
| `src/merge/resolve.rs` | `resolve_partition` | `Build` (sub-step) | `permutation_determinism_2ws_1change`, `permutation_determinism_3ws_1change`, `idempotence_2ws_identical_changes`, `idempotence_3ws_identical_adds`, `idempotence_delete_delete_resolves`, `conflict_monotonicity_no_silent_drops_2ws`, `conflict_monotonicity_no_path_duplication_2ws`, `conflict_monotonicity_modify_delete_always_conflicts`, `conflict_monotonicity_add_add_different_conflicts`, `resolve_output_paths_sorted`, `empty_patch_sets_produce_empty_result` | -- |
| `src/merge/types.rs` | `PatchSet::new` | -- | `patch_set_sorts_by_path` | -- |
| `src/merge/determinism_tests.rs` | 25+ proptest harnesses | -- | (Kani proofs upgrade these) | -- |
| `src/merge/pushout_tests.rs` | 1000+ proptest harnesses | -- | (Kani proofs upgrade these) | -- |
| `src/refs.rs` | `advance_epoch`, `write_ref_cas`, `read_ref` | `CommitEpoch`, `CommitBranch` | -- | G1 (I-G1.1), G3 (I-G3.1, I-G3.2) |

---

## CI Gate Summary

| CI Gate | Trigger | What It Checks | Formal Artifacts |
|---|---|---|---|
| `dst-fast` | Per-PR | 256 DST traces across crash/recovery boundaries | Stateright actions (Crash, Recover), oracle checks `check_g1..check_g4` |
| `dst-nightly` | Nightly | 10,000 DST traces, broader parameter space | All Stateright actions, all oracle checks |
| `formal-check` | Pre-release | Stateright model check (3 workspaces, 20 steps) | All Stateright safety/liveness properties |
| `contract-drift` | Nightly | Doc/code consistency (schema validation, invariant predicate alignment) | G5/G6 oracle checks, search schema validation |
| `incident-replay` | Nightly | Historical failure corpus replay | Regression coverage for past DST/production failures |

---

## Implementation Status

| Artifact Category | Implemented | Planned | Notes |
|---|---|---|---|
| Stateright model (`src/assurance/model.rs`) | 0/10 actions | 10/10 | Phase 4 deliverable; uses actual `MergePhase`/`MergeStateFile` types |
| Kani proof harnesses (`src/merge/kani_proofs.rs`) | 13/13 | -- | Pure algebra proofs on `classify_shared_path`; run with `cargo kani --no-default-features` |
| Oracle functions (`check_g1..check_g6`) | 0/6 | 6/6 | Phase 2 deliverable; specified in `notes/assurance/invariants.md` section 4 |
| DST scenarios (named `DST-G*-001`) | 0/4 | 4/4 | Phase 2-3 deliverable; `tests/concurrent_safety.rs` provides lightweight DST coverage |
| CI gates | 0/5 | 5/5 | All gates specified; none wired into CI yet |
| Failpoint framework | 0/1 | 1/1 | Phase 2 prerequisite; 30 failpoint IDs cataloged in `notes/assurance/failpoints.md` |
