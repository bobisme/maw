# DST Incident Corpus

Drop failing trace files here as JSON. Each file is replayed on every CI run.

## Format

```json
{
  "seed": 12345,
  "crash_phase": "commit",
  "num_workspaces": 2,
  "num_files_per_ws": 1,
  "create_candidate": true,
  "expected": "pass",
  "description": "Short description of the incident"
}
```

## Fields

- `seed`: u64 seed for deterministic reproduction
- `crash_phase`: one of `prepare`, `build`, `validate`, `commit`, `cleanup`
- `num_workspaces`: 1-3
- `num_files_per_ws`: 1-3
- `create_candidate`: whether to create a real merge candidate
- `expected`: `"pass"` (invariants should hold after fix) or `"known_violation"` (tracked regression)
- `description`: human-readable description of the original failure

## Adding a new entry

1. Copy the seed and crash phase from a failing DST trace
2. Create `tests/corpus/dst/<short-name>.json`
3. Run `just incident-replay` to verify

## Replay helpers

- Repo harness failures print exact cargo replay commands and write artifact bundles under `/tmp/maw-dst-artifacts/`.
- Repo/workspace DST replay now lives under maw itself:
  - `maw dev sim run --harness all --seeds 12 --steps 14 --print-only`
  - `maw dev sim replay --bundle /tmp/maw-dst-artifacts/<harness>/seed-.../bundle.json`
  - `maw dev sim replay --harness workflow --seed <seed> --print-only`
  - `maw dev sim replay --harness action --seed <seed> --steps <prefix> --print-only`
  - `maw dev sim shrink --bundle /tmp/maw-dst-artifacts/action-workflow-dst/seed-.../bundle.json --print-only`
  - `maw dev sim inspect /tmp/maw-dst-artifacts/<harness>/seed-.../bundle.json`
  - `maw dev sim inspect /tmp/maw-dst-artifacts/<harness>/success-.../summary.json --format json`
  - `maw dev sim inspect --latest --harness action`
- Bones protocol simulations use the built-in helper:
  - `bn dev sim run --seeds 100`
  - `bn dev sim replay --seed <seed>`

Use `maw dev sim replay` for repo/workspace DST failures. Use `bn dev sim replay` only when the failure came from the bones simulator itself.
