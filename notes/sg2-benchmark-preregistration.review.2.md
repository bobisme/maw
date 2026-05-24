# SG2 / maw benchmark preregistration — review 2

Reviewed: 2026-05-24  
Input: `/home/bob/src/maw/ws/bn-2ftq/notes/sg2-benchmark-preregistration.md`  
Pass 1: `/home/bob/src/maw/ws/bn-2ftq/notes/sg2-benchmark-preregistration.review.1.md`  
Output: `sg2-benchmark-preregistration.review.2.md`

## Executive verdict

**ACCEPT after small pre-run text fixes.** I do not recommend another broad methodology pass. §12 shows pass 1 already settled the major external-validity work: arm 4, concrete C4, blocking/randomization, work-loss split, jj crib, Wilson reporting, paired/bootstrap replacement for IQR overlap, task classes, friction metrics, and discard taxonomy. I did not reopen those decisions.

The remaining issues are narrow:

1. **Factual / Claude Code worktrees:** the arm-4 feature list is mostly right, but two details need to be made precise before running: `worktree.baseRef` is **not** an arbitrary base-ref knob, and `.worktreeinclude` is **not processed** when a `WorktreeCreate` hook replaces default git-worktree creation. Also, because the SG2 driver uses `claude -p`, the crib must capture the documented non-interactive cleanup behavior: `--worktree` + `-p` worktrees are not auto-cleaned.
2. **Structural / friction axis:** keep friction **report-but-don't-bar** for v1.0. Do **not** do a small SG2 dry-run whose results are then used to choose numeric bars; that would weaken the pre-registration by turning the pilot into disguised measurement. A harness-validation pilot is fine only if it is excluded from bar-setting and publication claims.
3. **Statistical:** the Wilson upper-bound table is correct. The paired bootstrap non-inferiority logic is defensible, but the prose should admit that it intentionally allows statistically real regressions below the ×1.15 materiality margin. The §4.3 “Wilson CI test OR point-estimate gap” language is misnamed: the point-estimate override is not a Wilson test. Finally, specify whether Cliff’s δ is unpaired/descriptive or replace it with a paired effect-size variant.

These are amendment-sized edits, not reasons to reject the preregistration.

## Scope discipline from §12

§12’s disposition table already accepted or adapted the pass-1 changes. In particular, it records that the agent-native worktree arm was added as §1.3 arm 4 / §8.1 crib / §7 R14; friction metrics were accepted with adaptation as informational-only at v1.0; Wilson reporting and paired bootstrap / Cliff’s δ were accepted as the replacement for IQR overlap; and product implications were deliberately deferred as non-preregistration material. This review only checks the three requested pass-2 targets.

## 1. Factual check: `claude-native-worktrees` surface

Sources checked: current official Claude Code docs on 2026-05-24:

- Claude Code documentation map, last updated `2026-05-23 01:34:53 UTC`: `https://code.claude.com/docs/en/claude_code_docs_map`
- CLI reference: `https://code.claude.com/docs/en/cli-reference`
- Worktrees guide: `https://code.claude.com/docs/en/worktrees`
- Hooks reference: `https://code.claude.com/docs/en/hooks`
- Week 19 changelog: `https://code.claude.com/docs/en/whats-new/2026-w19`

### 1.1 What §1.3 / §8.1 got right

The following claims are current and should remain:

- `claude --worktree` / `-w` is documented and starts Claude in an isolated git worktree under `.claude/worktrees/<name>` by default.
- If no worktree name is supplied, Claude Code auto-generates one.
- Claude Code supports worktree creation during a session via the `EnterWorktree` tool, though this does not need to be part of the benchmark arm unless the driver uses it.
- `.worktreeinclude` exists and copies selected gitignored files such as `.env` / `.env.local` into created worktrees.
- `WorktreeCreate` and `WorktreeRemove` are current hook event names.
- Subagents can use `isolation: worktree`.
- Claude Code has documented cleanup behavior for worktree sessions.

No feature in the current §1.3 / §8.1 list appears renamed or removed.

### 1.2 Required correction: `worktree.baseRef` is only `fresh` or `head`

Current docs say worktrees normally branch from the repo's default remote branch (`origin/HEAD`), falling back to local `HEAD` when remote/fetch is unavailable. The `worktree.baseRef` setting only accepts:

```json
{
  "worktree": {
    "baseRef": "fresh"
  }
}
```

or:

```json
{
  "worktree": {
    "baseRef": "head"
  }
}
```

The preregistration currently says `worktree.baseRef config` without implying arbitrary refs, so it is not strictly false. But it is underspecified enough to be risky. A reader could infer that `worktree.baseRef` can pin a benchmark-specific branch or SHA. It cannot. Specific PR checkout is a separate `claude --worktree "#1234"` / PR URL surface, not the `baseRef` setting.

**Suggested edit to §1.3 / §8.1:**

```text
`worktree.baseRef` pinned to either `"fresh"` (default: branch from the
remote default / `origin/HEAD`, falling back to local `HEAD` if needed) or
`"head"` (branch from local `HEAD`); the selected value is captured in the
run manifest. `worktree.baseRef` is not an arbitrary git-ref setting.
```

### 1.3 Required correction: `.worktreeinclude` and `WorktreeCreate` are mutually constraining

Current docs say `.worktreeinclude` is processed for default Claude Code git-worktree creation. They also say that a `WorktreeCreate` hook **replaces** default git behavior entirely, and when that hook is used, `.worktreeinclude` is **not processed**. Any env/config copying must then happen inside the hook.

The current text lists `.worktreeinclude` and `WorktreeCreate` / `WorktreeRemove` side by side. That is acceptable only if the manifest records which path was used:

- default git worktree creation + `.worktreeinclude`; or
- custom `WorktreeCreate` hook, with the hook itself responsible for ignored-file copying.

**Suggested edit to §8.1:**

```text
If default Claude Code git-worktree creation is used, `.worktreeinclude` may
copy selected gitignored files. If a `WorktreeCreate` hook is configured, it
replaces default git creation and `.worktreeinclude` is not processed; the hook
must perform any env/config copying itself. `WorktreeRemove` is recorded only
when custom cleanup is configured or invoked.
```

### 1.4 Required operational note: `claude -p --worktree` does not auto-clean

The preregistration’s driver is `claude -p --output-format json`. Current worktree docs say non-interactive runs created with `--worktree` alongside `-p` are not cleaned up automatically, because there is no exit prompt. They must be removed manually, e.g. with `git worktree remove`.

This matters because §1.1 includes `cleanup_success` and `orphaned_workspace_count`. If the arm uses `claude -p --worktree`, an uncleared worktree should not be misinterpreted as a surprising substrate failure; it is the documented behavior unless the task crib or harness includes explicit cleanup.

**Suggested edit to §8.1 or §8.6:**

```text
For non-interactive `claude -p --worktree` runs, Claude Code does not auto-clean
the worktree at session exit. The arm crib and oracle therefore treat cleanup as
an explicit step: either the agent is instructed to remove the worktree, or the
post-measurement harness cleanup is separated from measured `cleanup_success`.
```

### 1.5 Required preflight note: workspace trust

Current docs say `--worktree` exits with an error until the workspace trust dialog has been accepted once in that directory, including with `-p`.

This should be part of the arm-4 preflight, not a measured task behavior. Otherwise early arm-4 runs may enter `discard_auth` / harness-failure paths for a setup issue unrelated to the worktree substrate.

**Suggested edit to §8.2 or §8.6:**

```text
Before measured `claude-native-worktrees` runs, the benchmark accepts Claude
Code workspace trust once for each scenario repo/root. Trust-preflight failures
are harness setup failures, not substrate outcomes.
```

## 2. Structural check: friction axis informational-only at v1.0

### 2.1 Recommendation: keep report-but-don't-bar for v1.0

The current call is the right v1.0 trust posture. The friction metrics are exactly where maw may win, but there is no SP3 baseline. Choosing numeric bars after a small SG2 dry-run would create the failure mode §7 is designed to avoid: a measured pilot becomes the source of a tailored pass/fail threshold.

A dry-run can still be useful, but only for instrumentation sanity:

```text
Permitted: run a tiny unscored pilot to confirm the harness records friction
metrics correctly; exclude those data from SG2/SG3/SG4 analysis and do not use
them to set bars.

Not permitted without weakening the preregistration: run a pilot, inspect
friction values, choose numeric bars, re-freeze, and then present those bars as
pre-data constraints.
```

### 2.2 Why this does not leave v1.0 blind

The v1.0 report still has three protections:

1. The friction rows are recorded and published per arm / condition.
2. C0 is explicitly included to expose setup/lifecycle friction where there is no coordination contention.
3. The SG3 layout gate already bars on `turns_to_done`, `tool_calls`, `workflow_loss`, `wedge_incident`, and `interventions`, so large friction regressions should surface in verdict-bearing metrics even if the named friction counters are diagnostic only.

The only structural weakness is rhetorical: §3.1 says the SG3 gate is on “maw-internal ergonomic metrics,” while the most direct ergonomic/friction metrics are excluded from the gate. That is not fatal, but it should be made explicit so reviewers do not read it as a dodge.

**Suggested clarification to §3.1:**

```text
The SG3 gate is a safety/task-success/overall-efficiency non-regression gate.
The named friction counters are v1.0 diagnostics, not gate inputs, because no
SP3 baseline exists for calibrated friction bars. Their first measured baseline
is established by SG2 and may support future SG4 or post-v1.0 amendments, never
retroactive success claims.
```

## 3. Statistical check

### 3.1 Wilson upper-bound table: correct

The §6.1 Wilson upper bounds are correct to the shown precision:

| N | 0-event Wilson 95% upper bound |
|---:|---:|
| 10 | 0.2775 |
| 20 | 0.1611 |
| 50 | 0.0713 |
| 100 | 0.0370 |

The reporting rule is good: `0/N observed` must be reported with its Wilson interval and must not be phrased as “rate = 0” or “near zero.”

### 3.2 Paired bootstrap rule: defensible, but prose should say “materiality margin”

The §3.1 and §4.3 efficiency rule is:

```text
GO / not materially worse iff CI includes 0 OR median ratio <= ×1.15.
```

This is a non-inferiority / practical-margin rule. It intentionally allows a statistically real regression through if the median ratio is at or below ×1.15. Example: a tightly estimated +10% turn-count regression would pass.

That is not necessarily a bug. It is exactly what a materiality margin does. But the current parenthetical says “a regression must clear sampling noise to count,” which is incomplete. A regression must clear both:

1. statistical uncertainty; and
2. the pre-registered ×1.15 practical-materiality margin.

**Suggested edit:**

```text
A no-go efficiency regression exists only when the paired bootstrap CI excludes
0 in the worse direction AND the median ratio exceeds ×1.15. Regressions below
×1.15 may be statistically real, but are pre-registered as not material for this
gate.
```

Also specify the denominator:

- SG3: `median(new layout) / median(current layout)`.
- Dominance: `median(maw) / median(arm-X)` for lower-is-better metrics.

### 3.3 Bootstrap resampling unit should be explicit

The doc says “paired bootstrap” but should name the pair. Because §6.2 already blocks by `(condition, T-class, seed, replicate)`, that should be the bootstrap unit.

**Suggested edit:**

```text
Paired bootstrap resamples matched `(condition, T-class, seed, replicate)` units
with replacement; both arm outcomes for a unit are carried together.
```

This prevents an implementation from accidentally resampling per-arm runs independently and destroying the pairing.

### 3.4 §4.3 Wilson comparison rule is misnamed and slightly too loose if read as inference

§4.3 says rate comparisons use a “Wilson 95% CI test” defined as:

```text
X's lower bound >= maw's upper bound OR point-estimate gap exceeds +0.10
```

The first half is a conservative non-overlap rule using single-arm Wilson intervals. The second half is a practical point-estimate override. The combined rule is not a Wilson test.

This matters at N=10/20. For example, with maw `0/10` and arm X `2/10`, the point gap is +0.20 and would pass the `> +0.10` override, even though Wilson intervals overlap heavily. That may be acceptable as a pre-registered material-gap rule, but it should not be described as a CI test.

Two acceptable fixes:

**Minimal wording fix:**

```text
Rate comparisons use Wilson 95% CIs for display plus a pre-registered material
gap rule: either the Wilson intervals are separated in maw's favor, or the
point-estimate gap exceeds +0.10. The point-gap arm is a practical-effect rule,
not a statistical-significance test.
```

**Cleaner statistical fix:**

Use Wilson intervals for per-arm display, but use a paired binary difference rule for verdicts:

```text
For paired binary outcomes, compute the paired rate difference over matched
replicates and bootstrap its 95% CI. A rate win exists if the CI excludes 0 in
maw's favor OR the point-estimate gap exceeds the pre-registered +0.10 material
margin. Wilson CIs remain the per-arm reporting interval, especially for 0/N.
```

The cleaner fix is more consistent with the paired design, but the minimal wording fix is enough if the current decision rule is intentionally practical rather than inferential.

### 3.5 Hodges–Lehmann is fine; Cliff’s δ needs paired/unpaired clarification

The Hodges–Lehmann paired median difference is appropriate for paired efficiency comparisons.

The Cliff’s δ thresholds in §4.3 are the common Romano-style cutoffs:

```text
|δ| < 0.147 negligible
|δ| < 0.33  small
|δ| < 0.474 medium
|δ| >= 0.474 large
```

Those thresholds are not wrong. The issue is pairing. Standard Cliff’s δ is usually an independent-samples stochastic dominance statistic. The benchmark design is paired by seed/replicate. If the report computes ordinary all-pairs Cliff’s δ, it ignores the pairing even though the paired bootstrap / Hodges–Lehmann machinery uses it.

**Suggested edit:** choose one and name it:

```text
Option A: report ordinary Cliff's δ as an unpaired descriptive stochastic-
dominance statistic, explicitly not using the paired design.

Option B: replace Cliff's δ with a paired effect size, e.g. matched-pairs
rank-biserial correlation / sign dominance over paired differences, and report
that classification instead.
```

Option B is cleaner. Option A is acceptable only if the report labels it as descriptive and supplementary.

### 3.6 SG4 CI direction should be explicit

§3.2 says hostile-condition median `turns_to_done` must show ≥15% reduction and “paired bootstrap 95% CI excludes 0.” It should say excludes 0 **in the improvement direction**.

**Suggested edit:**

```text
paired bootstrap 95% CI for `(after - before)` excludes 0 on the negative side
and median(after) / median(before) <= 0.85
```

or equivalently:

```text
paired bootstrap 95% CI for `(before - after)` excludes 0 on the positive side
and median(before - after) / median(before) >= 0.15
```

Without this direction, a worsening CI that excludes 0 technically satisfies the literal “excludes 0” phrase, even though the adjacent “≥15% reduction” makes the intended direction obvious.

## Recommended patch list

Apply these before measured runs; no new review pass needed unless the changes alter the intended rules rather than clarify them.

1. In §1.3 / §8.1, say `worktree.baseRef` accepts only `"fresh"` or `"head"`; record the chosen value.
2. In §8.1, state that `.worktreeinclude` is only processed under default git-worktree creation and not when `WorktreeCreate` replaces creation.
3. In §8.1 / §8.6, add the non-interactive `claude -p --worktree` cleanup note and the workspace-trust preflight note.
4. In §3.1 / §4.3, reword the paired-bootstrap rule as a practical non-inferiority margin: no-go only if the CI excludes 0 in the worse direction **and** the median ratio exceeds ×1.15.
5. In §4.3, stop calling the rate comparison a “Wilson CI test” if the `+0.10` point-estimate override remains. Either label it as a practical-effect override or switch verdicts to paired binary bootstrap rate differences while keeping Wilson intervals for display.
6. In §4.3, specify the Cliff’s δ variant or replace it with a paired effect size.
7. In §3.2, specify the direction of the SG4 improvement CI.
8. Keep R-friction informational-only for v1.0; do not run a bar-setting dry-run. Add a sentence allowing harness-only pilots that cannot set bars or support claims.

## Final recommendation

**ACCEPT AFTER SMALL FIXES.** The current preregistration is still strong. The pass-2 factual findings do not invalidate the `claude-native-worktrees` arm; they just require the arm surface to be pinned accurately. The friction-axis choice is methodologically defensible as written. The statistical machinery is mostly sound, but the comparison language should be tightened so readers can distinguish formal CIs from practical materiality rules.
