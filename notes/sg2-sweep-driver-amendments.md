# SG2 sweep-driver pre-reg amendments (T2.6 / `bn-3l1f`)

**Status:** companion doc to `notes/sg2-benchmark-preregistration.md`.
**Scope:** documents the driver-side decisions T2.6 makes that the
frozen pre-reg leaves under-specified. These are NOT §9 amendments
(no measured run exists yet); they are pre-acceptance T2.6
implementation choices. If a measured run is started under any of
these, the choices are logged here and remain frozen for that
campaign.

---

## A1. `ConditionProfile` mapping (driver-side)

The pre-reg §5 names the four spectrum knobs abstractly:
`K_overlap`, `K_concurrency`, `K_rounds`, `between-rounds`. The
scenario generator (`maw_scenario::ConditionProfile`) carries a
different but parallel set:
`concurrency_degree`, `mid_op_kill_prob`,
`overlapping_edit_rate`, `stale_workspace_rate`. T2.6 must compose
the two. The frozen mapping (encoded in
`crates/maw-bench-sweep/src/grid.rs::ConditionPoint::to_profile`):

| §5 knob              | `ConditionProfile` field        | Rule                                                          |
| -------------------- | ------------------------------- | ------------------------------------------------------------- |
| `K_concurrency`      | `concurrency_degree`            | Direct: `K_concurrency` as `u8`.                              |
| `K_overlap / 8`      | `overlapping_edit_rate`         | The §5 fractions (`0/8 … 8/8`) become `0.0 … 1.0`.            |
| `between-rounds`     | `stale_workspace_rate`          | `0.2` if `burst`, else `0.0` (burst races over stale epoch).  |
| _(unused at §5)_     | `mid_op_kill_prob`              | **`0.0`** — fault injection is SG1's domain.                  |
| _(unused at §5)_     | `K_rounds`                      | Carried in `ConditionPoint` for diagnostic display only; the  |
|                      |                                 | scenario generator's plan length is sized separately at T2.6. |

**Why `mid_op_kill_prob = 0.0` on the headline sweep.** SG2
measures coordination contention (the variable SP3 §1 proved
drives the jj wedge); SG1 measures fault recovery
(`notes/sg1-soak-campaign.md`). Mixing fault injection into the
SG2 spectrum would conflate the two failure modes the pre-reg
deliberately keeps orthogonal. A future T2.6 amendment (logged
here under A1.1) may surface a fault-rate sweep axis if a
measurement need emerges.

---

## A2. Per-cell `N` defaults (driver-side)

Pre-reg §6.1 frozen:
- Headline cells: **N = 10**.
- Loss-regime / crossover-band cells: **N = 20**.

T2.6 driver default (`SweepGrid::seeds_per_cell`) sets a single
`N` across the grid; the headline-vs-loss-regime distinction is
applied by the caller invoking `spectrum_grid` twice (once with
N=10 for the headline cells, once with N=20 for the
C0/C3/C4 cells where the publishable claim is tight). The
real-run launcher (a future bone — likely T2.6's calendar
artifact) composes this. Until then the default is the
headline N=10, with the pilot using N=3 per `pilot_grid`.

**Pilot N = 3 rationale (pre-reg §3.1 Pilot rule binding).** The
pilot is harness-only validation; per §3.1 it MUST NOT be used to
set bars or support publication claims. N=3 is sufficient to
exercise every code path in the aggregator + crossover + renderer
(median, min/max, Wilson CI, rate gap, ratio threshold, both
regime sides) without crossing the 60s wall budget for the
recipe. The pilot Wilson UB at N=3 is ~0.708 — too wide for any
defensible claim, which is the discipline the §3.1 binding
exists to enforce.

---

## A3. Block-randomized run order (deferred to real-run wrapper)

Pre-reg §6.2 mandates block-randomized run order to defang
temporal drift in hosted-model behavior. The T2.6 sweep driver
itself iterates the grid in a fixed `cell -> arm -> replicate`
order; the §6.2 block-randomized shuffle is the responsibility of
the **real-run wrapper script** (the future calendar artifact).
The driver is decoupled from this so a pilot's determinism
(which requires fixed order for byte-equal JSON across runs) does
not conflict with the real-run's randomization requirement.

Implementation note for the wrapper: shuffle the iterator
`grid.iter_runs()` returns using a `StdRng` seeded from
`base_seed`, then write the realized order to
`<artifact_dir>/run-order.json` per the §6.4 manifest.

---

## A4. Schema-version forward-compat (BenchRun v1 + v2)

T2.5 (`bn-1rgk`) will bump `BenchRun::SCHEMA_VERSION` to 2 with
per-tool-call attribution fields. The T2.6 aggregator
(`maw_bench_sweep::aggregate_artifacts`) accepts both v1 and v2
records in the same directory (per
`AggregateError::UnsupportedSchema` only rejecting versions
outside `{1, 2}`). Cells aggregated from a v1 + v2 mix populate
`AggregateExtras::attributed_work_redone_turns = None` for v1
records and `Some(_)` for v2 records when the attribution field
is present.

This avoids a hard "stop the world" coupling between T2.5 and
T2.6: a partial T2.5 roll-out (a few v2 runs alongside many v1
runs) is loadable without re-running the v1 corpus.

---

## Crossover regime labels (per-axis interpretation)

The `CrossoverRegime` enum carries the same four labels
(`Overkill`, `Tie`, `Dominant`, `NoData`) for both efficiency
metrics and correctness rates. The doc renderer
(`render_crossover_doc`) **bucket-splits** correctness rate
classifications into a separate `## SAFETY` section so a reader
cannot mis-read a safety regression as an "overkill cost". This
preserves the pre-reg §4.1 axis-separation invariant in the
publishable doc, while keeping the API surface uniform for
consumers.

---

## Real-run launcher: what T2.6 does NOT ship

The bone HARD RULE forbids a full real-run sweep from T2.6 — that
is a downstream calendar artifact (~$100s in LLM tokens). What
T2.6 ships:

- The sweep harness (`SweepDriver`).
- The pure-data aggregator + crossover finder.
- The spectrum-mode renderer + the doc scaffold.
- A `MockAgent + NoopSubstrate` pilot recipe
  (`just sg2-sweep-pilot`) that runs in <60s wall and proves the
  pipeline is end-to-end functional.

What T2.6 does NOT ship:

- A real-LLM run wrapper.
- The §6.2 block-randomized scheduler.
- Per-replicate attribution coding (the human-in-the-loop step
  per pre-reg §6.3; T2.5 / `bn-1rgk` ships the attribution
  schema; the analyst pass is downstream).
- A diagnostic bundle (T2.8 / `bn-u9iy`) consuming the
  SweepSummary.

The latter four are bones that depend on T2.6 (the dependency
chain in `bn-3l1f`'s `dependents` shows `bn-2xfn`, `bn-u9iy`).
T2.6's API surface (the public types in
`maw_bench_sweep::lib.rs`) is the contract for those bones.
