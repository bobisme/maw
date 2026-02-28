# Recovery Contract

Canonical doc: `notes/assurance-plan.md`.

Status: normative discoverability and restoration contract

This document defines what "recoverable" means operationally in maw.

## 1) Recovery surfaces

Every maw-created recovery point MUST have a durable Git ref (the source of truth for bytes).

Artifacts are operation-specific but MUST include enough metadata to locate the
Git ref deterministically.

### Git surface (durable refs)

- `refs/manifold/recovery/<workspace>/<timestamp>`

The associated commit oid must be recorded in artifacts and exposed by CLI
output when relevant.

The pinned commit tree MUST be a byte-for-byte capture of the working copy at
the capture boundary (tracked + untracked non-ignored). This enables:

- exact byte recovery (via `git show <oid>:<path>`), and
- deterministic content search across "lost" work without restoring workspaces.

### Artifact surface (filesystem)

Rewrite artifacts:

- `.manifold/artifacts/rewrite/<workspace>/<timestamp>/`

Destroy artifacts:

- `.manifold/artifacts/ws/<workspace>/destroy/<timestamp>.json`
- `.manifold/artifacts/ws/<workspace>/destroy/latest.json`

(Other operations may add additional artifact locations, but they must be
documented and test-covered.)

Minimum required files for rewrite artifacts:

- `meta.json`
- `index.patch` (can be empty)
- `worktree.patch` (can be empty)
- `untracked.json`

Destroy/recover workflows MUST at least include the destroy-record locations
above. Rewrite recoverability must include the rewrite artifact directory above.

## 2) CLI discoverability requirements

When maw cannot safely complete a rewrite/destructive operation, output must:

1. clearly state whether operation was aborted, skipped, or rolled back;
2. clearly state whether merge COMMIT already succeeded (if applicable);
3. print snapshot ref and oid;
4. print artifact path (rewrite directory or destroy record);
5. print at least one executable recovery command.

The preferred command form is `maw ws recover --ref <recovery-ref> ...` because
it works for both destroy snapshots and rewrite captures.

## 3) Required recovery commands

At least one deterministic recovery path must be executable non-interactively,
for example:

- `maw ws recover <workspace>`
- `maw ws recover <workspace> --show <path>`
- `maw ws recover <workspace> --to <new-workspace>`
- `maw ws recover --ref <recovery-ref> --show <path>`
- `maw ws recover --ref <recovery-ref> --to <new-workspace>`
- `maw ws recover --search <pattern>`
- `maw ws recover <workspace> --search <pattern>`

If command suggestions include raw git commands, maw command equivalents should
also be included where available.

## 4) Testable discoverability criteria

The test harness must assert:

- recovery-producing failures include required fields from Section 2;
- suggested recovery commands execute successfully;
- restored files are byte-equivalent to preserved state for covered paths.
- `maw ws recover --search` finds known strings in recovery snapshots and returns
  file chunks with provenance (ref + path + line numbers).

## 5) Retention and garbage collection

Recovery refs and artifacts are part of the safety contract and must not be
garbage-collected until retention policy guarantees are met.

If pruning is introduced, it must be explicit, documented, and test-covered.

## 6) Incident response rule

On any suspected loss incident, responders should be able to locate recoverable
state with deterministic steps only from:

- `maw ws recover` output,
- contract-defined artifact directories,
- contract-defined recovery ref namespace.

Responders/agents should be able to locate unknown "lost" work via
`maw ws recover --search <pattern>` before choosing a restore target.
