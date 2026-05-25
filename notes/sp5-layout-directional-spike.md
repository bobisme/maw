# SP5 — layout-ergonomics directional spike

**Bone**: bn-2kgu · parent SG3 bn-2yh1 · spike · size m
**Gates**: T3.2 (bn-2sw3) implementation strategy
**Pre-reg status**: HARNESS-VALIDATION ONLY per `notes/sg2-benchmark-preregistration.md` §3.1 Pilot rule. SP5 numbers MUST NOT set bars and MUST NOT appear in the SG2/SG3/SG4 publication. The binding real-LLM gate remains T3.5 (bn-1uzn) + bn-iux4.
**Date**: 2026-05-25

---

## DIRECTIONAL VERDICT: **positive**

The proposed consolidated `.maw/workspaces/` layout passes every directional check the SP5 spike can perform under MockAgent fidelity: byte-equivalent integrated tree, on-spec structural deltas (+1 path depth, +13 char workspace path), green end-to-end lifecycle on both arms, near-identical wall time (engine-isolated), and a qualitative ergonomics win in the visible-entries SHAPE (project-shaped root vs admin-scaffolding-shaped root).

**T3.2 recommendation**: ship the consolidated `.maw/` layout **by default**, with `maw migrate` (T3.3) as the recommended one-shot path for existing repos. T3.5 (bn-1uzn) + bn-iux4 retain the formal real-LLM gate authority; if T3.5 surfaces an unexpected real-agent regression, T3.2 will need to ship a config-knob fallback at that point.

---

## 1. What this spike is and is not

**Is**: a *directional, mechanical* read on whether the proposed consolidated `.maw/` layout introduces any structural ergonomics regressions that should be caught **before** T3.2 commits to xl implementation. The output gates T3.2's strategy choice (consolidated-by-default vs configurability-only); it does not commit T3.2 to any implementation detail.

**Is not**:
- A formal benchmark — see T2.6 / `crates/maw-bench-sweep/` + the future T2.6 condition-spectrum campaign.
- A T3.5 (bn-1uzn) substitute — T3.5 runs real LLMs against both layouts and is the binding gate.
- A source of pre-registered bars — pilot-rule discipline per `notes/sg2-benchmark-preregistration.md` §3.1 applies verbatim. SP5 data is excluded from publication.

The HARD RULE from the bone description ("DO NOT run a real LLM campaign. Mock-agent + a small pilot grid") was honored: the spike's data is structural, not agent-behavioral.

---

## 2. Adapter approach — simulation (not path-translation-over-real-maw)

The spike adds two sibling adapters to `crates/maw-bench-adapters/`:

- `ws_layout_adapter::WsLayoutAdapter` — simulates the **current v2 `ws/` layout** (root bare + `ws/default/` privileged + `ws/<name>/` agents).
- `consolidated_layout_adapter::ConsolidatedLayoutAdapter` — simulates the **proposed consolidated `.maw/` layout** (root non-bare = privileged target; `.maw/workspaces/<name>/` agents; `.maw/manifold/`, `.maw/.gitignore`, `.maw/config.toml`, reserved `.maw/cache/`).

Both adapters use the same underlying engine (plain `git worktree`) so the *only* delta between BenchRuns is the on-disk path shape — exactly the layout variable SP5 asks about. This is the load-bearing isolation: SP4 (`notes/layout-engine-impact.md`) already proved the maw merge engine is layout-agnostic (relocation, not rewrite); SP5 does not need to re-prove that. SP5 only needs the *layout-only* delta, and the simulated git-worktree approach gives it cleanly.

**Why simulation, not real-maw + path-translation?** A wrapped real-maw approach would require either (a) a layout config-knob on `maw` that doesn't exist yet (this spike's whole point is to gate T3.2's implementation), or (b) a chroot/bind-mount layer that biases the measurement with implementation artifacts unrelated to the layout question. Simulation keeps the measurement honest.

**Per the 2026-05-25 terminology decision in the bone description**: this simulation uses `.maw/workspaces/` (not `.maw/worktrees/` as SG3 §1.3 originally specified). The SG3 design doc is being amended in the T3.2 downstream; SP5 honors the override.

---

## 3. Pilot grid and op-stream

Adheres to T2.7 §3.1 Pilot rule (small N for harness validation; bars-forbidden):

- **2 arms**: `sp5-ws-layout`, `sp5-consolidated-layout`.
- **2 task shapes per arm** (one collision-light, one with overlap), interleaved into a single op-stream so both arms drive an identical sequence.
- **N=3 replicates per arm** for wall-time distribution (override with `SP5_REPS=N`).
- Wall budget: < 1s in practice (well under the bone's ≤60s pilot cap).

The op-stream:

```rust
// Task 1: two independent agents, no overlap.
Create slot0 ; Create slot1
Edit slot0 src/lib_a.rs ; Edit slot1 src/lib_b.rs
Commit slot0 ; Commit slot1
Merge [slot0, slot1] → default --destroy

// Task 2: one agent touching an already-merged file.
Create slot0
Edit slot0 src/lib_a.rs (extends existing)
Commit slot0
Merge [slot0] → default --destroy
```

---

## 4. Pilot data (illustrative single-run; reproduce with `just sp5-pilot`)

| metric | ws (current default) | consolidated (proposed) | delta |
|---|---|---|---|
| adapter arm | `sp5-ws-layout` | `sp5-consolidated-layout` | — |
| layout shape | `ws/<name>/` (root bare, `ws/default` privileged) | `.maw/workspaces/<name>/` (root non-bare, root is target) | — |
| workspace path depth | 2 | 3 | +1 |
| workspace path chars (abs, slot=2) | 23 | 36 | +13 |
| visible top-level (ls, no -a) | 2 entries | 2 entries | +0 |
| visible entries | `repo.git, ws` | `README.md, src` | — |
| lifecycle ok (script all-green) | true | true | — |
| end-state live workspaces | 0 | 0 | — |
| end-state integrated files | 4 | 4 | — |
| wall-time (single run, ms) | 130 | 129 | -1 |

Wall-time distribution (N=3 reps/arm):

| arm | min (ms) | median (ms) | max (ms) |
|---|---|---|---|
| `sp5-ws-layout` | 128 | 130 | 136 |
| `sp5-consolidated-layout` | 126 | 128 | 130 |

Engine-equivalence bias check: medians are within ~5% of each other, confirming the layouts are engine-equivalent and the structural signals (depth, chars, visibility shape) are the load-bearing read.

**Equivalence check**: PASS — both layouts produce byte-identical integrated trees under the pilot op-stream (modulo `.gitignore`, which is layout-private and excluded by design).

---

## 5. Per-metric directional read

### 5.1 Workspace path depth (`+1`)
Consolidated layout inserts a single `.maw/` admin level (`<root>/.maw/workspaces/<name>` vs `<root>/ws/<name>`). One extra path component per `cd`/`ls`/`find` mental model. Within the expected envelope; the bone description ratified this insertion as part of the SG3 design.

**Read**: neutral. The depth cost is the *cost of admin namespacing* (cf. `.git/`, `.claude/`, `.jj/`) — agents already handle this idiom routinely. Real-LLM regression here would surface as longer `cd` paths in transcripts; T3.5 measures.

### 5.2 Workspace path char length (`+13`)
At the slot=2 sample (`<tempdir>/ws/slot-2-...` vs `<tempdir>/.maw/workspaces/slot-2-...`), the consolidated layout adds 13 chars. For workspace names of typical length (`bn-abcd` = 7 chars), per-command overhead is `len(".maw/workspaces/") - len("ws/") = 12 chars`. Small absolute cost; cumulative over a multi-turn agent run is modest.

**Read**: neutral. Path-length cost is bounded; real-LLM impact (if any) is measurable in T3.5.

### 5.3 Visible top-level entries (count `+0`; SHAPE win)
Both layouts show 2 entries to `ls` (no `-a`). The *shape* differs sharply:
- v2 ws-layout: `repo.git, ws/` — admin scaffolding.
- Consolidated: `README.md, src/` — project content.

For an agent dropped into `<root>` with `ls`, the consolidated layout is immediately recognizable as a project tree; the v2 layout requires navigation into `ws/default/` to see source.

**Read**: directional **positive** — this is the spike's strongest qualitative signal. We cannot quantify it under MockAgent (it requires real-agent path-discovery behavior), but the shape change is on-spec and aligns with the design intent ("the root IS the merge target", `notes/sg3-layout-design.md` §2.1).

### 5.4 Lifecycle green on both arms (`true`)
Create → edit → commit → merge → destroy → state-snapshot completes without error on both layouts. Bone AC #1 satisfied.

**Read**: positive. No layout-induced lifecycle regression detected.

### 5.5 Engine equivalence (wall-time medians within ~5%)
Both layouts produce near-identical wall times under the same op-stream, confirming the engine is layout-agnostic at the structural level. Any larger delta would have suggested incidental drag (path-traversal cost in the deeper consolidated tree).

**Read**: positive. The engine-equivalence assumption from SP4 holds operationally under the simulated harness.

### 5.6 Integrated-tree equivalence (byte-identical)
Both layouts produce identical agent-task-visible files after the same op-stream (modulo `.gitignore`, which is by-design layout-specific). This is the most load-bearing equivalence: a real T3.2 implementation that produces a different integrated tree would have surfaced as a behavioral regression here.

**Read**: positive. The consolidated layout does not introduce content drift at the integration head.

---

## 6. Named risks (T3.2 design should mitigate)

These are SP5's named risks for the downstream T3.2 implementation. None are blockers; all are MockAgent-undetectable and properly fall to T3.5's real-LLM measurement.

1. **Hidden-dir invisibility (MockAgent-undetectable)**. `.maw/` is dotfile-prefixed and hidden from `ls` (no `-a`) / `find` (no `-name '.*'`) by default. A real LLM that habitually uses `ls`/`find` to discover workspace structure may need explicit instructions or a visible `.maw/AGENTS.md`-stub redirector. **T3.2 mitigation**: ensure the agent crib (per `notes/sg2-benchmark-preregistration.md` §8.1) names `.maw/workspaces/` explicitly; do not rely on path-discovery.

2. **`cd .maw/workspaces/<name>` path-length doubling**. The on-disk path is 12 chars longer per command. In a sandboxed environment where every `Bash` call uses absolute paths (per `ws/default/AGENTS.md` "Output Guidelines"), this cost is paid every turn. **T3.2 mitigation**: the `worktree` / `wt` clap aliases (T3.4 scope) become the canonical agent surface — `maw exec <name> -- <cmd>` is the path-agnostic interface and should be promoted in the crib.

3. **AGENTS.md indirection at the new root**. The v2 layout had `ws/default/AGENTS.md` as the authoritative source plus a root stub. The consolidated layout makes the root IS the source — AGENTS.md becomes a tracked file at root. **T3.2 mitigation**: ensure `maw migrate` (T3.3) handles the AGENTS.md root-vs-stub transition cleanly, with a clear migration message.

4. **Equivalence-check tolerance for `.gitignore`**. The spike's equivalence check excludes `.gitignore` because each layout pins its own ignore rules (ws ignores `ws/`; consolidated ignores `.maw/`). T3.2's migration must verify the on-disk `.gitignore` produced after migration matches what a fresh consolidated init would produce — otherwise a migrated repo and a fresh-init repo diverge silently.

5. **The simulation is a proxy, not the real implementation**. The adapters use plain `git worktree`; the real T3.2 implementation will land in maw's merge engine. SP4 proved the engine is layout-agnostic, but SP5 does not exercise the real engine. **T3.2 must run the existing maw test suite (1931 tests) against the new layout** as the first downstream gate.

---

## 7. T3.2 implication recommendation

**Recommendation**: ship consolidated `.maw/` layout **by default** + `maw migrate` (T3.3) as the recommended path for existing v2 repos.

Justification:
- Directional signal is **positive** (5.1–5.6 above).
- The bone explicitly framed the negative branch as "ship as configurability with the OLD default still being `ws/` (config-knob present, no on-disk move)"; that fallback is only required on negative signal, which we do not have.
- The visible-entries SHAPE win (§5.3) is on-spec and aligns with the SG3 design intent.
- T3.5 (bn-1uzn) retains the binding real-LLM gate authority. If T3.5 surfaces a regression, T3.2 must ship a config-knob fallback at that point; until then, the recommended path is single-default + migration.

**Alternative considered**: configurability with old default + on-disk move only opt-in. Rejected because (a) the directional signal does not warrant the maintenance cost of two parallel layouts, (b) two defaults mean the agent crib must teach two layouts (worse agent ergonomics by construction), and (c) T3.5 can still mandate the fallback later — the alternative is not foreclosed by the recommended path.

---

## 8. Reproduction

```bash
# From the workspace, build everything once:
cargo check -p maw-bench-adapters --features bench

# Run the pilot (prints verdict to stdout):
just sp5-pilot

# Write Markdown output to a file in addition to stdout:
just sp5-pilot /tmp/sp5-out.md

# Override the wall-time replicate count (default 3):
SP5_REPS=10 just sp5-pilot
```

Adapter sources:
- `crates/maw-bench-adapters/src/ws_layout_adapter.rs`
- `crates/maw-bench-adapters/src/consolidated_layout_adapter.rs`
- `crates/maw-bench-adapters/src/bin/sp5_layout_pilot.rs`

---

## 9. Constraints for downstream bones

- **bn-2sw3 (T3.2)**: proceed with consolidated-by-default + `maw migrate` per §7 above. The named risks in §6 must be addressed in T3.2's design phase, not deferred to T3.5.
- **bn-3kkl (T3.3, migration)**: SP5 does not exercise migration logic; T3.3 is unconstrained by this spike beyond §6 risks 3 + 4 (AGENTS.md handling, gitignore parity).
- **bn-1jqo (T3.4, guardrail)**: the `worktree` / `wt` clap aliases mentioned in the bone description are NOT exercised here (they require a real maw binary with the new layout); T3.4 inherits scope unchanged.
- **bn-1uzn (T3.5)**: retains binding real-LLM gate authority. SP5 is a directional filter; T3.5 is the formal measurement.
- **bn-iux4 (parallel pre-reg)**: SP5's data must be excluded from the pre-reg dataset per T2.7 §3.1. The pre-reg should not cite SP5 numbers as evidence.

---

## 10. Pilot-rule discipline (frozen)

This spike's data is HARNESS-VALIDATION ONLY:
- MUST NOT set publication bars.
- MUST NOT appear in SG2/SG3/SG4 publication.
- MUST NOT feed any pre-registered claim.
- The verdict above is **directional**, not statistical. N=3 reps is a harness check, not a power calculation.

The `just sp5-pilot` recipe stamps the output with this notice automatically.
