# Assurance Test Matrix (G1-G6)

Canonical doc: `notes/assurance-plan.md`.

Status: breakdown-ready matrix
Purpose: map each guarantee/invariant to concrete tests and CI gates

## 1) Test ID scheme

- `UT-*`: module unit tests
- `IT-*`: integration tests (CLI + filesystem + git)
- `DST-*`: deterministic simulation/failpoint traces
- `FM-*`: formal model checks (TLA+/Lean-backed claims)

## 2) Matrix

| Claim | Invariants | Minimum tests | Required CI lane |
|---|---|---|---|
| G1 committed no-loss | I-G1.1, I-G1.2 | `IT-G1-001`, `DST-G1-001` | `dst-fast`, nightly DST |
| G2 rewrite no-loss | I-G2.1, I-G2.2, I-G2.3 | `IT-G2-001`, `IT-G2-002`, `UT-G2-001`, `DST-G2-001` | `dst-fast`, integration |
| G3 post-COMMIT monotonicity | I-G3.1, I-G3.2 | `UT-G3-001`, `IT-G3-001`, `DST-G3-001` | `dst-fast`, unit |
| G4 destructive gate | I-G4.1, I-G4.2 | `IT-G4-001`, `UT-G4-001`, `DST-G4-001` | integration, nightly DST |
| G5 discoverable recovery | I-G5.1, I-G5.2 | `IT-G5-001`, `IT-G5-002` | integration |
| G6 searchable recovery | I-G6.1, I-G6.2, I-G6.3 | `UT-G6-001`, `UT-G6-002`, `IT-G6-001`, `IT-G6-002` | unit, integration |

## 3) Test catalog (initial backlog)

### G1

- `IT-G1-001`: merge + cleanup preserves pre-committed reachability via durable/ref recovery paths.
- `DST-G1-001`: random interleavings with crash at commit/rewrite boundaries preserve I-G1.1.

### G2

- `UT-G2-001`: rewrite helper refuses destructive action without capture or no-work proof.
- `IT-G2-001`: dirty default (staged+unstaged+untracked) survives post-COMMIT rewrite.
- `IT-G2-002`: replay failure rolls back to snapshot; emitted recovery ref/artifact valid.
- `DST-G2-001`: failpoint sweep across capture/reset/replay enforces I-G2.1/2/3.

### G3

- `UT-G3-001`: partial commit recovery finalizes branch ref when epoch ref already advanced.
- `IT-G3-001`: COMMIT success followed by cleanup failure reports warning, not false merge failure.
- `DST-G3-001`: crash at each COMMIT step satisfies monotonicity and recoverability.

### G4

- `UT-G4-001`: destroy path returns refusal when status/capture preconditions fail.
- `IT-G4-001`: post-merge destroy does not delete workspace on capture/status failure.
- `DST-G4-001`: injected capture/status errors never allow destructive fallback.

### G5

- `IT-G5-001`: recovery-producing failures print ref+oid+artifact+command fields.
- `IT-G5-002`: emitted recovery command succeeds and restores expected bytes.

### G6

- `UT-G6-001`: recovery-ref search parser/validator enforces prefix and shape.
- `UT-G6-002`: snippet builder returns correct context boundaries and match marker.
- `IT-G6-001`: `maw ws recover --search` finds known strings in tracked and untracked snapshot files.
- `IT-G6-002`: `--ref ... --show` returns exact bytes for file from hit provenance.

## 4) Formal checks (planning)

- `FM-TLA-001`: commit protocol safety (no invalid ref states under crash/recover transitions).
- `FM-TLA-002`: no-loss protocol-level invariant under bounded model parameters.
- `FM-LEAN-001`: merge determinism under workspace permutation.
- `FM-LEAN-002`: non-conflicting side edit embedding / explicit conflict monotonicity.

## 5) CI wiring target

- PR required: `unit`, `integration-critical`, `dst-fast`
- Nightly required: `dst-nightly`, `incident-replay`, `contract-drift`
- Pre-release required: `formal-check` + zero open P0/P1 assurance failures

## 6) Ticket breakdown order (recommended)

1. G2/G4 hardening tests (`IT-G2-001`, `IT-G4-001`)
2. G5/G6 UX tests (`IT-G5-001`, `IT-G6-001`)
3. G1/G3 crash-path DST (`DST-G1-001`, `DST-G3-001`)
4. Formal stubs (`FM-TLA-001` and theorem skeletons)
