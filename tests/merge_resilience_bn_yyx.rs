//! bn-yyx: merge-engine-resilience integration tests.
//!
//! These tests prove the `ws_merge_structured_conflict` friction-cluster
//! mitigation is real and exercisable end-to-end:
//!
//! 1. A `maw ws merge` that surfaces structured conflicts writes a persistent
//!    last-conflict snapshot AND appends `integration_started` +
//!    `conflict_detected` + `integration_aborted` events to the merge event
//!    log.
//! 2. `maw merge last-conflict` reads back the snapshot's IDs, paths, sides,
//!    and copy-pasteable recovery commands — no second `maw ws merge` is
//!    needed to recall what happened.
//! 3. `maw merge events` tails the log; `--since-last-attempt` returns only
//!    the events from the most recent attempt.
//! 4. `maw merge resume --dry-run` derives the exact `maw ws merge` command
//!    the agent would otherwise have to reconstruct by hand from the prior
//!    conflict report — the load-bearing "avoid the retry" affordance.
//!
//! The friction-cluster cost is the wasted turn an agent burns re-running
//! `maw ws merge` after a conflict, because the prior conflict report has
//! scrolled off. These behaviours collectively replace that retry with a
//! single read of persistent state — the soft proxy for "next-bench will
//! show ≥50% reduction in attributed `ws_merge_structured_conflict` cost".

mod manifold_common;

use manifold_common::TestRepo;
use serde_json::Value;
use std::fs;

/// Force a real structured conflict between two workspaces touching the same
/// file in incompatible ways, then run `maw ws merge` to surface it.
///
/// Returns the test repo (still alive) after the conflicting merge has
/// failed.
fn setup_repo_with_pending_structured_conflict() -> TestRepo {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "base\n")]);

    // Workspace "alice" writes one version of shared.txt.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "shared.txt", "alice's edit\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice"]);

    // Workspace "bob" writes a conflicting version.
    repo.maw_ok(&["ws", "create", "bob"]);
    repo.add_file("bob", "shared.txt", "bob's edit\n");
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "bob"]);

    // Merge both into default — the BUILD phase must surface a structured
    // conflict on shared.txt.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--into",
        "default",
        "--message",
        "merge alice + bob (will conflict)",
    ]);
    assert!(
        !out.status.success(),
        "expected conflicting merge to fail, but it succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    repo
}

#[test]
fn conflict_persists_last_conflict_snapshot_on_disk() {
    // Load-bearing claim of the friction-cluster fix: after a conflicting
    // merge, the agent can recall what happened from disk without re-running
    // `maw ws merge`. This test asserts the on-disk artifact actually lands.
    let repo = setup_repo_with_pending_structured_conflict();

    let snapshot_path = repo
        .root()
        .join(".manifold/artifacts/merge/last-conflict.json");
    assert!(
        snapshot_path.exists(),
        "last-conflict snapshot missing at {}",
        snapshot_path.display()
    );

    let bytes = fs::read(&snapshot_path).expect("read snapshot");
    let parsed: Value = serde_json::from_slice(&bytes).expect("parse snapshot json");
    assert_eq!(parsed["schema_version"], 1);
    let sources = parsed["sources"].as_array().expect("sources array");
    assert!(
        sources.iter().any(|v| v == "alice") && sources.iter().any(|v| v == "bob"),
        "sources should record both workspaces; got {sources:?}"
    );
    let conflicts = parsed["conflicts"].as_array().expect("conflicts array");
    assert!(
        !conflicts.is_empty(),
        "snapshot should record at least one conflict entry"
    );
    let first = &conflicts[0];
    assert!(
        first["id"].as_str().unwrap_or("").starts_with("cf-"),
        "conflict id should be a cf-terseid; got {:?}",
        first["id"]
    );
    assert!(
        first["path"].as_str().unwrap_or("").contains("shared.txt"),
        "conflict should be on shared.txt; got {:?}",
        first["path"]
    );

    // Recovery commands must include `maw merge resume` so the agent's
    // recall surface points at a non-friction verb.
    let recovery = parsed["recovery_commands"]
        .as_array()
        .expect("recovery_commands array");
    let texts: Vec<&str> = recovery.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        texts.iter().any(|c| c.starts_with("maw merge resume")),
        "recovery commands must include `maw merge resume`; got {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|c| c.contains("maw ws merge") && c.contains("--resolve cf-")),
        "recovery commands must include a per-conflict `--resolve` form; got {texts:?}"
    );
}

#[test]
fn conflict_appends_events_to_event_log() {
    // Append-only event log is the second leg of the mitigation: it carries
    // an oracle-quality "what just happened" the agent reads via
    // `maw merge events` instead of re-running the merge.
    let repo = setup_repo_with_pending_structured_conflict();
    let log_path = repo.root().join(".manifold/events/merge.jsonl");
    assert!(
        log_path.exists(),
        "merge event log missing at {}",
        log_path.display()
    );

    let bytes = fs::read(&log_path).expect("read log");
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).collect();
    let events: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_slice(l).expect("parse event"))
        .collect();
    // Must include at least one IntegrationStarted, one ConflictDetected,
    // and one IntegrationAborted for the failed merge.
    let kinds: Vec<String> = events
        .iter()
        .filter_map(|ev| ev["kind"]["type"].as_str().map(str::to_string))
        .collect();
    assert!(
        kinds.contains(&"integration_started".to_string()),
        "missing integration_started in {kinds:?}"
    );
    assert!(
        kinds.contains(&"conflict_detected".to_string()),
        "missing conflict_detected in {kinds:?}"
    );
    assert!(
        kinds.contains(&"integration_aborted".to_string()),
        "missing integration_aborted in {kinds:?}"
    );

    // The conflict_detected event must carry parallel ids+paths the agent
    // can use without re-running the merge.
    let cd = events
        .iter()
        .find(|ev| ev["kind"]["type"] == "conflict_detected")
        .expect("conflict_detected event present");
    let ids = cd["kind"]["conflict_ids"].as_array().expect("conflict_ids");
    let paths = cd["kind"]["paths"].as_array().expect("paths");
    assert_eq!(ids.len(), paths.len(), "ids and paths must be parallel");
    assert!(
        paths.iter().any(|p| p.as_str().unwrap_or("").contains("shared.txt")),
        "conflict_detected.paths should include shared.txt; got {paths:?}"
    );
}

#[test]
fn last_conflict_cli_returns_snapshot_in_json() {
    // The agent's recall path: `maw merge last-conflict --format json` must
    // return the persisted snapshot's structured contents WITHOUT re-running
    // the merge. This is the call we expect to replace the wasted retry.
    let repo = setup_repo_with_pending_structured_conflict();

    let out = repo.maw_raw_exact(&["merge", "last-conflict", "--format", "json"]);
    assert!(
        out.status.success(),
        "last-conflict should succeed when a snapshot is present\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: Value = serde_json::from_str(&stdout).expect("parse json");
    assert_eq!(parsed["present"], true);
    let snapshot = &parsed["snapshot"];
    assert_eq!(snapshot["schema_version"], 1);
    let conflicts = snapshot["conflicts"].as_array().expect("conflicts");
    assert!(!conflicts.is_empty());
    assert!(
        snapshot["recovery_commands"]
            .as_array()
            .expect("recovery_commands")
            .iter()
            .any(|c| c.as_str().unwrap_or("").starts_with("maw merge resume")),
        "snapshot must point the agent at `maw merge resume`"
    );
}

#[test]
fn last_conflict_cli_returns_absent_when_nothing_recorded() {
    // Clean repo, never merged: `last-conflict` must NOT error — it should
    // return a stable shape the agent can dispatch on. This is the
    // self-describing-output principle: an empty answer is still an answer.
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "x\n")]);

    let out = repo.maw_raw_exact(&["merge", "last-conflict", "--format", "json"]);
    assert!(
        out.status.success(),
        "last-conflict should succeed even with no snapshot\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: Value = serde_json::from_str(&stdout).expect("parse json");
    assert_eq!(parsed["present"], false);
}

#[test]
fn events_cli_emits_recorded_events_as_json() {
    // `maw merge events --format json` returns the event log as a JSON array
    // suitable for piping into `jq` or another agent-side tool.
    let repo = setup_repo_with_pending_structured_conflict();
    let out = repo.maw_raw_exact(&["merge", "events", "--format", "json"]);
    assert!(out.status.success(), "events should succeed");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: Value = serde_json::from_str(&stdout).expect("parse json");
    let arr = parsed.as_array().expect("array");
    assert!(arr.len() >= 3, "expected ≥3 events; got {}", arr.len());
}

#[test]
fn events_cli_since_last_attempt_bounds_to_latest_start() {
    // The `--since-last-attempt` convenience filters to events from the most
    // recent `integration_started` onward. This is the agent's "what just
    // happened" answer without timestamp arithmetic.
    let repo = setup_repo_with_pending_structured_conflict();

    // Run a second attempt (still conflicting) to make sure the filter
    // actually drops the older attempt's events.
    let _ = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--into",
        "default",
        "--message",
        "second attempt — also fails",
    ]);

    let out = repo.maw_raw_exact(&[
        "merge",
        "events",
        "--since-last-attempt",
        "--format",
        "json",
    ]);
    assert!(out.status.success(), "events --since-last-attempt should succeed");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: Value = serde_json::from_str(&stdout).expect("parse json");
    let arr = parsed.as_array().expect("array");
    let starts: Vec<&Value> = arr
        .iter()
        .filter(|ev| ev["kind"]["type"] == "integration_started")
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "filter should keep exactly one integration_started (the latest); got {} in {arr:?}",
        starts.len()
    );
}

#[test]
fn resume_dry_run_derives_command_without_running_merge() {
    // The mergeback-queue affordance: `maw merge resume --dry-run` derives
    // the exact `maw ws merge` command the agent would otherwise have to
    // reconstruct by hand from the prior conflict report — and verifies
    // that the recall surface is sufficient without re-running the merge.
    let repo = setup_repo_with_pending_structured_conflict();

    let out = repo.maw_raw_exact(&[
        "merge",
        "resume",
        "--resolve-all=alice",
        "--dry-run",
    ]);
    assert!(
        out.status.success(),
        "resume --dry-run should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Would run: maw ws merge"),
        "dry-run should print derived command; got {stdout}"
    );
    assert!(
        stdout.contains("alice") && stdout.contains("bob"),
        "derived command should include both source workspaces; got {stdout}"
    );
    assert!(
        stdout.contains("--into default"),
        "derived command should include --into target; got {stdout}"
    );
    assert!(
        stdout.contains("--resolve-all=alice"),
        "derived command should propagate --resolve-all; got {stdout}"
    );
}

#[test]
fn resume_without_resolutions_refuses_with_self_describing_hint() {
    // Per CLI design conventions (.agents/edict/design/cli-conventions.md),
    // refusals must be self-describing. `maw merge resume` with no
    // --resolve / --resolve-all must explain WHAT to do, not just refuse.
    let repo = setup_repo_with_pending_structured_conflict();
    let out = repo.maw_raw_exact(&["merge", "resume"]);
    assert!(
        !out.status.success(),
        "resume without --resolve should refuse"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--resolve-all") && stderr.contains("--resolve cf-"),
        "refusal must name the fix-forward flags; got {stderr}"
    );
}

#[test]
fn successful_merge_clears_last_conflict_and_records_completion_event() {
    // Once the merge succeeds (here: trivially, no conflict), the snapshot
    // must NOT be present (it would be stale), and the event log must
    // record an `integration_completed`. This is the lifecycle close-out
    // that prevents `maw merge last-conflict` from returning stale data.
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "v0\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "feat.txt", "new file\n");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "feat"]);

    repo.maw_ok(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge feat",
    ]);

    let snapshot_path = repo
        .root()
        .join(".manifold/artifacts/merge/last-conflict.json");
    assert!(
        !snapshot_path.exists(),
        "successful merge must clear stale last-conflict snapshot"
    );

    let log_path = repo.root().join(".manifold/events/merge.jsonl");
    assert!(log_path.exists());
    let bytes = fs::read(&log_path).expect("read log");
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).collect();
    let kinds: Vec<String> = lines
        .iter()
        .map(|l| serde_json::from_slice::<Value>(l).expect("parse"))
        .filter_map(|ev| ev["kind"]["type"].as_str().map(str::to_string))
        .collect();
    assert!(
        kinds.contains(&"integration_completed".to_string()),
        "successful merge should emit integration_completed; got {kinds:?}"
    );
}

#[test]
fn conflict_report_includes_anti_retry_cue() {
    // The text-mode conflict report must lead with an explicit "do NOT
    // re-run `maw ws merge`" cue pointing at the recall verbs. The
    // attribution heuristic in maw-bench-metrics keys on the agent's NEXT
    // call after a conflict; moving the visible affordance away from
    // `maw ws merge` is the front-line UX fix.
    let repo = setup_repo_with_pending_structured_conflict();

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--into",
        "default",
        "--message",
        "should fail again",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("do NOT re-run `maw ws merge`"),
        "conflict report should include the anti-retry cue; got stdout: {stdout}"
    );
    // And it must surface the new recovery verbs by name.
    assert!(
        stdout.contains("maw merge last-conflict"),
        "conflict report should point at `maw merge last-conflict`; got: {stdout}"
    );
    assert!(
        stdout.contains("maw merge events"),
        "conflict report should point at `maw merge events`; got: {stdout}"
    );
}
