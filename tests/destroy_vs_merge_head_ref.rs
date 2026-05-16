//! Regression test for bn-cm63: `maw ws destroy <ws> --force` racing an
//! in-flight `maw ws merge <ws>` of the SAME workspace must not leak a
//! dangling `refs/manifold/head/<ws>` oplog head ref, and plain `maw gc`
//! (no `--refs`) must self-heal an already-dangling head ref.
//!
//! # The bug
//!
//! `maw ws destroy` deletes every ref owned by the workspace, including its
//! oplog head (`refs/manifold/head/<ws>`). A concurrent `maw ws merge <ws>`
//! freezes the source at PREPARE and, after a successful COMMIT, appends a
//! `Merge` op to that workspace's oplog via `record_merge_operations`. If
//! destroy already deleted the head ref, that append re-bootstraps a fresh
//! oplog head — resurrecting a ref destroy intended to remove and leaving a
//! permanently dangling blob ref with no owning workspace. `maw doctor` then
//! perpetually warns about "stale head refs", and plain `maw gc` did NOT
//! clear them (only `maw gc --refs` did).
//!
//! # What is verified
//!
//! * **Race is serialized, not corrupted**: with a slow post-merge
//!   validation widening the window, a backgrounded `maw ws merge rz` is
//!   driven to the `validate` phase, then `maw ws destroy rz --force` is
//!   run. Destroy is *refused* (a live merge owns the workspace). After the
//!   merge completes: the merged file lands in `default` (Prime Invariant),
//!   and NO `refs/manifold/head/rz` survives once the workspace is gone.
//!
//! * **Normal `--destroy` stays clean**: `maw ws merge X --destroy` removes
//!   the workspace and leaves no dangling `refs/manifold/head/X`.
//!
//! * **Plain `maw gc` self-heals**: a pre-seeded dangling
//!   `refs/manifold/head/<gone>` is pruned by plain `maw gc` (no `--refs`),
//!   exactly as `maw doctor` now advises.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod manifold_common;

use std::process::Command;
use std::time::{Duration, Instant};

use manifold_common::{TestRepo, maw_bin};

/// True if `refs/manifold/head/<name>` exists in the repo.
fn head_ref_exists(repo: &TestRepo, name: &str) -> bool {
    Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            &format!("refs/manifold/head/{name}"),
        ])
        .current_dir(repo.root())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Write `.manifold/config.toml` so post-merge validation runs `sleep <secs>`,
/// widening the COMMIT/VALIDATE window enough to drive the race deterministically.
fn set_slow_validation(repo: &TestRepo, secs: u32) {
    let manifold = repo.root().join(".manifold");
    std::fs::create_dir_all(&manifold).expect("create .manifold");
    std::fs::write(
        manifold.join("config.toml"),
        format!("[merge.validation]\ncommand = \"sleep {secs}\"\n"),
    )
    .expect("write .manifold/config.toml");
}

/// Poll `.manifold/merge-state.json` until it reaches the `validate` phase
/// (or any later phase, in case we miss the window). Returns true if a
/// non-terminal merge-state was observed.
fn wait_for_validate_phase(repo: &TestRepo, timeout: Duration) -> bool {
    let state_path = repo.root().join(".manifold").join("merge-state.json");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(&state_path) {
            // Phase serializes lowercase ("validate"); accept commit/cleanup
            // too so a fast machine that blew past validate still proceeds.
            if body.contains("\"validate\"")
                || body.contains("\"commit\"")
                || body.contains("\"cleanup\"")
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn destroy_racing_in_flight_merge_does_not_leak_head_ref() {
    // The race is timing-dependent; run it a few times under different
    // interleavings to be confident the serialization holds.
    for round in 0..3 {
        let repo = TestRepo::new();
        repo.seed_files(&[("README.md", "# project\n")]);
        set_slow_validation(&repo, 5);

        repo.create_workspace("rz");
        repo.add_file("rz", "file_rz.txt", &format!("rz change round {round}\n"));
        repo.git_in_workspace("rz", &["add", "-A"]);
        repo.git_in_workspace("rz", &["commit", "-m", "rz change"]);

        // Background the merge in its own process so destroy can race it.
        let merge = Command::new(maw_bin())
            .args(["ws", "merge", "rz", "--into", "default", "--message", "rz"])
            .current_dir(repo.root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn maw ws merge");

        assert!(
            wait_for_validate_phase(&repo, Duration::from_secs(20)),
            "round {round}: merge never reached validate phase",
        );

        // Race: destroy the same workspace while the merge holds it. With the
        // bn-cm63 fix, destroy is REFUSED while a live merge owns the source.
        let destroy = repo.maw_raw(&["ws", "destroy", "rz", "--force"]);
        let destroy_err = String::from_utf8_lossy(&destroy.stderr).to_string();
        assert!(
            !destroy.status.success(),
            "round {round}: destroy should be refused while a live merge owns 'rz'.\n\
             stdout: {}\nstderr: {destroy_err}",
            String::from_utf8_lossy(&destroy.stdout),
        );
        assert!(
            destroy_err.contains("being merged") || destroy_err.contains("merge"),
            "round {round}: refusal message should explain the in-flight merge.\nstderr: {destroy_err}",
        );

        // Let the merge finish.
        let out = merge.wait_with_output().expect("await merge");
        assert!(
            out.status.success(),
            "round {round}: backgrounded merge failed.\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        // Prime Invariant: the merged work landed in default.
        let default_file = repo.default_workspace().join("file_rz.txt");
        assert!(
            default_file.exists(),
            "round {round}: merged file must be present in default workspace",
        );

        // The merge did NOT auto-destroy (we did not pass --destroy and the
        // standalone destroy was refused), so `rz` should still exist with a
        // valid (non-dangling) head ref.
        assert!(
            repo.workspace_exists("rz"),
            "round {round}: 'rz' should still exist (destroy was refused)",
        );

        // Now destroy it cleanly (no merge in flight) — the head ref must be
        // gone afterwards, proving the lifecycle is coherent.
        repo.maw_ok(&["ws", "destroy", "rz", "--force"]);
        assert!(
            !head_ref_exists(&repo, "rz"),
            "round {round}: refs/manifold/head/rz must NOT survive a clean \
             destroy after the racing merge (bn-cm63 leak)",
        );
        assert!(
            !repo.workspace_exists("rz"),
            "round {round}: workspace 'rz' should be gone after clean destroy",
        );
    }
}

#[test]
fn normal_merge_destroy_stays_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# project\n")]);

    repo.create_workspace("cd");
    repo.add_file("cd", "file_cd.txt", "cd change\n");
    repo.git_in_workspace("cd", &["add", "-A"]);
    repo.git_in_workspace("cd", &["commit", "-m", "cd change"]);

    repo.maw_ok(&[
        "ws",
        "merge",
        "cd",
        "--into",
        "default",
        "--destroy",
        "--message",
        "cd",
    ]);

    assert!(
        !repo.workspace_exists("cd"),
        "merge --destroy should remove the workspace",
    );
    assert!(
        !head_ref_exists(&repo, "cd"),
        "merge --destroy must not leave a dangling refs/manifold/head/cd",
    );
    assert!(
        repo.default_workspace().join("file_cd.txt").exists(),
        "merged file must land in default",
    );
}

#[test]
fn plain_gc_prunes_dangling_head_ref() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# project\n")]);

    // Seed a dangling head ref for a workspace that does not exist, pointing
    // at a real blob (mirrors a leaked oplog-head ref). GC only cares about
    // the ref name + missing `ws/<name>/`, not the target object kind.
    let blob_src = repo.root().join("oplog-head-seed.json");
    std::fs::write(&blob_src, b"{\"oplog\":\"head\"}").expect("write seed blob");
    let blob = repo
        .git(&["hash-object", "-w", blob_src.to_str().expect("utf8 path")])
        .trim()
        .to_string();
    std::fs::remove_file(&blob_src).ok();

    repo.git(&["update-ref", "refs/manifold/head/ghost", &blob]);
    assert!(
        head_ref_exists(&repo, "ghost"),
        "precondition: dangling head ref should exist",
    );

    // Plain `maw gc` (no --refs) must now self-heal it — this is exactly what
    // `maw doctor` advises ("Run: maw gc").
    repo.maw_ok(&["gc"]);

    assert!(
        !head_ref_exists(&repo, "ghost"),
        "plain `maw gc` must prune the dangling refs/manifold/head/ghost \
         (bn-cm63 self-heal)",
    );
}
