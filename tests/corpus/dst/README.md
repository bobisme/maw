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
