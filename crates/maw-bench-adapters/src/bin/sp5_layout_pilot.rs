//! `sp5-layout-pilot` — SP5 directional ergonomics spike (bone bn-2kgu).
//!
//! Runs the same scripted op-stream against:
//!
//! 1. [`maw_bench_adapters::ws_layout_adapter::WsLayoutAdapter`] — current
//!    v2 `ws/` layout (the OLD default).
//! 2. [`maw_bench_adapters::consolidated_layout_adapter::ConsolidatedLayoutAdapter`]
//!    — proposed consolidated `.maw/workspaces/` layout (the SG3 target).
//!
//! Emits a pilot data block + a directional verdict per the SP5 ergonomics
//! contract.
//!
//! # What we measure (directional, not bar-setting)
//!
//! Per bone bn-2kgu hard rules, this is a **MockAgent-only** spike — the
//! script below is a fixed op-stream, not a real LLM. The signals we can
//! extract under these constraints are **structural** (path-shape
//! ergonomics) rather than agent-behavioral. They are:
//!
//! 1. **Equivalence:** both adapters produce the same integrated bytes
//!    given the same op-stream. (If false, the consolidated layout is
//!    not behaviorally identical and the spike has revealed a real
//!    problem before T3.2 implementation.)
//! 2. **Workspace path depth** (number of path components from root).
//!    `ws/<name>` = 2; `.maw/workspaces/<name>` = 3. Affects every
//!    `cd`/`ls`/`find` mental model and every command an agent types.
//! 3. **Workspace path char length** (absolute path length, in chars).
//!    Each command-line invocation referencing the workspace path pays
//!    this cost. Cumulative effect across a multi-turn agent run.
//! 4. **Root visibility footprint** (top-level entries returned by `ls`
//!    without `-a`). Consolidated layout: only the integration content +
//!    `.git/` (admin hidden). Old `ws/` layout: `ws/` is a *visible*
//!    sibling of source. Hidden vs visible coordination dirs are an
//!    agent-discoverability dimension.
//! 5. **End-to-end op-stream wall time**: substrate-internal cost of the
//!    same op sequence on each layout. Bias-check: should be near-equal
//!    (the engines are identical); a large delta indicates incidental
//!    drag (path traversal cost in deeper trees) we should account for.
//! 6. **Lifecycle smoke**: create -> edit -> commit -> merge -> destroy ->
//!    state-snapshot succeeds end-to-end on each layout (bone AC #1).
//!
//! # Pilot rule (frozen)
//!
//! Per `notes/sg2-benchmark-preregistration.md` §3.1, this output is
//! EXCLUDED from any SG2/SG3/SG4 analysis, MUST NOT set bars, MUST NOT
//! appear in publication. The verdict here is **directional only** and
//! gates T3.2 (bn-2sw3)'s implementation strategy, nothing else.
//!
//! Exit codes:
//! - `0` — pilot completed; verdict printed.
//! - `2` — invalid arguments.
//! - `3` — pilot pipeline error.

#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::format_push_string)]
// The pilot binary is a thin CLI shim. Match the existing
// `sg2_sweep_pilot` binary's pragmatic waiver set.

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use maw_bench_adapters::consolidated_layout_adapter::ConsolidatedLayoutAdapter;
use maw_bench_adapters::ws_layout_adapter::WsLayoutAdapter;
use maw_bench_adapters::{NoopAgent, ScriptedOp, StateSnapshot, StepOutcome, Substrate};
use maw_scenario::{BaseRef, WsId};

fn usage() -> &'static str {
    "usage: sp5-layout-pilot [<out-md>]\n\
     runs the SP5 layout-ergonomics directional spike (mock-agent only).\n\
     <out-md> if given, the verdict + pilot table are appended to this file."
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PilotResult {
    arm: &'static str,
    layout_label: &'static str,
    /// Path of last-created workspace, as the agent would see it.
    /// Held for diagnostic dumping; not currently rendered into the
    /// table — kept on the struct so a later pilot pass can surface it.
    last_ws_abs_path: PathBuf,
    /// Depth (number of components from root to ws dir).
    ws_path_depth: usize,
    /// Absolute path char length.
    ws_path_chars: usize,
    /// Top-level entries visible to `ls` (no `-a`) at root.
    visible_root_entries: Vec<String>,
    /// Did the lifecycle complete end-to-end without error?
    lifecycle_ok: bool,
    /// Step outcomes for the lifecycle script. Retained for future
    /// pilot extensions that surface per-step notes.
    steps: Vec<StepOutcome>,
    /// End-state snapshot.
    snapshot: StateSnapshot,
    /// Wall time for the entire scripted run.
    wall_ms: u128,
}

/// The op-stream — a small grid combining the two SP5 task shapes (per
/// bone description): one collision-light, one with overlap. We replay
/// the same script against each layout; the only delta between
/// PilotResults is the layout, isolating the directional signal.
fn pilot_script() -> Vec<ScriptedOp> {
    let a = WsId::slot(0);
    let b = WsId::slot(1);
    vec![
        // --- TASK 1: two independent agents, no overlap (light) ---
        ScriptedOp::Create {
            ws: a.clone(),
            base: BaseRef::Main,
        },
        ScriptedOp::Create {
            ws: b.clone(),
            base: BaseRef::Main,
        },
        ScriptedOp::Edit {
            ws: a.clone(),
            path: "src/lib_a.rs".to_string(),
            content: "pub fn alpha() {}\n".to_string(),
        },
        ScriptedOp::Edit {
            ws: b.clone(),
            path: "src/lib_b.rs".to_string(),
            content: "pub fn beta() {}\n".to_string(),
        },
        ScriptedOp::Commit {
            ws: a.clone(),
            msg: "feat: alpha".to_string(),
        },
        ScriptedOp::Commit {
            ws: b.clone(),
            msg: "feat: beta".to_string(),
        },
        ScriptedOp::Merge {
            srcs: vec![a.clone(), b.clone()],
            target: "default".to_string(),
            destroy: true,
        },
        // --- TASK 2: two agents touching same file (overlap-aware merge) ---
        ScriptedOp::Create {
            ws: a.clone(),
            base: BaseRef::Main,
        },
        ScriptedOp::Edit {
            ws: a.clone(),
            path: "src/lib_a.rs".to_string(),
            content: "pub fn alpha() {}\npub fn alpha_v2() {}\n".to_string(),
        },
        ScriptedOp::Commit {
            ws: a.clone(),
            msg: "feat: alpha_v2".to_string(),
        },
        ScriptedOp::Merge {
            srcs: vec![a.clone()],
            target: "default".to_string(),
            destroy: true,
        },
    ]
}

/// Capture the structural signals for a layout under the pilot script.
fn run_arm<S>(arm: &'static str, layout_label: &'static str, mut subs: S) -> PilotResult
where
    S: Substrate,
{
    let script = pilot_script();
    let t0 = Instant::now();
    let steps = NoopAgent::drive(&mut subs, &script).expect("driver");
    let wall_ms = t0.elapsed().as_millis();
    let snapshot = subs.state_snapshot().expect("snapshot");

    // We need the "last created workspace path" — for both layouts the
    // last-created ws is slot 0 (from Task 2), which has been destroyed
    // by end-of-script. To measure path shape, we re-create one extant
    // workspace for the structural sample.
    let probe = WsId::slot(2);
    subs.create_workspace(&probe, &BaseRef::Main)
        .expect("probe create");
    let probe_dir = synthesize_ws_path(&subs, arm, &probe);
    let depth = probe_dir
        .strip_prefix(subs.root())
        .map(|p| p.components().count())
        .unwrap_or(0);
    let chars = probe_dir.to_string_lossy().len();
    let visible = visible_top_level(subs.root());
    let lifecycle_ok = steps.iter().all(|s| s.ok);
    let _ = subs.destroy(&probe, false);
    let _ = subs.cleanup();

    PilotResult {
        arm,
        layout_label,
        last_ws_abs_path: probe_dir,
        ws_path_depth: depth,
        ws_path_chars: chars,
        visible_root_entries: visible,
        lifecycle_ok,
        steps,
        snapshot,
        wall_ms,
    }
}

/// The adapters all carry private knowledge of their `ws_dir` mapping;
/// we synthesize the absolute path here using the adapter arm name.
fn synthesize_ws_path<S: Substrate>(subs: &S, arm: &str, ws: &WsId) -> PathBuf {
    let root = subs.root().clone();
    match arm {
        "sp5-ws-layout" => root.join("ws").join(&ws.0),
        "sp5-consolidated-layout" => root.join(".maw").join("workspaces").join(&ws.0),
        _ => root.join(&ws.0),
    }
}

fn visible_top_level(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // `ls` (no -a) hides dotfiles.
        if name.starts_with('.') {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out
}

/// Equivalence check: the two layouts must produce identical
/// integrated-byte trees for the *agent-task-visible* file set.
/// If they don't, the consolidated layout has behavioral drift and
/// the spike has revealed a real T3.2 risk.
///
/// **Excluded keys** (by-design layout-specific, not part of the
/// agent-task surface):
/// - `.gitignore` — each layout pins its own ignore rules (the ws
///   layout ignores `ws/`; consolidated ignores `.maw/`). Different
///   strings, same *intent* (hide the runtime admin dir).
fn equivalence_check(ws: &PilotResult, cons: &PilotResult) -> EquivalenceVerdict {
    const LAYOUT_PRIVATE_KEYS: &[&str] = &[".gitignore"];
    let ws_files = &ws.snapshot.integrated_files;
    let cons_files = &cons.snapshot.integrated_files;
    let mut diffs: Vec<String> = Vec::new();
    let all_keys: std::collections::BTreeSet<&String> =
        ws_files.keys().chain(cons_files.keys()).collect();
    for k in &all_keys {
        if LAYOUT_PRIVATE_KEYS.contains(&k.as_str()) {
            continue;
        }
        match (ws_files.get(*k), cons_files.get(*k)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => diffs.push(format!("byte-diff: {k}")),
            (Some(_), None) => diffs.push(format!("only-in-ws-layout: {k}")),
            (None, Some(_)) => diffs.push(format!("only-in-consolidated: {k}")),
            (None, None) => {}
        }
    }
    EquivalenceVerdict {
        ok: diffs.is_empty(),
        diffs,
    }
}

struct EquivalenceVerdict {
    ok: bool,
    diffs: Vec<String>,
}

/// Repeat the pilot N times per arm to get a wall-time distribution and
/// rule out a one-shot timing artifact. Each iteration runs against a
/// fresh substrate (tempdir thrown away between runs).
fn repeat_wall_time<F, S>(n: u32, mk: F) -> Vec<u128>
where
    F: Fn() -> S,
    S: Substrate,
{
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let mut subs = mk();
        let script = pilot_script();
        let t0 = Instant::now();
        let _ = NoopAgent::drive(&mut subs, &script).expect("driver");
        out.push(t0.elapsed().as_millis());
        let _ = subs.cleanup();
    }
    out
}

fn median(mut xs: Vec<u128>) -> u128 {
    if xs.is_empty() {
        return 0;
    }
    xs.sort_unstable();
    xs[xs.len() / 2]
}

fn min_max(xs: &[u128]) -> (u128, u128) {
    let mn = *xs.iter().min().unwrap_or(&0);
    let mx = *xs.iter().max().unwrap_or(&0);
    (mn, mx)
}

fn render_table(ws: &PilotResult, cons: &PilotResult) -> String {
    let mut s = String::new();
    s.push_str("## SP5 directional pilot — structural read\n\n");
    s.push_str(&format!(
        "| metric | ws (current default) | consolidated (proposed) | delta |\n\
         |---|---|---|---|\n\
         | adapter arm | `{}` | `{}` | — |\n\
         | layout shape | {} | {} | — |\n\
         | workspace path depth | {} | {} | {:+} |\n\
         | workspace path chars (abs, slot=2) | {} | {} | {:+} |\n\
         | visible top-level (ls, no -a) | {} entries | {} entries | {:+} |\n\
         | visible entries | `{}` | `{}` | — |\n\
         | lifecycle ok (script all-green) | {} | {} | — |\n\
         | end-state live workspaces | {} | {} | — |\n\
         | end-state integrated files | {} | {} | — |\n\
         | wall-time (single run, ms) | {} | {} | {:+} |\n",
        ws.arm,
        cons.arm,
        ws.layout_label,
        cons.layout_label,
        ws.ws_path_depth,
        cons.ws_path_depth,
        cons.ws_path_depth as i64 - ws.ws_path_depth as i64,
        ws.ws_path_chars,
        cons.ws_path_chars,
        cons.ws_path_chars as i64 - ws.ws_path_chars as i64,
        ws.visible_root_entries.len(),
        cons.visible_root_entries.len(),
        cons.visible_root_entries.len() as i64 - ws.visible_root_entries.len() as i64,
        ws.visible_root_entries.join(", "),
        cons.visible_root_entries.join(", "),
        ws.lifecycle_ok,
        cons.lifecycle_ok,
        ws.snapshot.live_workspaces.len(),
        cons.snapshot.live_workspaces.len(),
        ws.snapshot.integrated_files.len(),
        cons.snapshot.integrated_files.len(),
        ws.wall_ms,
        cons.wall_ms,
        cons.wall_ms as i64 - ws.wall_ms as i64,
    ));
    s
}

fn render_repeat_block(ws_times: &[u128], cons_times: &[u128]) -> String {
    let (ws_min, ws_max) = min_max(ws_times);
    let (cs_min, cs_max) = min_max(cons_times);
    let ws_med = median(ws_times.to_vec());
    let cs_med = median(cons_times.to_vec());
    format!(
        "\n## Wall-time distribution (N={n} reps/arm; pilot — NOT a bar)\n\n\
         | arm | min (ms) | median (ms) | max (ms) |\n\
         |---|---|---|---|\n\
         | sp5-ws-layout | {ws_min} | {ws_med} | {ws_max} |\n\
         | sp5-consolidated-layout | {cs_min} | {cs_med} | {cs_max} |\n\
         \n\
         Bias check: the medians should be within an order of magnitude of\n\
         each other since the engine is identical (plain git-worktree).\n\
         A large delta would suggest path-traversal cost in the deeper\n\
         consolidated tree; a small delta confirms the layouts are\n\
         engine-equivalent and the structural signals above are the\n\
         load-bearing read.\n",
        n = ws_times.len(),
    )
}

fn render_verdict(
    ws: &PilotResult,
    cons: &PilotResult,
    eq: &EquivalenceVerdict,
) -> (String, String) {
    // Directional logic:
    // - If equivalence FAILS, the verdict is `negative` (layout has
    //   behavioral drift that T3.2 will inherit).
    // - If equivalence PASSES and the structural deltas are small
    //   (+1 depth, modest +chars, hidden admin dir is the expected
    //   feature not a bug, lifecycle green), the verdict is `positive`.
    // - Else `inconclusive`.
    let verdict = if !eq.ok || !ws.lifecycle_ok || !cons.lifecycle_ok {
        "negative"
    } else if cons.ws_path_depth == ws.ws_path_depth + 1
        && cons.ws_path_chars >= ws.ws_path_chars
        && cons.ws_path_chars <= ws.ws_path_chars + 20
        && cons.visible_root_entries.len() <= ws.visible_root_entries.len()
    {
        // The expected, on-spec shape: +1 depth (`.maw/` insertion),
        // a handful more chars, and fewer/equal visible entries (the
        // admin dir becomes hidden — design intent).
        "positive"
    } else {
        "inconclusive"
    };
    // Surface a qualitative observation: the *kind* of visible entries
    // matters more than the count. The ws-layout exposes `repo.git, ws`
    // (admin-shaped); the consolidated layout exposes `README.md, src`
    // (project-shaped). For an agent dropped into `<root>` with `ls`,
    // the consolidated layout is immediately recognizable as a project
    // tree; the v2 layout requires navigation into `ws/default/` to
    // see source. This is a directional ergonomics win we cannot
    // quantify under MockAgent but is worth flagging for T3.5.
    let visible_shape_note = {
        let ws_admin = ws
            .visible_root_entries
            .iter()
            .any(|e| e == "repo.git" || e == "ws");
        let cons_project = cons
            .visible_root_entries
            .iter()
            .any(|e| e == "README.md" || e == "src");
        if ws_admin && cons_project {
            " The visible-entries SHAPE also favors the consolidated layout: \
             at root, `ls` shows project content (`README.md, src/`) vs \
             admin scaffolding (`repo.git/, ws/`) — agents recognize the \
             former as a project tree without navigation."
        } else {
            ""
        }
    };
    let rationale = match verdict {
        "negative" => format!(
            "Equivalence-check failed ({} diffs) OR lifecycle errored. \
             T3.2 should NOT ship consolidated-by-default; ship configurability \
             with the old `ws/` default and escalate to T3.5 for resolution.",
            eq.diffs.len()
        ),
        "positive" => format!(
            "Equivalence holds (byte-identical integrated tree, gitignore \
             excluded as layout-private). Structural deltas are on-spec: \
             +1 path depth (single `.maw/` admin level inserted), +{} chars \
             per workspace path. Lifecycle green on both arms.{} \
             No directional red flag at MockAgent fidelity. T3.2 may ship \
             consolidated-by-default; the real-LLM gate (T3.5 / bn-iux4 / \
             bn-1uzn) remains the binding decision.",
            cons.ws_path_chars as i64 - ws.ws_path_chars as i64,
            visible_shape_note,
        ),
        _ => format!(
            "Structural deltas outside the expected envelope (depth \
             +{} expected +1; chars +{} expected ≤+20; visible \
             entries delta {}). Escalate to T3.5 for formal measurement.",
            cons.ws_path_depth as i64 - ws.ws_path_depth as i64,
            cons.ws_path_chars as i64 - ws.ws_path_chars as i64,
            cons.visible_root_entries.len() as i64 - ws.visible_root_entries.len() as i64,
        ),
    };
    (verdict.to_string(), rationale)
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let out_md: Option<PathBuf> = match args.next() {
        Some(a) if a == "--help" || a == "-h" => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Some(a) => Some(PathBuf::from(a)),
        None => None,
    };

    eprintln!("sp5-layout-pilot: running structural-ergonomics pilot...");

    // --- single-run structural read ---
    let ws_run = run_arm(
        "sp5-ws-layout",
        "ws/<name>/ (root bare, ws/default privileged)",
        match WsLayoutAdapter::new() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("ws-layout substrate init failed: {e:?}");
                return ExitCode::from(3);
            }
        },
    );
    let cons_run = run_arm(
        "sp5-consolidated-layout",
        ".maw/workspaces/<name>/ (root non-bare, root is target)",
        match ConsolidatedLayoutAdapter::new() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("consolidated-layout substrate init failed: {e:?}");
                return ExitCode::from(3);
            }
        },
    );

    // --- equivalence ---
    let eq = equivalence_check(&ws_run, &cons_run);

    // --- repeat for wall-time distribution (small N — pilot, not bar) ---
    let n_reps: u32 = std::env::var("SP5_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let ws_times = repeat_wall_time(n_reps, || WsLayoutAdapter::new().expect("ws-layout"));
    let cons_times = repeat_wall_time(n_reps, || {
        ConsolidatedLayoutAdapter::new().expect("consolidated-layout")
    });

    // --- verdict ---
    let (verdict, rationale) = render_verdict(&ws_run, &cons_run, &eq);

    let mut report = String::new();
    report.push_str("# SP5 layout-ergonomics directional spike — pilot output\n\n");
    report.push_str(
        "Per `notes/sg2-benchmark-preregistration.md` §3.1 (Pilot rule), \
         this output is HARNESS-VALIDATION ONLY. It MUST NOT set bars and \
         MUST NOT appear in the SG2/SG3/SG4 publication. The verdict below \
         is directional and gates T3.2 (bn-2sw3) strategy only.\n\n",
    );
    report.push_str(&render_table(&ws_run, &cons_run));
    report.push_str(&render_repeat_block(&ws_times, &cons_times));
    report.push_str("\n## Equivalence check\n\n");
    if eq.ok {
        report.push_str(
            "PASS — both layouts produce byte-identical integrated trees \
             under the pilot op-stream.\n",
        );
    } else {
        report.push_str("FAIL — divergent integrated trees:\n");
        for d in &eq.diffs {
            report.push_str(&format!("- {d}\n"));
        }
    }
    report.push_str(&format!(
        "\n## DIRECTIONAL VERDICT: {verdict}\n\n{rationale}\n\n\
         **Caveat (T2.7 §3.1):** MockAgent-only structural read. A real-LLM \
         agent might trip on hidden-dir invisibility, deeper `cd`/`find` \
         paths, or AGENTS.md-stub indirection in ways this spike cannot \
         detect. The binding gate remains T3.5 (bn-1uzn) + bn-iux4 \
         pre-registration.\n",
    ));

    print!("{report}");

    if let Some(p) = out_md {
        if let Err(e) = std::fs::write(&p, &report) {
            eprintln!("WARN: failed to write {}: {e}", p.display());
        } else {
            eprintln!("sp5-layout-pilot: wrote {}", p.display());
        }
    }

    eprintln!("sp5-layout-pilot: done; verdict={verdict}");
    ExitCode::SUCCESS
}
