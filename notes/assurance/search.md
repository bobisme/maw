# Content search and chunk retrieval contract

Canonical doc: `notes/assurance-plan.md`.

This document defines the required behavior for searching "lost" work and
extracting bounded file chunks from recovery points.

## Scope

- Search operates over *pinned recovery snapshots*: commits referenced by
  `refs/manifold/recovery/<workspace>/<timestamp>`.
- Search must work whether the recovery point originated from a workspace
  destroy, a workspace rewrite, or any other maw operation that pins a recovery
  ref.

## Requirements

### R1: Searchable bytes

The snapshot commit tree pinned by a recovery ref MUST contain the full file
bytes captured at the boundary, including untracked non-ignored files.

### R2: Deterministic search surface

maw MUST provide a deterministic CLI for searching across recovery points:

- global search across all pinned recovery refs
- optional filtering to a single workspace
- optional targeting of a single recovery ref

Search MUST NOT modify the repository.

### R3: Chunk retrieval

Search results MUST return bounded file excerpts ("chunks") suitable for
machine consumption:

- provenance: recovery ref (and derived workspace/timestamp), snapshot oid
- file identity: relative path
- location: 1-based line number(s)
- bytes: excerpt lines (UTF-8 lossy is acceptable for display, but exact bytes
  MUST remain recoverable via `--show`/`git show`)

Chunks MUST be obtainable without restoring an entire workspace.

### R4: Stable output

For agent consumption:

- `--format json` MUST emit a stable schema.
- `--format text` MUST emit a stable, line-oriented format.

## CLI contract

### Search

- `maw ws recover --search <pattern>`
  - searches all recovery refs under `refs/manifold/recovery/`

- `maw ws recover <workspace> --search <pattern>`
  - searches only refs whose `<workspace>` component matches

- `maw ws recover --ref <recovery-ref> --search <pattern>`
  - searches only that snapshot

Search flags:

- `--context <N>`: include N lines of context before/after each match
- `--max-hits <N>`: hard cap on total matches returned
- `--regex`: treat pattern as regex (default is fixed-string)
- `--ignore-case`
- `--text`: treat binary blobs as text for search (default: skip binary)

### Show exact bytes

- `maw ws recover --ref <recovery-ref> --show <path>`

This command MUST print exact bytes for `<path>` from the snapshot.

### Restore

- `maw ws recover --ref <recovery-ref> --to <new-workspace>`

This command MUST restore the snapshot into a new workspace.

## Test coverage

Implementations MUST include harness tests that prove:

- a known string in a tracked file is found by `--search`
- a known string in an untracked file is found by `--search`
- emitted chunks include provenance + correct line numbers
- `--show` returns byte-equivalent content for a covered file
