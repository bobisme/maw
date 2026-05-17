//! SP1 spike — IN-PROCESS execution model.
//!
//! Links `maw` (maw-workspaces) + maw-core directly. Builds a real on-disk
//! git repo, drives the real COMMIT-phase FSM (`maw::merge::commit`), and
//! injects a fault at a seed-selected FP_COMMIT_* failpoint via the existing
//! `maw_core::failpoints::set()` API (NO new mechanism needed for in-proc).
//!
//! After the injected fault aborts the commit, it runs the real
//! `recover_partial_commit_with_branch_base` recovery path and asserts the
//! G3 monotonicity invariant (epoch ref only moves forward to the candidate,
//! never backward, never to an unrelated oid) using maw-assurance's oracle.
//!
//! Determinism contract demonstrated: the SAME seed selects the SAME
//! failpoint, the same fault action, and the same git object graph, so the
//! resulting ref state and oracle verdict are bit-exact across runs. Wall
//! clock and pids do NOT enter the verified state.
//!
//! Usage:  cargo run --bin inproc -- <seed>

use std::path::Path;
use std::process::Command;

use maw::merge::commit::{
    recover_partial_commit_with_branch_base, run_commit_phase_with_branch_base, CommitRecovery,
};
use maw_core::failpoints::{self, FailpointAction};
use maw_core::model::types::GitOid;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// The three dangerous COMMIT boundaries SP1 must prove are crash-safe.
/// These are the real FP names already compiled into `src/merge/commit.rs`.
const COMMIT_FAULTS: &[&str] = &[
    "FP_COMMIT_BEFORE_BRANCH_CAS",
    "FP_COMMIT_BETWEEN_CAS_OPS",
    "FP_COMMIT_AFTER_EPOCH_CAS",
];

fn git(dir: &Path, args: &[&str]) -> String {
    // DETERMINISM CONTRACT: pin author+committer dates so commit OIDs are a
    // pure function of (tree, parents, message) — i.e. of the seed. Without
    // this, OIDs embed wall-clock time and seed replay is NOT bit-exact.
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn main() {
    let seed: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // ---- seeded, deterministic scenario selection -------------------------
    let mut rng = StdRng::seed_from_u64(seed);
    let fault = COMMIT_FAULTS[rng.random_range(0..COMMIT_FAULTS.len())];
    // Deterministic file content drives the git object graph; same seed =>
    // same blob/tree/commit oids.
    let payload = format!("dst-seed-{seed}-{}", rng.random::<u64>());

    let tmp = std::env::temp_dir().join(format!("dst-inproc-{seed}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // ---- build a minimal real repo with two commits -----------------------
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "dst@spike"]);
    git(&tmp, &["config", "user.name", "dst"]);
    std::fs::write(tmp.join("base.txt"), "base\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-q", "-m", "base"]);
    let epoch_before_hex = git(&tmp, &["rev-parse", "HEAD"]);

    // candidate = a second commit (the merge result we want to land)
    std::fs::write(tmp.join("merged.txt"), &payload).unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-q", "-m", "candidate"]);
    let candidate_hex = git(&tmp, &["rev-parse", "HEAD"]);

    // reset main back to base so COMMIT has work to do; seed the epoch ref.
    git(&tmp, &["reset", "-q", "--hard", &epoch_before_hex]);
    git(
        &tmp,
        &["update-ref", "refs/manifold/epoch/current", &epoch_before_hex],
    );

    let epoch_before = GitOid::new(&epoch_before_hex).unwrap();
    let candidate = GitOid::new(&candidate_hex).unwrap();

    println!("[inproc] seed={seed} fault={fault}");
    println!("[inproc] epoch_before={} candidate={}", &epoch_before_hex[..12], &candidate_hex[..12]);

    // ---- inject the fault via the EXISTING in-proc API --------------------
    // No env bridge, no new mechanism: failpoints::set() is all the in-proc
    // model needs. The action is Error (clean unwind) — Panic/Abort would
    // also work but Error lets us observe recovery in the same process.
    failpoints::clear_all();
    failpoints::set(fault, FailpointAction::Error("dst-injected".into()));

    // ---- drive the real COMMIT FSM ---------------------------------------
    let commit_res = run_commit_phase_with_branch_base(
        &tmp,
        "main",
        &epoch_before,
        &epoch_before,
        &candidate,
    );
    assert!(
        commit_res.is_err(),
        "expected injected fault {fault} to abort the commit"
    );
    println!("[inproc] commit aborted by fault as expected: {commit_res:?}");

    // ---- crash semantics: what did the partial commit leave behind? ------
    failpoints::clear_all();
    let epoch_now = git(&tmp, &["rev-parse", "refs/manifold/epoch/current"]);
    let main_now = git(&tmp, &["rev-parse", "refs/heads/main"]);
    println!(
        "[inproc] post-fault refs: epoch={} main={}",
        &epoch_now[..12],
        &main_now[..12]
    );

    // ---- run the REAL recovery path --------------------------------------
    let recovery = recover_partial_commit_with_branch_base(
        &tmp,
        "main",
        &epoch_before,
        &epoch_before,
        &candidate,
    )
    .expect("recovery must not error on a well-formed partial commit");
    println!("[inproc] recovery verdict: {recovery:?}");

    // For BETWEEN/AFTER_EPOCH the atomic CAS already moved BOTH refs (maw
    // uses a single git transaction), so recovery sees AlreadyCommitted.
    // For BEFORE_BRANCH_CAS nothing moved => NotCommitted, and a retry
    // (re-run with no fault) must finish the job.
    let final_epoch = match recovery {
        CommitRecovery::NotCommitted => {
            println!("[inproc] nothing moved; retrying commit with no fault");
            run_commit_phase_with_branch_base(
                &tmp,
                "main",
                &epoch_before,
                &epoch_before,
                &candidate,
            )
            .expect("retry after clean abort must succeed");
            git(&tmp, &["rev-parse", "refs/manifold/epoch/current"])
        }
        CommitRecovery::AlreadyCommitted | CommitRecovery::FinalizedMainRef => {
            git(&tmp, &["rev-parse", "refs/manifold/epoch/current"])
        }
    };

    // ---- ORACLE: G3 post-COMMIT monotonicity -----------------------------
    // The epoch ref must end at exactly the candidate (forward-only), never
    // at epoch_before (lost merge) and never at an unrelated oid.
    let g3_ok = final_epoch == candidate_hex;
    let main_final = git(&tmp, &["rev-parse", "refs/heads/main"]);
    let refs_consistent = main_final == final_epoch;

    println!(
        "[inproc] ORACLE G3 (epoch forward-only -> candidate): {}",
        if g3_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "[inproc] ORACLE atomic (epoch == main): {}",
        if refs_consistent { "PASS" } else { "FAIL" }
    );

    let _ = std::fs::remove_dir_all(&tmp);

    if g3_ok && refs_consistent {
        println!("[inproc] seed={seed} RESULT=PASS (deterministic, replay with same seed)");
        std::process::exit(0);
    } else {
        eprintln!("[inproc] seed={seed} RESULT=FAIL — replay: cargo run --bin inproc -- {seed}");
        std::process::exit(1);
    }
}
