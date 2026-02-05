# Eval Report: Post-Merge Visibility + Push Workflow

**Date**: 2026-02-05
**Features tested**: Post-merge rebase, bookmark management in merge, `maw push`
**Beads**: bd-2kn, bd-2c1
**Model**: claude-haiku-4-5-20251001
**Agent count**: 6 (3 before, 3 after)

## Summary

Post-merge rebase and automatic bookmark management are **high-impact improvements**.
They cut tool calls by ~50% and eliminate confusion entirely in S1 and S2 scenarios.
The full workflow (S4) was already clean and shows no significant delta.

## Results

### Metrics Table

| Scenario | Variant | Tool Calls | Errors | Confusion | Goal | Efficiency | Confusion Rate |
|----------|---------|-----------|--------|-----------|------|------------|----------------|
| S1: Post-merge visibility | before | 15 | 1 | 8 | Yes | 6.7 | 0.53 |
| S1: Post-merge visibility | **after** | **8** | **0** | **0** | Yes | **12.5** | **0.00** |
| S2: Push workflow | before | 15 | 1 | 7 | Yes | 6.7 | 0.47 |
| S2: Push workflow | **after** | **7** | **0** | **0** | Yes | **14.3** | **0.00** |
| S4: Full workflow | before | 15 | 0 | 0 | Yes | 6.7 | 0.00 |
| S4: Full workflow | **after** | ~15 | **0** | **0** | Yes | ~6.7 | **0.00** |

### Deltas

| Scenario | Tool Calls | Errors | Confusion | Notes |
|----------|-----------|--------|-----------|-------|
| S1 | **-47%** (15 → 8) | -100% | **-100%** | Files immediately visible after merge |
| S2 | **-53%** (15 → 7) | -100% | **-100%** | Push works on first try |
| S4 | ~0% | 0 | 0 | Already clean; features don't hurt or help |

## Scenario Narratives

### S1: Post-Merge File Visibility

**Before**: Agent found old files in `src/` — `main.rs` still had the original scaffold content, `lib.rs` didn't exist. Investigated with `jj show main` and `jj diff -r main`, confirming the changes existed in the commit graph but weren't on disk. Tried `jj checkout main` (git muscle memory) which failed. Recovered with `jj new main`. Required 15 tool calls with heavy confusion investigating why the working copy didn't reflect the merge.

**After**: Agent found both files immediately. `ls src/` showed `main.rs` and `lib.rs`. Read both, confirmed `greet` and `add` functions. Even ran `cargo test` (both tests passed). Completed in 8 tool calls with zero confusion. The post-merge rebase feature worked exactly as intended — after `maw ws merge`, the default workspace's working copy was rebased onto the new main, so on-disk files reflected the merge immediately.

**Verdict**: Post-merge rebase is the single most impactful feature tested. Eliminates a class of confusion that would affect every agent every time.

### S2: Push Workflow

**Before**: Agent needed to push already-merged changes. Tried `jj bookmark set main -r @-` — got "Refusing to move bookmark backwards or sideways." Confused, investigated with `jj log`. Tried `jj git push` — got "No bookmarks found in the default push revset. Nothing changed." More confusion. Listed bookmarks with `jj bookmark list -a`, discovered the tracking relationship, and finally succeeded with `jj git push --branch main`. Required 15 tool calls with significant confusion about jj's bookmark/push model.

**After**: Agent checked log/status/remote, tried `jj bookmark set main -r @-` — "Nothing changed" (bookmark already correct from merge). Ran `jj git push` — succeeded immediately. Verified with hash comparison. Completed in 7 tool calls with zero confusion. The merge now correctly positions the main bookmark, so `jj git push` works without manual bookmark management.

**Verdict**: Automatic bookmark management during merge eliminates the most confusing jj concept for agents (bookmarks + push revsets). Cuts the push workflow in half.

### S4: Full Workflow (create → edit → merge → push)

**Before**: Agent created workspace `dev`, wrote `lib.rs` with a `multiply` function, described the commit, merged with `--destroy`, set bookmark, and pushed. Clean execution: 15 tool calls, 0 errors, 0 confusion. The agent understood `maw ws` commands and raw `jj` well enough to complete the full cycle.

**After**: Nearly identical. Agent used `maw ws create`, wrote file, `maw ws jj dev describe`, `maw ws merge dev --destroy`, then set bookmark and pushed with raw `jj git push`. Also ~15 tool calls, 0 errors, 0 confusion. The merge output suggested `maw push` but the agent used raw `jj git push` instead.

**Verdict**: No meaningful delta. The full workflow was already smooth. This is good — the new features don't regress the happy path. But it also shows S4 doesn't exercise the specific pain points (stale working copy, broken bookmark state) that S1 and S2 target.

## Key Findings

### 1. Post-merge rebase is the highest-impact feature
The working copy being stale after merge was the #1 source of agent confusion. Agents can't reason about jj's commit graph vs. on-disk state distinction — they expect `ls` and `cat` to show what was just merged. The rebase makes this "just work."

### 2. Automatic bookmark management during merge is critical
jj's bookmark/push model is the #2 confusion source. By having merge set the bookmark correctly, `jj git push` works on the first try. This eliminates the entire "why won't it push" debugging cycle.

### 3. `maw push` wasn't discovered or used
In the S4-after scenario, the merge output printed `Next: push to remote: maw push`, but the agent still used raw `jj git push`. In S2-after, the merge was already done (pre-merged scenario), so the agent never saw the hint.

**Implication**: `maw push` needs stronger discoverability, or agents need to be trained to prefer `maw` commands over raw `jj`. However, since the bookmark is now set correctly by merge, raw `jj git push` works fine — so `maw push` is more of a convenience (sync checks, error messages) than a necessity.

### 4. All agents succeeded — features don't break anything
6/6 agents completed their tasks. The new features reduced friction without introducing new failure modes.

### 5. Haiku-4.5 is capable but has jj knowledge gaps
The S1-before agent tried `jj checkout` (a git command, not jj). The S2-before agent didn't know about `--branch` flag until it investigated. These are model knowledge issues, not maw issues, but maw's job is to paper over them.

## Recommendations

1. **Ship post-merge rebase** — clear, quantifiable improvement.
2. **Ship automatic bookmark management** — eliminates the push confusion entirely.
3. **Consider making `maw push` output more prominent** — e.g., if agent runs `jj git push` and it fails, maw could intercept and suggest `maw push`. Or the merge output could be even more directive.
4. **S4 needs a harder variant** — the current S4 is too clean to surface issues. Consider adding: stale workspace mid-workflow, conflict during merge, or push to a repo with upstream changes.

## Raw Data

| Agent ID | Scenario | Dir |
|----------|----------|-----|
| a210d3a | S1-before | /tmp/maw-eval-before-v2/s1-before |
| a47e673 | S1-after | /tmp/maw-eval-2743202/s1-after |
| ab5e1e6 | S2-before | /tmp/maw-eval-before-v2/s2-before |
| a025c95 | S2-after | /tmp/maw-eval-2743779/s2-after |
| ad819fc | S4-before | /tmp/maw-eval-before-v2/s4-before |
| adb58df | S4-after | /tmp/maw-eval-2744257/s4-after |

Transcripts: `/home/bob/.claude/projects/-home-bob-src-maw/4f21e327-ba47-412a-a421-b4eee9146753/subagents/`
