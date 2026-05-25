//! Pure functions that map a [`maw_bench::BenchRun`] to a
//! [`crate::MetricRecord`].
//!
//! All functions here are **deterministic and I/O-free**. Given the
//! same `BenchRun`, [`extract_metrics`] returns byte-equal
//! `MetricRecord`s; the `equivalence_of_counting` test enforces this.
//!
//! # Counting rules — the canonical reference is
//!   `notes/sg2-metric-definitions.md`
//!
//! Each metric has a section there. The implementations below cite
//! that section. If you change a counting rule, update the doc in the
//! same commit — the rule of record is the doc, not the code.

use maw_bench::run::{BenchRun, OracleBSummary, RunVerdict, ToolCall, Turn};

use crate::record::{MetricRecord, MetricValue};

#[cfg(test)]
use crate::record::Axis;

/// Map a [`BenchRun`] to a [`MetricRecord`].
///
/// **Pure** — no allocation other than what the record itself owns.
/// **Deterministic** — same `BenchRun` -> same `MetricRecord`.
///
/// The counting rules (one per metric) are documented in
/// `notes/sg2-metric-definitions.md`. Edge cases:
///
/// - `turns_to_done = Infinite` iff `verdict != Success`
///   (pre-reg §1.1: "turns the agent took to finish"; an unfinished
///   agent has no finite finish-turn count).
/// - `work_lost_events` counts both `OracleBSummary::Red` (each
///   violation = 1 event) AND `RunVerdict::SubstrateIncoherent`
///   (= 1 event). They can coincide; the count is additive — see
///   the doc for the rationale.
/// - `cost_usd = Unavailable` when `BenchRun::cost_usd` is `None`
///   (MockAgent, or a provider envelope that lacks the field).
/// - `human_intervention_events = Unavailable` today —
///   `BenchRun` does not currently carry a transcript marker for
///   the "agent escalated to human" event class. The hook is
///   reserved; see `notes/sg2-metric-definitions.md`
///   §human_intervention_events for the future signal source.
#[allow(clippy::cast_possible_truncation)]
pub fn extract_metrics(run: &BenchRun) -> MetricRecord {
    let turns_to_done = match &run.verdict {
        RunVerdict::Success => MetricValue::count(u64::from(run.total_turns)),
        // Anything other than Success is "did not finish the planned
        // battery" — pre-reg §1.1 wording. Use the sentinel so
        // downstream renderers cannot quietly treat 0 as "fast".
        _ => MetricValue::Infinite,
    };

    let cost_usd = match run.cost_usd {
        Some(d) if d.is_finite() && d >= 0.0 => {
            // Round to nearest cent×100 (cents-of-cents) to preserve
            // sub-cent precision (real for cheap providers).
            // The match guard plus `.round()` makes the cast safe:
            // the value is a non-negative finite number; any
            // plausible per-run cost is well within u64 range.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let cents = (d * 10_000.0).round() as u64;
            MetricValue::usd_cents(cents)
        }
        _ => MetricValue::Unavailable,
    };

    MetricRecord {
        schema_version: MetricRecord::SCHEMA_VERSION,
        run_id: run.run_id.clone(),
        arm: run.manifest.arm.clone(),
        condition_id: run.manifest.condition_id.clone(),
        t_class: run.manifest.t_class.clone(),

        // ----- correctness -----
        work_lost_events: MetricValue::count(count_work_lost_events(run)),
        human_intervention_events: count_human_intervention_events(run),

        // ----- efficiency -----
        tool_calls_total: MetricValue::count(u64::from(run.total_tool_calls)),
        turns_to_done,
        wall_duration_ms: MetricValue::duration_ms(run.duration_ms),
        cost_usd,
        work_redone_turns: MetricValue::count(count_work_redone_turns(&run.transcript.turns)),
    }
}

/// Count work-loss events for a run.
///
/// Pre-reg §1.1 splits "lost work" into multiple signal sources;
/// at this layer we conservatively combine the ones the BenchRun
/// schema carries directly:
///
/// 1. [`OracleBSummary::Red`] violations — each is a distinct
///    coherence breach (Prime-Invariant class, e.g. dangling head ref).
/// 2. [`RunVerdict::SubstrateIncoherent`] — the harness-level
///    "agent finished but the substrate is broken" signal. Counted
///    as **+1** even if Oracle B also fired, because the agent's
///    obliviousness is itself an event.
///
/// `OracleBSummary::NotApplicable` contributes 0 (non-maw arm; we do
/// not pretend Oracle B applies to substrates it cannot judge).
pub fn count_work_lost_events(run: &BenchRun) -> u64 {
    let mut n: u64 = 0;
    match &run.oracle_b {
        OracleBSummary::Red { violations } => {
            n = n.saturating_add(violations.len() as u64);
        }
        OracleBSummary::Green | OracleBSummary::NotApplicable { .. } => {}
    }
    if matches!(run.verdict, RunVerdict::SubstrateIncoherent) {
        n = n.saturating_add(1);
    }
    n
}

/// Future hook — see module docs. Returns `Unavailable` until a
/// BenchRun field signals human intervention.
const fn count_human_intervention_events(_run: &BenchRun) -> MetricValue {
    // Reserved. The doc lists three candidate signals (transcript
    // marker, harness escalation event, substrate-side prompt
    // escalation). None are in the schema today; the placeholder
    // ensures the metric name is stable while the source matures.
    MetricValue::Unavailable
}

/// Count agent turns spent re-doing work.
///
/// **This is the most subjective metric.** Pre-reg §6.3 specifies
/// blind double-coding by two analysts on a 20% transcript sample
/// for the publication-grade number; this function implements a
/// **conservative, transparent heuristic** suitable for live
/// dashboards and the `sg2-report` printout. The two numbers will
/// diverge in absolute terms; the heuristic's job is to be
/// monotone across arms so dominance comparisons remain valid.
///
/// The heuristic counts a turn as "work-redone" if either:
///
/// 1. Its tool-call set contains a substring matching the
///    conflict-recovery vocabulary — `"conflict"`, `"ws conflicts"`,
///    `"resolve"`, `"recover"`, `"rebase"` — AND the prior turn did
///    not (a fresh entry into recovery, not a continuation).
/// 2. Its tool-call set contains a repeat-of-prior-turn `Bash` call
///    with identical args_json (a literal retry).
///
/// Per-op attribution against [`maw_bench_adapters::StepOutcome`] is
/// the principled grounding T2.5 (`bn-1rgk`) will implement; see
/// `notes/sg2-metric-definitions.md` §work_redone_turns for the
/// op-vocabulary mapping.
pub fn count_work_redone_turns(turns: &[Turn]) -> u64 {
    let mut count: u64 = 0;
    for (i, turn) in turns.iter().enumerate() {
        let prev = if i == 0 { None } else { Some(&turns[i - 1]) };
        if turn_is_recovery_entry(turn, prev) || turn_is_literal_retry(turn, prev) {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Heuristic 1: this turn enters recovery vocabulary; prior turn did
/// not. Identifies a "we now realize prior work was lost and are
/// redoing it" boundary.
fn turn_is_recovery_entry(turn: &Turn, prev: Option<&Turn>) -> bool {
    if !any_call_matches(turn, is_recovery_call) {
        return false;
    }
    match prev {
        None => true,
        Some(p) => !any_call_matches(p, is_recovery_call),
    }
}

/// Heuristic 2: this turn repeats a prior turn's Bash call byte-for-
/// byte. Caught literal retries (the most unambiguous redo signal).
fn turn_is_literal_retry(turn: &Turn, prev: Option<&Turn>) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    for call in &turn.tool_calls {
        if call.name != "Bash" {
            continue;
        }
        if prev.tool_calls.iter().any(|p| {
            p.name == "Bash" && p.args_json == call.args_json
        }) {
            return true;
        }
    }
    false
}

fn any_call_matches<F>(turn: &Turn, f: F) -> bool
where
    F: Fn(&ToolCall) -> bool,
{
    turn.tool_calls.iter().any(f)
}

fn is_recovery_call(call: &ToolCall) -> bool {
    // Match against args_json — tool names are coarse ("Bash") so
    // the discriminator is the command string. Case-insensitive on
    // ASCII; we don't try to be clever about word boundaries because
    // false positives here cost less than false negatives (counts a
    // few extra "rebase" calls as recovery; the across-arm gradient
    // still holds).
    let hay = call.args_json.to_ascii_lowercase();
    hay.contains("conflict")
        || hay.contains("ws conflicts")
        || hay.contains("resolve")
        || hay.contains("recover")
        || hay.contains("rebase")
}

/// Trait-bound smoke test: the doc says the renderer reads
/// [`MetricRecord::axed`] and partitions on axis. This function is
/// not used outside tests but lives in the module so the trait
/// boundary stays obvious if someone refactors.
#[cfg(test)]
fn axis_of(name: &str, rec: &MetricRecord) -> Option<Axis> {
    rec.axed()
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, a)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maw_bench::run::{
        BenchRun, OracleBSummary, RunManifest, RunVerdict, ToolCall, Transcript, Turn,
    };

    fn synth_run(
        arm: &str,
        verdict: RunVerdict,
        oracle: OracleBSummary,
        turns: u32,
        tool_calls: u32,
        cost: Option<f64>,
    ) -> BenchRun {
        BenchRun {
            schema_version: BenchRun::SCHEMA_VERSION,
            run_id: format!("test-{arm}"),
            manifest: RunManifest {
                arm: arm.to_string(),
                condition_id: "C0".into(),
                t_class: "T2".into(),
                ..RunManifest::default()
            },
            verdict,
            oracle_b: oracle,
            transcript: Transcript {
                prompt: String::new(),
                prompt_sha256: String::new(),
                convention_text: String::new(),
                turns: (0..turns)
                    .map(|i| Turn {
                        index: i + 1,
                        ts_unix_ms: 0,
                        reply_text: String::new(),
                        tool_calls: Vec::new(),
                    })
                    .collect(),
            },
            total_tool_calls: tool_calls,
            total_turns: turns,
            cost_usd: cost,
            duration_ms: 1234,
            substrate_final_files: Vec::new(),
        }
    }

    #[test]
    fn maw_success_run_record_shape() {
        let run = synth_run(
            "maw",
            RunVerdict::Success,
            OracleBSummary::Green,
            4,
            12,
            Some(0.034_2),
        );
        let m = extract_metrics(&run);
        assert_eq!(m.arm, "maw");
        assert_eq!(m.work_lost_events, MetricValue::count(0));
        assert_eq!(m.tool_calls_total, MetricValue::count(12));
        assert_eq!(m.turns_to_done, MetricValue::count(4));
        assert_eq!(m.wall_duration_ms, MetricValue::duration_ms(1234));
        assert_eq!(m.cost_usd, MetricValue::usd_cents(342));
        assert_eq!(m.work_redone_turns, MetricValue::count(0));
        assert_eq!(m.human_intervention_events, MetricValue::Unavailable);
        // Axes.
        assert_eq!(axis_of("work_lost_events", &m), Some(Axis::Correctness));
        assert_eq!(axis_of("tool_calls_total", &m), Some(Axis::Efficiency));
    }

    #[test]
    fn jj_oracle_not_applicable_no_loss() {
        // jj arm: oracle not applicable; should NOT inflate loss.
        let run = synth_run(
            "jj-workspaces",
            RunVerdict::Success,
            OracleBSummary::NotApplicable {
                reason: "arm = jj".into(),
            },
            5,
            18,
            Some(0.05),
        );
        let m = extract_metrics(&run);
        assert_eq!(m.work_lost_events, MetricValue::count(0));
    }

    #[test]
    fn substrate_incoherent_counts_as_loss() {
        let run = synth_run(
            "maw",
            RunVerdict::SubstrateIncoherent,
            OracleBSummary::Red {
                violations: vec![
                    "B1 DanglingHeadRef ws-1".into(),
                    "B3 RefPointsToMissingObject ws-2".into(),
                ],
            },
            7,
            30,
            Some(0.10),
        );
        let m = extract_metrics(&run);
        // 2 violations + 1 SubstrateIncoherent = 3.
        assert_eq!(m.work_lost_events, MetricValue::count(3));
        // Verdict != Success -> turns_to_done is Infinite.
        assert_eq!(m.turns_to_done, MetricValue::Infinite);
    }

    #[test]
    fn agent_failed_turns_is_infinite() {
        let run = synth_run(
            "maw",
            RunVerdict::AgentFailed {
                reason: "max_turns".into(),
            },
            OracleBSummary::Green,
            10,
            40,
            Some(0.20),
        );
        let m = extract_metrics(&run);
        assert_eq!(m.turns_to_done, MetricValue::Infinite);
        // No oracle red, no substrate incoherent -> 0 work lost.
        assert_eq!(m.work_lost_events, MetricValue::count(0));
    }

    #[test]
    fn cost_unavailable_when_none() {
        let run = synth_run("maw", RunVerdict::Success, OracleBSummary::Green, 3, 9, None);
        let m = extract_metrics(&run);
        assert_eq!(m.cost_usd, MetricValue::Unavailable);
    }

    #[test]
    fn cost_unavailable_when_non_finite() {
        let run = synth_run(
            "maw",
            RunVerdict::Success,
            OracleBSummary::Green,
            3,
            9,
            Some(f64::NAN),
        );
        let m = extract_metrics(&run);
        assert_eq!(m.cost_usd, MetricValue::Unavailable);
    }

    #[test]
    fn equivalence_of_counting_same_run_same_record() {
        let run = synth_run(
            "maw",
            RunVerdict::Success,
            OracleBSummary::Green,
            5,
            17,
            Some(0.078_9),
        );
        let m1 = extract_metrics(&run);
        let m2 = extract_metrics(&run);
        assert_eq!(m1, m2);
        // Byte-identity after JSON round-trip.
        assert_eq!(m1.to_json().unwrap(), m2.to_json().unwrap());
    }

    fn turn_with_calls(idx: u32, calls: Vec<ToolCall>) -> Turn {
        Turn {
            index: idx,
            ts_unix_ms: 0,
            reply_text: String::new(),
            tool_calls: calls,
        }
    }

    fn bash(args: &str) -> ToolCall {
        ToolCall {
            name: "Bash".into(),
            args_json: args.into(),
            ts_unix_ms: 0,
            result_truncated: None,
        }
    }

    #[test]
    fn redone_heuristic_recovery_entry() {
        let turns = vec![
            turn_with_calls(1, vec![bash(r#"{"cmd":"maw ws merge a"}"#)]),
            // Recovery entry: agent calls `maw ws conflicts`.
            turn_with_calls(2, vec![bash(r#"{"cmd":"maw ws conflicts a"}"#)]),
            // Continuing the same recovery — not a fresh entry.
            turn_with_calls(3, vec![bash(r#"{"cmd":"maw ws resolve a --list"}"#)]),
        ];
        // 1 recovery entry on turn 2; turn 3 is a continuation.
        assert_eq!(count_work_redone_turns(&turns), 1);
    }

    #[test]
    fn redone_heuristic_literal_retry() {
        let cmd = r#"{"cmd":"git push"}"#;
        let turns = vec![
            turn_with_calls(1, vec![bash(cmd)]),
            // Literal retry of the prior turn's Bash call.
            turn_with_calls(2, vec![bash(cmd)]),
        ];
        assert_eq!(count_work_redone_turns(&turns), 1);
    }

    #[test]
    fn redone_heuristic_zero_on_clean_run() {
        let turns = vec![
            turn_with_calls(1, vec![bash(r#"{"cmd":"maw ws create x"}"#)]),
            turn_with_calls(2, vec![bash(r#"{"cmd":"git add -A"}"#)]),
            turn_with_calls(3, vec![bash(r#"{"cmd":"git commit -m foo"}"#)]),
        ];
        assert_eq!(count_work_redone_turns(&turns), 0);
    }
}
