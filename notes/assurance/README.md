# Assurance Docs Index

Canonical doc: `notes/assurance-plan.md`.

Status: working index for safety contract and verification work

This directory is the source of truth for maw safety claims around concurrent
workspace operations, destructive rewrites, and recovery discoverability.

Primary entrypoint for agents/reviewers:

- `notes/assurance-plan.md`

## Documents

- `notes/assurance/claims.md`
  - Contract definitions and global guarantees (G1-G6).
- `notes/assurance/working-copy.md`
  - Normative preserve/materialize/replay semantics for worktree rewrites.
- `notes/assurance/recovery-contract.md`
  - Required recovery surfaces, output requirements, and validation criteria.
- `notes/assurance/search.md`
  - Content search + chunk retrieval contract (agents).
- `notes/assurance/invariants.md`
  - Machine-checkable invariant definitions for G1-G6.
- `notes/assurance/test-matrix.md`
  - Claim -> test -> CI mapping and initial backlog IDs.
- `notes/assurance/search-schema-v1.md`
  - Stable JSON schema for `maw ws recover --search --format json`.
- `notes/assurance/failpoints.md`
  - Required failpoint IDs, injection modes, and coverage expectations.
- `notes/assurance/retention-and-security.md`
  - Retention baseline and searchable-recovery security policy.

## Code Mapping

- `src/workspace/merge.rs`
  - Main merge orchestration; post-COMMIT cleanup; default workspace update;
    post-merge `--destroy` behavior.
- `src/merge_state.rs`
  - Persistent merge state file and crash-recovery protocol state.
- `src/merge/prepare.rs`
  - PREPARE input freezing and initial merge-state persistence.
- `src/merge/build_phase.rs`
  - BUILD pipeline (collect/partition/resolve/build candidate).
- `src/merge/validate.rs`
  - VALIDATE execution and failure policy handling.
- `src/merge/commit.rs`
  - COMMIT CAS ref movement and partial-commit recovery semantics.
- `src/workspace/capture.rs`
  - Pre-destroy/rewrite capture and recovery ref pinning.
- `src/workspace/destroy_record.rs`
  - Destroy metadata artifacts consumed by recovery UX.
- `src/workspace/recover.rs` — destroyed-workspace recovery + recovery-ref search/show/restore.
  - User/agent recovery CLI surface and command discoverability.
- `src/workspace/sync.rs`
  - Workspace rewrite/update flows outside merge cleanup.
- `src/workspace/advance.rs`
  - Per-workspace advance logic and dirty-state preservation behavior.

## Test Mapping (Contract -> Evidence)

- G1 (no silent loss of committed work)
  - ref reachability assertions after merge, cleanup, and crash recovery.
- G2 (no silent loss on rewrites)
  - dirty rewrite tests: staged + unstaged + untracked preservation.
- G3 (post-COMMIT monotonicity)
  - COMMIT success remains success even if cleanup warns/fails.
- G4 (destructive operation gate)
  - destroy/rewrite must abort or skip when capture/status preconditions fail.
- G5 (discoverable recovery)
  - output includes required recovery fields; emitted commands execute.
- G6 (searchable recovery)
  - `maw ws recover --search` finds content with provenance and snippets.

Planned placement:

- deterministic simulation and failpoint scenarios: `tests/dst/`
- focused module tests: colocated `#[cfg(test)]` suites in affected modules
- integration coverage for CLI contract: `tests/` integration tests

## PR Checklist for Safety-Sensitive Changes

If a PR changes destructive/rewrite behavior, update all of:

1. `notes/assurance/claims.md` (if guarantee semantics change)
2. `notes/assurance/working-copy.md` (if rewrite mechanics change)
3. `notes/assurance/recovery-contract.md` (if recovery UX/surfaces change)
4. `notes/assurance/invariants.md` + `notes/assurance/test-matrix.md` (test mappings)
5. `notes/assurance/search-schema-v1.md` (if JSON shape/search semantics change)
6. tests proving G1-G6 still hold

Do not merge safety-sensitive behavior changes without matching contract and
test updates.
