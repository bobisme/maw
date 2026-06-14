//! Permanent regression-seed corpus generator (bn-3ryq, T1.8).
//!
//! This file is the **documented recipe** by which the two permanent
//! regression-seed corpus entries under `tests/corpus/dst/` are produced.
//! Both tests are `#[ignore]` so they only run on demand; the corpus
//! `*.json` files they emit are checked into git and consumed by the
//! T1.7 [`sg1_per_commit_corpus`] replay loop in `sg1_dst.rs` (which
//! auto-loads any file that deserializes as
//! [`maw_assurance::shrinker::ShrinkerCorpusEntry`]).
//!
//! ## Why a test (and not a `cargo run --example` binary)
//!
//! The entries are produced by feeding planted defects through the
//! exact same `ShrinkerCorpusEntry::from_report(...)` codepath the
//! shrinker uses to emit a failing-seed bundle in CI. Living next to
//! `sg1_dst.rs` keeps that contract obvious — schema drift in
//! `ShrinkerCorpusEntry` would break compilation here, not silently
//! produce a corpus entry the per-commit gate skips.
//!
//! ## Why permanent regression seeds (the bone)
//!
//! Both entries are pinned `expected = "known_violation"`. The per-commit
//! corpus loader treats that as: *the planted defect MUST trip the
//! oracle on every replay*; if it stops tripping, the gate goes red.
//! This way the corpus encodes both the historical incident's class AND
//! a self-test that the gate's detection of that class still works.
//!
//! Hand-construction proofs that the underlying production fixes are
//! load-bearing (i.e. that reverting them would turn the gate red) live
//! in:
//!   - `oracle_b::tests::b1_fires_on_bn_cm63_reproduction` (the bn-cm63
//!     scar: dangling `refs/manifold/head/<ws>` with no ws and no live
//!     merge → Oracle B B1 RED);
//!   - `oracle_a::tests::lost_commits_2026_02_05_incident_trips_oracle_a`
//!     (the documented 2026-02-05 incident: overlap-edit + conflict-
//!     resolution drops one side + no recovery ref → Oracle A
//!     `ReachabilityLost`).
//!
//! ## Re-running
//!
//! ```bash
//! cargo test -p maw-assurance --features oracles --test sg1_corpus_emit \
//!     -- --ignored --nocapture
//! ```
//!
//! After re-emit, inspect the diff under `tests/corpus/dst/*.json` — the
//! shrinker output is deterministic per (seed, planted) so a clean
//! re-emit on a clean main should be a no-op diff.

#![cfg(feature = "oracles")]
#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::items_after_statements
)]

use std::fs;
use std::path::PathBuf;

use maw_assurance::in_proc::{InProcDriver, PlantedDefect, StepVerdict};
use maw_assurance::scenario::{
    CANONICAL_BN_CM63_SEED, ConditionProfile, Op, ScenarioPlan, generate_plan,
};
use maw_assurance::shrinker::{ShrinkerCorpusEntry, shrink};

/// Path to `tests/corpus/dst/` resolved from the maw-assurance manifest dir.
/// Mirrors the resolver in `sg1_dst.rs::corpus_dir`.
fn corpus_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(std::path::Path::parent)
        .expect("CARGO_MANIFEST_DIR has at least two ancestors");
    workspace_root.join("tests").join("corpus").join("dst")
}

/// Drive `plan` + `planted` once and return the verdict (used to harvest the
/// original `StepVerdict` for the shrinker).
fn drive_once(plan: &ScenarioPlan, planted: &[PlantedDefect]) -> StepVerdict {
    let mut driver = InProcDriver::new()
        .expect("in-proc driver init")
        .with_planted(planted.to_vec());
    driver.drive(plan).verdict
}

/// Write `entry` to `tests/corpus/dst/<name>.json` as pretty JSON.
fn write_corpus_entry(name: &str, entry: &ShrinkerCorpusEntry) {
    let dir = corpus_dir();
    fs::create_dir_all(&dir).expect("create corpus dir");
    let path = dir.join(format!("{name}.json"));
    let body = serde_json::to_string_pretty(entry).expect("serialize corpus entry");
    fs::write(&path, body).expect("write corpus entry");
    eprintln!(
        "[corpus] wrote {} ({} steps, {} planted, expected={})",
        path.display(),
        entry.num_steps,
        entry.planted.len(),
        entry.expected
    );
}

/// Build the bn-cm63 corpus entry.
///
/// Recipe:
///   1. Generate a plan from [`CANONICAL_BN_CM63_SEED`] under the default
///      profile. The plan reaches the bn-cm63 hostile interleaving by
///      construction (proven by `scenario::tests::seed_reaches_bn_cm63_class`).
///   2. Plant a [`PlantedDefect::DanglingHeadRef`] on `ws-0` (the generator
///      pre-seeds slot 0 as a starter ws, so it always exists in the in-proc
///      driver's repo). The plant materialises the **exact ref shape** of the
///      bn-cm63 incident: `refs/manifold/head/<ws>` present, `ws/<ws>/`
///      absent, no live merge.
///   3. Drive once to confirm Oracle B B1 fires (sanity).
///   4. Shrink the plan against the planted defect (delta-debug against the
///      same `DanglingHeadRef` verdict). Output is minimal and replay-sound.
///   5. Tag `expected = "known_violation"` so the per-commit corpus loader
///      treats this as a regression check: if the planted defect ever stops
///      tripping Oracle B (because someone broke the gate's B1 detection),
///      the per-commit gate goes RED.
#[test]
#[ignore = "Emits tests/corpus/dst/bn-cm63-destroy-vs-inflight-merge.json — run on demand."]
fn emit_bn_cm63_corpus_entry() {
    let plan = generate_plan(CANONICAL_BN_CM63_SEED, &ConditionProfile::default(), 32);
    let planted = vec![PlantedDefect::DanglingHeadRef { ws: "ws-0".into() }];
    let verdict = drive_once(&plan, &planted);
    assert!(
        matches!(verdict, StepVerdict::OracleB(_)),
        "bn-cm63 plant must trip Oracle B; got {verdict:?}"
    );
    let report = shrink(&plan, &planted, verdict);
    let mut entry = ShrinkerCorpusEntry::from_report(&report, &planted);
    // Override the generic shrinker description with the load-bearing
    // human-readable context for this permanent regression seed.
    entry.expected = "known_violation".to_string();
    entry.description = format!(
        "bn-cm63 destroy-vs-in-flight-merge head-ref leak (seed={seed}, \
         Oracle B B1 DanglingHeadRef on ws-0). Permanent regression seed: \
         the planted DanglingHeadRef defect materialises the exact ref \
         shape of the bn-cm63 incident (refs/manifold/head/<ws> present, \
         ws/<ws>/ absent, no live merge). Encodes the bn-cm63 *class* so \
         the SG1 gate can never silently regress on it. Hand-construction \
         proof that reverting the production guard (guard_destroy_against_\
         inflight_merge in crates/maw-cli/src/workspace/create.rs OR \
         prune_dangling_head_refs in crates/maw-cli/src/ref_gc.rs) would \
         trip this class lives in oracle_b::tests::b1_fires_on_bn_cm63_\
         reproduction.",
        seed = entry.seed
    );
    write_corpus_entry("bn-cm63-destroy-vs-inflight-merge", &entry);
}

/// Find the lowest seed under the default profile + 64-step plan that:
///   - contains an `Op::Merge { srcs, .. }` with `srcs.len() >= 2` whose
///     sources include at least two workspaces that previously edited a
///     `shared/*` path (the generator's overlapping-edit pattern in
///     `scenario.rs::try_emit::OpKind::EditFiles`); AND
///   - leaves at least one workspace with a commit and a state ref still
///     present at plan end (so a `WorkLoss` plant has a real witness blob
///     to make unreachable).
///
/// Returns `(seed, plan, plant_target_ws)`.
fn find_lost_commits_seed() -> (u64, ScenarioPlan, String) {
    let profile = ConditionProfile::default();
    for seed in 0..1000_u64 {
        let plan = generate_plan(seed, &profile, 64);
        if !plan_has_overlap_merge(&plan) {
            continue;
        }
        if let Some(target) = pick_committed_surviving_ws(&plan) {
            return (seed, plan, target);
        }
    }
    panic!(
        "no seed in 0..1000 produced an overlapping-edit + multi-source merge \
         pattern with a committed surviving workspace under the default \
         profile; widen the search or re-tune the chooser."
    );
}

/// Does `plan` contain an `Op::Merge` with `>=2` sources where at least two
/// of those sources previously edited a `shared/*` path?
fn plan_has_overlap_merge(plan: &ScenarioPlan) -> bool {
    let mut shared_editors: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for step in &plan.steps {
        match &step.op {
            Op::EditFiles { ws, files } => {
                if files.iter().any(|f| f.path.starts_with("shared/")) {
                    shared_editors.insert(ws.0.clone());
                }
            }
            Op::Merge { srcs, .. } if srcs.len() >= 2 => {
                let n = srcs
                    .iter()
                    .filter(|s| shared_editors.contains(&s.0))
                    .count();
                if n >= 2 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Walk the plan abstractly and pick a workspace that:
///   - has at least one `Op::Commit` against it (so it authored a blob);
///   - is not subsequently `Op::Destroy`ed before plan end; AND
///   - is not subsequently a source of an `Op::Merge { destroy: true }`
///     (the in-proc driver's `do_merge` destroys sources when `destroy=true`
///     and no fault is attached — see `in_proc.rs::do_merge`).
///
/// Returns the first such workspace (deterministic ordering: by first
/// commit step in plan order). Returns `None` if no such ws exists.
fn pick_committed_surviving_ws(plan: &ScenarioPlan) -> Option<String> {
    let mut committed_at: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut destroyed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (i, step) in plan.steps.iter().enumerate() {
        match &step.op {
            Op::Commit { ws, .. } => {
                committed_at.entry(ws.0.clone()).or_insert(i);
            }
            Op::Destroy { ws, .. } => {
                destroyed.insert(ws.0.clone());
            }
            Op::Merge {
                srcs,
                destroy: true,
                ..
            } => {
                for s in srcs {
                    destroyed.insert(s.0.clone());
                }
            }
            _ => {}
        }
    }
    // Pick the survivor with the earliest commit (most witnesses harvested).
    committed_at
        .into_iter()
        .filter(|(ws, _)| !destroyed.contains(ws))
        .min_by_key(|(_, i)| *i)
        .map(|(ws, _)| ws)
}

/// Build the lost-commits 2026-02-05 corpus entry.
///
/// Recipe:
///   1. Search `0..1000` for the lowest seed whose plan emits an
///      overlapping-edit + multi-source merge pattern AND has a
///      committed surviving workspace to plant on (see
///      [`find_lost_commits_seed`] for the precise criteria).
///   2. Plant a [`PlantedDefect::WorkLoss`] on that surviving workspace.
///      The plant materialises the **Oracle A class** the incident
///      produced: a blob authored by an extant workspace leaves the
///      frontier `U(F)` because no recovery ref was pinned.
///   3. Drive once to confirm Oracle A fires (sanity).
///   4. Shrink against the planted defect.
///   5. Tag `expected = "known_violation"`.
///
/// Hand-construction proof of the underlying Oracle A class lives in
/// `oracle_a::tests::lost_commits_2026_02_05_incident_trips_oracle_a`
/// (which encodes the literal incident: overlap edit on shared.txt,
/// merge picks B's tree dropping A, A's refs deleted with no recovery).
#[test]
#[ignore = "Emits tests/corpus/dst/lost-commits-2026-02-05.json — run on demand."]
fn emit_lost_commits_corpus_entry() {
    let (seed, plan, target_ws) = find_lost_commits_seed();
    eprintln!("[corpus] lost-commits seed pinned at {seed}, plant target ws={target_ws}");
    let planted = vec![PlantedDefect::WorkLoss {
        ws: target_ws.clone(),
    }];
    let verdict = drive_once(&plan, &planted);
    assert!(
        matches!(verdict, StepVerdict::OracleA(_)),
        "lost-commits plant must trip Oracle A; got {verdict:?}"
    );
    let report = shrink(&plan, &planted, verdict);
    let mut entry = ShrinkerCorpusEntry::from_report(&report, &planted);
    entry.expected = "known_violation".to_string();
    entry.description = format!(
        "2026-02-05 lost-commits incident class (seed={seed}, Oracle A \
         ReachabilityLost on {target_ws}). Permanent regression seed: \
         the plan emits an overlapping-edit + multi-source merge pattern \
         (the exact shape of the incident: two workspaces overlap-edit \
         the same file, a merge resolves by dropping one side, the \
         dropped side's recovery ref is absent). The planted WorkLoss \
         defect materialises the Oracle A class — a blob authored by an \
         extant workspace leaves U(F) because no recovery ref was \
         pinned. Encodes the incident *class* so the SG1 gate can never \
         silently regress on it. Hand-construction proof of the literal \
         incident lives in oracle_a::tests::lost_commits_2026_02_05_\
         incident_trips_oracle_a; see notes/incident-lost-commits-2026-\
         02-05.md for the original report.",
        seed = seed,
        target_ws = target_ws
    );
    write_corpus_entry("lost-commits-2026-02-05", &entry);
}
