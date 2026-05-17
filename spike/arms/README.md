# Arms (SP3 spike — defined, not yet built out)

Each arm: take a fresh `scenario/seed.sh` copy, place it OUTSIDE this repo
(`/tmp/...`, no CLAUDE.md/AGENTS.md — see feasibility memo §Auth), run the
3 agents (TASK-A, TASK-B, TASK-C from `scenario/.../TASKS.md`) via
`../drive_agent.sh`, then reconcile per the arm's coordination model and
emit `metrics.json`.

- **maw**: `maw ws create agent-{1,2,3}`; agents work in `ws/agent-N/`;
  `maw ws merge agent-1 agent-2 agent-3 --into default`. Coordination =
  maw epoch/merge engine. (Run against a /tmp scratch maw repo, not this
  repo.)
- **git-worktrees + thin convention**: `git worktree add` per agent +
  a minimal coordination convention (e.g. a `COORD.md` claim file + manual
  rebase/merge order). This is the *fair pragmatic baseline* — agents are
  git-fluent so this arm must NOT be hobbled.
- **jj-workspaces**: `jj workspace add` per agent. Agents drive jj directly
  WITH a maw-equivalent command crib (controls the training-data-scarcity
  confound — see memo §1 fairness caveat). The §1 reproduction proves this
  arm genuinely wedges; that is the point, and it is fair.

Spike status: scenario + driver + metric contract are real and exercised
(see ../../notes/agent-benchmark-feasibility.md). Per-arm orchestration
scripts are SG2 build work, intentionally scoped out of the spike (the
spike proves feasibility/fairness, it does not pay for the full sweep).
