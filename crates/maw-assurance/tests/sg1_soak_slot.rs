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
case "${STUB_MODE:?}" in
  clean)
    echo "[sg1] nightly soak begin: seeds=$SG1_NIGHTLY_SEEDS"
    echo "[sg1] nightly soak end: seeds=$((SG1_NIGHTLY_SEEDS + 1)) clean=$((SG1_NIGHTLY_SEEDS + 1)) violations=0 driver_total=1s wall=1s"
    exit 0 ;;
  clean_evidence)
    echo "[sg1] nightly soak end: seeds=$((SG1_NIGHTLY_SEEDS + 1)) clean=$((SG1_NIGHTLY_SEEDS + 1)) violations=0 driver_total=1s wall=1s oracle_a_checks=55 oracle_b_checks=55 witnesses=17 workspaces_observed=9 commits_observed=6 harness_errors=0"
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

    // Clean slot accrues (SLOT_SEEDS + canonical seed) * STEPS.
    let (code, text) = c.slot("clean");
    assert_eq!(code, 0, "{text}");
    assert!(c.tmpdir.is_dir(), "slot.sh must mkdir -p TMPDIR");
    let per_clean = (SLOT_SEEDS + 1) * STEPS;
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
    let (code, text) = c.slot("clean");
    assert_eq!(code, 0, "{text}");
    let ledger = c.ledger();
    assert_eq!(ledger.len(), 2);
    let new: serde_json::Value = serde_json::from_str(&ledger[0]).expect("JSON");
    assert_eq!(new["status"], "clean");
    assert_eq!(new["oracle_a_checks"], 55);
    assert_eq!(new["witnesses"], 17);
    assert_eq!(new["harness_errors"], 0);
    let old: serde_json::Value = serde_json::from_str(&ledger[1]).expect("JSON");
    assert_eq!(old["status"], "clean");
    assert!(old["oracle_a_checks"].is_null(), "{old}");
    assert!(old["witnesses"].is_null(), "{old}");
}
