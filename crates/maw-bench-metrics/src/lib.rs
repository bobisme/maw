//! Per-run dominance metrics + report renderer for the SG2 benchmark
//! (T2.4 / `bn-oko4`).
//!
//! # What this crate is
//!
//! The **metric-extraction half** of the SG2 pipeline. Inputs are
//! [`maw_bench::BenchRun`] records (one JSON file per run, written by
//! T2.2's [`maw_bench::BenchHarness`]). Outputs are:
//!
//! 1. A per-run [`MetricRecord`] — flat, schema-versioned, never
//!    averaged into a single score. Each metric is tagged with its
//!    [`Axis`] (efficiency or correctness) so a downstream reporter
//!    cannot accidentally collapse axes.
//! 2. A [`Report`] renderer that takes `&[MetricRecord]` and emits a
//!    per-arm dominance table. The renderer **deliberately omits**
//!    a composite column; the correctness axis is printed first and
//!    visually separated from the efficiency axis per the pre-reg
//!    §4.1 frozen shape.
//!
//! # The non-composite rule (binding)
//!
//! The bone (`bn-oko4`) and the pre-registration
//! (`notes/sg2-benchmark-preregistration.md` §1.2 + §4) explicitly
//! forbid any weighted-sum, ranking, or "maw is X% better" headline.
//! `maw wins at this cell` is a **dominance** statement: maw is
//! `<=` every correctness/safety metric AND not materially worse on
//! efficiency, per the pre-reg §4.3 verdict rules. This crate's
//! reporter prints the per-axis values side-by-side and lets the
//! reader apply the §4.3 rule.
//!
//! No method in this crate produces a scalar that summarizes axes.
//! Tests (`tests/no_composite.rs`) assert this invariant against the
//! rendered output.
//!
//! # Architectural decision (recorded for downstream agents)
//!
//! This is a **sibling crate** of `maw-bench` and `maw-bench-adapters`,
//! not a module inside `maw-bench`. The rationale is documented in
//! `Cargo.toml`. The short version: compile-cycle isolation, one-way
//! read-only dependency, and the pre-reg's "T2.4 reads these files"
//! framing in §1.1.
//!
//! # The two complementary `Substrate` traits
//!
//! Two traits share the name `Substrate` on `main`:
//!
//! - [`maw_bench::Substrate`] (T2.2) — lifecycle hook
//!   (`label/setup/teardown`); the harness uses this to set up an
//!   environment for an LLM agent.
//! - [`maw_bench_adapters::Substrate`] (T2.3) — per-op vocabulary
//!   (`create_workspace/edit_file/commit/merge/sync/destroy`);
//!   produces [`maw_bench_adapters::StepOutcome`].
//!
//! These are **complementary, not duplicative**:
//!
//! - The live SG2 path is **agent-driven** — the agent emits tool
//!   calls inside its turn; the harness records them into a
//!   [`maw_bench::BenchRun`]. The per-op verbs are NOT directly
//!   invoked during a real run.
//! - The per-op verbs DO define the **observable event vocabulary**:
//!   each metric definition in
//!   `notes/sg2-metric-definitions.md` references a substrate-op
//!   class (e.g. "an agent retry after a `StepOutcome { conflicted:
//!   true }` outcome") as the canonical per-substrate semantic the
//!   metric counts.
//!
//! Practically: `extract_metrics` reads what the harness produces
//! (tool calls, turns, transcript, verdicts) and approximates the
//! per-op signal via documented heuristics on tool-call patterns.
//! The exact transcript-attribution rule for `work_redone_turns`
//! lives in pre-reg §6.3 (blind double-coding) and is the most
//! subjective metric in the doc — `extract_metrics` exposes a
//! conservative implementation here and pushes the human-in-the-loop
//! coding step to T2.5 (`bn-1rgk`, maw-per-verb attribution).
//!
//! # Schema stability
//!
//! [`MetricRecord::SCHEMA_VERSION`] is `1`. Bumped only when a field
//! is removed or its type changes (additive optional fields do NOT
//! bump). The metric-definitions doc (`notes/sg2-metric-definitions.md`)
//! is the stable reference; this crate's tests assert the
//! `MetricRecord` field set matches the doc.

#![cfg(feature = "bench")]
#![deny(rust_2018_idioms)]
// Per-lint waivers — explicit, with rationale. The crate is gated
// behind `bench` (test-adjacent infrastructure) so we mirror the
// pragmatism of `maw-bench-adapters`: pedantic/nursery stay on by
// default, but lints that fight against the crate's idiom are
// allowed below with one-line rationales.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::module_name_repetitions)]
// MetricValue is intentionally `PartialEq` only — float ordering
// could enter later without re-derive cascades. Eq adds nothing.
#![allow(clippy::derive_partial_eq_without_eq)]
// `Option::map_or(true, |_| ...)` is the canonical "default-true"
// pattern but reads worse than match { None => true, Some(_) => ... }.
#![allow(clippy::option_if_let_else)]
// Several small functions are eligible-for-const but const-fnness
// has no value here (called once per run, not in hot paths).
#![allow(clippy::missing_const_for_fn)]
// Tests use unwrap() on assert_eq! values where panic IS the failure
// signal — matches the maw-bench-adapters pattern.
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(test, allow(clippy::expect_used))]
#![cfg_attr(test, allow(clippy::cast_sign_loss))]
#![cfg_attr(test, allow(clippy::cast_possible_truncation))]

pub mod attribution;
pub mod extract;
pub mod friction_list;
pub mod record;
pub mod report;

pub use attribution::{
    attribute_tool_call, DiagnosticBundle, MawVerbAttribution, PerVerbCluster,
};
pub use extract::{
    count_attribution_driven_redone_turns, count_work_redone_turns, extract_metrics,
    per_verb_attribution,
};
pub use friction_list::{
    friction_list_from_bundles, recommended_fix_class, render_friction_list_md, ExcerptOutcome,
    FrictionCluster, FrictionList, FrictionSource, SweepRunRef, TranscriptExcerpt,
    EVIDENCE_RUN_ID_CAP, EXAMPLE_EXCERPT_CAP, FRICTION_LIST_SCHEMA_VERSION,
};
pub use record::{Axis, MetricRecord, MetricValue, MetricsSchemaError};
pub use report::{render_dominance_table, ReportOptions};
