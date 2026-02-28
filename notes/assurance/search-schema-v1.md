# Search JSON Schema v1

Canonical doc: `notes/assurance-plan.md`.

Status: normative output contract for `maw ws recover --search --format json`

This document defines the stable machine-readable schema for searchable recovery
results.

## 1) Command surface

Supported command forms:

- `maw ws recover --search <pattern> --format json`
- `maw ws recover <workspace> --search <pattern> --format json`
- `maw ws recover --ref <recovery-ref> --search <pattern> --format json`

## 2) Top-level object

Type: JSON object

Required fields:

- `pattern` (string)
- `workspace_filter` (string or null)
- `ref_filter` (string or null)
- `scanned_refs` (integer >= 0)
- `hit_count` (integer >= 0)
- `truncated` (boolean)
- `hits` (array of `SearchHit`)
- `advice` (array of string)

Example:

```json
{
  "pattern": "needle",
  "workspace_filter": "alice",
  "ref_filter": null,
  "scanned_refs": 3,
  "hit_count": 2,
  "truncated": false,
  "hits": [],
  "advice": [
    "Show file: maw ws recover --ref <ref> --show <path>",
    "Restore:   maw ws recover --ref <ref> --to <new-workspace>"
  ]
}
```

## 3) `SearchHit` object

Required fields:

- `ref_name` (string, full ref path)
- `workspace` (string)
- `timestamp` (string, suffix component from ref)
- `oid` (string, full commit oid)
- `oid_short` (string, short commit oid)
- `path` (string, repo-relative file path in snapshot tree)
- `line` (integer >= 1, 1-based line number)
- `snippet` (array of `SnippetLine`)

Example:

```json
{
  "ref_name": "refs/manifold/recovery/alice/2026-02-27T00-00-00Z",
  "workspace": "alice",
  "timestamp": "2026-02-27T00-00-00Z",
  "oid": "0123456789abcdef0123456789abcdef01234567",
  "oid_short": "0123456789ab",
  "path": "src/lib.rs",
  "line": 42,
  "snippet": []
}
```

## 4) `SnippetLine` object

Required fields:

- `line` (integer >= 1)
- `text` (string, UTF-8 lossy display text)
- `is_match` (boolean)

Semantics:

- if `--context 0`, snippet SHOULD contain exactly one line (the match line)
- for `--context N`, snippet SHOULD include up to `N` lines before/after within
  file bounds

## 5) Determinism requirements

For fixed repo state + fixed command args:

- refs are scanned in deterministic ref-name order
- hits are emitted in deterministic scan order
- truncation occurs exactly when `hit_count == max_hits` threshold is reached

## 6) Encoding and bytes

- JSON `text` fields are display-oriented (UTF-8 lossy acceptable)
- exact bytes remain available via:
  - `maw ws recover --ref <recovery-ref> --show <path>`
  - equivalent `git show <oid>:<path>`

## 7) Compatibility policy

- v1 compatibility means field names/types above remain stable
- additive fields are allowed
- removals/renames/type changes require a new schema version document
