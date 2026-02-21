# maw ws diff contract

This document defines the CLI and output contract for `maw ws diff`.

## Command surface

- `maw ws diff <workspace>`
- `maw ws diff <workspace> --against <target>`
- `maw ws diff <workspace> --format <summary|patch|json>`
- `maw ws diff <workspace> --name-only`
- `maw ws diff <workspace> --paths <glob,...>`

## Defaults

- `--against` defaults to `default`.
- `--format` defaults to `summary`.
- `--name-only` requires `--format summary`.

## Against target resolution

`--against` accepts:

- `default`: compare against default workspace state.
- `epoch`: compare against `refs/manifold/epoch/current`.
- `branch:<name>`: compare against `refs/heads/<name>` (or full `refs/*`).
- `oid:<sha>`: compare against explicit commit/revision.
- bare `<sha>` (7..40 hex): treated as OID.
- other bare value: treated as branch name.

## Output formats

### summary

- Human-readable overview:
  - base/head labels + short OIDs
  - file/status counts
  - line additions/deletions
  - per-file status lines

### patch

- Unified diff output from git (`diff --git ...` blocks).
- No extra prose added, so output remains patch-consumable.

### json

Stable machine-readable contract:

- `workspace`: requested workspace name
- `against`: `{ label, rev, oid }`
- `head`: `{ label, rev, oid }`
- `stats`: aggregate counts
- `files[]`: `{ path, old_path, status, additions, deletions, binary }`

## Determinism

- File entries are sorted by path then status.
- Summary and JSON consume the same sorted entry list.
- `--name-only` prints one normalized path per line in sorted order.

## Error taxonomy and UX

- Invalid workspace name
- Workspace not found
- Missing default workspace
- Missing epoch ref
- Missing/invalid branch or revision
- Invalid glob in `--paths`
- Invalid flag combination (`--name-only` with non-summary format)

Errors must include copy-pasteable recovery commands (`maw ws list`, `maw init`, git check commands).
