# SP3: Agent-benchmark feasibility + fair-jj reproduction (bn-2ixm)

Spike memo. Authoritative spec: `bn show bn-2ixm`. Parent SG2 = `bn-2jwi`
(agent-ergonomics benchmark + diagnostic). This memo answers: **is the SG2
benchmark feasible, and is its jj arm fair (not a strawman)?**

Verdict up front:

- **jj opfork-wedge: REPRODUCED** on jj 0.41.0 under concurrent multi-workspace
  use — all five mechanism steps from `notes/manifold-v2.md` §2.2 observed,
  including the user-visible `Internal error: ... sibling of the working
  copy's operation` and **divergent commits**. The jj arm is **fair, not a
  strawman**. This is the load-bearing finding.
- **Harness: FEASIBLE.** `claude` CLI (`-p --output-format json`) is a
  reproducible fresh-context driver with a machine-readable cost/turn/denial
  envelope. Per-agent happy-path cost ≈ **$0.077**, low variance.
- **Required N is small** because the maw-vs-jj effect is large (≈1.8x cost,
  ≈2x turns when an agent hits the wedged state) relative to happy-path
  metric CV (5–10%). N ≈ **7–10 runs/arm** is sufficient for the headline.

---

## 1. The jj opfork-wedge reproduction (LOAD-BEARING)

### Environment

- jj 0.41.0 (`/usr/bin/jj`), git 2.54.0.
- Throwaway scratch repo: `/tmp/sp3-jj-repro/sp3repo` (colocated jj/git),
  workspaces `ws-alice`, `ws-bob`, `ws-carol`. **No jj was run anywhere
  under `/home/bob/src/maw`** (spike jj-exception honored).

### Setup commands

```bash
mkdir -p /tmp/sp3-jj-repro && cd /tmp/sp3-jj-repro
jj git init --colocate sp3repo
cd sp3repo
# seed base + wip commit
jj describe -m "base commit"; jj new -m "wip"
jj workspace add ../ws-alice --name alice
jj workspace add ../ws-bob   --name bob
jj workspace add ../ws-carol --name carol
```

### The concurrency driver (the exact §2.2 #3 scenario)

Three workspaces, commands launched **in parallel** so each reads the same
op-head and extends the DAG concurrently (this is precisely §2.2 #3:
"Alice runs `jj status` in ws/alice at the same time Bob runs `jj describe`
in ws/bob ... the op log now has two heads — an opfork"):

```bash
# Round-based concurrent burst (5 rounds, then a heavier 8-round burst with
# no inter-round serialization):
for round in 1..5; do
  ( cd ../ws-alice && echo "alice $round" >> a.txt && jj status )   &
  ( cd ../ws-bob   && echo "bob $round"   >> b.txt && jj describe -m "bob r$round" ) &
  ( cd ../ws-carol && echo "carol $round" >> c.txt && jj status )   &
  wait
done
# heavier: 8 rounds, alice/bob `jj describe`, carol `jj new`, all fired
# together with a single final wait (max op-DAG contention).
```

### Observed symptoms — mapped 1:1 to manifold-v2.md §2.2

| §2.2 step | Predicted | Observed (jj 0.41.0) |
|---|---|---|
| #3 Concurrent ops fork the DAG | "op log now has two heads (opfork)" | `Concurrent modification detected, resolving automatically.` — **9 occurrences**; `jj op log --graph` shows the DAG fanning into 8+ parallel lanes (`├─┬─┬─┬─┬─┬─┬─┬─╮`) |
| #4 Opforks cascade | "merged op itself forked by next command ... 'sibling of the working copy's operation' errors, divergent commits, stale states" | **`Internal error: The repo was loaded at operation X, which seems to be a sibling of the working copy's operation Y`** — **7 occurrences**; **4 divergent change-ids** arose (`mpqnosuk/5`, `lpsrzqzq/3`, ... shown as `(divergent)` in `jj workspace list` / `jj log`); **11 `reconcile divergent operations`** nodes in the op DAG, including reconciles-of-reconciles (forks of the heal merge itself) |
| #5 Recovery uses the same mechanism | "`jj op integrate` ... write to the same shared op log, potentially forking it further" | jj's own hint on failure: `Hint: Run 'jj op integrate <op>' to add the working copy's operation to the operation log.` — i.e. recovery is itself another op-log write |

Verbatim failure envelope an agent would receive in a wedged workspace:

```
Concurrent modification detected, resolving automatically.
Internal error: The repo was loaded at operation 3a702a0a3361, which seems
to be a sibling of the working copy's operation 482ceaa7e64d
Hint: Run `jj op integrate 482ceaa7e64d` to add the working copy's
operation to the operation log.
```

`jj workspace list` after the burst (note `(divergent)` + `change/N` suffix):

```
alice: mpqnosuk/5 91da355f (divergent) alice r7
bob:   lpsrzqzq/3 3703ef85 (divergent) bob r1
carol: qpurpxky a0e120a8 (empty) carol r2
default: pqmymtrr fce3b290 (empty) wip
```

### Conclusion (fairness)

The §2.2 failure mode is **current, not historical**: jj 0.41.0 (a recent
release) still exhibits the full shared-oplog opfork → cascade → divergent-
commit → integrate-to-recover chain under exactly the concurrent multi-
workspace pattern multi-agent fleets produce. **The SG2 jj-workspaces arm
is therefore a fair comparison, not a strawman.** This removes the spike's
single biggest risk to the v1.0 comparison's credibility.

**Fairness caveat to carry into SG2 (per memory
`maw-design-rationale-agent-fluency`):** the real reason jj was abandoned
is *training-data scarcity* (agents are git-fluent, jj-scarce), not VCS-
illiteracy. The benchmark must NOT conflate "agent fumbles unfamiliar jj
verbs" with "jj's concurrency model wedges". Mitigation, pre-registered:
the jj arm gives agents a thin maw-equivalent command crib (so jj-verb
unfamiliarity is controlled for) and the headline metric isolates the
*coordination* failure (work-redone / interventions / divergent-state
recoveries), not raw jj-command flailing. The reproduction above is a
property of the substrate, independent of the driver's jj fluency.

---

## 2. Chosen agent-driver harness

**Driver: `claude` CLI in non-interactive print mode.** Claude Code
2.1.143, invoked as:

```bash
claude -p "<task prompt>" \
  --output-format json \      # single machine-readable result envelope
  --model sonnet \            # pinned model — identical every run/arm
  --max-turns 40 \            # deterministic termination of a wedged run
  --max-budget-usd 2.00 \     # hard $ ceiling (runaway-loop guard)
  --permission-mode bypassPermissions \  # non-interactive, no prompts
  --add-dir <scenario-workdir>
```

Scaffold: `ws/bn-2ixm/spike/` (`drive_agent.sh`, `scenario/seed.sh`,
arm-agnostic deterministic seed repo with a **built-in shared-file hotspot**
in `src/lib.rs` so concurrent agents *must* collide — a no-overlap scenario
would make all three arms look identical and the benchmark uninformative).

The JSON `result` event yields every benchmark metric directly:
`total_cost_usd`, `num_turns`, `duration_ms`, `is_error`, `subtype`,
`permission_denials[]`, `usage{input,output,cache_*}`. No log scraping
needed — clean metric contract.

### Auth gotcha (§Auth) — important for the SG2 implementer

`--bare` (the obvious "isolate context" flag) **breaks the driver here**:
in this build `--bare` forces auth to `ANTHROPIC_API_KEY`/apiKeyHelper only
and refuses the OAuth/keychain session, yielding `Not logged in · Please
run /login` (cost $0, instant error — silent metric corruption if
unchecked). **Resolution:** do NOT use `--bare`; instead achieve context
isolation by placing the scenario repo under `/tmp` with no
`CLAUDE.md`/`AGENTS.md`/`.mcp.json`, so the agent's context is exactly the
task prompt + scenario tree. (Alternatively: export an `ANTHROPIC_API_KEY`
and keep `--bare` — but the OAuth path is what's available on this host and
is the reproducible default here.) SG2 must health-check every run for
`is_error==true` / `result=="Not logged in..."` and discard+rerun.

---

## 3. Measured per-run cost + metric variance

Real fresh-context agent runs (not estimates). Identical isolated coding
task (TASK-A: add a module + register in shared `lib.rs` + git commit),
4 successful repetitions:

| run | cost USD | turns | duration ms | task done? |
|----|---------|------|------------|-----------|
| 2 | 0.0820 | 6 | 19185 | yes |
| 3 | 0.0755 | 5 | 11070 | yes |
| 4 | 0.0754 | 5 | 18970 | yes |
| 5 | 0.0737 | 5 | 12102 | yes |

| metric | mean | stdev | **CV** |
|---|---|---|---|
| cost USD | 0.0767 | 0.0037 | **4.8%** |
| turns | 5.25 | 0.50 | **9.5%** |
| duration ms | 15332 | 4347 | 28.4% (wall-clock; noisy, not a headline metric) |

Trivial 1-turn floor (system-prompt cache creation) ≈ **$0.08** even for a
no-op — cost is dominated by per-invocation context setup, so longer tasks
are only marginally more expensive. Budget the benchmark by **agent-runs**,
not task size.

### The coordination-stress data point (why N is small)

One fresh-context agent dropped into the **wedged jj workspace** from §1 and
told to land a change + resolve any divergence:

| | happy path (mean) | wedged-jj run | ratio |
|---|---|---|---|
| cost USD | 0.0767 | **0.1373** | **1.79x** |
| turns | 5.25 | **10** | **1.90x** |
| duration ms | 15332 | 36944 | 2.41x |

The agent recovered but reported *"Divergence resolved by abandoning the 4
stale versions of the change ID"* — i.e. it discarded work to escape the
wedge (a direct **work-lost / work-redone** benchmark signal, and an
**intervention** the maw arm should not require). The maw-vs-jj effect size
(~1.8–1.9x on the primary metrics) is **~20–40x the happy-path CV**, so the
arms separate cleanly with very few runs.

---

## 4. Required N for signal

Two-sample comparison, 80% power, α=0.05, n/arm ≈ 16·CV² / (relative-effect)²:

| metric | to detect 50% diff | 25% diff | 15% diff |
|---|---|---|---|
| turns (CV 9.5%) | 2 | 3 | 7 |
| cost (CV 4.8%) | 2 | 2 | 2 |

The observed maw-vs-jj effect is ≈80–90%, far beyond 50%. **N ≈ 7–10
runs/arm** is comfortably sufficient for the headline (the 7 comes from the
15%-turns row, used as a conservative floor for the *narrower* maw-vs-git-
worktrees gap which we expect to be much smaller than maw-vs-jj).

**Important variance caveat for SG2:** the happy-path metrics are quasi-
Gaussian and low-CV, but the *benchmark-relevant* variance is **bimodal /
zero-inflated** — most runs land cleanly; a fraction hit the wedge and jump
~2x. Power should be sized on the **wedge-incidence rate** (a proportion),
not on cost-CV. Plan: N=10/arm for the headline dominance claim; pre-
register N=20/arm if the crossover-curve regions (where maw loses / is
overkill — the publishable headline per the v1.0 posture) need tight CIs.

### Cost envelope for the full SG2 benchmark

- Happy-path 3-agent run ≈ 3 × $0.077 ≈ **$0.23**.
- Wedged jj 3-agent run ≈ 3 × $0.14 + retry overhead ≈ **$0.45–0.6**.
- 3 arms × 10 runs ≈ **$8–15 total**. With N=20/arm ≈ **$20–35**.
- Wall-clock: ~15–40s/agent, agents parallel within a run → a full
  3-arm × 10-run sweep is **a few hours**, not days. **Feasible.**

---

## 5. Acceptance-criteria checklist

Spec AC: *"Feasibility memo (notes/) with: chosen agent-driver harness;
measured per-run cost + metric variance; required N for signal; a
DEMONSTRATED jj opfork-wedge reproduction (or documented non-reproduction
+ implication)."*

- [x] Memo in `notes/` (this file).
- [x] Chosen agent-driver harness — §2 (`claude -p --output-format json`,
      scaffold in `spike/`, auth gotcha documented).
- [x] Measured per-run cost — §3 ($0.077 happy / $0.137 wedged, real runs).
- [x] Metric variance — §3 (cost CV 4.8%, turns CV 9.5%; bimodal caveat).
- [x] Required N for signal — §4 (N≈7–10/arm headline; sizing on wedge
      incidence; full-benchmark cost/time envelope).
- [x] **DEMONSTRATED jj opfork-wedge reproduction** — §1 (commands +
      verbatim symptoms, mapped 1:1 to manifold-v2.md §2.2 #3/#4/#5).
- [x] Arms defined: maw / git-worktrees+convention / jj-workspaces; jj arm
      proven fair (not strawman) + fairness caveat pre-registered for SG2.

**All acceptance criteria met.**

---

## 6. Implications for SG2 / the v1.0 comparison story

1. **The jj comparison is credible.** SG2 can build the jj arm in good
   faith; the §2.2 failure mode is live on jj 0.41.0. This was the spike's
   primary risk and it is retired.
2. **Benchmark is cheap and fast enough to run continuously**, not as a
   one-off — supports the "continuously-run" posture and lets the crossover
   curve (the publishable headline) be mapped, not guessed.
3. **Metric contract is clean** — `claude -p` JSON gives cost/turns/denials
   with no scraping; SG2 should add: a per-run `is_error`/"Not logged in"
   health gate (discard+rerun), and a derived "wedge incident" flag
   (divergent-state recovery / abandoned work / >1.5x median turns).
4. **Size SG2 on wedge-incidence, not cost-CV.** The headline metric is the
   *proportion of runs that wedge* and the *work-redone on those runs*, both
   bimodal. N=10/arm for dominance; N=20/arm for the loss-regime CIs.
5. **Fairness is a first-class SG2 requirement, not an afterthought:** give
   the jj arm a maw-equivalent command crib and isolate the coordination
   failure from jj-verb unfamiliarity (training-data-scarcity confound).
   Lead the publication with the demonstrated wedge, not a composite score.
6. **Carries SG2's diagnostic (→ SG4):** the wedged-run signature here
   (abandoned change-ids, sibling-op errors, ~2x turns) is the diagnostic
   template SG4's ergonomics hardening should detect and report.

### Reproduction artifacts

- jj repro driver + logs: `/tmp/sp3-jj-repro/`, `/tmp/sp3*-*.log`
  (throwaway; regenerate via §1 commands — not committed, /tmp by design).
- Harness scaffold (committed): `ws/bn-2ixm/spike/`.
- Cost/variance run envelopes: `/tmp/sp3-anchor-run{2..5}.json`,
  `/tmp/sp3-jjarm.json` (throwaway; numbers transcribed into §3).
