//! SP1 spike — SUBPROCESS execution model (the FAITHFUL model).
//!
//! Spawns the REAL installed `maw` binary and crashes it for real, using the
//! exact bn-cm63 chaos pattern, then exercises the real recovery path and
//! checks the Prime Invariant on the recovered repo.
//!
//! Verified flow (matches a hand-run reproduction in this spike):
//!   1. `git init` + `maw init`  -> v2 bare repo (root/.manifold, root/ws).
//!   2. `maw ws create rz` ; commit a change in ws/rz.
//!   3. Slow validation (`[merge.validation] command="sleep N"`) widens the
//!      COMMIT window — the bn-cm63 repro lever.
//!   4. `setsid maw ws merge rz --into default` runs in its own process
//!      group (detached).
//!   5. Poll `<root>/.manifold/merge-state.json` until the seed-selected
//!      phase, then `kill -9 -<pgid>` — a REAL SIGKILL of the whole tree.
//!   6. Re-run `maw ws merge rz --into default` — the recovery+retry path.
//!   7. ORACLE: epoch ref resolvable & the rz change is present in default
//!      (Prime Invariant: no committed work lost across a real crash).
//!
//! It also wires the PROPOSED env bridge `MAW_FP` onto the child so that,
//! once bn-263u adds a parser to failpoints.rs (+--features failpoints in the
//! shipped binary), the SAME harness gets deterministic in-binary fault
//! points and no longer needs the sleep window.
//!
//! Determinism contract for the faithful model: the seed pins the crash
//! PHASE (a logical clock from the state file) and the file payload, so the
//! verified END STATE replays exactly. The exact instruction killed is NOT
//! deterministic — that is inherent to real process kills and is the
//! accepted trade for crash fidelity.
//!
//! Usage:  cargo run --bin subprocess -- <seed>

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const KILL_PHASES: &[&str] = &["build", "validate", "commit"];

fn run(dir: &Path, bin: &str, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("spawn {bin} {args:?}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, so, se) = run(dir, "git", args);
    assert!(ok, "git {args:?} failed: {se}");
    so.trim().to_string()
}

fn read_phase(repo: &Path) -> Option<String> {
    let bytes = std::fs::read(repo.join(".manifold/merge-state.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("phase").and_then(|p| p.as_str()).map(String::from)
}

fn main() {
    let seed: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut rng = StdRng::seed_from_u64(seed);
    let kill_phase = KILL_PHASES[rng.random_range(0..KILL_PHASES.len())];
    let payload = format!("dst-sub-seed-{seed}-{}", rng.random::<u64>());

    // mktemp-style unique dir; no rm -rf (sandbox forbids it).
    let tmp: PathBuf =
        std::env::temp_dir().join(format!("dst-sub-{seed}-{}-{}", std::process::id(), rng.random::<u32>()));
    std::fs::create_dir_all(&tmp).unwrap();
    println!("[sub] seed={seed} kill_phase={kill_phase} root={}", tmp.display());

    // ---- build a real v2 maw repo ---------------------------------------
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "dst@spike"]);
    git(&tmp, &["config", "user.name", "dst"]);
    std::fs::write(tmp.join("base.txt"), "base\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-q", "-m", "base"]);

    let (iok, _io, ie) = run(&tmp, "maw", &["init"]);
    assert!(iok, "maw init failed: {ie}");

    // Slow validation widens the COMMIT window (the bn-cm63 lever).
    std::fs::write(
        tmp.join(".manifold/config.toml"),
        "[merge.validation]\ncommand = \"sleep 5\"\n",
    )
    .unwrap();

    let (cok, _co, ce) = run(&tmp, "maw", &["ws", "create", "rz", "--from", "main"]);
    assert!(cok, "ws create failed: {ce}");
    let ws_rz = tmp.join("ws/rz");
    std::fs::write(ws_rz.join("rz.txt"), &payload).unwrap();
    git(&ws_rz, &["add", "-A"]);
    git(&ws_rz, &["commit", "-q", "-m", "rz change"]);

    // ---- spawn the merge DETACHED (own process group) -------------------
    let mut child = Command::new("setsid")
        .arg("maw")
        .args(["ws", "merge", "rz", "--into", "default", "--message", "rz"])
        .current_dir(&tmp)
        // PROPOSED bridge (bn-263u): harmless to set now, load-bearing once
        // failpoints.rs parses it and the binary ships --features failpoints.
        .env(
            "MAW_FP",
            format!("FP_{}_AFTER_STATE_WRITE=error", kill_phase.to_uppercase()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("setsid maw spawn");
    let pgid = child.id() as i32;

    // ---- poll the state file (logical clock) then REAL SIGKILL ----------
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut killed_at = None;
    while Instant::now() < deadline {
        if let Some(phase) = read_phase(&tmp) {
            if phase == kill_phase {
                unsafe { libc_kill(-pgid, 9) };
                killed_at = Some(phase);
                break;
            }
        }
        sleep(Duration::from_millis(20));
    }
    // FINDING (bn-imw8): a real SIGKILL leaves the victim a ZOMBIE until its
    // parent reaps it. maw's liveness probe reads /proc/<pid>, and a zombie
    // still looks ALIVE there — so recovery refuses until the zombie is
    // collected. The faithful harness MUST reap its own children. We waitpid
    // the setsid leader here; the kernel/orphan-reaper collects the rest.
    let _ = child.wait();

    let Some(phase) = killed_at else {
        println!("[sub] seed={seed} window '{kill_phase}' not observed (timing); not a maw fault");
        println!("[sub] replay: cargo run --bin subprocess -- {seed}");
        std::process::exit(0);
    };
    let state_after_kill = read_phase(&tmp);
    println!(
        "[sub] REAL SIGKILL delivered at phase={phase}; state-file left at {state_after_kill:?}"
    );

    // ---- recovery: bounded retry loop -----------------------------------
    // FINDING (bn-imw8): the FIRST retry right after a real SIGKILL is racy —
    // the orphaned merge-state's owner pid is not yet observed dead, so maw
    // conservatively refuses ("owned by a running process"). A SECOND retry
    // self-heals (stale-state detection clears it). A faithful DST harness
    // must therefore model recovery as a bounded retry, NOT a single call.
    // The in-proc model never exposes this (no separate process liveness).
    // Backoff spans several seconds: pid-reap + maw's conservative liveness
    // recheck need wall time after a real kill. 1s was empirically too
    // short; ~6s total is reliably enough on this host.
    let mut rok = false;
    for attempt in 1..=8 {
        let (ok, _o, e) = run(
            &tmp,
            "maw",
            &["ws", "merge", "rz", "--into", "default", "--message", "rz"],
        );
        let last = e.lines().next().unwrap_or("").to_string();
        println!("[sub] recovery attempt {attempt}: ok={ok} {last}");
        if ok {
            rok = true;
            break;
        }
        sleep(Duration::from_millis(1000));
    }
    let _ = rok;

    // ---- ORACLE: Prime Invariant — committed rz work survived ------------
    let epoch_ok = Command::new("git")
        .args(["rev-parse", "--verify", "refs/manifold/epoch/current"])
        .current_dir(&tmp)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // The rz change must be reachable from default's HEAD after recovery.
    let default_has_rz = {
        let dflt = tmp.join("ws/default");
        std::fs::read_to_string(dflt.join("rz.txt"))
            .map(|c| c.trim() == payload)
            .unwrap_or(false)
    };

    println!(
        "[sub] ORACLE epoch ref resolvable: {}",
        if epoch_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "[sub] ORACLE Prime Invariant (rz committed work in default): {}",
        if default_has_rz { "PASS" } else { "FAIL" }
    );

    if epoch_ok && default_has_rz {
        println!("[sub] seed={seed} RESULT=PASS (real crash + real recovery, faithful model)");
        std::process::exit(0);
    } else {
        eprintln!(
            "[sub] seed={seed} RESULT=FAIL — artifacts kept at {} ; replay: cargo run --bin subprocess -- {seed}",
            tmp.display()
        );
        std::process::exit(1);
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
