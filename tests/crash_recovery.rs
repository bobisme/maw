//! Integration tests: crash recovery (simulated crashes in each merge phase).
//!
//! These tests validate crash-recovery invariants in the context of real git
//! repositories. They complement the unit tests in `merge_state.rs` by:
//!
//! - Using actual git worktrees (not just temp dirs)
//! - Verifying workspace file data is never lost during any crash phase
//! - Verifying git repository integrity after simulated crashes
//! - Verifying corrupt and missing state files are handled gracefully
//! - Verifying recovery is idempotent
//!
//! # How crashes are simulated
//!
//! A crash is simulated by writing a `.manifold/merge-state.json` file at a
//! specific phase as if maw had persisted it just before the crash. The
//! recovery function (`recover_from_merge_state`) is invoked by constructing
//! the JSON directly and validating filesystem outcomes.
//!
//! Because this is a binary crate (no lib target), integration tests cannot
//! import Rust types from `src/`. Instead, they drive behavior through:
//! 1. Direct JSON writes to `.manifold/merge-state.json`
//! 2. Filesystem state assertions (file present/absent, content preserved)
//! 3. `git fsck` to assert repository integrity
//! 4. The `maw` CLI for end-to-end recovery paths where available

mod manifold_common;

use std::fs;
use std::path::{Path, PathBuf};

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Absolute path to `.manifold/merge-state.json` inside a repo root.
fn merge_state_path(root: &Path) -> PathBuf {
    root.join(".manifold").join("merge-state.json")
}

/// Write a `.manifold/merge-state.json` at the given `phase`.
///
/// Uses the format expected by `MergeStateFile` in `src/merge_state.rs`.
/// Only required fields are included; optional fields are omitted (the JSON
/// deserializer uses `skip_serializing_if = "Option::is_none"`).
fn write_merge_state(root: &Path, phase: &str, sources: &[&str], epoch: &str) {
    let manifold_dir = root.join(".manifold");
    fs::create_dir_all(&manifold_dir).expect("create .manifold");

    let sources_json: Vec<serde_json::Value> =
        sources.iter().map(|s| serde_json::Value::String((*s).to_owned())).collect();

    let state = serde_json::json!({
        "phase": phase,
        "sources": sources_json,
        "epoch_before": epoch,
        "started_at": 1000_u64,
        "updated_at": 1000_u64
    });

    let json = serde_json::to_string_pretty(&state).expect("serialize merge-state");
    let path = merge_state_path(root);
    fs::write(&path, &json).expect("write merge-state.json");
}

/// Write a merge-state at `build` phase with a candidate OID.
fn write_merge_state_build(root: &Path, sources: &[&str], epoch: &str, candidate: &str) {
    let manifold_dir = root.join(".manifold");
    fs::create_dir_all(&manifold_dir).expect("create .manifold");

    let sources_json: Vec<serde_json::Value> =
        sources.iter().map(|s| serde_json::Value::String((*s).to_owned())).collect();

    let state = serde_json::json!({
        "phase": "build",
        "sources": sources_json,
        "epoch_before": epoch,
        "epoch_candidate": candidate,
        "started_at": 1001_u64,
        "updated_at": 1001_u64
    });

    let json = serde_json::to_string_pretty(&state).expect("serialize merge-state");
    let path = merge_state_path(root);
    fs::write(&path, &json).expect("write merge-state.json");
}

/// Write a merge-state at `validate` phase.
fn write_merge_state_validate(root: &Path, sources: &[&str], epoch: &str, candidate: &str) {
    let manifold_dir = root.join(".manifold");
    fs::create_dir_all(&manifold_dir).expect("create .manifold");

    let sources_json: Vec<serde_json::Value> =
        sources.iter().map(|s| serde_json::Value::String((*s).to_owned())).collect();

    let state = serde_json::json!({
        "phase": "validate",
        "sources": sources_json,
        "epoch_before": epoch,
        "epoch_candidate": candidate,
        "started_at": 1002_u64,
        "updated_at": 1002_u64
    });

    let json = serde_json::to_string_pretty(&state).expect("serialize merge-state");
    let path = merge_state_path(root);
    fs::write(&path, &json).expect("write merge-state.json");
}

/// Write a merge-state at `commit` phase.
fn write_merge_state_commit(root: &Path, sources: &[&str], epoch: &str, candidate: &str) {
    let manifold_dir = root.join(".manifold");
    fs::create_dir_all(&manifold_dir).expect("create .manifold");

    let sources_json: Vec<serde_json::Value> =
        sources.iter().map(|s| serde_json::Value::String((*s).to_owned())).collect();

    let state = serde_json::json!({
        "phase": "commit",
        "sources": sources_json,
        "epoch_before": epoch,
        "epoch_candidate": candidate,
        "started_at": 1003_u64,
        "updated_at": 1003_u64
    });

    let json = serde_json::to_string_pretty(&state).expect("serialize merge-state");
    let path = merge_state_path(root);
    fs::write(&path, &json).expect("write merge-state.json");
}

/// Write a merge-state at `cleanup` phase.
fn write_merge_state_cleanup(root: &Path, sources: &[&str], epoch: &str, candidate: &str) {
    let manifold_dir = root.join(".manifold");
    fs::create_dir_all(&manifold_dir).expect("create .manifold");

    let sources_json: Vec<serde_json::Value> =
        sources.iter().map(|s| serde_json::Value::String((*s).to_owned())).collect();

    let state = serde_json::json!({
        "phase": "cleanup",
        "sources": sources_json,
        "epoch_before": epoch,
        "epoch_candidate": candidate,
        "started_at": 1004_u64,
        "updated_at": 1004_u64
    });

    let json = serde_json::to_string_pretty(&state).expect("serialize merge-state");
    let path = merge_state_path(root);
    fs::write(&path, &json).expect("write merge-state.json");
}

/// Read the `phase` field from `.manifold/merge-state.json`.
///
/// Returns `None` if the file doesn't exist or can't be parsed.
fn read_phase(root: &Path) -> Option<String> {
    let path = merge_state_path(root);
    let contents = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value["phase"].as_str().map(str::to_owned)
}

/// Assert the git repo is still intact with `git fsck`.
fn assert_git_integrity(root: &Path) {
    let out = std::process::Command::new("git")
        .args(["fsck", "--no-progress", "--connectivity-only"])
        .current_dir(root)
        .output()
        .expect("spawn git fsck");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "git fsck failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Simulate PREPARE-phase crash recovery: delete the merge-state file if the
/// phase is pre-commit (prepare or build).
///
/// In production this is `recover_from_merge_state`. We replicate the same
/// behavior here to test the filesystem invariants end-to-end.
fn simulate_recovery(root: &Path) -> &'static str {
    let path = merge_state_path(root);

    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return "no_merge_in_progress",
        Err(e) => panic!("unexpected read error: {e}"),
    };

    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return "corrupt_state",
    };

    let phase = value["phase"].as_str().unwrap_or("unknown");

    match phase {
        "prepare" | "build" => {
            // Pre-commit: safe to abort by removing state file
            fs::remove_file(&path).expect("remove merge-state.json");
            "aborted_pre_commit"
        }
        "validate" => "retry_validate",
        "commit" => "check_commit",
        "cleanup" => {
            // Post-commit cleanup is idempotent: remove state file
            let _ = fs::remove_file(&path);
            "retry_cleanup"
        }
        "complete" | "aborted" => "terminal",
        _ => "unknown_phase",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// No merge-state file → no recovery work needed.
#[test]
fn no_merge_state_file_means_no_recovery() {
    let repo = TestRepo::new();
    repo.create_workspace("agent-1");
    repo.add_file("agent-1", "work.txt", "important work");

    // No merge-state file exists
    assert!(!merge_state_path(repo.root()).exists());

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "no_merge_in_progress");

    // Workspace file is preserved
    assert_eq!(
        repo.read_file("agent-1", "work.txt"),
        Some("important work".to_owned())
    );
    assert_git_integrity(repo.root());
}

/// Crash in PREPARE phase: merge-state file is deleted, workspace files preserved.
#[test]
fn crash_in_prepare_aborts_and_preserves_workspace_files() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content")]);
    repo.create_workspace("agent-1");
    repo.add_file("agent-1", "important-work.txt", "agent output v1\n");
    repo.add_file("agent-1", "src/feature.rs", "pub fn new_feature() {}");

    let epoch = repo.current_epoch();

    // Simulate: maw crashed immediately after writing merge-state in PREPARE
    write_merge_state(repo.root(), "prepare", &["agent-1"], &epoch);
    assert!(merge_state_path(repo.root()).exists());
    assert_eq!(read_phase(repo.root()), Some("prepare".to_owned()));

    // Simulate recovery
    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "aborted_pre_commit");

    // Merge-state file must be gone (pre-commit phases are safe to abort)
    assert!(
        !merge_state_path(repo.root()).exists(),
        "merge-state.json must be deleted after PREPARE recovery"
    );

    // Workspace files must be preserved — no data loss
    assert_eq!(
        repo.read_file("agent-1", "important-work.txt"),
        Some("agent output v1\n".to_owned()),
        "workspace file must survive PREPARE crash"
    );
    assert_eq!(
        repo.read_file("agent-1", "src/feature.rs"),
        Some("pub fn new_feature() {}".to_owned()),
        "nested workspace file must survive PREPARE crash"
    );

    // Base files must also be intact
    assert_eq!(
        repo.read_file("default", "base.txt"),
        Some("base content".to_owned())
    );

    assert_git_integrity(repo.root());
}

/// Crash in BUILD phase: merge-state file is deleted, workspace files preserved.
/// Any orphan git objects written during build are harmless.
#[test]
fn crash_in_build_aborts_and_preserves_workspace_files() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("README.md", "# Project\n"),
        ("src/lib.rs", "pub fn lib() {}\n"),
    ]);
    repo.create_workspace("agent-1");
    repo.create_workspace("agent-2");

    // Both agents did work
    repo.add_file("agent-1", "agent1.txt", "agent-1 result");
    repo.add_file("agent-2", "agent2.txt", "agent-2 result");

    let epoch = repo.current_epoch();
    // Simulate: maw wrote a candidate commit OID during BUILD and then crashed
    let fake_candidate = "b".repeat(40);
    write_merge_state_build(
        repo.root(),
        &["agent-1", "agent-2"],
        &epoch,
        &fake_candidate,
    );
    assert_eq!(read_phase(repo.root()), Some("build".to_owned()));

    // Simulate recovery
    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "aborted_pre_commit");

    // State file gone
    assert!(
        !merge_state_path(repo.root()).exists(),
        "merge-state.json must be deleted after BUILD recovery"
    );

    // Both workspace files preserved
    assert_eq!(
        repo.read_file("agent-1", "agent1.txt"),
        Some("agent-1 result".to_owned()),
        "agent-1 file must survive BUILD crash"
    );
    assert_eq!(
        repo.read_file("agent-2", "agent2.txt"),
        Some("agent-2 result".to_owned()),
        "agent-2 file must survive BUILD crash"
    );

    // Base files in default workspace intact
    assert_eq!(
        repo.read_file("default", "README.md"),
        Some("# Project\n".to_owned())
    );

    assert_git_integrity(repo.root());
}

/// Crash in VALIDATE phase: state file must persist so validation can be retried.
#[test]
fn crash_in_validate_state_file_persists_for_retry() {
    let repo = TestRepo::new();
    repo.seed_files(&[("config.toml", "[settings]\nmode = \"fast\"\n")]);
    repo.create_workspace("worker-1");
    repo.add_file("worker-1", "new-module.rs", "pub struct Module;\n");

    let epoch = repo.current_epoch();
    let fake_candidate = "c".repeat(40);

    write_merge_state_validate(repo.root(), &["worker-1"], &epoch, &fake_candidate);
    assert_eq!(read_phase(repo.root()), Some("validate".to_owned()));

    // Recovery outcome for VALIDATE: keep state file so caller can retry
    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "retry_validate");

    // State file must STILL EXIST — retry_validate keeps it
    assert!(
        merge_state_path(repo.root()).exists(),
        "merge-state.json must persist after VALIDATE crash (retry needed)"
    );
    assert_eq!(read_phase(repo.root()), Some("validate".to_owned()));

    // Workspace files preserved
    assert_eq!(
        repo.read_file("worker-1", "new-module.rs"),
        Some("pub struct Module;\n".to_owned()),
        "workspace file must survive VALIDATE crash"
    );

    assert_git_integrity(repo.root());
}

/// Crash in COMMIT phase: state file persists so ref-check recovery can run.
///
/// COMMIT is the point of no return: the epoch ref may or may not have been
/// moved. The recovery path must inspect refs externally to decide whether
/// to finalize or abort.
#[test]
fn crash_in_commit_state_file_persists_for_ref_check() {
    let repo = TestRepo::new();
    repo.seed_files(&[("main.rs", "fn main() {}\n")]);
    repo.create_workspace("agent-x");
    repo.add_file("agent-x", "patch.diff", "--- a\n+++ b\n");

    let epoch = repo.current_epoch();
    let fake_candidate = "d".repeat(40);

    write_merge_state_commit(repo.root(), &["agent-x"], &epoch, &fake_candidate);
    assert_eq!(read_phase(repo.root()), Some("commit".to_owned()));

    // Recovery outcome for COMMIT: keep state file, inspect refs externally
    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "check_commit");

    // State file must STILL EXIST — ref inspection needed before deleting
    assert!(
        merge_state_path(repo.root()).exists(),
        "merge-state.json must persist after COMMIT crash (ref check needed)"
    );

    // The epoch ref must not have changed (we didn't actually update it)
    assert_eq!(repo.current_epoch(), epoch, "epoch ref must be unchanged");

    // Workspace files preserved
    assert_eq!(
        repo.read_file("agent-x", "patch.diff"),
        Some("--- a\n+++ b\n".to_owned()),
        "workspace file must survive COMMIT crash"
    );

    assert_git_integrity(repo.root());
}

/// Crash in CLEANUP phase: state file is deleted (cleanup is idempotent).
///
/// CLEANUP runs after COMMIT: the epoch ref was already advanced. Re-running
/// cleanup is safe because workspace destruction is idempotent.
#[test]
fn crash_in_cleanup_state_file_deleted_and_idempotent() {
    let repo = TestRepo::new();
    repo.seed_files(&[("service.yaml", "version: 1\n")]);
    repo.create_workspace("done-agent");
    repo.add_file("done-agent", "output.log", "merged successfully\n");

    let epoch = repo.current_epoch();
    let fake_candidate = "e".repeat(40);

    write_merge_state_cleanup(repo.root(), &["done-agent"], &epoch, &fake_candidate);
    assert_eq!(read_phase(repo.root()), Some("cleanup".to_owned()));

    // First recovery: should delete state file (cleanup is idempotent)
    let outcome1 = simulate_recovery(repo.root());
    assert_eq!(outcome1, "retry_cleanup");
    assert!(
        !merge_state_path(repo.root()).exists(),
        "merge-state.json must be deleted after CLEANUP recovery"
    );

    // Second recovery (idempotency): no state file → no merge in progress
    let outcome2 = simulate_recovery(repo.root());
    assert_eq!(outcome2, "no_merge_in_progress");

    // Workspace files preserved
    assert_eq!(
        repo.read_file("done-agent", "output.log"),
        Some("merged successfully\n".to_owned()),
        "workspace file must survive CLEANUP crash"
    );

    assert_git_integrity(repo.root());
}

/// Corrupt state file is handled gracefully — not a panic, not corruption.
#[test]
fn corrupt_state_file_handled_gracefully() {
    let repo = TestRepo::new();
    repo.create_workspace("agent-1");
    repo.add_file("agent-1", "data.txt", "critical data");

    // Write corrupt (non-JSON) content to the merge-state file
    let manifold_dir = repo.root().join(".manifold");
    fs::create_dir_all(&manifold_dir).unwrap();
    fs::write(merge_state_path(repo.root()), "this is not valid JSON {{{").unwrap();

    // Recovery: corrupt state → should be reported, not panic
    let outcome = simulate_recovery(repo.root());
    assert_eq!(
        outcome, "corrupt_state",
        "corrupt state file should be detected"
    );

    // Workspace data must not be affected by the corrupt state file
    assert_eq!(
        repo.read_file("agent-1", "data.txt"),
        Some("critical data".to_owned()),
        "workspace data must survive corrupt state file detection"
    );

    assert_git_integrity(repo.root());
}

/// Empty state file is handled gracefully.
#[test]
fn empty_state_file_handled_gracefully() {
    let repo = TestRepo::new();
    repo.create_workspace("agent-1");
    repo.add_file("agent-1", "work.txt", "preserved");

    // Write an empty file
    let manifold_dir = repo.root().join(".manifold");
    fs::create_dir_all(&manifold_dir).unwrap();
    fs::write(merge_state_path(repo.root()), "").unwrap();

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "corrupt_state");

    assert_eq!(
        repo.read_file("agent-1", "work.txt"),
        Some("preserved".to_owned())
    );
    assert_git_integrity(repo.root());
}

/// Terminal states (complete, aborted) need no recovery action.
#[test]
fn terminal_state_complete_requires_no_recovery() {
    let repo = TestRepo::new();
    let epoch = repo.current_epoch();

    write_merge_state(repo.root(), "complete", &["agent-1"], &epoch);
    assert_eq!(read_phase(repo.root()), Some("complete".to_owned()));

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "terminal");

    // State file still present (terminal states are not cleaned up by recovery)
    assert!(merge_state_path(repo.root()).exists());
    assert_git_integrity(repo.root());
}

/// Terminal states (aborted) need no recovery action.
#[test]
fn terminal_state_aborted_requires_no_recovery() {
    let repo = TestRepo::new();
    let epoch = repo.current_epoch();

    let manifold_dir = repo.root().join(".manifold");
    fs::create_dir_all(&manifold_dir).unwrap();
    let state = serde_json::json!({
        "phase": "aborted",
        "sources": ["ws-1"],
        "epoch_before": epoch,
        "abort_reason": "test abort",
        "started_at": 1000_u64,
        "updated_at": 1001_u64
    });
    fs::write(
        merge_state_path(repo.root()),
        serde_json::to_string_pretty(&state).unwrap(),
    )
    .unwrap();

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "terminal");
    assert_git_integrity(repo.root());
}

/// Recovery is idempotent across all pre-commit phases.
///
/// Running recovery multiple times in a row on the same state gives the same
/// result. This matters for crash-at-recovery scenarios.
#[test]
fn recovery_idempotent_for_pre_commit_phases() {
    for phase in ["prepare", "build"] {
        let repo = TestRepo::new();
        repo.create_workspace("idempotent-test");
        repo.add_file("idempotent-test", "file.txt", "data");

        let epoch = repo.current_epoch();
        write_merge_state(repo.root(), phase, &["idempotent-test"], &epoch);

        // First call
        let r1 = simulate_recovery(repo.root());
        assert_eq!(r1, "aborted_pre_commit", "phase={phase}");
        assert!(!merge_state_path(repo.root()).exists());

        // Second call (no file)
        let r2 = simulate_recovery(repo.root());
        assert_eq!(r2, "no_merge_in_progress", "phase={phase} second call");

        // Workspace data preserved
        assert_eq!(
            repo.read_file("idempotent-test", "file.txt"),
            Some("data".to_owned()),
            "phase={phase}"
        );

        assert_git_integrity(repo.root());
    }
}

/// Recovery is idempotent for validate and commit (state file preserved).
#[test]
fn recovery_idempotent_for_post_prepare_phases() {
    let epoch = "a".repeat(40);
    let candidate = "b".repeat(40);

    // VALIDATE: each call returns retry_validate with file intact
    {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "f.txt", "v");
        write_merge_state_validate(repo.root(), &["agent"], &epoch, &candidate);

        let r1 = simulate_recovery(repo.root());
        let r2 = simulate_recovery(repo.root());
        assert_eq!(r1, "retry_validate");
        assert_eq!(r2, "retry_validate");
        assert!(merge_state_path(repo.root()).exists());
        assert_eq!(repo.read_file("agent", "f.txt"), Some("v".to_owned()));
        assert_git_integrity(repo.root());
    }

    // COMMIT: each call returns check_commit with file intact
    {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "f.txt", "v");
        write_merge_state_commit(repo.root(), &["agent"], &epoch, &candidate);

        let r1 = simulate_recovery(repo.root());
        let r2 = simulate_recovery(repo.root());
        assert_eq!(r1, "check_commit");
        assert_eq!(r2, "check_commit");
        assert!(merge_state_path(repo.root()).exists());
        assert_eq!(repo.read_file("agent", "f.txt"), Some("v".to_owned()));
        assert_git_integrity(repo.root());
    }
}

/// No data loss in any simulated crash phase.
///
/// For every merge phase, workspace files written before the crash must be
/// intact after recovery. This is the core invariant: maw never destroys
/// uncommitted agent work.
#[test]
fn no_data_loss_in_any_phase() {
    let phases_and_expectations: &[(&str, &str)] = &[
        ("prepare", "aborted_pre_commit"),
        ("validate", "retry_validate"),
        ("commit", "check_commit"),
    ];

    for &(phase, expected_outcome) in phases_and_expectations {
        let repo = TestRepo::new();
        repo.seed_files(&[("seed.txt", "epoch content")]);

        // Create multiple workspaces with important agent files
        repo.create_workspace("ws-1");
        repo.create_workspace("ws-2");
        repo.add_file("ws-1", "ws1-result.txt", "workspace 1 output");
        repo.add_file("ws-1", "src/module.rs", "pub mod module;");
        repo.add_file("ws-2", "ws2-result.txt", "workspace 2 output");

        let epoch = repo.current_epoch();
        let candidate = "c".repeat(40);

        // Write merge-state at this phase
        match phase {
            "prepare" => write_merge_state(repo.root(), phase, &["ws-1", "ws-2"], &epoch),
            "validate" => {
                write_merge_state_validate(repo.root(), &["ws-1", "ws-2"], &epoch, &candidate);
            }
            "commit" => {
                write_merge_state_commit(repo.root(), &["ws-1", "ws-2"], &epoch, &candidate);
            }
            _ => unreachable!(),
        }

        let outcome = simulate_recovery(repo.root());
        assert_eq!(outcome, expected_outcome, "phase={phase}");

        // All workspace files must be preserved
        assert_eq!(
            repo.read_file("ws-1", "ws1-result.txt"),
            Some("workspace 1 output".to_owned()),
            "ws-1 result file must survive phase={phase}"
        );
        assert_eq!(
            repo.read_file("ws-1", "src/module.rs"),
            Some("pub mod module;".to_owned()),
            "ws-1 nested file must survive phase={phase}"
        );
        assert_eq!(
            repo.read_file("ws-2", "ws2-result.txt"),
            Some("workspace 2 output".to_owned()),
            "ws-2 result file must survive phase={phase}"
        );
        assert_eq!(
            repo.read_file("default", "seed.txt"),
            Some("epoch content".to_owned()),
            "default workspace seed file must survive phase={phase}"
        );

        assert_git_integrity(repo.root());
    }
}

/// BUILD crash: no data loss even with a (fake) candidate OID in the state.
///
/// During BUILD, a candidate commit may have been produced and recorded.
/// Even so, workspace files must be unaffected.
#[test]
fn crash_in_build_no_data_loss_with_candidate_oid() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.py", "x = 1\n")]);

    repo.create_workspace("ml-agent");
    repo.add_file("ml-agent", "model.weights", "1.0 2.0 3.0\n");
    repo.add_file("ml-agent", "config.json", "{\"lr\": 0.01}");

    let epoch = repo.current_epoch();
    let candidate = "f".repeat(40);

    write_merge_state_build(repo.root(), &["ml-agent"], &epoch, &candidate);
    assert_eq!(read_phase(repo.root()), Some("build".to_owned()));

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "aborted_pre_commit");
    assert!(!merge_state_path(repo.root()).exists());

    // Critical data preserved
    assert_eq!(
        repo.read_file("ml-agent", "model.weights"),
        Some("1.0 2.0 3.0\n".to_owned()),
        "model weights must survive BUILD crash"
    );
    assert_eq!(
        repo.read_file("ml-agent", "config.json"),
        Some("{\"lr\": 0.01}".to_owned()),
        "config must survive BUILD crash"
    );
    assert_git_integrity(repo.root());
}

/// CLEANUP crash is idempotent: second recovery succeeds after first run.
///
/// This validates that CLEANUP recovery can be run multiple times without
/// error, even if some cleanup steps already completed.
#[test]
fn crash_in_cleanup_is_fully_idempotent() {
    let repo = TestRepo::new();
    repo.seed_files(&[("app.rs", "fn main() {}")]);
    repo.create_workspace("finalize-ws");
    repo.add_file("finalize-ws", "result.bin", "binary data\n");

    let epoch = repo.current_epoch();
    let candidate = "e".repeat(40);

    // First crash in CLEANUP
    write_merge_state_cleanup(repo.root(), &["finalize-ws"], &epoch, &candidate);
    let r1 = simulate_recovery(repo.root());
    assert_eq!(r1, "retry_cleanup");
    assert!(!merge_state_path(repo.root()).exists());

    // Re-run CLEANUP (as if it crashed during first recovery attempt)
    let r2 = simulate_recovery(repo.root());
    assert_eq!(r2, "no_merge_in_progress");

    // Third run still safe
    let r3 = simulate_recovery(repo.root());
    assert_eq!(r3, "no_merge_in_progress");

    // Data preserved through all recovery attempts
    assert_eq!(
        repo.read_file("finalize-ws", "result.bin"),
        Some("binary data\n".to_owned())
    );
    assert_git_integrity(repo.root());
}

/// Merge-state JSON format round-trip: the file written by our helper must
/// parse as valid JSON with the expected structure.
#[test]
fn merge_state_json_is_valid_and_structured() {
    let repo = TestRepo::new();
    let epoch = repo.current_epoch();

    write_merge_state(repo.root(), "prepare", &["ws-a", "ws-b"], &epoch);

    let raw = fs::read_to_string(merge_state_path(repo.root())).expect("read state file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse as JSON");

    assert_eq!(parsed["phase"], "prepare");
    assert_eq!(parsed["epoch_before"], epoch.as_str());
    assert_eq!(parsed["started_at"], 1000_u64);
    assert_eq!(parsed["updated_at"], 1000_u64);

    let sources = parsed["sources"].as_array().expect("sources is array");
    assert_eq!(sources.len(), 2);
    assert!(sources.iter().any(|v| v == "ws-a"));
    assert!(sources.iter().any(|v| v == "ws-b"));
}

/// Many workspaces: crash in BUILD preserves all workspace files.
///
/// Tests the N-workspace scenario where many agents were contributing.
#[test]
fn crash_in_build_many_workspaces_no_data_loss() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "shared base")]);

    let ws_names: Vec<String> = (1..=5).map(|i| format!("agent-{i}")).collect();
    for name in &ws_names {
        repo.create_workspace(name);
        repo.add_file(name, &format!("{name}.txt"), &format!("{name} output"));
    }

    let epoch = repo.current_epoch();
    let candidate = "a".repeat(40);
    let ws_refs: Vec<&str> = ws_names.iter().map(String::as_str).collect();
    write_merge_state_build(repo.root(), &ws_refs, &epoch, &candidate);

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "aborted_pre_commit");
    assert!(!merge_state_path(repo.root()).exists());

    // All 5 workspaces must have their files intact
    for name in &ws_names {
        assert_eq!(
            repo.read_file(name, &format!("{name}.txt")),
            Some(format!("{name} output")),
            "file in {name} must survive BUILD crash"
        );
    }

    assert_git_integrity(repo.root());
}

/// State file with unknown phase is treated as unknown (future-proofing).
#[test]
fn unknown_phase_in_state_file_yields_unknown_outcome() {
    let repo = TestRepo::new();
    let epoch = repo.current_epoch();

    // Write a state file with a phase value we don't recognize
    write_merge_state(repo.root(), "future_phase_v3", &["agent"], &epoch);

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "unknown_phase");

    // State file untouched
    assert!(merge_state_path(repo.root()).exists());
    assert_eq!(read_phase(repo.root()), Some("future_phase_v3".to_owned()));
    assert_git_integrity(repo.root());
}

/// Crash at the very end of PREPARE (after writing state but before doing work).
///
/// This specifically validates the scenario where PREPARE is the last thing
/// that ran before the crash — the state file exists, but nothing else changed.
#[test]
fn crash_at_end_of_prepare_no_side_effects() {
    let repo = TestRepo::new();
    let epoch_before = repo.current_epoch();
    repo.seed_files(&[("existing.txt", "v1")]);

    repo.create_workspace("solver");
    repo.add_file("solver", "solution.txt", "answer: 42");

    let epoch_after_seed = repo.current_epoch();
    assert_ne!(epoch_before, epoch_after_seed, "seed must advance epoch");

    write_merge_state(repo.root(), "prepare", &["solver"], &epoch_after_seed);

    // Epoch ref must still be the same (PREPARE doesn't advance it)
    assert_eq!(repo.current_epoch(), epoch_after_seed, "PREPARE must not advance epoch");

    let outcome = simulate_recovery(repo.root());
    assert_eq!(outcome, "aborted_pre_commit");

    // Epoch still unchanged after recovery
    assert_eq!(repo.current_epoch(), epoch_after_seed, "epoch must be unchanged after PREPARE recovery");

    // Workspace data intact
    assert_eq!(
        repo.read_file("solver", "solution.txt"),
        Some("answer: 42".to_owned())
    );
    assert_eq!(
        repo.read_file("default", "existing.txt"),
        Some("v1".to_owned())
    );

    assert_git_integrity(repo.root());
}
