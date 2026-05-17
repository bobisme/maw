# SP3 spike harness scaffold (bn-2ixm)

Minimal, reproducible 2-task / 3-agent agent-ergonomics harness used to
prove the SG2 benchmark is **feasible** and **fair** before building it.

This is a *spike* scaffold, not the production harness. It exists to:

1. Define the canonical scenario (2 tasks, 3 agents, deterministic seed
   repo) so every arm sees an identical workload.
2. Define the three arms (maw / git-worktrees+convention / jj-workspaces).
3. Define the metric extraction contract (what we read out of each run).
4. Carry the measured per-run cost + variance numbers (see
   `../notes/agent-benchmark-feasibility.md`).

## Layout

- `scenario/` — the deterministic seed repo + task prompts (arm-agnostic).
- `arms/` — one driver script per arm. Each takes a fresh seed-repo copy,
  runs the 3 agents, and emits `metrics.json`.
- `drive_agent.sh` — the fresh-context agent driver wrapper (Claude Code
  CLI, `--print` non-interactive, JSON output for cost/turn extraction).
- `metrics.md` — the metric contract (what each field means, how derived).

## Why not run the full benchmark here

Running 3 fresh-context coding agents x 3 arms x N>=10 is real money and
hours. The spike's job is to **size** that, not pay it. We run the driver
*once* end-to-end on the cheapest arm to get a real per-run cost+turn
anchor, then extrapolate N from observed metric variance. See the memo.
