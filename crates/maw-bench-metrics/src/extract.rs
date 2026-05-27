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

use std::collections::BTreeMap;

use maw_bench::run::{BenchRun, OracleBSummary, RunVerdict, StepOutcome, ToolCall, Turn};

use crate::attribution::{MawVerbAttribution, attribute_tool_call};
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

    // T2.5: per-verb attribution. The maw arm gets a populated map;
    // non-maw arms get an empty map (substrate has no maw verbs).
    let mut per_verb_wasted_turns = if is_maw_arm(&run.manifest.arm) {
        per_verb_attribution(&run.transcript.turns)
    } else {
        BTreeMap::new()
    };

    // Task-aware recover suppression (bn-27ai, Fix A.2 / Approach α).
    //
    // `WsRecoverInvoked` is unconditionally attributed by
    // `attribute_tool_call` (recover is "intrinsically a recovery
    // op"). That is correct when the agent is reacting to an
    // unsolicited loss — but **wrong** when the scenario task
    // literally instructs the agent to call recover (e.g. SG3 C2
    // tasks include "Recover the previously destroyed workspace
    // `ws-0` into a new workspace named `ws-1`"). Each such
    // task-required recover invocation correctly executed by the
    // agent is forward progress, not friction.
    //
    // Fix: count recover-tasks in the prompt's task battery and
    // decrement the `WsRecoverInvoked` cluster by that count
    // (saturating at 0). Removes the cluster entry entirely when the
    // count zeroes out, so the diagnostic JSON does not surface a
    // `0` row from a deletion.
    //
    // Approach β (cleaner, deferred): thread `task_intended` through
    // `ToolCall.attributed_op` so the attribution function sees
    // intent. Tracked separately if α's coverage proves insufficient.
    // See `notes/sg3-no-go-rootcause-v2.md` §3 (root cause) and §5
    // (fix taxonomy) for the full reasoning.
    let task_required_recovers = count_recover_tasks_in_prompt(&run.transcript.prompt);
    if task_required_recovers > 0
        && let Some(slot) = per_verb_wasted_turns.get_mut(&MawVerbAttribution::WsRecoverInvoked)
    {
        *slot = slot.saturating_sub(task_required_recovers);
        if *slot == 0 {
            per_verb_wasted_turns.remove(&MawVerbAttribution::WsRecoverInvoked);
        }
    }

    // T2.5: attribution-driven work_redone_turns. For maw arm, count
    // the attributed clusters (each attribution = one wasted turn).
    // For non-maw arms, fall back to the T2.4 substring heuristic so
    // those arms still have a comparable signal.
    let work_redone = if is_maw_arm(&run.manifest.arm) {
        per_verb_wasted_turns.values().map(|n| u64::from(*n)).sum()
    } else {
        count_work_redone_turns(&run.transcript.turns)
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
        work_redone_turns: MetricValue::count(work_redone),

        // ----- T2.5 diagnostic axis -----
        per_verb_wasted_turns,
    }
}

/// Is `arm` the maw arm? The diagnostic axis only applies there.
/// Treats any arm name starting with `"maw"` as maw (covers `"maw"`,
/// `"maw-bare"`, and `"maw@<flavor>"` such as the SG3
/// `maw@old-layout` / `maw@new-layout` arms). Non-maw arms get an
/// empty attribution map.
///
/// **Why both `-` and `@` delimiters**: historical variants used
/// `maw-<flavor>`; SG3 (`bn-iux4`) introduced `maw@<flavor>` to
/// disambiguate layout-only deltas from substrate-rewrite arms. Both
/// must route through the principled T2.5 attribution path —
/// otherwise the substring fallback in
/// [`count_work_redone_turns`] misclassifies task-required
/// `maw ws recover` invocations as friction (see
/// `notes/sg3-no-go-rootcause-v2.md` §3 for the full mechanism).
fn is_maw_arm(arm: &str) -> bool {
    arm == "maw" || arm.starts_with("maw-") || arm.starts_with("maw@")
}

/// Compute per-verb attribution for a transcript.
///
/// Walks the turn sequence linearly, threading `prior_outcome` between
/// adjacent calls so the conservative attribution can distinguish
/// "first attempt" from "retry after conflict".
///
/// Tracks two attribution sources:
///
/// 1. **Per-call attribution** via [`attribute_tool_call`] — covers
///    the verb-failure cluster family (merge conflict, sync stale,
///    destroy refused, etc.).
/// 2. **Read-from-X detection** via [`crate::attribution::detect_stale_read`]
///    — covers the state-misread cluster family (a status call
///    followed by an op the workspace state shouldn't have permitted).
///
/// Returns a `BTreeMap` (stable iteration order for JSON output).
#[must_use]
pub fn per_verb_attribution(turns: &[Turn]) -> BTreeMap<MawVerbAttribution, u32> {
    let mut counts: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
    let mut prior_outcome: Option<StepOutcome> = None;
    let mut prior_call: Option<&ToolCall> = None;

    for turn in turns {
        for call in &turn.tool_calls {
            // (1) Per-call attribution.
            if let Some(att) = attribute_tool_call(call, prior_outcome.as_ref()) {
                *counts.entry(att).or_insert(0) += 1;
            }
            // (2) Two-call window: prior was a status/list/diff, now
            //     we see the next call's outcome reveal a misread.
            if let (Some(prev), Some(next_out)) = (prior_call, call.attributed_outcome.as_ref())
                && let Some(att) = crate::attribution::detect_stale_read(prev, Some(next_out))
            {
                *counts.entry(att).or_insert(0) += 1;
            }
            // Update the rolling-window state.
            prior_outcome.clone_from(&call.attributed_outcome);
            prior_call = Some(call);
        }
    }
    counts
}

/// Public: replacement for `count_work_redone_turns` — attribution-
/// driven count. Equals `per_verb_attribution(turns).values().sum()`
/// for the maw arm; identical fallback behavior on non-maw arms.
///
/// Returns the count of turns whose calls attributed to any
/// [`MawVerbAttribution`] cluster — i.e. wasted turns explicitly
/// linked to a named maw friction point.
///
/// **Semantics (T2.5 update):** a turn is "redone" iff the agent
/// retried after a `StepOutcome { conflicted: true }` (or other
/// failure signal) and re-issued an op of the same class on the same
/// target. Implemented by routing through [`per_verb_attribution`]
/// so the same signal feeds both metrics — no drift possible.
#[must_use]
pub fn count_attribution_driven_redone_turns(turns: &[Turn]) -> u64 {
    per_verb_attribution(turns)
        .values()
        .map(|n| u64::from(*n))
        .sum()
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
        if prev
            .tool_calls
            .iter()
            .any(|p| p.name == "Bash" && p.args_json == call.args_json)
        {
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

/// Count scenario tasks whose intent is "recover a destroyed
/// workspace". Used by [`extract_metrics`] to suppress the
/// `WsRecoverInvoked` cluster's count for task-required (i.e.
/// expected, forward-progress) recover invocations.
///
/// The scenario generator emits prompts whose `## Task battery`
/// section enumerates numbered tasks, one per line, in the form
/// "`N. <imperative verb> ...`". The recover vocabulary is the
/// canonical "Recover the previously destroyed workspace..." phrasing
/// from `scenario_plan::tasks::recover_task`. We match
/// case-insensitively on the verb token at the start of the task
/// description (after the number+dot prefix) — conservative against
/// false positives (a task that merely *mentions* "recover" without
/// starting with the verb is not counted).
///
/// Returns 0 when the prompt has no `Task battery` section (e.g.
/// non-SG scenario prompts), which is the safe default — no
/// suppression happens.
///
/// **Why parse the prompt rather than thread intent through the
/// harness?** Approach α (this function) is the cheapest patch: it
/// works on every committed BenchRun without re-running anything.
/// Approach β (intent-threading) is the principled long-term shape;
/// see `notes/sg3-no-go-rootcause-v2.md` §5 for the taxonomy.
#[must_use]
pub fn count_recover_tasks_in_prompt(prompt: &str) -> u32 {
    let Some(battery_start) = prompt.find("Task battery") else {
        return 0;
    };
    let tail = &prompt[battery_start..];
    // Tasks live in the same block until the next markdown section
    // (next blank line followed by `## ` or end-of-prompt). Be
    // tolerant: count every numbered line in the rest of the prompt
    // whose body starts with the "recover" verb.
    let mut count: u32 = 0;
    for line in tail.lines() {
        let trimmed = line.trim_start();
        // Match `N.` or `N)` numbered list prefix (N may be
        // multi-digit; future task batteries might enumerate past 9).
        let digit_end = trimmed
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(i, _)| i);
        let Some(digit_end) = digit_end else { continue };
        if digit_end == 0 {
            continue;
        }
        let after_digits = &trimmed[digit_end..];
        let body_after_num = after_digits
            .strip_prefix('.')
            .or_else(|| after_digits.strip_prefix(')'))
            .map(str::trim_start);
        let Some(body) = body_after_num else { continue };
        let lower = body.to_ascii_lowercase();
        // Conservative match: only counts tasks whose imperative verb
        // is literally `recover` (the scenario generator's wording).
        if lower.starts_with("recover ") || lower.starts_with("recover\t") {
            count = count.saturating_add(1);
        }
    }
    count
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
        let run = synth_run(
            "maw",
            RunVerdict::Success,
            OracleBSummary::Green,
            3,
            9,
            None,
        );
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
            attributed_op: None,
            attributed_outcome: None,
        }
    }

    fn bash_with_outcome(args: &str, outcome: StepOutcome) -> ToolCall {
        ToolCall {
            name: "Bash".into(),
            args_json: args.into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: None,
            attributed_outcome: Some(outcome),
        }
    }

    fn conflicted() -> StepOutcome {
        StepOutcome {
            ok: true,
            conflicted: true,
            advanced_integration: false,
            notes: "structured conflict".into(),
        }
    }

    fn refused() -> StepOutcome {
        StepOutcome {
            ok: false,
            conflicted: false,
            advanced_integration: false,
            notes: "refused".into(),
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

    // ---------- T2.5 attribution-driven tests ----------

    /// End-to-end: synthetic BenchRun with a planted retry-after-conflict.
    /// `count_attribution_driven_redone_turns` matches the cluster sum.
    #[test]
    fn attribution_driven_matches_per_verb_sum() {
        // Turn 1: merge that conflicts. Turn 2: retry merge after the
        // conflict — should attribute as WsMergeStructuredConflict.
        let merge_attempt =
            bash_with_outcome(r#"{"cmd":"maw ws merge a --into default"}"#, conflicted());
        let retry = bash(r#"{"cmd":"maw ws merge a --into default"}"#);
        let turns = vec![
            turn_with_calls(1, vec![merge_attempt]),
            turn_with_calls(2, vec![retry]),
        ];
        let per_verb = per_verb_attribution(&turns);
        let total = count_attribution_driven_redone_turns(&turns);
        let sum: u64 = per_verb.values().map(|n| u64::from(*n)).sum();
        assert_eq!(total, sum, "per-verb sum must equal aggregate count");
        assert_eq!(
            per_verb.get(&MawVerbAttribution::WsMergeStructuredConflict),
            Some(&1),
            "retry-after-conflict attributed to merge cluster"
        );
    }

    #[test]
    fn maw_arm_record_carries_per_verb_axis() {
        let conflict_then_retry = [
            bash_with_outcome(r#"{"cmd":"maw ws merge a --into default"}"#, conflicted()),
            bash(r#"{"cmd":"maw ws merge a --into default"}"#),
        ];
        let mut run = synth_run(
            "maw",
            RunVerdict::Success,
            OracleBSummary::Green,
            0,
            2,
            None,
        );
        run.transcript.turns = vec![
            turn_with_calls(1, vec![conflict_then_retry[0].clone()]),
            turn_with_calls(2, vec![conflict_then_retry[1].clone()]),
        ];
        run.total_turns = 2;
        let m = extract_metrics(&run);
        // Diagnostic axis populated for maw arm.
        assert!(!m.per_verb_wasted_turns.is_empty());
        assert_eq!(
            m.per_verb_wasted_turns
                .get(&MawVerbAttribution::WsMergeStructuredConflict),
            Some(&1)
        );
        // work_redone_turns matches the diagnostic-axis sum (no drift).
        let sum: u64 = m
            .per_verb_wasted_turns
            .values()
            .map(|n| u64::from(*n))
            .sum();
        assert_eq!(m.work_redone_turns, MetricValue::count(sum));
    }

    #[test]
    fn non_maw_arm_record_has_empty_per_verb_axis() {
        let run = synth_run(
            "jj-workspaces",
            RunVerdict::Success,
            OracleBSummary::NotApplicable {
                reason: "arm = jj".into(),
            },
            3,
            5,
            None,
        );
        let m = extract_metrics(&run);
        assert!(
            m.per_verb_wasted_turns.is_empty(),
            "non-maw arm must have empty per-verb axis"
        );
    }

    #[test]
    fn destroy_refused_attribution_end_to_end() {
        // Agent issues `maw ws destroy x` -> refused. Then retries
        // with --force. The retry attributes to WsDestroyRefused.
        let attempt = bash_with_outcome(r#"{"cmd":"maw ws destroy alice"}"#, refused());
        let retry = bash(r#"{"cmd":"maw ws destroy alice --force"}"#);
        let turns = vec![
            turn_with_calls(1, vec![attempt]),
            turn_with_calls(2, vec![retry]),
        ];
        let per_verb = per_verb_attribution(&turns);
        assert_eq!(
            per_verb.get(&MawVerbAttribution::WsDestroyRefused),
            Some(&1)
        );
    }

    #[test]
    fn recover_invoked_attribution_independent_of_prior() {
        // Recover is intrinsic recovery — attributes even on turn 1.
        let turns = vec![turn_with_calls(
            1,
            vec![bash(r#"{"cmd":"maw ws recover alice --to alice2"}"#)],
        )];
        let per_verb = per_verb_attribution(&turns);
        assert_eq!(
            per_verb.get(&MawVerbAttribution::WsRecoverInvoked),
            Some(&1)
        );
    }

    // ---------- bn-27ai Fix A.1: `is_maw_arm` routes `maw@<flavor>` ----------

    /// Regression test for bn-27ai Fix A.1. Pre-fix, both
    /// `maw@old-layout` and `maw@new-layout` fell into the substring
    /// fallback because `is_maw_arm` only matched `maw` / `maw-*`.
    /// The SG3 R6 NO-GO root cause v2 (`notes/sg3-no-go-rootcause-v2.md`
    /// §3) traced the asymmetry to this misrouting.
    #[test]
    fn is_maw_arm_recognises_sg3_at_flavors() {
        assert!(is_maw_arm("maw"));
        assert!(is_maw_arm("maw-bare"));
        assert!(is_maw_arm("maw@old-layout"), "SG3 old-layout arm");
        assert!(is_maw_arm("maw@new-layout"), "SG3 new-layout arm");
        // Negative: non-maw arms still excluded.
        assert!(!is_maw_arm("jj-workspaces"));
        assert!(!is_maw_arm("git-worktrees-bare"));
        assert!(!is_maw_arm("claude-native-worktrees"));
        // Negative: arms that merely contain `maw` substring but
        // do not start with it.
        assert!(!is_maw_arm("not-maw"));
    }

    /// End-to-end: a `maw@new-layout` BenchRun now routes through
    /// the principled T2.5 attribution path (per-verb map populated)
    /// instead of the substring fallback.
    #[test]
    fn maw_at_flavor_arm_uses_attribution_path() {
        let conflict_then_retry = [
            bash_with_outcome(r#"{"cmd":"maw ws merge a --into default"}"#, conflicted()),
            bash(r#"{"cmd":"maw ws merge a --into default"}"#),
        ];
        let mut run = synth_run(
            "maw@new-layout",
            RunVerdict::Success,
            OracleBSummary::Green,
            0,
            2,
            None,
        );
        run.transcript.turns = vec![
            turn_with_calls(1, vec![conflict_then_retry[0].clone()]),
            turn_with_calls(2, vec![conflict_then_retry[1].clone()]),
        ];
        run.total_turns = 2;
        let m = extract_metrics(&run);
        // Attribution path populated (substring fallback would leave
        // the per-verb map empty).
        assert!(
            !m.per_verb_wasted_turns.is_empty(),
            "maw@<flavor> arm must use the T2.5 attribution path"
        );
        assert_eq!(
            m.per_verb_wasted_turns
                .get(&MawVerbAttribution::WsMergeStructuredConflict),
            Some(&1)
        );
    }

    // ---------- bn-27ai Fix A.2: task-aware recover attribution ----------

    /// `count_recover_tasks_in_prompt` matches the SG3 scenario
    /// generator's wording. Used by [`extract_metrics`] to suppress
    /// false-positive `WsRecoverInvoked` attribution for correctly-
    /// executed recover tasks.
    #[test]
    fn count_recover_tasks_in_prompt_matches_sg3_battery() {
        let prompt = "preamble\n\nTask battery\n\n\
            Complete the following abstract tasks.\n\n\
            1. Create a coordination workspace named `ws-0`.\n\
            2. Remove workspace `ws-0`.\n\
            3. Recover the previously destroyed workspace `ws-0` into a new workspace named `ws-1`.\n\
            4. Remove workspace `ws-1`.\n";
        assert_eq!(count_recover_tasks_in_prompt(prompt), 1);
    }

    #[test]
    fn count_recover_tasks_in_prompt_zero_when_no_recover() {
        let prompt = "Task battery\n\n\
            1. Create `ws-0`.\n\
            2. Edit a file.\n\
            3. Commit changes.\n\
            4. Merge `ws-0`.\n";
        assert_eq!(count_recover_tasks_in_prompt(prompt), 0);
    }

    #[test]
    fn count_recover_tasks_in_prompt_zero_when_no_battery_section() {
        // Non-SG prompts (no `Task battery` heading) → safe default.
        let prompt = "Just do the thing.";
        assert_eq!(count_recover_tasks_in_prompt(prompt), 0);
    }

    #[test]
    fn count_recover_tasks_in_prompt_handles_multiple_recover_tasks() {
        // Defensive: a battery with two recover tasks counts both.
        let prompt = "Task battery\n\n\
            1. Create `ws-0`.\n\
            2. Recover the previously destroyed `ws-X`.\n\
            3. Recover the previously destroyed `ws-Y`.\n";
        assert_eq!(count_recover_tasks_in_prompt(prompt), 2);
    }

    #[test]
    fn count_recover_tasks_handles_multidigit_numbering() {
        // Defensive: future task batteries might enumerate past 9.
        let prompt = "Task battery\n\n\
            10. Recover the previously destroyed `ws-X`.\n\
            11. Recover the previously destroyed `ws-Y`.\n";
        assert_eq!(count_recover_tasks_in_prompt(prompt), 2);
    }

    #[test]
    fn count_recover_tasks_does_not_match_mere_mention() {
        // Conservative: a task that *mentions* recover but does not
        // start with the verb must not be counted.
        let prompt = "Task battery\n\n\
            1. Verify recover is documented in README.\n\
            2. Test that the recover snapshot exists.\n";
        assert_eq!(count_recover_tasks_in_prompt(prompt), 0);
    }

    /// End-to-end Fix A.2: a BenchRun whose prompt asks the agent to
    /// recover, and whose transcript contains exactly one
    /// `maw ws recover` invocation, must NOT classify that invocation
    /// as friction. The `WsRecoverInvoked` cluster is suppressed.
    #[test]
    fn task_required_recover_is_not_classified_as_friction() {
        let mut run = synth_run(
            "maw@new-layout",
            RunVerdict::Success,
            OracleBSummary::Green,
            0,
            1,
            None,
        );
        run.transcript.prompt =
            "Task battery\n\n1. Recover the previously destroyed workspace `ws-0`.\n".to_string();
        run.transcript.turns = vec![turn_with_calls(
            1,
            vec![bash(r#"{"cmd":"maw ws recover ws-0 --to ws-1"}"#)],
        )];
        run.total_turns = 1;
        let m = extract_metrics(&run);
        // WsRecoverInvoked cluster suppressed (decremented from 1
        // to 0, and the zeroed entry removed).
        assert_eq!(
            m.per_verb_wasted_turns
                .get(&MawVerbAttribution::WsRecoverInvoked),
            None,
            "task-required recover must not surface as WsRecoverInvoked"
        );
        // And the aggregate work_redone_turns is 0.
        assert_eq!(m.work_redone_turns, MetricValue::count(0));
    }

    /// Inverse: an unsolicited `maw ws recover` (no recover task in
    /// the prompt) IS classified as friction — the suppression is
    /// task-conditional, not unconditional.
    #[test]
    fn unsolicited_recover_still_classified_as_friction() {
        let mut run = synth_run(
            "maw@new-layout",
            RunVerdict::Success,
            OracleBSummary::Green,
            0,
            1,
            None,
        );
        // Task battery has NO recover task.
        run.transcript.prompt =
            "Task battery\n\n1. Create `ws-0`.\n2. Edit a file.\n3. Commit.\n".to_string();
        run.transcript.turns = vec![turn_with_calls(
            1,
            vec![bash(r#"{"cmd":"maw ws recover ws-0 --to ws-1"}"#)],
        )];
        run.total_turns = 1;
        let m = extract_metrics(&run);
        assert_eq!(
            m.per_verb_wasted_turns
                .get(&MawVerbAttribution::WsRecoverInvoked),
            Some(&1),
            "unsolicited recover IS friction"
        );
        assert_eq!(m.work_redone_turns, MetricValue::count(1));
    }

    /// Excess recovers (more invocations than tasks asked for) are
    /// partially suppressed: only the task-required count is removed.
    #[test]
    fn excess_recovers_beyond_task_count_remain_friction() {
        let mut run = synth_run(
            "maw@new-layout",
            RunVerdict::Success,
            OracleBSummary::Green,
            0,
            3,
            None,
        );
        // 1 recover task in the battery.
        run.transcript.prompt =
            "Task battery\n\n1. Recover the previously destroyed `ws-0`.\n".to_string();
        // Agent invoked recover 3 times (2 are noise).
        run.transcript.turns = vec![
            turn_with_calls(1, vec![bash(r#"{"cmd":"maw ws recover ws-0 --to ws-1"}"#)]),
            turn_with_calls(2, vec![bash(r#"{"cmd":"maw ws recover ws-2 --to ws-3"}"#)]),
            turn_with_calls(3, vec![bash(r#"{"cmd":"maw ws recover ws-4 --to ws-5"}"#)]),
        ];
        run.total_turns = 3;
        let m = extract_metrics(&run);
        // 3 attributed - 1 task-required = 2 remaining friction.
        assert_eq!(
            m.per_verb_wasted_turns
                .get(&MawVerbAttribution::WsRecoverInvoked),
            Some(&2),
            "excess recover invocations beyond the task count remain attributed"
        );
    }
}
