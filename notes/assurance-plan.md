# maw Assurance Plan (Consolidated)

Date: 2026-02-27
Status: draft (single entrypoint)
Audience: maintainers, reviewers, agent implementers

This is the single starting document for maw assurance work.

If an agent reads only one file, read this one first.

If this plan conflicts with a subsidiary assurance note, this plan wins and the
subsidiary note must be updated.

Related details are still maintained under `notes/assurance/`.

## 1) Problem and objective

maw allows concurrent changes in multiple workspaces and merges them into one
mainline. The assurance objective is:

1. no silent work loss,
2. deterministic recovery when failures happen,
3. recovery discoverability that works for agents,
4. searchable recovery so "lost" content can be found without full restore.

## 2) Contract summary (G1-G6)

### Definitions (normative)

- user work: committed + uncommitted tracked + untracked non-ignored
- reachable: reachable from durable refs (`refs/**`) only
- lost: not present in resulting state and not reachable via recovery refs/artifacts
- recoverable: restorable via documented maw CLI + deterministic surfaces
- searchable: recoverable content can be queried by pattern with provenanced chunks

### Assumptions (explicit)

- git ref updates used by maw are atomic on supported platforms
- required fsync/rename semantics hold on supported filesystems
- `.manifold` is not mutated by external tools during critical sections

### Guarantees

- G1 committed no-loss: pre-op committed content remains durably reachable
- G2 rewrite no-loss: destructive rewrites require capture or proof of no user work
- G3 post-COMMIT monotonicity: cleanup failures do not undo/obscure successful COMMIT
- G4 destructive gate: no "best effort destroy anyway" paths
- G5 discoverable recovery: output contains actionable recovery commands + locations
- G6 searchable recovery: `maw ws recover --search` finds content in pinned snapshots

## 3) Normative rewrite behavior

For any operation that rewrites workspace content:

1. derive user deltas from explicit base (`base_epoch`; merge cleanup uses `epoch_before`)
2. if user work exists, create pinned recovery ref under
   `refs/manifold/recovery/<workspace>/<timestamp>`
3. record deterministic artifacts under `.manifold/artifacts/rewrite/...`
4. materialize target commit in clean worktree state
5. replay tracked deltas deterministically
6. replay/rehydrate untracked content per policy
7. on replay failure, rollback to captured snapshot (or safe abort before destruction)

Important nuance:

- after COMMIT advances branch refs, naive dirty detection can include non-user
  "old checkout" content; rewrite logic must anchor deltas to `epoch_before`
  rather than naive post-COMMIT status.

## 4) Recovery surfaces and CLI contract

### Required surfaces

- durable refs: `refs/manifold/recovery/<workspace>/<timestamp>`
- rewrite artifacts: `.manifold/artifacts/rewrite/<workspace>/<timestamp>/`
- destroy artifacts: `.manifold/artifacts/ws/<workspace>/destroy/*.json`

### Required output fields on recovery-producing failures

1. operation result (aborted/skipped/rolled back)
2. whether COMMIT already succeeded (if applicable)
3. snapshot ref + oid
4. artifact path
5. at least one executable recovery command

### Required command forms

- `maw ws recover`
- `maw ws recover <workspace>`
- `maw ws recover <workspace> --show <path>`
- `maw ws recover <workspace> --to <new-workspace>`
- `maw ws recover --ref <recovery-ref> --show <path>`
- `maw ws recover --ref <recovery-ref> --to <new-workspace>`
- `maw ws recover --search <pattern>`
- `maw ws recover <workspace> --search <pattern>`

Search options required for agent workflows:

- `--context`, `--max-hits`, `--regex`, `--ignore-case`, `--text`, `--format`

## 5) Invariants (implementation targets)

The implementation must enforce invariants mapped in `notes/assurance/invariants.md`:

- I-G1.1/I-G1.2 durable reachability and rewrite pin-before-risk
- I-G2.1/I-G2.2/I-G2.3 capture gate, replay safety, untracked preservation
- I-G3.1/I-G3.2 COMMIT monotonicity and partial-commit recoverability
- I-G4.1/I-G4.2 destroy refusal and no destructive fallback
- I-G5.1/I-G5.2 output surface completeness and executable next steps
- I-G6.1/I-G6.2/I-G6.3 searchable coverage, provenance, deterministic order/truncation

## 6) Test and CI requirements

Test mapping is defined in `notes/assurance/test-matrix.md`.

Minimum CI gates:

- PR: `unit`, `integration-critical`, `dst-fast`
- Nightly: `dst-nightly`, `incident-replay`, `contract-drift`
- Pre-release: `formal-check` and no open P0/P1 assurance failures

## 7) Failpoint and DST requirements

Failpoint catalog lives in `notes/assurance/failpoints.md`.

Required:

- deterministic `error` and `crash` injection modes
- restart/recovery execution after crash
- invariant checks after each trace transition
- deterministic shrinking and saved repro seeds

High-priority boundaries:

- COMMIT CAS transitions
- CLEANUP capture/reset/replay transitions
- DESTROY status/capture/delete transitions

## 8) Formal proof boundary

### Lean (pure semantics)

Use Lean for merge algebra properties (determinism, embedding, conflict laws).

### TLA+ (protocol/concurrency)

Use TLA+ for merge-state transitions, crash/restart protocol safety, and ref
movement invariants.

### DST (real implementation)

Use deterministic simulation and failpoints to validate Rust+Git+filesystem
behavior that is not practical to fully prove in Lean.

## 9) Search JSON contract

Machine output stability for `maw ws recover --search --format json` is
normatively defined in `notes/assurance/search-schema-v1.md`.

Any breaking field/type change requires a new versioned schema doc.

## 10) Retention and security policy

Baseline policy in `notes/assurance/retention-and-security.md`:

- no automatic pruning of recovery refs/artifacts unless explicit policy lands
- searchable snapshots may include sensitive content; treat as privileged surface
- audit search/show/restore actions with minimal sensitive output

## 11) Breakdown order (recommended)

1. land G2/G4 hardening + tests (highest loss-risk reduction)
2. land G5/G6 discoverability/search tests and schema checks
3. land COMMIT/CLEANUP/DESTROY failpoints + dst-fast gate
4. land nightly DST and incident replay corpus
5. land TLA+ protocol checks and Lean theorem skeletons

## 12) Maintainer checklist

For any PR touching destructive/rewrite/recovery/search behavior:

1. update this plan if semantics changed
2. update affected docs in `notes/assurance/`
3. update mapped tests in `notes/assurance/test-matrix.md`
4. ensure CI gates covering impacted claims pass

## 13) Supporting docs

- `notes/assurance/README.md`
- `notes/assurance/claims.md`
- `notes/assurance/working-copy.md`
- `notes/assurance/recovery-contract.md`
- `notes/assurance/search.md`
- `notes/assurance/invariants.md`
- `notes/assurance/test-matrix.md`
- `notes/assurance/search-schema-v1.md`
- `notes/assurance/failpoints.md`
- `notes/assurance/retention-and-security.md`
- `notes/assurance-near-proof-proposal.md`
