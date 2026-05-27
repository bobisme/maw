//! Condition-spectrum sweep harness + crossover analysis for the SG2
//! agent-ergonomics benchmark (T2.6 / `bn-3l1f`).
//!
//! # What this crate is
//!
//! The **sweep half** of the SG2 pipeline. Inputs are a [`SweepGrid`]
//! (condition × T-class × arm × seed) plus a [`maw_bench::Substrate`]
//! per arm and a [`maw_bench::AgentBackend`]. Outputs are:
//!
//! 1. A directory of per-run [`maw_bench::BenchRun`] JSONs, one
//!    file per cell × replicate (the [`SweepDriver`] writes; the
//!    aggregator reads).
//! 2. A [`SweepSummary`] keyed by `(arm, condition_id, t_class)`
//!    containing per-metric median + Wilson 95% CI for zero-event
//!    proportion cells. Mirrors the T1.9
//!    `notes/sg1-soak-campaign.md` §3 reporting discipline.
//! 3. Per-(metric, ref_arm) [`CrossoverPoint`]s — the publishable
//!    headline of where, on the benign→hostile axis, an arm's
//!    behavior crosses the reference arm.
//! 4. A **spectrum-mode** renderer extending the T2.4 report
//!    renderer; emits an ASCII summary plus a
//!    `crossover-summary.md` doc with the publishable narrative
//!    scaffolding (explicit `OVERKILL_REGIME` and
//!    `HOSTILE_REGIME` sections per the §2 publish-the-loss-regime
//!    commitment of `notes/sg2-benchmark-preregistration.md`).
//!
//! # Hard contracts (binding)
//!
//! - **No composite.** Crossover is computed per-metric, per-substrate
//!   pair; there is no aggregate "maw wins by X" output anywhere.
//!   The `no_composite.rs` invariant on the T2.4 renderer continues
//!   to pass; this crate adds its own [`tests::no_composite`].
//! - **The overkill regime is shipped, not hidden.** The renderer
//!   emits an explicit `OVERKILL_REGIME` section and the
//!   `find_crossover` API returns the overkill-regime cells in the
//!   crossover output rather than clipping them.
//! - **No real sweep runs from `cargo test`.** Every test in this
//!   crate uses [`maw_bench::MockAgent`] +
//!   [`maw_bench::NoopSubstrate`]. The just-target
//!   `sg2-sweep-pilot` is the developer-facing entry point.
//! - **Forward-compat to BenchRun schema v2.** The aggregator
//!   parses BenchRun JSON tolerantly: any record with
//!   `schema_version ∈ {1, 2}` loads; schema v2-only fields are
//!   read into [`AggregateExtras`] when present and gracefully
//!   absent on v1 records.
//!
//! # Pre-registration alignment
//!
//! The frozen §5 five-point spectrum (C0..C4) plus the §5.1
//! T-class taxonomy (T0..T5) is encoded verbatim in [`grid`].
//! Per-cell `N` defaults follow §6.1: 10 for headline cells, 20
//! for loss/crossover cells. The pilot uses much smaller N (3
//! seeds/cell) for harness validation only — never bar-setting
//! (§3.1 Pilot rule).

// The module-level allows below must come before #![cfg(...)] so
// that they apply even when the `bench` feature is OFF and the
// rest of the file is excluded — clippy still scans the
// crate-level doc comments at the top of this file.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![cfg(feature = "bench")]
#![deny(rust_2018_idioms)]
// Per-lint waivers — mirror the maw-bench-metrics rationale.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::derive_partial_eq_without_eq)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::missing_const_for_fn)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(test, allow(clippy::expect_used))]
#![cfg_attr(test, allow(clippy::cast_sign_loss))]
#![cfg_attr(test, allow(clippy::cast_possible_truncation))]
#![cfg_attr(test, allow(clippy::cast_precision_loss))]
#![cfg_attr(test, allow(clippy::float_cmp))]

pub mod aggregate;
pub mod crossover;
pub mod driver;
pub mod grid;
pub mod preflight;
pub mod real_runtime;
pub mod render;
pub mod sg3_decision;

pub use aggregate::{
    AggregateError, AggregateExtras, CellAggregate, CellKey, SweepSummary, WilsonCi,
    aggregate_artifacts, load_runs,
};
pub use preflight::{PreflightOutcome, check_maw_version_skew, check_maw_version_skew_with};
pub use crossover::{CrossoverPoint, CrossoverRegime, MetricName, find_crossover};
pub use driver::{SweepDriver, SweepDriverError, SweepPlan};
pub use real_runtime::{
    AnyAgent, BackendChoice, RealSubstrate, SubstrateChoice, make_any_agent, validate_pairing,
};
pub use grid::{
    ARMS_PUBLICATION, ConditionPoint, SweepCell, SweepGrid, TClass, pilot_grid, spectrum_grid,
};
pub use render::{SpectrumReportOptions, render_crossover_doc, render_spectrum_table};
pub use sg3_decision::{
    ARM_NEW, ARM_OLD, Decision, EvaluatedRule, Evidence, PairedCiSignal, PairedCiSignals,
    PrereggedBars, RuleStatus, SUBSET_CELLS, decide_go_no_go,
};
