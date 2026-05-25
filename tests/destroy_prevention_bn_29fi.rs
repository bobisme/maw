//! Integration tests for SG4 `bn-29fi` (destroy-prevention).
//!
//! These tests prove the behavior change is real:
//!
//! 1. `maw ws destroy --dry-run --format json` returns a structured
//!    plan WITHOUT mutating workspace state — the agent can preview
//!    before deciding which verb to run.
//! 2. `maw ws destroy --dry-run` surfaces `would_need_recovery=true`
//!    for the dirty-with-force case, so the agent sees the
//!    recover-round-trip cost before paying it.
//! 3. `maw doctor` warns about `abandoned-with-snapshot` workspaces,
//!    naming the destroy-prevention queue so the agent drains it
//!    before destroying more workspaces.
//! 4. The destroy-refusal message now points at the `--dry-run`
//!    pre-flight + the merge-with-destroy alternative — the agent
//!    has the safer path on the first refusal, not after a
//!    discovery turn.
//!
//! All four behaviors target the upstream cause of
//! `MawVerbAttribution::WsRecoverInvoked` turns: the agent destroys,
//! the work needs recovering, the recover call burns a turn. Naming
//! the queue and providing the preview shortens or eliminates that
//! cycle.

mod manifold_common;

use manifold_common::TestRepo;

/// `--dry-run` on a clean workspace returns `would-destroy` and does
/// NOT touch the workspace.
#[test]
fn dry_run_on_clean_workspace_predicts_destroy_proceeds_without_mutating() {
    let repo = TestRepo::new();
    repo.create_workspace("alice");
    // No edits — workspace is clean.

    let out = repo.maw_ok(&["ws", "destroy", "alice", "--dry-run", "--format", "json"]);
    let preview: serde_json::Value =
        serde_json::from_str(&out).expect("dry-run --format json is valid JSON");

    assert_eq!(preview["workspace"], "alice");
    assert_eq!(preview["action"], "would-destroy");
    assert_eq!(preview["would_proceed"], true);
    assert_eq!(preview["would_capture_snapshot"], false);
    assert_eq!(preview["would_need_recovery"], false);
    assert_eq!(preview["touched_count"], 0);
    assert_eq!(preview["lifecycle_state"], "clean");
    assert!(
        preview["recommended_command"]
            .as_str()
            .expect("string")
            .contains("maw ws destroy alice")
    );

    // The workspace must still exist — dry-run is pure inspection.
    assert!(
        repo.workspace_exists("alice"),
        "dry-run must not destroy the workspace"
    );
}

/// `--dry-run` on a dirty workspace WITHOUT `--force` returns
/// `would-refuse` and the recommended command is the merge-with-destroy
/// alternative — the destroy-prevention surface.
#[test]
fn dry_run_on_dirty_workspace_predicts_refuse_and_recommends_merge_destroy() {
    let repo = TestRepo::new();
    repo.create_workspace("bob");
    repo.add_file("bob", "important.txt", "critical data\n");

    let out = repo.maw_ok(&["ws", "destroy", "bob", "--dry-run", "--format", "json"]);
    let preview: serde_json::Value = serde_json::from_str(&out).expect("dry-run JSON");

    assert_eq!(preview["action"], "would-refuse");
    assert_eq!(preview["would_proceed"], false);
    assert_eq!(preview["would_capture_snapshot"], false);
    assert!(
        preview["touched_count"].as_u64().is_some_and(|n| n >= 1),
        "dirty workspace should have touched_count >= 1, got {:?}",
        preview["touched_count"]
    );
    let rec = preview["recommended_command"]
        .as_str()
        .expect("recommended_command is a string");
    assert!(
        rec.contains("maw ws merge bob") && rec.contains("--destroy"),
        "recommended should be merge-with-destroy, got: {rec}"
    );

    // Workspace must still exist + still be dirty.
    assert!(repo.workspace_exists("bob"));
    assert!(
        repo.file_exists("bob", "important.txt"),
        "dry-run must not delete dirty files"
    );
}

/// `--dry-run --force` on a dirty workspace returns
/// `would-force-snapshot` AND `would_need_recovery=true` so the agent
/// sees the round-trip cost BEFORE paying it.
#[test]
fn dry_run_force_on_dirty_workspace_flags_would_need_recovery() {
    let repo = TestRepo::new();
    repo.create_workspace("carol");
    repo.add_file("carol", "draft.md", "wip\n");

    let out = repo.maw_ok(&[
        "ws",
        "destroy",
        "carol",
        "--dry-run",
        "--force",
        "--format",
        "json",
    ]);
    let preview: serde_json::Value = serde_json::from_str(&out).expect("dry-run JSON");

    assert_eq!(preview["action"], "would-force-snapshot");
    assert_eq!(preview["would_proceed"], true);
    assert_eq!(preview["would_capture_snapshot"], true);
    assert_eq!(
        preview["would_need_recovery"], true,
        "force-destroying dirty work IS the upstream cause of ws_recover_invoked turns; \
         the preview must name it"
    );

    // Workspace must still exist.
    assert!(repo.workspace_exists("carol"));
}

/// After a real `destroy --force`, the same workspace name comes back
/// in `--dry-run` as `already-absent` with `has_prior_snapshot=true`
/// AND the lifecycle is `abandoned-with-snapshot`. The destroy-
/// prevention cue surfaces the mergeback queue.
#[test]
fn dry_run_after_force_destroy_reports_abandoned_with_snapshot() {
    let repo = TestRepo::new();
    repo.create_workspace("dave");
    repo.add_file("dave", "wip.txt", "queued work\n");
    repo.maw_ok(&["ws", "destroy", "dave", "--force"]);
    assert!(!repo.workspace_exists("dave"));

    let out = repo.maw_ok(&["ws", "destroy", "dave", "--dry-run", "--format", "json"]);
    let preview: serde_json::Value = serde_json::from_str(&out).expect("dry-run JSON");

    assert_eq!(preview["action"], "already-absent");
    assert_eq!(preview["has_prior_snapshot"], true);
    assert_eq!(preview["would_need_recovery"], true);
    assert_eq!(
        preview["lifecycle_state"], "abandoned-with-snapshot",
        "destroyed workspaces with pinned snapshots get the safe-cleanup vocabulary name"
    );
    let rec = preview["recommended_command"]
        .as_str()
        .expect("recommended_command is a string");
    assert!(
        rec.contains("maw ws recover dave") && rec.contains("--to"),
        "abandoned-with-snapshot's fix command names the recover-to path, got: {rec}"
    );
}

/// `maw doctor --format json` includes the `abandoned-with-snapshot`
/// check, which warns when there are queued recovery snapshots from
/// prior destroys.
#[test]
fn doctor_warns_about_abandoned_with_snapshot_workspaces() {
    let repo = TestRepo::new();
    repo.create_workspace("eve");
    repo.add_file("eve", "drafted.txt", "fragments\n");
    repo.maw_ok(&["ws", "destroy", "eve", "--force"]);

    let out = repo.maw_ok(&["doctor", "--format", "json"]);
    let env: serde_json::Value = serde_json::from_str(&out).expect("doctor JSON");
    let checks = env["checks"].as_array().expect("checks array");

    let abandoned = checks
        .iter()
        .find(|c| c["name"] == "abandoned-with-snapshot")
        .expect("abandoned-with-snapshot check is registered in `maw doctor`");
    assert_eq!(
        abandoned["status"], "warn",
        "an existing snapshot should produce a `warn`, not silent `ok`; got: {abandoned}"
    );
    assert!(
        abandoned["message"]
            .as_str()
            .expect("message")
            .contains("eve"),
        "check should name the queued workspace, got: {abandoned}"
    );
    let fix = abandoned["fix"]
        .as_str()
        .expect("fix is populated when status is warn");
    assert!(
        fix.contains("maw ws recover"),
        "fix hint should name the recover verb, got: {fix}"
    );
}

/// `maw doctor` on a pristine repo reports the new check as `ok`.
/// Sanity test for the negative case.
#[test]
fn doctor_reports_ok_when_no_abandoned_with_snapshot_workspaces() {
    let repo = TestRepo::new();
    let out = repo.maw_ok(&["doctor", "--format", "json"]);
    let env: serde_json::Value = serde_json::from_str(&out).expect("doctor JSON");
    let checks = env["checks"].as_array().expect("checks array");

    let abandoned = checks
        .iter()
        .find(|c| c["name"] == "abandoned-with-snapshot")
        .expect("abandoned-with-snapshot check is registered in `maw doctor`");
    assert_eq!(abandoned["status"], "ok");
}

/// The destroy-refusal message points at the `--dry-run` pre-flight
/// AND at `merge ... --destroy`. The agent has the safer path named on
/// the first refusal, not after a discovery turn.
#[test]
fn destroy_refusal_names_dry_run_and_merge_destroy() {
    let repo = TestRepo::new();
    repo.create_workspace("frank");
    repo.add_file("frank", "needed.txt", "saved work\n");

    let err = repo.maw_fails(&["ws", "destroy", "frank"]);
    assert!(
        err.contains("--dry-run"),
        "destroy refusal must point to the dry-run preview surface (bn-29fi); got: {err}"
    );
    assert!(
        err.contains("merge") && err.contains("--destroy"),
        "destroy refusal must name the merge-with-destroy alternative; got: {err}"
    );
    // Sanity: the original `--force` escape hatch is still surfaced.
    assert!(err.contains("--force"));
}

/// Force-destroying dirty work emits a "abandoned-with-snapshot" cue
/// AND the recover→merge command sequence — naming the upstream cause
/// of `ws_recover_invoked` turns in the output the agent reads
/// immediately after the operation.
#[test]
fn force_destroy_output_emits_mergeback_queue_cue() {
    let repo = TestRepo::new();
    repo.create_workspace("grace");
    repo.add_file("grace", "stuff.txt", "queued\n");

    let out = repo.maw_ok(&["ws", "destroy", "grace", "--force"]);
    assert!(
        out.contains("abandoned-with-snapshot"),
        "force-destroy stdout must name the safe-cleanup vocabulary state (bn-29fi); got: {out}"
    );
    assert!(
        out.contains("maw ws recover grace --to grace-restored"),
        "force-destroy stdout must name the recover-into-new-ws cue (bn-29fi); got: {out}"
    );
    assert!(
        out.contains("maw ws merge grace-restored"),
        "force-destroy stdout must name the follow-up merge step (bn-29fi); got: {out}"
    );
}
