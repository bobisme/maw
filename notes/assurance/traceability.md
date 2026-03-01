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
| **classify_shared_path (decision tree, 13 harnesses)** | | |
| `totality_2_entries` | No panics for any 2-entry kind/boolean combination. Exercises `classify_shared_path`. | `kani-check` |
| `totality_3_entries` | No panics for any 3-entry kind/boolean combination. Exercises `classify_shared_path`. | `kani-check` |
| `no_silent_drops_2_entries` | Every result is ResolvedDelete, ResolvedIdentical, Conflict*, or NeedsDiff3. | `kani-check` |
| `commutativity_2_entries` | Swapping 2 entries produces the same classification. | `kani-check` |
| `commutativity_3_entries` | All 6 permutations of 3 entries produce the same classification. | `kani-check` |
| `idempotence_identical_non_deletes` | Identical non-delete inputs resolve to ResolvedIdentical. | `kani-check` |
| `idempotence_all_deletes` | All-delete inputs resolve to ResolvedDelete. | `kani-check` |
| `modify_delete_always_conflicts` | Mixed delete/non-delete always produces ConflictModifyDelete. | `kani-check` |
| `add_add_different_always_conflicts` | Add/add with different content and no base produces ConflictAddAddDifferent. | `kani-check` |
| `missing_content_always_conflicts` | Missing content on non-delete entries produces ConflictMissingContent. | `kani-check` |
| `needs_diff3_conditions` | Different content with base produces NeedsDiff3. | `kani-check` |
| `no_base_mixed_kinds_is_missing_base` | Different content, no base, not all adds produces ConflictMissingBase. | `kani-check` |
| `exhaustive_2_entry_decision_table` | Structural consistency of all 72 input combinations (9 kind pairs × 8 boolean combos). | `kani-check` |
| **resolve_entries (full pipeline with k-way diff3 fold, 11 harnesses)** | | |
| `re_totality_2_entries` | No panics for any 2-entry input with symbolic content. Exercises `resolve_entries<u8>`. | `kani-check` |
| `re_totality_3_entries` | No panics for any 3-entry input with symbolic content. Exercises `resolve_entries<u8>`. | `kani-check` |
| `re_outcome_consistency_2_entries` | Every result is Delete, Upsert, or Conflict with valid structural invariants. | `kani-check` |
| `re_commutativity_2_entries` | Swapping 2 entries (kinds + content) produces the same MergeOutcome. | `kani-check` |
| `re_commutativity_3_entries` | All 6 permutations of 3 entries produce the same MergeOutcome (bounded content 0..4). | `kani-check` |
| `re_idempotence_identical_content` | Identical non-delete content resolves to Upsert with that content. | `kani-check` |
| `re_idempotence_all_deletes` | All-delete entries resolve to Delete. | `kani-check` |
| `re_conflict_monotonicity` | Pre-diff3 conflicts from classify_shared_path remain conflicts through resolve_entries. | `kani-check` |
| `re_diff3_one_side_changed` | When one side matches base, resolve picks the changed side (clean merge). | `kani-check` |
| `re_diff3_one_of_three_changed` | When 2 of 3 sides match base, resolve picks the changed value. | `kani-check` |
| `re_diff3_both_sides_changed_conflicts` | Both sides changed differently from base produces Diff3Conflict. | `kani-check` |

---

## Table 3: Guarantees -> Invariants -> Tests -> CI

Guarantees are defined in `notes/assurance-plan.md` section 4. Invariant
predicates are specified in `notes/assurance/invariants.md`. Oracle functions
are implemented in `src/assurance/oracle.rs` and exercised by
`tests/dst_harness.rs` when run with `--features assurance`.

| Guarantee | Invariant ID | Oracle Function | DST Test | CI Gate |
|---|---|---|---|---|
| G1: Committed no-loss | I-G1.1 (durable reachability) | `check_g1_reachability(pre, post)` | `tests/dst_harness.rs:dst_g1_random_crash_preserves_committed_data` | `dst-fast`, `dst-nightly` |
| G1: Committed no-loss | I-G1.2 (rewrite pin-before-risk) | `check_g1_reachability(pre, post)` | `tests/dst_harness.rs:dst_g1_random_crash_preserves_committed_data` | `dst-fast`, `dst-nightly` |
| G2: Rewrite no-loss | I-G2.1 (capture-or-proof gate) | `check_g2_rewrite_preservation(pre, post)` | `tests/dst_harness.rs:dst_g2_rewrite_path_preserves_workspace_data` | `dst-fast` |
| G2: Rewrite no-loss | I-G2.2 (replay/rollback safety) | `check_g2_rewrite_preservation(pre, post)` | `tests/dst_harness.rs:dst_g2_rewrite_path_preserves_workspace_data` | `dst-fast` |
| G2: Rewrite no-loss | I-G2.3 (untracked preservation) | `check_g2_rewrite_preservation(pre, post)` | `tests/dst_harness.rs:dst_g2_rewrite_path_preserves_workspace_data` | `dst-fast` |
| G3: Post-COMMIT monotonicity | I-G3.1 (commit success monotonic) | `check_g3_commit_monotonicity(pre, post)` | `tests/dst_harness.rs:dst_g3_crash_at_commit_satisfies_monotonicity`; `tests/crash_recovery.rs:crash_in_commit_state_file_persists_for_ref_check` | `dst-fast`, `dst-nightly`, `formal-check` |
| G3: Post-COMMIT monotonicity | I-G3.2 (partial commit recoverable) | `check_g3_commit_monotonicity(pre, post)` | `tests/dst_harness.rs:dst_g3_crash_at_commit_satisfies_monotonicity`; `src/merge/commit.rs` inline tests (`recovery_finalizes_when_only_epoch_moved`, `recovery_reports_already_committed_when_both_refs_new`, `recovery_reports_not_committed_when_both_refs_old`) | `dst-fast`, `dst-nightly`, `formal-check` |
| G4: Destructive gate | I-G4.1 (destroy refuses on unknown safety) | `check_g4_destructive_gate(pre, post)` | `tests/dst_harness.rs:dst_g4_destroy_requires_successful_capture` | `dst-fast` |
| G4: Destructive gate | I-G4.2 (no best-effort destructive fallback) | `check_g4_destructive_gate(pre, post)` | `tests/dst_harness.rs:dst_g4_destroy_requires_successful_capture` | `dst-fast` |
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
| `src/merge/partition.rs` | `partition_by_path` | `Build` (sub-step) | -- | -- |
| `src/merge/resolve.rs` | `classify_shared_path`, `resolve_entries` | `Build` (sub-step) | 13 `classify_shared_path` harnesses + 11 `resolve_entries<u8>` harnesses (see Table 2) | -- |
| `src/merge/types.rs` | `PatchSet::new`, `ChangeKind` | -- | (via `classify_shared_path` and `resolve_entries` harnesses) | -- |
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
| Stateright model (`src/assurance/model.rs`) | 10/10 actions | 10/10 | Model + ignored integration checks are in-tree (`tests/formal_model.rs`) |
| Kani proof harnesses (`src/merge/kani_proofs.rs`) | 24/24 | -- | 13 `classify_shared_path` (decision tree) + 11 `resolve_entries<u8>` (full pipeline with k-way diff3 fold); run with `cargo kani --no-default-features` |
| Oracle functions (`check_g1..check_g6`) | 6/6 | 6/6 | Implemented in `src/assurance/oracle.rs`; used by DST harness in assurance mode |
| DST scenarios (named `DST-G*-001`) | 4/4 | 4/4 | Implemented in `tests/dst_harness.rs` (`dst_g1`, `dst_g2`, `dst_g3`, `dst_g4`) |
| CI gates | 5/5 recipes | 5/5 | Implemented as `just` gates; repository CI workflow wiring may still be external |
| Failpoint framework | 0/1 | 1/1 | Phase 2 prerequisite; 30 failpoint IDs cataloged in `notes/assurance/failpoints.md` |
