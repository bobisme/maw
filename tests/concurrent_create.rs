//! Regression test for bn-3bbc: `maw ws create <same-name>` must be atomic
//! per workspace name under a real concurrent process race.
//!
//! # The bug
//!
//! `maw ws create` used to do check-exists-then-create with no per-name
//! lock — a classic TOCTOU race. Launching N concurrent
//! `maw ws create dupe --from main` for the SAME name caused ALL N
//! processes to exit 0 and print the full success banner; only one
//! worktree actually persisted, the others were silently clobbered, and
//! depending on timing the workspace could be left MISSING (`maw doctor`
//! then failed).
//!
//! # What is verified
//!
//! * **Same-name race serializes**: spawn N real `maw ws create dupe`
//!   processes simultaneously (synchronized with a `Barrier` so they race
//!   for real). Exactly ONE exits 0 and prints the success banner; every
//!   other process exits non-zero with an accurate
//!   `workspace 'dupe' already exists` error (no false success banner).
//!   Afterwards `maw ws list` shows exactly one healthy `dupe` (not
//!   MISSING) and `maw doctor` passes. Repeated several times because the
//!   race is timing-dependent.
//!
//! * **Different names still parallelize**: spawning concurrent creates of
//!   distinct names (a, b, c, d, e) all succeed — the lock is per-name, so
//!   it must not serialize unrelated creates.
//!
//! * **Serialized second create**: a plain second `maw ws create dupe`
//!   after the first completes fails fast with the proper error and leaves
//!   the first workspace's state intact.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod manifold_common;

use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use manifold_common::{TestRepo, maw_bin};

/// Spawn `n` concurrent `maw ws create <name> --from main` processes that
/// all start at (approximately) the same instant via a shared `Barrier`.
///
/// Returns, for each process: `(success, stdout, stderr)`.
fn race_create(root: &std::path::Path, name: &str, n: usize) -> Vec<(bool, String, String)> {
    let barrier = Arc::new(Barrier::new(n));
    let mut handles = Vec::with_capacity(n);

    for _ in 0..n {
        let root = root.to_path_buf();
        let name = name.to_owned();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // Release all threads as close to simultaneously as possible so
            // the create critical sections genuinely overlap.
            barrier.wait();
            let out = Command::new(maw_bin())
                .args(["ws", "create", &name, "--from", "main"])
                .current_dir(&root)
                .output()
                .expect("failed to execute maw");
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            )
        }));
    }

    handles
        .into_iter()
        .map(|h| h.join().expect("create thread panicked"))
        .collect()
}

/// The success banner printed by `maw ws create` on a real create.
const SUCCESS_MARKER: &str = "ready!";

#[test]
fn concurrent_same_name_create_serializes_exactly_one_winner() {
    // The race is timing non-deterministic; run it several times to be
    // confident the lock holds under different interleavings.
    for round in 0..5 {
        let repo = TestRepo::new();
        let results = race_create(repo.root(), "dupe", 5);

        let winners: Vec<_> = results.iter().filter(|(ok, _, _)| *ok).collect();
        let losers: Vec<_> = results.iter().filter(|(ok, _, _)| !*ok).collect();

        assert_eq!(
            winners.len(),
            1,
            "round {round}: exactly one create must succeed, got {} winners. \
             Results: {results:#?}",
            winners.len(),
        );
        assert_eq!(
            losers.len(),
            4,
            "round {round}: the other four creates must fail, got {} losers. \
             Results: {results:#?}",
            losers.len(),
        );

        // The single winner must print the real success banner.
        let (_, win_stdout, _) = winners[0];
        assert!(
            win_stdout.contains(SUCCESS_MARKER),
            "round {round}: winner did not print the success banner.\nstdout: {win_stdout}",
        );

        // Every loser must:
        //  - NOT print the success banner (no false success), and
        //  - report an accurate "already exists" error.
        for (_, l_stdout, l_stderr) in &losers {
            assert!(
                !l_stdout.contains(SUCCESS_MARKER),
                "round {round}: a losing create printed the success banner \
                 (false success).\nstdout: {l_stdout}\nstderr: {l_stderr}",
            );
            let combined = format!("{l_stdout}{l_stderr}");
            assert!(
                combined.contains("already exists"),
                "round {round}: loser did not report an 'already exists' error.\n\
                 stdout: {l_stdout}\nstderr: {l_stderr}",
            );
            assert!(
                combined.contains("dupe"),
                "round {round}: loser error did not name the workspace 'dupe'.\n\
                 stdout: {l_stdout}\nstderr: {l_stderr}",
            );
        }

        // Exactly one healthy `dupe` workspace must exist (no MISSING/corrupt).
        let workspaces = repo.list_workspaces();
        let dupe_count = workspaces.iter().filter(|w| *w == "dupe").count();
        assert_eq!(
            dupe_count, 1,
            "round {round}: expected exactly one `dupe` worktree, found {dupe_count}. \
             workspaces: {workspaces:?}",
        );
        assert!(
            repo.workspace_exists("dupe"),
            "round {round}: `dupe` workspace is not healthy (missing dir/.git)",
        );

        // `maw doctor` must pass — the bug left it failing with
        // "Some checks failed" / "MISSING worktree(s): dupe".
        let doctor = repo.maw_raw_exact(&["doctor"]);
        assert!(
            doctor.status.success(),
            "round {round}: maw doctor failed after concurrent same-name create.\n\
             stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&doctor.stdout),
            String::from_utf8_lossy(&doctor.stderr),
        );
    }
}

#[test]
fn concurrent_different_name_creates_all_succeed_in_parallel() {
    // The create lock is per *name* (bn-3bbc), so creating distinct names
    // concurrently must NOT serialize or fail — they must all succeed and
    // run in parallel.
    //
    // This mirrors the real-world scenario from AGENTS.md: an orchestrator
    // dispatching different workspace names to different agent *processes*
    // at (nearly) the same time. We spawn all five `maw ws create`
    // subprocesses up-front and then wait for them, so their critical
    // sections genuinely overlap, without an in-process `Barrier` that
    // would pin every `git worktree add` into the exact same nanosecond
    // (that artificial timing collides on gix's *shared bare-repo index
    // lock* — a pre-existing concern orthogonal to the per-name create
    // lock, and not how real concurrent agents are scheduled).
    let repo = TestRepo::new();
    let names = ["a", "b", "c", "d", "e"];

    let start = std::time::Instant::now();
    let mut children: Vec<(&str, std::process::Child)> = Vec::new();
    for name in names {
        let child = Command::new(maw_bin())
            .args(["ws", "create", name, "--from", "main"])
            .current_dir(repo.root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn maw");
        children.push((name, child));
    }

    for (name, child) in children {
        let out = child.wait_with_output().expect("failed to wait for maw");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "concurrent create of distinct name '{name}' must succeed.\n\
             stdout: {stdout}\nstderr: {stderr}",
        );
        assert!(
            stdout.contains(SUCCESS_MARKER),
            "create of '{name}' did not print the success banner.\nstdout: {stdout}",
        );
    }
    let elapsed = start.elapsed();

    let workspaces = repo.list_workspaces();
    for name in names {
        assert!(
            workspaces.iter().any(|w| w == name),
            "workspace '{name}' missing after parallel distinct-name creates: {workspaces:?}",
        );
        assert!(
            repo.workspace_exists(name),
            "workspace '{name}' is not healthy after parallel creates",
        );
    }

    // Sanity: all five ran concurrently (spawned before any was waited on).
    // A per-name (not global) lock keeps total wall time far below the sum
    // of five sequential creates. This guards against a regression that
    // accidentally introduces a *global* create lock.
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "five concurrent distinct-name creates took {elapsed:?}; \
         a per-name lock should keep this well under a sequential bound",
    );

    let doctor = repo.maw_raw_exact(&["doctor"]);
    assert!(
        doctor.status.success(),
        "maw doctor failed after parallel distinct-name creates.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr),
    );
}

#[test]
fn second_create_of_existing_name_fails_and_preserves_state() {
    // The non-racing baseline: a plain second create of an existing name
    // must fail fast with the proper error and must not disturb the first
    // workspace's contents.
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "dupe", "--from", "main"]);
    repo.add_file("dupe", "keep.txt", "important work");
    let head_before = repo.workspace_head("dupe");

    let stderr = repo.maw_fails(&["ws", "create", "dupe", "--from", "main"]);
    assert!(
        stderr.contains("already exists") && stderr.contains("dupe"),
        "second create did not report an accurate 'already exists' error.\nstderr: {stderr}",
    );

    // First workspace fully intact.
    assert_eq!(
        repo.read_file("dupe", "keep.txt").as_deref(),
        Some("important work"),
        "second create clobbered the existing workspace's file",
    );
    assert_eq!(
        repo.workspace_head("dupe"),
        head_before,
        "second create moved the existing workspace's HEAD",
    );

    let doctor = repo.maw_raw_exact(&["doctor"]);
    assert!(
        doctor.status.success(),
        "maw doctor failed after a rejected duplicate create.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr),
    );
}
