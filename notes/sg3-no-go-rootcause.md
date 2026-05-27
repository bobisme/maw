# SG3 NO-GO Root Cause (bn-2ert)

Investigation of the 2026-05-26 frozen-N SG3 layout eval that flagged
NO-GO on R6 (interventions) at cell C2-T0:

> total(new) = 10, total(old) = 0 (proxy: median × n, n=10)

Forensic read of all 20 BenchRun JSONs under
`notes/eval-real-2026-05-26/sg3/maw-{old,new}-layout/C2-T0/`.

---

## §0 Summary (1-sentence)

**Root cause**: the SG3 eval was run against an installed `maw` binary
(v0.61.0) that predates T3.2's consolidated-layout implementation;
`maw doctor` correctly flagged the simulated `.maw/` substrate as foreign
and instructed every agent to run `maw init`, which migrated the
substrate back to v2 `ws/` mid-run — a substrate/binary version skew,
not a real-layout ergonomics regression.

**Recommended fix class**: **rerun the eval with a post-T3.2 binary**
(rebuild & reinstall from `main` HEAD, which carries `42d7ca66` T3.2 +
`f6cf96c1` T3.3 + `241231e3` SG4 wire-up); the substring-heuristic R6
proxy itself is also borderline-noisy and ties get amplified ×n by
`median × n` aggregation — a secondary cleanup but not the blocker.

**In-scope for v1.0**: **YES, but cheap**: rebuild-and-rerun, not a code
change. v1.0-pre.1 layout default does NOT need to be reverted; the
NO-GO is a measurement artifact, not a real-LLM ergonomics regression.

---

## §1 Evidence: per-run table (10 new-layout C2-T0 runs)

Columns: `turns` = `total_turns`, `calls` = `total_tool_calls`,
`tasks` = battery verbs (`cr` create, `ed` edit, `cm` commit, `ds`
destroy, `df` destroy --force, `RC` recover, `mg` merge),
`doctor` = `maw doctor` invocations, `init` = `maw init` invocations,
`FAILs` = `[FAIL]` lines in any tool output (out of 3 max per doctor
call), `redone` = heuristic count per `count_work_redone_turns()`,
`H` = hypothesis classification.

| run | turns | calls | tasks                 | doctor | init | FAILs | redone | H        |
|-----|-------|-------|-----------------------|--------|------|-------|--------|----------|
| r001|    10 |    11 | cr, ed, cr, cm        |     2  |   1  |    3  |    0   | **H5**   |
| r002|    15 |    14 | cr, ds, **RC**, ds    |     2  |   1  |    3  |    1   | H5 + ❶   |
| r003|    14 |    13 | cr, df, **RC**, ed    |     2  |   1  |    3  |    1   | H5 + ❶   |
| r004|    14 |    13 | cr, ds, **RC**, ds    |     2  |   1  |    3  |    2   | H5 + ❶   |
| r005|     9 |    11 | cr, cr, ed, cm        |     1  |   0  |    3  |    0   | **H5**   |
| r006|    11 |    10 | cr, ed, ed, ed        |     2  |   1  |    3  |    0   | **H5**   |
| r007|    10 |    10 | cr, ed, cm, ds        |     2  |   1  |    3  |    1   | H5 + ❷   |
| r008|     8 |     8 | cr, cr, ed, df        |     2  |   1  |    3  |    0   | **H5**   |
| r009|    11 |    10 | cr, ed, df, **RC**    |     2  |   1  |    3  |    1   | H5 + ❶   |
| r010|    11 |    10 | cr, ds, **RC**, ds    |     2  |   1  |    3  |    1   | H5 + ❶   |

❶ = `maw ws recover` invoked as the agent's correct response to a
recover task. The substring heuristic counts each `recover` token as a
"recovery_entry" turn even though it is the literal task-required verb.

❷ = `maw ws destroy ws-0` refused (Prime-Invariant guard); agent
re-issued with `--force`. The retry's description string `"recovery
snapshot is captured automatically"` triggers the substring heuristic.

### Per-run intervention trace (new-layout, in order)

**Every one of the 10 new-layout runs**, regardless of redone count, shows
the SAME prefix in turns 1–2:

```
turn 1: Bash maw doctor [&& maw ws list]
        ┖─── [FAIL] manifold metadata: .manifold/ is missing
             [FAIL] default workspace: ws/default/ does not exist
             [FAIL] repo root: 3 unexpected file(s)/dir(s) … .gitignore, README.md, .maw
turn 2: Bash maw init [&& maw doctor]      (9 of 10 runs)
        ┖─── "Cleaned: 3 root item(s) moved into ws/default/"
             All checks passed!
```

The one exception is **r005**, where the agent inspected `.maw/config.toml`
(saw the `# maw config (SP5 simulation; T3.2 will define schema)`
comment), then proceeded WITHOUT running `maw init`. The agent's next
`maw ws create --from main ws-0` succeeded BUT placed the workspace at
`ws/ws-0/` (because the installed binary doesn't honor the simulation's
`workspaces_dir = ".maw/workspaces"` config). So even r005 was silently
migrated to v2 by the binary.

After turn 2, the simulated `.maw/` layout is gone for all 10 runs: the
binary owns `ws/` and the eval is effectively running against the v2
substrate from turn 3 onward.

---

## §2 Counterfactual: 10 old-layout C2-T0 runs

| run | turns | calls | tasks                 | doctor | init | FAILs | redone |
|-----|-------|-------|-----------------------|--------|------|-------|--------|
| r001|    11 |    10 | cr, ed, df, **RC**    |     0  |   0  |    0  |    1   |
| r002|     9 |    11 | cr, cr, ed, ed        |     0  |   0  |    0  |    0   |
| r003|    11 |    10 | cr, ds, **RC**, ds    |     0  |   0  |    0  |    1   |
| r004|    10 |     9 | cr, ed, cr, df        |     1  |   0  |    0  |    0   |
| r005|    10 |    11 | cr, ed, cm, mg        |     0  |   0  |    0  |    0   |
| r006|    13 |    12 | cr, cr, df, ed        |     0  |   0  |    0  |    0   |
| r007|    16 |    15 | cr, ds, **RC**, ed    |     1  |   0  |    0  |    1   |
| r008|    13 |    12 | cr, ds, **RC**, ds    |     0  |   0  |    0  |    1   |
| r009|     4 |     3 | cr, cr, df, df        |     0  |   0  |    0  |    0   |
| r010|    11 |    10 | cr, ed, ed, cm        |     1  |   0  |    0  |    0   |

Key observation: **`maw doctor` returns clean on the old-layout substrate
(0 FAILs in all 10 runs)** because the v2 `ws/` layout is exactly what
v0.61.0's doctor expects. No agent ever needed to run `maw init`. The
substring heuristic still fires on the 4 runs whose task battery
contains "Recover the previously destroyed workspace…" (r001, r003,
r007, r008 — exactly the runs with RC in the tasks column) — these
"interventions" are NOT regressions, they are the task explicitly
requesting the use of `maw ws recover`.

### Cross-tab fires-vs-tasks

|                                    | new | old |
|------------------------------------|-----|-----|
| Runs whose battery contains RC     |   5 |   4 |
| Of those, `redone > 0`             | 5/5 | 4/4 |
| Runs whose battery has NO RC       |   5 |   6 |
| Of those, `redone > 0`             | 1/5 | 0/6 |

So the asymmetry is exactly **one extra heuristic fire** (r007 new-layout,
the destroy-refused → --force chain where the agent's description
string contained "recovery snapshot"). The `median × n` aggregation
turned 1 vs 0 in median into **10 vs 0 in proxy total** — the §3.1 R6
bar is highly sensitive to one-bit shifts in the median.

---

## §3 Mechanism: specific behavior the new layout induces vs old

The mechanism is **NOT** what SP5's named risks predicted. It is a
**substrate/binary version skew**:

1. The SG3 eval substrate is provided by
   `ConsolidatedLayoutAdapter` (`crates/maw-bench-adapters/src/consolidated_layout_adapter.rs`),
   which is a pure `git worktree` simulation of the `.maw/` layout — it
   creates `.maw/`, `.maw/workspaces/`, `.maw/manifold/`,
   `.maw/config.toml` on disk and stops there. It does NOT change which
   `maw` binary is on `$PATH`.
2. The agent invokes `maw` from `$PATH`. At eval time (2026-05-26), the
   installed binary was **v0.61.0**, which predates T3.2 (commit
   `42d7ca66`, T3.3 `f6cf96c1`, SG4 `241231e3` — all landed after the
   v0.61.0 release). v0.61.0's `LayoutFlavor` enum has only
   `Ws` and treats `.maw/` as a foreign artifact.
3. The agent (correctly!) follows v0.61.0 `maw doctor`'s explicit
   "Run: maw init" advice. `maw init` migrates the substrate back to
   the v2 `ws/` layout: it creates `ws/default/` and moves the 3
   pre-existing root entries (`.gitignore`, `README.md`, `.maw`) into
   it. From that point on, the run is effectively measuring v2 layout
   behavior with a 2-turn `(doctor, init)` startup tax.
4. Old-layout runs hit the same `maw doctor` that's "happy" with the v2
   substrate the adapter set up — no tax, no migration, no skew.

So the divergence is not "agents struggle with `.maw/`" — it is
"v0.61.0's doctor refuses the `.maw/` substrate and the agent does
exactly what doctor told it to do." The R6 NO-GO is **not measuring
layout ergonomics**; it is measuring "did the installed binary
recognize the substrate the eval set up."

### The 2-turn cost is invisible to R4/R5 medians

Every new-layout run pays the `(doctor, init)` 2-turn tax. The median
new vs median old for `turns_to_done` is 11 vs 11 — identical — because
both arms have similar variance in the task-execution portion and the
medians happen to land at the same integer despite the means being 11.3
new vs 10.8 old (per the `total_turns` distributions).

### Why R6 specifically tripped

R6's proxy is `median(work_redone_turns) × n`. `work_redone_turns` runs
the substring heuristic (`["conflict", "ws conflicts", "resolve",
"recover", "rebase"]`). The doctor+init dance is invisible to this
heuristic (no recovery substring). What R6 captured was a **secondary
signal**: in 1 of the 5 new-layout runs lacking a recover task (r007),
the agent's description text used the word "recovery" when force-
destroying a workspace; that bumped the median by 1; `median × n = 10`.

The `is_maw_arm()` predicate in `extract.rs` (`arm == "maw" ||
arm.starts_with("maw-")`) does NOT match the eval's arm names
`maw@old-layout` / `maw@new-layout` (start with `maw@`, not `maw-`), so
both arms fall into the substring fallback rather than the T2.5
attribution-driven path. The attribution path would have been
better-resolved (it requires prior `StepOutcome { conflicted: true }`)
but is not exercised by the SG3 eval.

---

## §4 Hypothesis verdict

| Hyp | Hypothesis (SP5 §6 risk)                                          | Verdict     |
|-----|-------------------------------------------------------------------|-------------|
| H1  | Path-length doubling (`.maw/workspaces/<n>` vs `ws/<n>`)          | **REFUTED** |
| H2  | Hidden-dir invisibility (`ls` doesn't show `.maw/`)               | **REFUTED** |
| H3  | AGENTS.md root-vs-stub guidance gap                               | **REFUTED** |
| H4  | New failure mode unrelated to SP5                                 | partial     |
| H5  | **Substrate/binary version skew** (NEW)                           | **SUPPORTED** |

**H1 (path-length) REFUTED**: Every workspace path the agent actually
USED in turns 3+ was `ws/<name>/` (v2 layout), not `.maw/workspaces/<name>/`.
The path-length doubling never had a chance to manifest because the
`.maw/` substrate was migrated away in turn 2.

**H2 (hidden-dir invisibility) REFUTED**: In r005 the agent did `ls /tmp/.tmpWZj085`
and got `README.md` (no `.maw/`) but immediately followed up with
`ls /tmp/.tmpWZj085/.maw/` based on `maw doctor`'s mention of `.maw`,
finding `config.toml workspaces manifold cache`. The agent navigated the
hidden dir without friction. None of the 10 runs got blocked on
visibility; they ALL found `.maw/` (through `maw doctor`'s output).

**H3 (AGENTS.md indirection) REFUTED**: The SG3 eval substrate is a
tempdir created from scratch, not a migrated existing repo. There is no
`AGENTS.md` at all in either substrate (old or new). The crib delivered
in the system prompt is the only guidance the agent reads; both cribs
are equivalent in structure and instructions.

**H4 (new unpredicted failure mode) — partial**: H5 below IS new and
unpredicted, but it's a measurement-design failure, not an
agent-behavior failure mode in the bone's intended sense.

**H5 (substrate/binary version skew) SUPPORTED (NEW)**: see §3. The
adapter simulates a future layout the installed binary doesn't
implement. The agent gets correct-but-undesired advice from `maw doctor`
("run maw init") and follows it, collapsing the new layout to v2 within
2 turns of every run. Every single observation in the new-layout arm at
C2 is contaminated by this collapse. The mechanism is uniform (10/10
runs trigger it) and decisive (the substrate is gone before any task is
attempted), satisfying the bone's "same root cause = same fingerprint"
test.

---

## §5 Fix recommendation

**Bone class: small (s).** Two cleanups, neither of which blocks
v1.0-pre.1:

### Fix A (the blocker fix): rebuild + reinstall + rerun SG3

The post-v0.61.0 main HEAD already contains:
- T3.2 (`42d7ca66`) — `LayoutFlavor::ConsolidatedMawDir`, default for
  new repos, v2 back-compat preserved.
- T3.3 (`f6cf96c1`) — `maw migrate` v2 → consolidated.

Installing `main` HEAD into `$PATH` and re-running
`just sg3-layout-eval` (or whichever recipe drives the eval) should
make `maw doctor` accept the `.maw/` substrate cleanly. **Expected
outcome**: the doctor+init dance disappears from new-layout runs;
new-layout fires drop from 6/10 to ~4/10 (matching old-layout); R6 flips
from FAIL to PASS_EQUIVALENT.

This is the v1.0-pre.1 in-scope fix: it's a build-and-rerun, not a code
change. The cost is the eval-run wall time (the published run took
2572s = ~43min and $9.05).

### Fix B (the secondary cleanup, NOT v1.0-pre.1 blocker)

`is_maw_arm()` in `crates/maw-bench-metrics/src/extract.rs:117` should
recognize `maw@<flavor>` arm names in addition to `maw` and `maw-…`:

```rust
fn is_maw_arm(arm: &str) -> bool {
    arm == "maw" || arm.starts_with("maw-") || arm.starts_with("maw@")
}
```

Without this, both old-layout and new-layout arms fall into the
substring-fallback heuristic that triggers on the natural-language word
"recover" in task batteries — making R6's signal noisy by construction.
The attribution-driven path (T2.5) is more principled and would have
both arms tied at the literal count of `maw ws recover` invocations.

### Fix C (deferred to v1.1 / post-pre.1)

The `median × n` proxy in `sg3_decision.rs::sum_proxy` amplifies
one-bit median shifts to ×n in R6's total. The doc-comment already
flags this: `"Production callers feeding R6 should plumb the raw per-
replicate sum once T2.5/T2.6 add the attribution-driven total"`. This
is a pre-existing instrumentation debt, not introduced by SG3, and is
v1.1 work.

---

## §6 v1.0-pre.1 layout-default recommendation

**Proposed (lead decides)**: option **(c)** — ship a fix in pre.1 that
addresses the root cause.

Specifically:

1. **Keep T3.2's `ConsolidatedMawDir` as the v1.0 default for new repos**
   (the source code is correct and `notes/sg3-layout-design.md` is
   sound). DO NOT revert T3.2.
2. **Before cutting pre.1, install the post-T3.2 binary locally** (per
   the bn-3uj4 acceptance criterion 4: `just install`).
3. **Rerun the SG3 eval against the pre.1 binary** (same seeds, same
   recipe). Add the GO/NO-GO output to the pre.1 release notes as the
   binding v1.0-pre.1 layout-eval evidence; supersede the
   2026-05-26 NO-GO with a footnote explaining the version-skew
   confound and pointing to the rerun.

**Rejected options**:
- (a) **revert T3.2 to ws/ default**: would discard correct code based
  on a measurement artifact; would require also reverting T3.3 (the
  migrate path) and the SG4 destroy-guidance wiring that depends on
  the consolidated layout. Cost: ~3 PRs of unwinding; benefit: nil.
- (b) **keep .maw/ default with CHANGELOG caveat (no rerun)**: ships
  v1.0 on uncertain ergonomics evidence. The pre-reg §5 failure-mode
  language requires a NO-GO writeup — but a NO-GO writeup that admits
  the measurement was version-skewed is worse for the trust artifact
  than a rerun.

**Cost of (c) vs urgency**: the rerun is ~43min + ~$9 + ~1 person-hour
to install / kick / file results. v1.0-pre.1 is a pre-release for a
multi-week dogfood window — there is time. If the rerun is also NO-GO,
then SP5's named risks (H1-H3) come back into play and a more thorough
investigation is warranted before v1.0 final; but the **expected**
outcome based on this forensic read is GO once the version skew is
removed.

---

## §7 Open items for the implementor

- The eval JSON files do NOT capture `maw --version` from the agent's
  environment. The `manifest.maw_version` field is empty in every run.
  Consider populating it from `cmd::run(["maw", "--version"])` at
  setup time so version-skew confounds are catchable at-source. This
  is a bench-harness fix; out of scope for bn-2ert but worth a tracking
  bone.
- Cell C0-T0 was R6 = 20 / 20 (PASS_EQUIVALENT) despite presumably hitting
  the same doctor+init dance. C0's benign condition (no overlapping
  edits, no merge contention) gives MORE breathing room — the heuristic
  fires equally on both arms because every run has plenty of natural
  recovery-vocab in its tasks. C2's "moderate hostility" amplifies the
  one-bit median shift because the variance is tighter. This explains
  why ONLY C2 tripped, despite the version skew being uniform across all
  cells.
