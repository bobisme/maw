# SG3 cross-workspace contamination forensic (bn-das6)

**Date:** 2026-05-28
**Author:** maw-dev (lead)
**Blocks:** bn-3uj4 (v1.0-pre.1 cut) — layout-default decision
**Data:** `notes/eval-real-2026-05-28/sg3-final/maw-{old,new}-layout/` (60 BenchRuns, sonnet, C0+C2)
**Method script:** `notes/eval-real-2026-05-28/sg3-final/classify_contamination.py`
**Raw output:** `notes/eval-real-2026-05-28/sg3-final/contamination_rows.json`

---

## The question

Does the new consolidated `.maw/` layout (source files live at the repo root /
default workspace; agent workspaces hidden under `.maw/workspaces/<name>/`) cause
agents to **accidentally edit the integration target** more than the old `ws/`
layout did?

The hypothesis (bn-das6 reframe): the old layout had a structural visual cue —
agent workspaces at `ws/<name>/`, **no source anywhere at root** — so editing a
root file was obviously wrong. The new layout removes that cue (source *is* at
root). The only remaining guardrail is `maw exec` cwd + AGENTS.md / `maw ws create`
output text. **We never measured whether that handoff actually works.** This is
that measurement.

This is the *real* layout concern. The R6 `--into` vs `--to` vocabulary friction
is separately tracked and is fixable via aliasing/help — not a reason to pick a
layout.

---

## Method

For all 60 runs (30 per layout: 20× C0 lifecycle, 10× C2 file-edit), every
file-mutating operation was classified by resolved target path:

- **Write/Edit tool calls** — absolute `file_path`, classified directly.
- **Bash writes** — redirect (`>`,`>>`), `tee`, heredoc targets. Shell variables
  (`WS=…/.maw/workspaces/ws-0`) are tracked per-run and expanded; `maw exec <ws> --`
  context is detected so relative paths resolve inside that workspace.

Buckets, relative to the per-run substrate root:
- `in_ws` — under a non-default workspace (`.maw/workspaces/<name>/` or `ws/<name>/`) → **correct**
- `at_root` — at substrate root, no workspace prefix (new layout: *this is the default/integration target*) → **contamination**
- `ws_default` — under `ws/default/` (old layout integration target) → **contamination**
- `admin` — `.maw/` or `.git/` non-workspace path → likely accidental

**Two independent methods**, cross-checked:
1. **Command-parse** — classify the path the agent *targeted* in each tool call (intent).
2. **Execution ground truth** — scan `substrate_final_files` for task files (`shared/…`,
   `ws-N/…`, `file-N.txt`) that *materialized* at the root rather than under a workspace.

The C2 task asks the agent to edit `shared/file-1.txt`, `shared/file-3.txt`,
`ws-0/file-0.txt` **inside workspace ws-0**. Note the task's own filenames contain
a `shared/` and `ws-0/` prefix, so the correct absolute target is e.g.
`.maw/workspaces/ws-0/shared/file-1.txt`. (Early parsing flagged `…/ws-0/ws-0/file-0.txt`
"doubled" paths as suspicious — they are **correct**: the inner `ws-0/` is the
task-specified filename.)

---

## Headline result: zero integration-target contamination in *either* layout

**Primary metric — runs that wrote to the integration root/default:**

| Layout | Cell | n  | Contaminated runs | Rate (Wilson 95% CI) |
|--------|------|----|-------------------|----------------------|
| old `ws/`   | C0 | 20 | 0 | 0.0% [0.0, 16.1] |
| old `ws/`   | C2 | 10 | 0 | 0.0% [0.0, 27.8] |
| old `ws/`   | **ALL** | **30** | **0** | **0.0% [0.0, 11.4]** |
| new `.maw/` | C0 | 20 | 0 | 0.0% [0.0, 16.1] |
| new `.maw/` | C2 | 10 | 0 | 0.0% [0.0, 27.8] |
| new `.maw/` | **ALL** | **30** | **0** | **0.0% [0.0, 11.4]** |

**Execution ground truth** (`substrate_final_files`): **0/30** runs in *either*
layout left a task file at the integration root. The only root-level entry in any
run is the baseline `README.md` seeded into the substrate — never a task file.

77 file-writes total were captured and **every one resolved into a workspace**
(ws-0 or ws-1); none to the root, `ws/default`, or admin paths. Not a single agent
in either layout issued a bare relative write (`echo … > shared/file-1.txt`) from
the root cwd.

**The bn-das6 hypothesis is refuted.** Removing the `ws/`-layout structural cue did
**not** cause agents to edit the integration target. New layout = old layout = zero.

---

## Secondary: cross-workspace writes (wrote to ws-1) — symmetric, not a layout effect

The C2 task expects all edits in **ws-0**. Some runs wrote into **ws-1** instead of
or in addition to ws-0 — but at *identical* rates across layouts:

| Layout | C2 cross-ws-to-ws-1 | Rate (Wilson 95% CI) | Runs |
|--------|---------------------|----------------------|------|
| old `ws/`   | 2/10 | 20.0% [5.7, 51.0] | r002, r007 |
| new `.maw/` | 2/10 | 20.0% [5.7, 51.0] | r003, r008 |

Identical point estimate **and** CI. Drivers (all benign, all *inside a workspace* —
never the root):
- **recover-rename** (old r007, new r003): agent destroyed ws-0, recovered it via
  `maw ws recover ws-0 --to ws-1`, then made the task edits in the recovered ws-1.
  The work is correct and recoverable, just in a renamed workspace.
- **over-completion** (old r002): agent edited ws-0 *per task* and *also* added extra
  files to ws-1.
- **workspace confusion** (new r008): agent edited in ws-1 instead of ws-0, then
  destroyed ws-1. The one genuine misdirection — and it has an exact analog absent
  only by coin-flip in the old layout (the recover-rename cases are the same class).

This is workspace↔workspace, fully covered by the Prime Invariant (every destroy
snapshots; every workspace merges). It is **not** the root-contamination failure
mode the layout question is about, and it shows **no layout sensitivity**.

---

## Why the new layout is safe: the textual guardrail works

The structural visual cue is gone in the new layout, but `maw ws create` emits an
**equally strong textual cue in both layouts**, verbatim:

```
  Path:   <abs>/.maw/workspaces/ws-0/        (new)   |   <abs>/ws/ws-0/   (old)
  IMPORTANT: All file reads, writes, and edits must use this path.
  This is your working directory for ALL operations, not just bash.
```

Agent behavior confirms the handoff lands: across 77 writes, agents consistently
used (a) absolute workspace paths, (b) a `WS=<abs workspace path>` variable, or
(c) `maw exec <ws> --` context. The cwd + create-output guardrail specified in
T3.4 / bn-1jqo is doing its job. The old layout's structural cue turns out to be
**redundant** with the textual one, not load-bearing.

---

## Verdict & recommendation for bn-3uj4

**GO for the new consolidated `.maw/` layout as the v1.0 default**, on the
contamination axis.

- Root/default contamination: **0/30 vs 0/30**. No differential risk. Hypothesis refuted.
- Cross-workspace writes: **2/10 vs 2/10**, identical, benign, recoverable, layout-insensitive.
- Mechanism understood: the `maw ws create` "IMPORTANT: …use this path" output is the
  real guardrail and it works equally in both layouts; the old `ws/` structural cue
  was redundant.

### Caveats (do not over-claim)
- **sonnet only.** Plan D found a haiku overkill regime at C0–C3; weaker models may
  not honor the textual path cue as reliably. If layout safety must hold for haiku,
  that needs its own (cheap) transcript pass — but it does not block choosing the
  default for the sonnet-class target.
- **Wide CIs.** 30 runs/layout bounds root contamination at ≤11.4% (upper 95%), not 0%
  in truth. The claim is *no differential* between layouts (both observed 0), not
  *impossible*.
- **Abstract tasks.** Benchmark filenames include literal `shared/`/`ws-0/` prefixes;
  real source-tree edits may stress path selection differently. The guardrail
  mechanism (explicit absolute path in create output) is task-agnostic, so this is a
  minor caveat.

### Follow-ups (separate bones, not blockers)
- Optional: cheap haiku transcript pass to confirm the textual guardrail holds for
  weaker models before GA.
- The new-layout `maw ws create` output is the load-bearing guardrail — keep the
  "IMPORTANT: …use this path" line prominent; treat it as a tested invariant, not
  cosmetic copy.
