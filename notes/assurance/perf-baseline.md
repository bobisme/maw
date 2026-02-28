# Merge Performance Baseline (bn-3bc5)

Pre-Phase 0 baseline measurements for `maw ws merge` wall-clock time.
These numbers establish the **before** state so Phase 0 (capture-before-rewrite + replay)
can be evaluated for regression.

## Environment

| Property | Value |
|----------|-------|
| maw version | 0.48.0 |
| CPU | AMD Ryzen 9 3900X 12-Core (24 threads) |
| Memory | 62 GiB |
| Kernel | Linux 6.18.13-arch1-1 (Arch) |
| /tmp filesystem | tmpfs (RAM-backed) |
| git | 2.53.0 |
| Date | 2026-02-28 |

## Methodology

Each scenario creates a fresh manifold repo in `/tmp` via:

1. `git init` + seed commit + `maw init`
2. `maw ws create <name>` for each workspace
3. Write N files to `ws/<name>/src/` (simple text files, ~40 bytes each)
4. `maw ws merge <names...> --destroy`

Wall time is measured with nanosecond timestamps around step 4 only.
Each scenario is repeated 5 times in separate repos; min/median/max are reported.

The benchmark script lives at `notes/assurance/perf-merge-bench.sh`.

### What is measured

The full `maw ws merge --destroy` pipeline:
- PREPARE: snapshot + freeze inputs
- BUILD: diff collection, partition, resolve, candidate commit creation
- VALIDATE: (skipped -- no validation commands configured)
- COMMIT: epoch advance (CAS update of refs/manifold/epoch/current + refs/heads/main)
- CLEANUP: worktree snapshot + destroy

### What is NOT measured

- `maw init` setup time (excluded from timing)
- `maw ws create` time (excluded from timing)
- File write time (excluded from timing)
- Network I/O (no remote configured)

## Results

### Run 1

| Scenario | Files | Workspaces | Min (ms) | Median (ms) | Max (ms) |
|----------|------:|------------|----------|-------------|----------|
| S1: small merge | 10 | 1 | 270 | 446 | 948 |
| S2: large merge | 100 | 1 | 1376 | 3550 | 4837 |
| S3: multi-workspace | 30 (10 each) | 3 | 776 | 938 | 1577 |

Raw samples (ms):
- S1: 948, 446, 457, 270, 315
- S2: 1376, 1754, 4837, 4819, 3550
- S3: 1577, 776, 938, 1176, 913

### Run 2

| Scenario | Files | Workspaces | Min (ms) | Median (ms) | Max (ms) |
|----------|------:|------------|----------|-------------|----------|
| S1: small merge | 10 | 1 | 344 | 539 | 819 |
| S2: large merge | 100 | 1 | 909 | 1480 | 2001 |
| S3: multi-workspace | 30 (10 each) | 3 | 663 | 940 | 3884 |

Raw samples (ms):
- S1: 532, 539, 344, 819, 762
- S2: 1874, 1263, 2001, 909, 1480
- S3: 663, 768, 940, 2292, 3884

## Summary (combined best estimates)

| Scenario | Typical (median) | Budget for Phase 0 |
|----------|-----------------|---------------------|
| S1: 1 ws, 10 files | ~490ms | <750ms (1.5x) |
| S2: 1 ws, 100 files | ~2500ms | <3750ms (1.5x) |
| S3: 3 ws, 10 files each | ~940ms | <1400ms (1.5x) |

The "budget" column is 1.5x the approximate median across both runs. Phase 0
should stay within this budget. If capture-before-rewrite + replay adds more
than 50% overhead to any scenario, investigate before merging.

## Observations

1. **Variance is high.** First-run-in-series samples are consistently slower (process cache cold, tmpfs page faults). The min values across runs are more representative of steady-state.

2. **S2 scales roughly linearly with file count.** 10x more files yields roughly 5-8x more time, suggesting per-file overhead dominates (diff collection, tree building).

3. **S3 multi-workspace overhead is modest.** Merging 3 workspaces with 30 total files (~940ms median) is only ~2x the cost of 1 workspace with 10 files (~490ms), despite 3x more files and additional partition work. The workspace iteration overhead is small relative to per-file cost.

4. **Bottleneck is likely in git operations** (diff, commit-tree, update-ref), not in maw's Rust code. The existing criterion benchmarks show `partition_by_path` runs in microseconds even at 1000 files.

## Reproducing

```bash
# Default: 5 runs per scenario
bash notes/assurance/perf-merge-bench.sh

# More runs for tighter confidence intervals
N_RUNS=10 bash notes/assurance/perf-merge-bench.sh
```
