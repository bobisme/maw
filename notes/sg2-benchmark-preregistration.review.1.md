# SG2 / maw benchmark preregistration — review 1

Reviewed: 2026-05-18  
Input: `sg2-benchmark-preregistration.md`  
Output: `sg2-benchmark-preregistration.review.1.md`

## Executive verdict

This is a strong preregistration. The good parts are unusually good: the freeze clause, named double-investment bias, explicit overkill/loss regime, refusal to use a composite score, and separation of safety from efficiency all make the artifact look much less like author marketing.

The main problem is not honesty. It is **external validity**.

The current benchmark is built around “maw vs plain git worktrees + thin convention vs jj workspaces.” That framing is already dated. Current agent products have moved toward native worktree isolation, setup hooks, ignored-file copying, cleanup, PR/diff workflows, and multi-agent worktree modes. If maw only beats “plain git worktree plus a convention,” reviewers can say it beat the wrong incumbent.

The bar for maw should be reframed:

> Git worktrees are table stakes. maw must prove it is a better **agent coordination layer** over/around worktrees: lifecycle, state visibility, mergeback, recovery, environment setup, cleanup, and machine-readable affordances.

## What is already working

### Trust posture

The document is credible because it precommits to losing cases, no-go outcomes, and non-composite reporting. Keep that. The “maw is overkill below this line” commitment is probably the strongest trust move in the whole doc.

### Metric separation

Correctness/safety first, efficiency second, no weighted score. This avoids the usual benchmark sin: hiding a safety failure behind cheaper/faster runs.

### Bias naming

The double-investment bias section is sharp and useful. It makes the SG3 layout decision auditable instead of vibes-based.

### Honest ledger

The R1–R8 table is exactly the right instinct: label decision rules as decision rules instead of laundering them as measurements.

## Highest-priority changes

### P0 — Add an agent-native worktree baseline

**Problem**

The “git-worktrees + thin convention” arm is no longer the strongest practical baseline. Claude Code now documents native `--worktree` behavior, default `.claude/worktrees/...` placement, `.worktreeinclude` for ignored files, create/remove hooks, subagent worktrees, and cleanup behavior. Codex and Windsurf also expose worktree isolation directly in their agent UX.

Because the benchmark driver is Claude Code, this is not a peripheral market fact. It directly challenges the chosen baseline.

**Why this matters**

If maw beats plain Git worktrees, critics can say:

> You did not compare against the worktree workflow the same agent tool already ships.

That is a fatal reviewer objection for a tool whose thesis is “better than git worktrees for agents.”

**Recommendation**

Add either:

1. a fourth arm: `claude-native-worktrees`, or
2. a replacement baseline: `best-current-agent-worktree workflow`.

Do not bury this as “thin convention.” Treat it as a serious incumbent.

**Concrete arm definition**

```text
4. claude-native-worktrees — Claude Code's documented `claude --worktree`
   workflow, including configured `worktree.baseRef`, `.worktreeinclude`,
   WorktreeCreate/WorktreeRemove hooks where applicable, and documented cleanup
   behavior. The arm receives an equivalent task crib and no maw-specific
   affordances.
```

If adding a fourth arm is too expensive, run it at C0, C2, and C4 only. Those three points are enough to test whether maw is beating modern agent worktree ergonomics or merely beating a bare Git convention.

### P0 — Make C4 concrete; remove `3+`

**Problem**

The condition spectrum is supposed to be frozen, but C4 says `3+ (max op-DAG contention)`. That is not actually frozen. “Max” can move after implementation.

**Recommendation**

Pick exact values.

Example:

```text
C4 hostile: K_overlap = 100%; K_concurrency = 4; K_rounds = 8 burst;
no inter-round serialization; all agents start from the same base ref/op-head.
```

If the generator uses a task battery, define overlap as exact task counts, not approximate percentages:

```text
C1 = 2/8 hotspot tasks
C2 = 4/8 hotspot tasks
C3 = 6/8 hotspot tasks
C4 = 8/8 hotspot tasks
```

Approximate `~25%` is weaker than a preregistration should be.

### P0 — Add randomization/blocking

**Problem**

The doc says same seed, same model, same driver. It does not specify run order. With hosted model/agent tooling, temporal drift matters: rate limits, caching, service changes, model routing, local machine load, and auth/session behavior can all correlate with time.

**Recommendation**

Predefine a blocked randomized schedule:

```text
For each `(condition, seed, replicate)` block, run arms in randomized order.
No arm may complete all replicates for a condition before other arms start.
The schedule is generated before runs and committed with the benchmark config.
```

Record:

- Claude Code version
- model identifier and effective model in result metadata
- Git version
- jj version
- OS/kernel
- maw commit
- benchmark harness commit
- scenario generator commit
- prompt hash
- seed
- arm order
- retry/discard reason

### P0 — Split `work_lost` into integrated, orphaned, and irrecoverable

**Problem**

The current `work_lost` definition is “committed work unreachable at run end.” That is too VCS-centric.

For an agent workflow, work can be:

1. present and integrated into the intended deliverable,
2. present but orphaned or not discovered by the agent,
3. recoverable only by an expert knowing hidden state,
4. truly unreachable/irrecoverable.

Only #4 is “lost” in a low-level Git sense. But #2 and #3 are still product failures for maw if the agent cannot use the work.

**Recommendation**

Replace or augment `work_lost` with:

```text
deliverable_integrated: boolean
  The required final task result is present on the target integration branch / final workspace.

recoverable_orphaned_work: boolean
  Work exists in some branch/worktree/op-log/reflog/maw state but is not integrated
  into the final deliverable by the agent.

irrecoverable_lost_work: boolean
  The expected work is not reachable by normal VCS/maw recovery mechanisms.

agent_recovered_orphan: boolean
  The agent detected and integrated orphaned work without human help.
```

Then report safety as:

```text
hard loss = irrecoverable_lost_work
workflow loss = !deliverable_integrated || recoverable_orphaned_work
```

This prevents maw from claiming victory because work is technically recoverable while the agent still failed the workflow.

### P0 — Add a best-effort jj protocol, not only a jj crib

**Problem**

The jj arm is defensible, but it is easy to make it look worse than it is. Current jj docs explicitly discuss multi-workspace behavior, stale working-copy commits, operation-log concurrency, and divergent changes. jj also has automation-relevant behavior such as operation integration controls.

**Recommendation**

Keep jj, but benchmark a best-effort jj workflow. The jj crib should include:

- workspace creation/listing
- stale working copy handling
- divergent-change detection
- explicit divergence resolution policy
- operation-log recovery commands
- when to avoid integrating background operations
- exact commands the agent may use to inspect op state

Also add a “jj mitigation appendix” in the publication: show the crib and state that maw did not beat a naive jj strawman.

If budget permits, add a mitigation sub-arm at C2/C4:

```text
jj-workspaces-best-practice — jj workspace workflow with the documented
automation/concurrency crib and explicit divergence-resolution instructions.
```

### P1 — Add power/MDE table before running

**Problem**

N=10/20 is fine for an exploratory artifact, but “tight CIs” is too strong for proportions.

For example, with Wilson 95% intervals:

| n | observed wedge events | observed rate | Wilson 95% upper bound |
|---:|---:|---:|---:|
| 10 | 0 | 0.00 | ~0.278 |
| 20 | 0 | 0.00 | ~0.161 |
| 50 | 0 | 0.00 | ~0.071 |
| 100 | 0 | 0.00 | ~0.037 |

So even zero wedge incidents in 20 runs still means “compatible with up to ~16% true wedge rate,” not “near zero.”

**Recommendation**

Add a preregistered power/MDE table:

```text
The benchmark is powered to detect large coordination failures, not small
ergonomic differences. For wedge incidence, N=20 can distinguish gross
differences but cannot prove near-zero rates. Any “0 observed” statement must
report its Wilson upper bound.
```

Then publish all wedge claims as:

```text
0/20 observed, Wilson 95% CI [0.00, 0.16]
```

not:

```text
maw wedge rate = 0
```

### P1 — Replace “IQRs do not overlap” with paired/bootstrap tests

**Problem**

“IQRs do not overlap” is conservative but statistically sloppy. With N=10/20, it can miss real differences or produce weird threshold behavior.

**Recommendation**

Keep medians/IQRs for display, but define decisions using paired effect estimates:

```text
For paired conditions, compute the paired median difference in turns/tool_calls
between arms. Use bootstrap 95% CI over paired replicates. A material efficiency
loss exists iff:
  median_ratio > 1.15
  AND bootstrap CI for paired median difference excludes 0.
```

Also report a nonparametric effect size:

- Cliff’s delta, or
- Hodges–Lehmann median paired difference.

Do not let this become a composite score. It is just a cleaner decision rule for one axis.

### P1 — Blind/double-code `wasted_turns`

**Problem**

`wasted_turns` attribution is high-value but subjective. It is exactly where author bias can leak back in through transcript interpretation.

**Recommendation**

Add a coding protocol:

```text
A random 20% sample of transcripts, plus all wedged runs, are independently
coded by two reviewers blind to arm name where feasible. Disagreements are
adjudicated before aggregate metrics are computed. Report agreement rate and
examples of each attribution class.
```

If a second reviewer is impossible, at least do delayed self-review with arm labels masked and publish the raw transcript snippets used for attribution.

### P1 — Add real workflow task classes beyond shared-file hotspots

**Problem**

The K_overlap/K_concurrency axis models one failure mode: concurrent edits in a shared hotspot. That is important but too narrow.

Real agent-worktree pain includes:

- ignored files and secrets missing from worktrees
- dependency installation per worktree
- port/runtime collisions
- relative-path monorepo dependencies
- submodules/LFS
- branch/base drift
- cleanup/orphaned worktrees
- PR creation and mergeback
- stale main/rebase behavior
- conflicting generated files
- interrupted agent commands
- renamed/moved files
- multiple agents working on logically coupled but not same-file changes

**Recommendation**

Keep the five-point coordination spectrum, but add a small orthogonal scenario taxonomy:

```text
T0 code-only shared hotspot
T1 ignored-env setup required
T2 dependency/install side effects
T3 mergeback/PR required
T4 stale-base/rebase required
T5 cleanup/recovery after interrupted run
```

Then either:

- run all T classes at C2 only, or
- include one representative T-class per condition.

This will make the benchmark about agent ergonomics, not just conflict mechanics.

### P1 — Add setup/cleanup friction metrics instead of speed

**Problem**

The doc correctly refuses speed/perf claims. But it currently undermeasures the exact place maw may win: lifecycle friction.

**Recommendation**

Add non-wall-clock friction metrics:

```text
workspace_setup_tool_calls
first_correct_workspace_tool_call_index
workspace_discovery_failures
mergeback_tool_calls
cleanup_success
orphaned_workspace_count
doctor_repair_required
```

These are not throughput. They are ergonomics.

### P1 — Reclassify discarded runs

**Problem**

Auth failures are discarded and rerun. Good. But a broad discard rule can hide substrate-induced failures if the classifier is too permissive.

**Recommendation**

Use explicit discard classes:

```text
discard_auth
discard_harness_bug
discard_external_service_outage
counted_substrate_failure
counted_agent_failure
```

Add max retries:

```text
At most 2 discarded reruns per `(arm, condition, replicate)`.
Further failures are reported separately and do not silently disappear.
```

Also avoid regex-only `/login` classification if possible. Prefer structured `is_error`, SDK result subtype, or a fixed list of exact auth messages.

### P2 — Fix “directly readable” wording

The doc says the metric set is directly readable from the Claude JSON envelope or deterministically derivable. But `tool_calls` comes from transcript event count, and `work_lost` comes from a scenario oracle. That is fine, but the sentence overclaims.

Suggested text:

```text
The driver records the Claude result envelope plus transcript/tool events.
The benchmark derives the following metrics from the envelope, transcript,
and scenario oracle.
```

### P2 — Fix the malformed markdown in §5

The line:

```text
K*overlap percentages, and the C0/C4 endpoints are pre-registered \_design choices*
```

should be:

```text
K_overlap percentages and the C0/C4 endpoints are pre-registered _design choices_
```

### P2 — Replace “all three arms identical”

The doc says a no-overlap scenario makes all three arms identical. Not quite. They still differ in setup, cleanup, path discovery, command crib, and branch/workspace lifecycle.

Suggested text:

```text
A no-overlap scenario removes the main coordination failure mode and primarily
measures setup/lifecycle friction rather than contention behavior.
```

## Product implications for maw

The benchmark should force maw toward a sharper product thesis.

### maw should not sell “worktrees, but nicer”

That market is gone or disappearing. Claude Code, Codex, Windsurf, Cursor, and ordinary Git workflows already make isolated worktrees accessible.

maw should sell:

```text
agent-safe coordination state + lifecycle + recovery over worktrees
```

### Features maw likely needs

#### 1. Machine-readable workspace manifest

```json
{
  "workspace_id": "maw-abc123",
  "task_id": "bn-...",
  "owner": "agent/session id",
  "base_ref": "main@<sha>",
  "branch": "maw/bn-...",
  "path": ".maw/worktrees/bn-...",
  "status": "active|ready|integrating|merged|abandoned",
  "created_at": "...",
  "last_seen_at": "...",
  "changed_files": ["..."],
  "integration_target": "main"
}
```

Agents should not infer state from directory names and Git plumbing.

#### 2. `maw status --json`

Must answer, in one command:

- where am I?
- what workspace am I in?
- what other workspaces exist?
- what changed?
- what is stale?
- what must be integrated?
- what is safe to delete?
- what is blocked?

#### 3. `maw doctor` / `maw repair`

Should handle:

- moved root
- deleted worktree directory
- stale Git worktree metadata
- missing `.maw` state
- interrupted command
- duplicate branch
- orphaned workspace
- concurrent maw process lock
- mismatched base ref

#### 4. Append-only event log

A simple WAL/event log gives maw something Git worktrees lack:

```text
workspace_created
agent_started
snapshot_recorded
integration_started
conflict_detected
integration_completed
workspace_cleaned
repair_performed
```

This becomes the oracle for recovery and the benchmark’s raw evidence.

#### 5. Agent crib generated by the tool

`maw crib claude`, `maw crib codex`, `maw crib cursor`, etc.

The tool should emit a short agent-facing protocol:

```text
Before editing: run maw status --json.
Before integrating: run maw integrate --check.
Never delete .maw/worktrees manually.
If confused: run maw doctor --json.
```

This reduces prompt variance and makes maw’s affordance explicit.

#### 6. Environment propagation

Add a `.mawinclude` or hooks equivalent:

```text
.mawinclude
.env.example
.env.local
config/*.local.json
```

Plus:

```text
maw hook setup
maw hook cleanup
```

The modern baseline already has versions of this. maw cannot ignore it.

#### 7. Safe cleanup

`maw gc` should never delete work with unintegrated changes unless the work is preserved, snapshotted, or explicitly abandoned.

Add states:

```text
clean
dirty-uncommitted
committed-unintegrated
integrated
abandoned-with-snapshot
```

#### 8. Mergeback queue

For multi-agent work, the hard part is not creating worktrees. It is serializing integration.

Add:

```text
maw integrate --queue
maw integrate --next
maw integrate --abort
maw integrate --resume
```

Track conflict provenance and changed-file overlap before attempting merge.

#### 9. First-class “overkill line”

The publication should not just say maw is overkill at C0. The product should expose that as guidance:

```text
Use plain Claude/Codex worktrees for one-off independent tasks.
Use maw when N agents, overlapping files, or integration queue risk exceeds X.
```

That is more credible than pretending maw is always the answer.

## Suggested amendment package

Because the document has a freeze clause, do not silently rewrite frozen rules. Add an amendment before any affected run.

Suggested §9 entry:

```text
2026-05-18T__:__:__Z — Review amendment A1: modern agent-worktree baseline.
Reason: external review found that current agent tools expose native worktree
isolation and lifecycle affordances, including Claude Code `--worktree`,
ignored-file inclusion, hooks, cleanup, and subagent worktree behavior. The
original `git-worktrees + thin convention` arm remains useful but is not the
strongest practical baseline.

Change: add `claude-native-worktrees` as a fourth comparator arm for C0/C2/C4
minimum, or replace `git-worktrees + thin convention` with
`best-current-agent-worktree` if run budget prohibits four full arms.

This amendment is committed before any affected SG2 measured run.
```

Suggested §9 entry:

```text
2026-05-18T__:__:__Z — Review amendment A2: frozen C4 parameters.
Reason: `3+` and `max op-DAG contention` are not fully frozen values.

Change: define C4 as K_overlap = 100%, K_concurrency = <exact N>,
K_rounds = <exact N>, with no inter-round serialization. Define all
K_overlap percentages as exact task counts for the fixed battery.
```

Suggested §9 entry:

```text
2026-05-18T__:__:__Z — Review amendment A3: blocked randomized run order.
Reason: hosted agent/model behavior can drift over time; arm order must not
confound substrate with temporal effects.

Change: generate and commit a randomized block schedule per
(condition, seed, replicate), interleaving all arms.
```

Suggested non-frozen implementation-protocol addendum:

```text
Add discard taxonomy, version capture, raw transcript publication, dual coding
for wasted_turns, and split work_lost into integrated/orphaned/irrecoverable.
```

## Reviewer-objection checklist

Use this as a pre-publication attack list.

| Objection | Current risk | Fix |
|---|---:|---|
| “You compared against a toy Git worktree workflow.” | High | Add native Claude/current agent-worktree arm. |
| “jj was used naively.” | Medium-high | Publish jj crib and best-practice mitigation path. |
| “C4 was tuned after the fact.” | High | Replace `3+`/`max` with exact values. |
| “0/20 does not prove near-zero wedge rate.” | High | Report Wilson CIs and MDE table. |
| “Manual wasted-turn labels are biased.” | Medium-high | Blind/double-code subset; publish examples. |
| “Synthetic hotspot does not represent real worktree pain.” | High | Add env/setup/mergeback/stale-base scenario classes. |
| “Work was recoverable but the agent failed; you counted no loss.” | High | Split integrated/orphaned/irrecoverable. |
| “Service/model drift affected results.” | Medium | Block randomization + version/prompt/seed capture. |
| “maw wins because its crib is better.” | Medium | Equalize crib length/detail; publish all cribs. |
| “No speed metric hides overhead.” | Low | Keep no speed claims; add setup/cleanup friction metrics. |

## References consulted

- Git `git-worktree` documentation, current manual.
- Claude Code worktrees documentation: native `--worktree`, `.worktreeinclude`, hooks, cleanup, and subagent worktrees.
- Claude Code CLI and SDK result-message documentation: result envelope fields including `num_turns`, `total_cost_usd`, `is_error`, and related usage metadata.
- Jujutsu working-copy, workspace, operation-log, concurrency, divergence, and changelog documentation.
- OpenAI Codex app features: local/worktree/cloud modes and Git worktree isolation.
- Windsurf Cascade worktrees documentation: worktree mode, hooks, cleanup, and source-control behavior.
- Anthropic research note on statistical evaluation: confidence intervals, standard errors, and power analysis.
- “Establishing Best Practices for Building Rigorous Agentic Benchmarks” (2025).
- “Efficient Benchmarking of AI Agents” (2026).
- METR note on SWE-bench passing PRs not necessarily being mergeable, used as a reminder that automated task success can diverge from real integration quality.
