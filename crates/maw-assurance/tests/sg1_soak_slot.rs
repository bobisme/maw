//! bn-30v6e: `scripts/sg1-soak/slot.sh` + `status.sh` against a stub binary.
//!
//! Proves both directions of the infra-vs-violation split in the soak driver:
//! - exit 75 + `[sg1] INFRA-FAILURE:` marker => infra row, no accrual, no
//!   STOP, bounded by `INFRA_HALT_AFTER` consecutive slots (INFRA-HALT);
//! - every other failure shape (plain failure, exit 75 without marker,
//!   marker with another exit code, marker next to a violation) => violation
//!   STOP, exactly as before (fail closed).
//!
//! Runs only against a temp `SG1_SOAK_STATE` — never the live campaign.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::missing_panics_doc)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const STEPS: u64 = 4;
const SLOT_SEEDS: u64 = 10;
const BASE: u64 = 1000;

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sg1-soak")
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn tools_available() -> bool {
    let ok = ["bash", "flock", "nice", "ionice", "grep"]
        .iter()
        .all(|t| have(t));
    if !ok {
        eprintln!("skipping: bash/flock/nice/ionice/grep not all available");
    }
    ok
}

/// Stub for the pinned `sg1_dst` binary. `STUB_MODE` picks the behaviour.
const STUB: &str = r#"#!/usr/bin/env bash
# The real harness's begin line (unchanged since bn-1gp4).
begin() {
  printf '[sg1] nightly soak begin: seeds=%s steps=%s base_seed=0x%016x\n' \
    "${1:-$SG1_NIGHTLY_SEEDS}" "${2:-$SG1_NIGHTLY_STEPS}" "${3:-$SG1_BASE_SEED}"
}
# The real harness's end line: the allocated range plus the canonical seed.
# The evidence counters are what a healthy trunk-tier harness reports for
# SLOT_SEEDS=10 (bn-36chi evidence floor: all at or above it).
TRUNK_OK="trunk_updates=30 trunk_crashes=6 dirty_trunk_merges=27 displacement_checks=91 replay_checks=200 trunk_drains=1"
end_clean() {
  local n=$(( ${1:-$SG1_NIGHTLY_SEEDS} + 1 ))
  echo "[sg1] nightly soak end: seeds=$n clean=$n violations=0 driver_total=1s wall=1s oracle_a_checks=55 oracle_b_checks=55 witnesses=17 $TRUNK_OK harness_errors=0"
}
case "${STUB_MODE:?}" in
  clean)
    begin; end_clean
    exit 0 ;;
  wrong_steps)
    begin "" $((SG1_NIGHTLY_STEPS + 1)); end_clean
    exit 0 ;;
  wrong_base)
    begin "" "" $((SG1_BASE_SEED + 1)); end_clean
    exit 0 ;;
  default_seeds)
    begin 100000; end_clean 100000
    exit 0 ;;
  no_begin)
    end_clean
    exit 0 ;;
  infra_while_peer_violates)
    echo "VIOLATION: peer slot at base_seed=7 (rc=101). Log: x" > "$SG1_SOAK_STATE/STOP"
    echo '[sg1] INFRA-FAILURE: EDQUOT: Disk quota exceeded (seed=5; run is NOT counted)'
    exit 75 ;;
  clean_while_peer_violates)
    # A parallel slot halts the campaign while this one is still running.
    echo "VIOLATION: peer slot at base_seed=7 (rc=101). Log: x" > "$SG1_SOAK_STATE/STOP"
    begin; end_clean
    exit 0 ;;
  clean_evidence)
    begin
    echo "[sg1] nightly soak end: seeds=$((SG1_NIGHTLY_SEEDS + 1)) clean=$((SG1_NIGHTLY_SEEDS + 1)) violations=0 driver_total=1s wall=1s oracle_a_checks=55 oracle_b_checks=55 witnesses=17 workspaces_observed=9 commits_observed=6 trunk_writes=40 trunk_updates=12 trunk_crashes=2 dirty_trunk_merges=9 displacement_checks=31 replay_judgements=10 replay_checks=77 trunk_drains=3 harness_errors=0"
    exit 0 ;;
  clean_no_trunk)
    # A pre-bn-1h9ue harness: no trunk counters at all.
    begin
    echo "[sg1] nightly soak end: seeds=$((SG1_NIGHTLY_SEEDS + 1)) clean=$((SG1_NIGHTLY_SEEDS + 1)) violations=0 driver_total=1s wall=1s"
    exit 0 ;;
  clean_no_crashes)
    # Clean and non-vacuous per seed, but the campaign stopped crashing the
    # target update (e.g. a generator change zeroed the crash windows).
    begin
    echo "[sg1] nightly soak end: seeds=$((SG1_NIGHTLY_SEEDS + 1)) clean=$((SG1_NIGHTLY_SEEDS + 1)) violations=0 driver_total=1s wall=1s oracle_a_checks=55 oracle_b_checks=55 witnesses=17 trunk_updates=30 trunk_crashes=0 dirty_trunk_merges=27 displacement_checks=91 replay_checks=200 trunk_drains=0 harness_errors=0"
    exit 0 ;;
  harness_error)
    echo "[sg1] HARNESS-ERROR seed=3 (oracles did not judge this seed; counted as a violation): HarnessError(..)"
    echo "SG1 nightly soak FAILED (release-blocking; §7 acceptance gate):"
    exit 101 ;;
  harness_error_with_marker)
    echo "[sg1] HARNESS-ERROR seed=3 (oracles did not judge this seed; counted as a violation): HarnessError(..)"
    echo "[sg1] INFRA-FAILURE: EDQUOT: Disk quota exceeded"
    exit 75 ;;
  infra)
    echo "[infra] host resource failure: EDQUOT: in-proc driver init: git init failed"
    echo '[sg1] INFRA-FAILURE: EDQUOT: in-proc driver init: "git" init: Disk quota exceeded (seed=5, after 3 clean seeds; run is NOT counted)'
    exit 75 ;;
  violation)
    echo "SG1 nightly soak FAILED (release-blocking; §7 acceptance gate):"
    echo "  - seed=7 verdict=OracleA(OracleAClass { kind: \"ReachabilityLost\" })"
    exit 101 ;;
  rc75_no_marker)
    echo "something exited 75 without the marker"
    exit 75 ;;
  marker_rc1)
    echo "[sg1] INFRA-FAILURE: EDQUOT: Disk quota exceeded"
    exit 1 ;;
  marker_with_violation)
    echo "[sg1] nightly soak progress: 5/11  clean=4  violations=1  elapsed=1s"
    echo "[sg1] INFRA-FAILURE: ENOSPC: No space left on device"
    exit 75 ;;
  indented_marker)
    echo "  note: [sg1] INFRA-FAILURE: EDQUOT: quoted from a log"
    exit 75 ;;
  *) echo "bad STUB_MODE"; exit 2 ;;
esac
"#;

struct Campaign {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    tmpdir: PathBuf,
}

impl Campaign {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("config.env"),
            format!(
                "MAW_REPO=/nonexistent\nSTEPS={STEPS}\nSLOT_SEEDS={SLOT_SEEDS}\nPARALLEL=1\n\
                 TARGET_OPSTEPS=1000000\nINFRA_HALT_AFTER=3\nPINNED_SRC_SHA=stub\nPINNED_AT=now\n"
            ),
        )
        .unwrap();
        fs::write(state.join("cursor"), format!("{BASE}\n")).unwrap();
        fs::write(state.join("cumulative"), "0\n").unwrap();
        fs::write(state.join("ledger.jsonl"), "").unwrap();
        let bin = state.join("sg1_dst.pinned");
        fs::write(&bin, STUB).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let tmpdir = tmp.path().join("soak-tmp").join("nested");
        Self {
            _tmp: tmp,
            state,
            tmpdir,
        }
    }

    fn run(&self, script: &str, mode: &str) -> (i32, String) {
        let out = Command::new("bash")
            .arg(scripts_dir().join(script))
            .env("SG1_SOAK_STATE", &self.state)
            .env("TMPDIR", &self.tmpdir)
            .env("STUB_MODE", mode)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), text)
    }

    /// Append a `KEY=value` line to the campaign's `config.env`.
    fn config(&self, line: &str) {
        let p = self.state.join("config.env");
        let mut cfg = fs::read_to_string(&p).unwrap();
        cfg.push_str(line);
        cfg.push('\n');
        fs::write(p, cfg).unwrap();
    }

    fn slot(&self, mode: &str) -> (i32, String) {
        self.run("slot.sh", mode)
    }

    fn status(&self) -> String {
        let (code, text) = self.run("status.sh", "clean");
        assert_eq!(code, 0, "{text}");
        text
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }

    fn num(&self, name: &str) -> u64 {
        self.read(name).trim().parse().unwrap_or(0)
    }

    fn ledger(&self) -> Vec<String> {
        self.read("ledger.jsonl")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn stop(&self) -> Option<String> {
        let p = self.state.join("STOP");
        p.exists().then(|| fs::read_to_string(p).unwrap())
    }

    fn count_files(&self, dir: &str) -> usize {
        fs::read_dir(self.state.join(dir)).map_or(0, Iterator::count)
    }
}

#[test]
fn infra_slots_record_but_do_not_accrue_or_halt_until_the_streak_limit() {
    if !tools_available() {
        return;
    }
    let c = Campaign::new();

    // Clean slot accrues SLOT_SEEDS * STEPS: only the allocated seed range.
    // The canonical bn-cm63 seed the harness replays in EVERY slot is the
    // same deterministic plan each time, so it adds no new op-steps
    // (bn-2qamr).
    let (code, text) = c.slot("clean");
    assert_eq!(code, 0, "{text}");
    assert!(c.tmpdir.is_dir(), "slot.sh must mkdir -p TMPDIR");
    let per_clean = SLOT_SEEDS * STEPS;
    assert_eq!(c.num("cumulative"), per_clean);
    assert_eq!(c.num("cursor"), BASE + SLOT_SEEDS);

    // Two infra slots: no STOP, no accrual, ranges consumed, rows recorded.
    for i in 1..=2 {
        let (code, text) = c.slot("infra");
        assert_eq!(code, 0, "infra slot {i}: {text}");
        assert!(c.stop().is_none(), "infra slot {i} must not STOP");
        assert_eq!(c.num("cumulative"), per_clean, "infra must not accrue");
        assert_eq!(c.num("infra_consecutive"), i);
    }
    assert_eq!(
        c.num("cursor"),
        BASE + 3 * SLOT_SEEDS,
        "infra ranges consumed"
    );
    let ledger = c.ledger();
    assert_eq!(ledger.len(), 3);
    for row in &ledger[1..] {
        assert!(row.contains(r#""status":"infra""#), "{row}");
        assert!(row.contains(r#""op_steps":0"#), "{row}");
        assert!(row.contains("Disk quota exceeded"), "{row}");
        let v: serde_json::Value = serde_json::from_str(row).expect("ledger row is JSON");
        assert_eq!(v["rc"], 75);
    }
    assert!(ledger[1].contains(&format!(r#""base_seed":{}"#, BASE + SLOT_SEEDS)));
    assert_eq!(c.count_files("infra"), 2);
    assert_eq!(c.count_files("violations"), 0);

    // A clean slot ends the streak.
    let (code, text) = c.slot("clean");
    assert_eq!(code, 0, "{text}");
    assert_eq!(c.num("infra_consecutive"), 0);
    assert_eq!(c.num("cumulative"), 2 * per_clean);

    // Three consecutive infra slots => INFRA-HALT on the third.
    for i in 1..=2 {
        let (code, text) = c.slot("infra");
        assert_eq!(code, 0, "{text}");
        assert!(c.stop().is_none(), "streak {i} < 3 must not STOP");
    }
    let (code, text) = c.slot("infra");
    assert_eq!(code, 1, "third consecutive infra slot halts: {text}");
    let stop = c.stop().expect("INFRA-HALT writes STOP");
    assert!(stop.starts_with("INFRA-HALT:"), "{stop}");
    assert_eq!(c.num("cumulative"), 2 * per_clean);
    assert_eq!(
        c.count_files("violations"),
        0,
        "infra never writes violations/"
    );

    // STOP holds the campaign: the next slot is a no-op.
    let rows = c.ledger().len();
    let (code, _) = c.slot("clean");
    assert_eq!(code, 0);
    assert_eq!(c.ledger().len(), rows);

    let status = c.status();
    assert!(status.contains("INFRA-HALT"), "{status}");
    assert!(status.contains("infra failures:     5"), "{status}");
    assert!(status.contains("violations:         0"), "{status}");
    assert!(!status.contains("STOPPED (VIOLATION)"), "{status}");
}

#[test]
fn every_non_infra_failure_shape_is_still_a_violation() {
    if !tools_available() {
        return;
    }
    for mode in [
        "violation",
        "rc75_no_marker",
        "marker_rc1",
        "marker_with_violation",
        "indented_marker",
        "harness_error",
        "harness_error_with_marker",
    ] {
        let c = Campaign::new();
        let (code, text) = c.slot(mode);
        assert_eq!(code, 1, "{mode}: {text}");
        let stop = c.stop().unwrap_or_else(|| panic!("{mode}: must STOP"));
        assert!(stop.starts_with("VIOLATION:"), "{mode}: {stop}");
        let ledger = c.ledger();
        assert_eq!(ledger.len(), 1, "{mode}");
        assert!(
            ledger[0].contains(r#""status":"VIOLATION_OR_ERROR""#),
            "{mode}: {}",
            ledger[0]
        );
        assert_eq!(c.count_files("violations"), 1, "{mode}");
        assert_eq!(c.count_files("infra"), 0, "{mode}");
        assert_eq!(c.num("cumulative"), 0, "{mode}");

        let status = c.status();
        assert!(status.contains("STOPPED (VIOLATION)"), "{mode}: {status}");
        assert!(!status.contains("INFRA-HALT"), "{mode}: {status}");
    }
}

#[test]
fn a_bare_stop_file_is_not_reported_as_infra() {
    if !tools_available() {
        return;
    }
    let c = Campaign::new();
    fs::write(c.state.join("STOP"), "").unwrap();
    let status = c.status();
    assert!(status.contains("manual pause or violation"), "{status}");
    assert!(!status.contains("INFRA-HALT"), "{status}");
}

/// bn-25pac: clean ledger rows record the evidence totals from the summary
/// line (JSON null when the pinned binary predates them).
#[test]
fn clean_rows_record_evidence_totals() {
    if !tools_available() {
        return;
    }
    let c = Campaign::new();
    let (code, text) = c.slot("clean_evidence");
    assert_eq!(code, 0, "{text}");
    // A pre-trunk-tier binary only runs with the evidence floor switched off.
    c.config("TRUNK_EVIDENCE_FLOOR=0");
    let (code, text) = c.slot("clean_no_trunk");
    assert_eq!(code, 0, "{text}");
    let ledger = c.ledger();
    assert_eq!(ledger.len(), 2);
    let new: serde_json::Value = serde_json::from_str(&ledger[0]).expect("JSON");
    assert_eq!(new["status"], "clean");
    assert_eq!(new["oracle_a_checks"], 55);
    assert_eq!(new["witnesses"], 17);
    assert_eq!(new["harness_errors"], 0);
    // bn-1h9ue: dirty-trunk tier evidence.
    assert_eq!(new["trunk_updates"], 12);
    assert_eq!(new["trunk_crashes"], 2);
    assert_eq!(new["dirty_trunk_merges"], 9);
    assert_eq!(new["displacement_checks"], 31);
    assert_eq!(new["replay_checks"], 77);
    assert_eq!(new["trunk_drains"], 3);
    let old: serde_json::Value = serde_json::from_str(&ledger[1]).expect("JSON");
    assert_eq!(old["status"], "clean");
    assert!(old["oracle_a_checks"].is_null(), "{old}");
    assert!(old["witnesses"].is_null(), "{old}");
    assert!(old["displacement_checks"].is_null(), "{old}");
}

/// bn-2qamr: a clean slot accrues exactly its allocated range, and only when
/// the harness provably ran what the slot allocated (seed count, steps, base
/// seed from its begin line). Anything else fails closed like a violation:
/// the op-step count would otherwise be wrong, or the seeds would overlap
/// other slots' ranges.
#[test]
fn clean_slots_accrue_only_the_allocated_range_run_as_allocated() {
    if !tools_available() {
        return;
    }
    let c = Campaign::new();
    let (code, text) = c.slot("clean");
    assert_eq!(code, 0, "{text}");
    assert_eq!(c.num("cumulative"), SLOT_SEEDS * STEPS);
    let row: serde_json::Value = serde_json::from_str(&c.ledger()[0]).expect("JSON");
    assert_eq!(row["op_steps"], SLOT_SEEDS * STEPS);
    assert_eq!(
        row["clean"],
        SLOT_SEEDS + 1,
        "harness clean count kept as is"
    );
    assert_eq!(row["range_clean"], SLOT_SEEDS);

    for mode in ["wrong_steps", "wrong_base", "default_seeds", "no_begin"] {
        let c = Campaign::new();
        let (code, text) = c.slot(mode);
        assert_eq!(code, 1, "{mode}: {text}");
        let stop = c.stop().unwrap_or_else(|| panic!("{mode}: must STOP"));
        assert!(stop.starts_with("VIOLATION:"), "{mode}: {stop}");
        assert_eq!(c.num("cumulative"), 0, "{mode}: must not accrue");
        let ledger = c.ledger();
        assert_eq!(ledger.len(), 1, "{mode}");
        assert!(
            ledger[0].contains(r#""status":"VIOLATION_OR_ERROR""#),
            "{mode}: {}",
            ledger[0]
        );
    }
}

/// bn-2qamr: a violation STOP written by a PARALLEL slot while this slot was
/// still running must never be masked by this slot crossing the target:
/// slot.sh must not write DONE under a STOP, and status.sh must never report
/// "DONE — 0 violations" while the ledger records a violation.
#[test]
fn a_peer_violation_is_never_reported_as_done() {
    if !tools_available() {
        return;
    }
    let c = Campaign::new();
    fs::write(c.state.join("cumulative"), "999999\n").unwrap();
    let (code, text) = c.slot("clean_while_peer_violates");
    assert_eq!(code, 0, "{text}");
    assert!(c.stop().is_some(), "the peer's STOP stays");
    assert!(
        !c.state.join("DONE").exists(),
        "DONE must not be written while a violation STOP exists"
    );

    // Even with a DONE marker present (e.g. written by an older slot.sh), a
    // violation in the ledger wins.
    let c = Campaign::new();
    fs::write(
        c.state.join("ledger.jsonl"),
        "{\"status\":\"VIOLATION_OR_ERROR\",\"ts\":\"2026-01-01T00:00:00+00:00\"}\n",
    )
    .unwrap();
    fs::write(c.state.join("STOP"), "VIOLATION: x\n").unwrap();
    fs::write(c.state.join("DONE"), "").unwrap();
    fs::write(c.state.join("cumulative"), "1000000\n").unwrap();
    let status = c.status();
    assert!(!status.contains("DONE"), "{status}");
    assert!(status.contains("STOPPED (VIOLATION)"), "{status}");
}

/// bn-2qamr: an INFRA-HALT must never overwrite a PARALLEL slot's violation
/// STOP. Its operator instruction is "free disk, then rm STOP to resume" —
/// following it would silently resume the campaign past a real violation.
#[test]
fn infra_halt_never_overwrites_a_peer_violation_stop() {
    if !tools_available() {
        return;
    }
    let c = Campaign::new();
    fs::write(c.state.join("infra_consecutive"), "2\n").unwrap();
    let (code, text) = c.slot("infra_while_peer_violates");
    assert_eq!(code, 1, "the streak limit still halts: {text}");
    let stop = c.stop().expect("STOP");
    assert!(
        stop.starts_with("VIOLATION:"),
        "peer violation STOP kept: {stop}"
    );
    let status = c.status();
    assert!(status.contains("STOPPED (VIOLATION)"), "{status}");
    assert!(!status.contains("INFRA-HALT"), "{status}");
}

/// bn-36chi: a clean slot whose dirty-trunk evidence is below the campaign
/// floor (or missing) does not accrue and STOPs the campaign as a violation:
/// per-seed vacuity guards cannot see a harness that quietly stopped
/// crashing, draining or dirtying the trunk.
#[test]
fn clean_slots_below_the_trunk_evidence_floor_stop_the_campaign() {
    if !tools_available() {
        return;
    }
    for (mode, short) in [
        ("clean_no_crashes", "trunk_crashes=0<1"),
        ("clean_no_trunk", "trunk_updates=null<10"),
        ("clean_evidence", ""),
    ] {
        let c = Campaign::new();
        let (code, text) = c.slot(mode);
        if mode == "clean_evidence" {
            // 12 >= 10 etc.: at the floor, accrues normally.
            assert_eq!(code, 0, "{mode}: {text}");
            assert_eq!(c.num("cumulative"), SLOT_SEEDS * STEPS, "{mode}");
            assert!(c.stop().is_none(), "{mode}");
            continue;
        }
        assert_eq!(code, 1, "{mode}: {text}");
        let stop = c.stop().unwrap_or_else(|| panic!("{mode}: must STOP"));
        assert!(stop.starts_with("VIOLATION:"), "{mode}: {stop}");
        assert!(stop.contains(short), "{mode}: {stop}");
        assert_eq!(c.num("cumulative"), 0, "{mode}: must not accrue");
        let ledger = c.ledger();
        assert_eq!(ledger.len(), 1, "{mode}");
        let row: serde_json::Value = serde_json::from_str(&ledger[0]).expect("JSON");
        assert_eq!(row["status"], "VIOLATION_OR_ERROR", "{mode}");
        assert_eq!(row["reason"], "evidence_floor", "{mode}");
        assert!(c.status().contains("STOPPED (VIOLATION)"), "{mode}");
    }
    // Explicitly off: a pre-trunk-tier harness accrues.
    let c = Campaign::new();
    c.config("TRUNK_EVIDENCE_FLOOR=0");
    let (code, text) = c.slot("clean_no_trunk");
    assert_eq!(code, 0, "{text}");
    assert_eq!(c.num("cumulative"), SLOT_SEEDS * STEPS);
}
