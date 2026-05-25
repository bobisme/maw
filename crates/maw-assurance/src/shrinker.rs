//! Failing-seed shrinker (bn-32k3, T1.6).
//!
//! Reduces a failing [`crate::scenario::ScenarioPlan`] to a minimal repro
//! using **delta debugging** over `plan.steps`, replaying through the
//! in-process driver ([`crate::in_proc::InProcDriver`]). A reduction is
//! kept iff the SAME oracle trips with the SAME violation class on replay
//! (`StepVerdict::same_class`); this is the equivalence relation that
//! prevents the shrinker from drifting onto an unrelated bug.
//!
//! ## Strategy (`notes/sg1-dst-architecture.md` §6)
//!
//! 1. **Remove contiguous halves** (classic ddmin granularity descent).
//!    Start with `granularity = 2`; if no half can be removed without
//!    losing the violation, halve to quarters, eighths, ... down to
//!    single-step removals.
//! 2. **Remove individual steps** at every granularity once halves stop
//!    working. (`ddmin` "isolate-and-remove" phase.)
//! 3. **Drop step faults** (`FaultSpec::None`) one at a time — if the
//!    violation persists without the fault, the fault wasn't load-bearing.
//! 4. **Lower `ConditionProfile` knobs** (`concurrency_degree`, all probs)
//!    monotonically toward zero — if the violation reproduces under a
//!    tamer profile, the original noise wasn't load-bearing.
//!
//! Termination: when no single removal or knob-lowering preserves the
//! violation, the plan is locally minimal. We additionally early-terminate
//! when `plan.steps.len() < TARGET_MIN_STEPS` (default 10 per the bone's
//! acceptance criterion).
//!
//! ## Cost
//!
//! Per the architecture's §6 budget the in-proc driver clocks ~42 ms per
//! replay, so even a 50-step start typically shrinks to <10 steps in
//! O(n log n) ≈ a few hundred replays = single-digit seconds. The
//! [`ShrinkReport`] returned by [`shrink`] surfaces both the iteration
//! count and the wall-clock so the T1.6 tests can quote the measurement.

#![cfg(feature = "oracles")]
// Harness/test-support; see analogous notes in `in_proc.rs`.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::format_push_string)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::needless_collect)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::min_max)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::doc_overindented_list_items)]

use std::time::{Duration, Instant};

use crate::in_proc::{InProcDriver, PlantedDefect, StepVerdict};
use crate::scenario::{ConditionProfile, FaultSpec, ScenarioPlan};

/// Default target — accepts a plan as "minimal enough" when it drops to
/// fewer than this many steps. Matches the T1.6 bone's <10-step
/// acceptance criterion.
pub const TARGET_MIN_STEPS: usize = 10;

/// Outcome of [`shrink`].
#[derive(Clone, Debug)]
pub struct ShrinkReport {
    /// The minimal plan that still trips the same violation class.
    pub minimal: ScenarioPlan,
    /// The first-violating verdict on the original plan (recorded so the
    /// caller can confirm soundness without re-running the driver).
    pub original_verdict: StepVerdict,
    /// The first-violating verdict on the *minimal* plan (must
    /// `same_class` as `original_verdict` — this is the load-bearing
    /// invariant of the shrinker).
    pub minimal_verdict: StepVerdict,
    /// Total in-proc replays the shrinker performed.
    pub iterations: usize,
    /// Wall-clock for the whole shrink run.
    pub wall: Duration,
    /// A copy-pasteable replay command for the minimal seed (consumed by
    /// `FailureBundle.minimized_replay_command`).
    pub minimized_replay_command: String,
}

/// Shrink `failing` against the same set of `planted` defects that
/// produced it. The result is **sound**: replaying the returned
/// `minimal` plan through a fresh driver with the same planted defects
/// MUST trip the same violation class.
///
/// `planted` is a slice (cloned per replay) — the planted defects are
/// the seed of the failure, not part of the plan, so they ride with the
/// shrinker unchanged. The shrinker MAY drop a defect from the
/// re-evaluation when it shrinks past its `after_step`, but it never
/// edits the defect contents.
pub fn shrink(
    failing: &ScenarioPlan,
    planted: &[PlantedDefect],
    original_verdict: StepVerdict,
) -> ShrinkReport {
    debug_assert!(
        original_verdict.is_violation(),
        "shrinker called on a non-violating verdict"
    );
    let start = Instant::now();
    let mut iterations = 0usize;
    let mut current = failing.clone();

    // -------- Phase 1: delta-debugging over steps (halves → quarters → ...) --
    let mut granularity = 2usize.max(current.steps.len().min(2));
    while granularity <= current.steps.len() && current.steps.len() > TARGET_MIN_STEPS {
        let chunk_size = (current.steps.len() / granularity).max(1);
        let mut reduced_this_pass = false;
        let mut i = 0usize;
        while i < current.steps.len() && current.steps.len() > TARGET_MIN_STEPS {
            let end = (i + chunk_size).min(current.steps.len());
            // Skip the synthetic step 0 (the pre-seeded WsCreate the
            // generator emits at index 0). Removing it would break the
            // model invariant the chooser relies on; safer to keep.
            let removal_lo = i.max(1);
            if removal_lo >= end {
                i = end;
                continue;
            }
            let mut candidate = current.clone();
            candidate.steps.drain(removal_lo..end);
            candidate = renormalize_indices(candidate);
            iterations += 1;
            if try_reproduces(&candidate, planted, &original_verdict) {
                current = candidate;
                reduced_this_pass = true;
                // Don't advance `i`; keep trying to remove from this position.
            } else {
                i = end;
            }
        }
        if reduced_this_pass {
            // Keep this granularity — we may be able to remove more.
            continue;
        }
        granularity = granularity.saturating_mul(2);
    }

    // -------- Phase 2: single-step removals (finer than ddmin halves) -------
    let mut idx = current.steps.len();
    while idx > 1 && current.steps.len() > TARGET_MIN_STEPS {
        idx -= 1;
        let mut candidate = current.clone();
        candidate.steps.remove(idx);
        candidate = renormalize_indices(candidate);
        iterations += 1;
        if try_reproduces(&candidate, planted, &original_verdict) {
            current = candidate;
        }
    }

    // -------- Phase 3: drop faults from individual steps --------------------
    for step_idx in 0..current.steps.len() {
        if matches!(current.steps[step_idx].fault, FaultSpec::None) {
            continue;
        }
        let mut candidate = current.clone();
        candidate.steps[step_idx].fault = FaultSpec::None;
        iterations += 1;
        if try_reproduces(&candidate, planted, &original_verdict) {
            current = candidate;
        }
    }

    // -------- Phase 4: lower ConditionProfile knobs toward zero -------------
    // The profile is metadata on the plan — it doesn't affect the in-proc
    // driver's replay (only the generator consults it), so this phase is
    // primarily for the human-readable bundle output. We still try the
    // lowering with a replay so the soundness invariant holds end-to-end.
    let tame = ConditionProfile::new(1, 0.0, 0.0, 0.0);
    let mut candidate = current.clone();
    candidate.profile = tame;
    iterations += 1;
    if try_reproduces(&candidate, planted, &original_verdict) {
        current = candidate;
    }

    // -------- Verify final and produce a replay command ---------------------
    let minimal_verdict = drive_once(&current, planted);
    let minimized_replay_command = replay_command_for(&current, planted);
    ShrinkReport {
        minimal: current,
        original_verdict,
        minimal_verdict,
        iterations,
        wall: start.elapsed(),
        minimized_replay_command,
    }
}

/// Run the in-proc driver once on `plan` + `planted` in **fast** mode
/// (no per-step oracle check; just one final check). The shrinker only
/// needs to know "does the violation still reproduce"; the per-step
/// check is unnecessary cost per replay.
fn drive_once(plan: &ScenarioPlan, planted: &[PlantedDefect]) -> StepVerdict {
    let mut driver = InProcDriver::new()
        .expect("in-proc driver init")
        .with_planted(planted.to_vec());
    driver.drive_fast(plan).verdict
}

fn try_reproduces(plan: &ScenarioPlan, planted: &[PlantedDefect], target: &StepVerdict) -> bool {
    drive_once(plan, planted).same_class(target)
}

/// Re-pack the `index` field of every step so it matches its new position
/// after a removal. The driver doesn't read `index` (it iterates by Vec
/// position), but downstream consumers (corpus JSON, replay tooling) do.
fn renormalize_indices(mut plan: ScenarioPlan) -> ScenarioPlan {
    for (i, step) in plan.steps.iter_mut().enumerate() {
        step.index = i;
    }
    plan
}

/// Render a copy-pasteable replay command for `plan` + planted defects.
/// The format must round-trip through future T1.7/T1.8 tooling; we keep
/// it explicit (seed + every planted defect) rather than a one-line
/// `--seed=N` so a reader can immediately see what the harness will do.
fn replay_command_for(plan: &ScenarioPlan, planted: &[PlantedDefect]) -> String {
    let mut s = format!(
        "maw assurance dst replay --seed {} --steps {}",
        plan.seed,
        plan.steps.len()
    );
    if !planted.is_empty() {
        s.push_str(" --planted '");
        s.push_str(
            &serde_json::to_string(&PlantedDefects(planted.to_vec()))
                .unwrap_or_else(|_| String::from("[]")),
        );
        s.push('\'');
    }
    s
}

// We need a serde wrapper for `PlantedDefect` so the replay command is
// self-contained. PlantedDefect is in `crate::in_proc` and already
// derives PartialEq+Eq; we add Serialize/Deserialize via this newtype so
// the in_proc module stays serde-clean for non-shrinker callers.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlantedDefects(Vec<PlantedDefect>);

// Make PlantedDefect serializable for the replay command JSON. (We don't
// derive Serialize on the type itself in in_proc.rs to keep the public
// API minimal there.)
mod planted_serde {
    #![allow(dead_code)]
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum PlantedDefectShim {
        WorkLoss { ws: String },
        DanglingHeadRef { ws: String },
    }
}

impl serde::Serialize for crate::in_proc::PlantedDefect {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let shim = match self {
            Self::WorkLoss { ws } => planted_serde::PlantedDefectShim::WorkLoss { ws: ws.clone() },
            Self::DanglingHeadRef { ws } => {
                planted_serde::PlantedDefectShim::DanglingHeadRef { ws: ws.clone() }
            }
        };
        shim.serialize(ser)
    }
}

impl<'de> serde::Deserialize<'de> for crate::in_proc::PlantedDefect {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let shim = planted_serde::PlantedDefectShim::deserialize(de)?;
        Ok(match shim {
            planted_serde::PlantedDefectShim::WorkLoss { ws } => Self::WorkLoss { ws },
            planted_serde::PlantedDefectShim::DanglingHeadRef { ws } => {
                Self::DanglingHeadRef { ws }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// FailureBundle hook — bridge to tests/dst_support
// ---------------------------------------------------------------------------

/// JSON record written into `tests/dst_support`'s `FailureBundle`. Mirrors
/// the field shape `tests/corpus/dst/sample-g1-commit-crash.json` uses
/// (`seed`, `description`, `expected`, ...) so a shrinker output is a
/// drop-in for the corpus directory; T1.8 (bn-3ryq) consumes this verbatim.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ShrinkerCorpusEntry {
    /// The minimal plan's seed (preserved from the original failure).
    pub seed: u64,
    /// The crash phase that the original failure landed in, if any
    /// (corpus convention — `prepare|build|validate|commit|cleanup`,
    /// or `"none"` if the failure didn't involve a merge fault).
    pub crash_phase: String,
    /// Number of plan steps in the minimal seed.
    pub num_steps: usize,
    /// Number of workspaces touched by the minimal seed.
    pub num_workspaces: usize,
    /// Whether the plan creates a real merge candidate.
    pub create_candidate: bool,
    /// `"pass"` for the standard "after the fix this stays green" corpus
    /// entry; `"known_violation"` for a deliberately tracked regression.
    pub expected: String,
    /// Human-readable description (oracle + violation class + entity).
    pub description: String,
    /// The minimal plan itself (the shrinker output).
    pub plan: ScenarioPlan,
    /// The planted defects that ride with the seed.
    pub planted: Vec<crate::in_proc::PlantedDefect>,
    /// The replay command (matches `ShrinkReport.minimized_replay_command`).
    pub replay_command: String,
}

impl ShrinkerCorpusEntry {
    /// Build a corpus entry from a shrink report.
    #[must_use]
    pub fn from_report(report: &ShrinkReport, planted: &[crate::in_proc::PlantedDefect]) -> Self {
        let (kind, entity) = match &report.minimal_verdict {
            StepVerdict::OracleA(a) => (a.kind, a.oid.clone()),
            StepVerdict::OracleB(b) => (b.kind, b.entity.clone()),
            StepVerdict::Clean => ("Clean", String::new()),
        };
        let crash_phase = report
            .minimal
            .steps
            .iter()
            .find_map(|s| match &s.fault {
                FaultSpec::Failpoint { phase, .. } => Some(phase.clone()),
                FaultSpec::None => None,
            })
            .unwrap_or_else(|| "none".to_string());
        let mut workspaces: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut create_candidate = false;
        for step in &report.minimal.steps {
            match &step.op {
                crate::scenario::Op::WsCreate { ws, .. }
                | crate::scenario::Op::EditFiles { ws, .. }
                | crate::scenario::Op::Commit { ws, .. }
                | crate::scenario::Op::Sync { ws }
                | crate::scenario::Op::Destroy { ws, .. } => {
                    workspaces.insert(ws.0.clone());
                }
                crate::scenario::Op::Merge { srcs, .. } => {
                    create_candidate = true;
                    for s in srcs {
                        workspaces.insert(s.0.clone());
                    }
                }
                crate::scenario::Op::Recover { ws, to } => {
                    workspaces.insert(ws.0.clone());
                    workspaces.insert(to.0.clone());
                }
            }
        }
        Self {
            seed: report.minimal.seed,
            crash_phase,
            num_steps: report.minimal.steps.len(),
            num_workspaces: workspaces.len(),
            create_candidate,
            expected: "pass".to_string(),
            description: format!(
                "Minimized seed: {kind}({entity}); {} steps, {} ws",
                report.minimal.steps.len(),
                workspaces.len()
            ),
            plan: report.minimal.clone(),
            planted: planted.to_vec(),
            replay_command: report.minimized_replay_command.clone(),
        }
    }
}
