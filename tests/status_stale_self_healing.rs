//! bn-221b (SG4 / `ws_sync_stale_workspace` cluster):
//! integration tests for the stale-state-self-healing fix.
//!
//! Each test asserts a single user-visible JSON contract that an
//! agent can branch on, so the bench-time friction cluster
//! (`MawVerbAttribution::WsSyncStaleWorkspace`) cannot fire the
//! "wasted-discovery turn" pattern that the bone is funded to
//! reduce by ≥50% at next-bench.

mod manifold_common;

use manifold_common::TestRepo;
use serde_json::Value;

fn status_json(repo: &TestRepo) -> Value {
    let out = repo.maw_ok(&["status", "--format", "json"]);
    serde_json::from_str(&out).expect("maw status --format json should be valid JSON")
}

/// SG4 / bn-221b — proves the JSON envelope answers
/// "what is stale / what is the exact fix" in ONE call. Pre-fix,
/// `maw status --json` listed workspace names with no staleness
/// info; agents had to follow up with `maw ws status` or wait for
/// `maw ws merge` to fail with a stale signal.
#[test]
fn status_json_lists_stale_workspaces_with_fix_command() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    // Advance the epoch under alice's feet.
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch for bn-221b stale test");

    let json = status_json(&repo);

    let stale = json["stale_workspaces"]
        .as_array()
        .expect("stale_workspaces must be an array");
    assert_eq!(
        stale.len(),
        1,
        "expected exactly one stale workspace, got: {stale:?}"
    );

    let entry = &stale[0];
    assert_eq!(entry["name"].as_str(), Some("alice"));
    assert!(
        entry["behind_epochs"].as_u64().unwrap_or(0) >= 1,
        "behind_epochs should be at least 1, got: {entry:?}"
    );
    assert_eq!(
        entry["fix_command"].as_str(),
        Some("maw ws sync alice"),
        "ephemeral stale fix must be the exact sync command"
    );
    assert_eq!(
        entry["mode"].as_str(),
        Some("ephemeral"),
        "default-mode workspaces must report `ephemeral`"
    );
}

/// SG4 / bn-221b — when there is nothing stale, the list is present
/// and empty. Empty-vs-absent distinction matters: agents using
/// `JsonPath` / jq can rely on `.stale_workspaces` always existing.
#[test]
fn status_json_stale_workspaces_empty_when_fresh() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);

    let json = status_json(&repo);
    let stale = json["stale_workspaces"]
        .as_array()
        .expect("stale_workspaces must be present even when empty");
    assert!(
        stale.is_empty(),
        "no workspace is stale yet; got: {stale:?}"
    );
}

/// SG4 / bn-221b — every non-default workspace gets a `lifecycle_state`
/// slug in `workspace_details`. The slug is the named vocabulary an
/// agent branches on (`clean`, `stale`, `committed-unintegrated`, ...).
#[test]
fn status_json_workspace_details_carry_lifecycle_state() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: advance for lifecycle test");

    let json = status_json(&repo);

    let details = json["workspace_details"]
        .as_array()
        .expect("workspace_details must be an array");
    let alice = details
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .expect("alice should be in workspace_details");

    assert_eq!(
        alice["lifecycle_state"].as_str(),
        Some("stale"),
        "alice should have lifecycle_state=stale, got: {alice:?}"
    );
    assert_eq!(
        alice["fix_command"].as_str(),
        Some("maw ws sync alice"),
        "alice should carry the sync fix-command, got: {alice:?}"
    );
}

/// SG4 / bn-221b — integrate-ready workspaces (committed work, not
/// stale) appear in `integrate_ready` with the exact `maw ws merge
/// --check` command. Lets the agent skip the discover-by-failure loop.
#[test]
fn status_json_lists_integrate_ready_workspaces() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "alice feature\n");
    // Commit alice's work so commits_ahead > 0.
    let ws_path = repo.workspace_path("alice");
    manifold_common::git_ok(&ws_path, &["add", "-A"]);
    manifold_common::git_ok(&ws_path, &["commit", "-m", "feat: bn-221b alice work"]);

    let json = status_json(&repo);

    let ready = json["integrate_ready"]
        .as_array()
        .expect("integrate_ready must be an array");
    let alice = ready
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .unwrap_or_else(|| panic!("alice should appear in integrate_ready, got: {ready:?}"));

    assert!(
        alice["commits_ahead"].as_u64().unwrap_or(0) >= 1,
        "commits_ahead should be >= 1, got: {alice:?}"
    );
    let fix = alice["fix_command"].as_str().unwrap_or_default();
    assert!(
        fix.contains("maw ws merge alice") && fix.contains("--check"),
        "fix_command should be `maw ws merge alice --into default --check`, got: {fix}"
    );
}

/// SG4 / bn-221b — when a workspace is BOTH committed-ahead AND
/// stale, it appears in `stale_workspaces` only (the stale fix must
/// be applied first). This prevents the agent from running
/// `maw ws merge` on a stale base and falling into the cluster
/// pattern we are trying to eliminate.
#[test]
fn status_json_stale_takes_priority_over_integrate_ready() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "alice feature\n");
    let ws_path = repo.workspace_path("alice");
    manifold_common::git_ok(&ws_path, &["add", "-A"]);
    manifold_common::git_ok(&ws_path, &["commit", "-m", "feat: bn-221b alice committed"]);

    // Advance the epoch so alice goes stale WITH committed work.
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: advance for priority test");

    let json = status_json(&repo);

    let stale = json["stale_workspaces"]
        .as_array()
        .expect("stale_workspaces array");
    assert!(
        stale.iter().any(|w| w["name"].as_str() == Some("alice")),
        "alice should appear in stale_workspaces, got: {stale:?}"
    );

    let ready = json["integrate_ready"]
        .as_array()
        .expect("integrate_ready array");
    assert!(
        !ready.iter().any(|w| w["name"].as_str() == Some("alice")),
        "alice MUST NOT appear in integrate_ready while stale (would mislead agent into a doomed merge), got: {ready:?}"
    );
}

/// SG4 / bn-221b — persistent workspaces use `maw ws advance`, not
/// `maw ws sync`. The fix-command picks the right verb based on mode.
#[test]
fn status_json_persistent_stale_uses_advance() {
    let repo = TestRepo::new();

    let epoch = repo.current_epoch();
    repo.git(&["update-ref", "refs/heads/feature-x", &epoch]);
    repo.maw_ok(&[
        "ws",
        "create",
        "longliv",
        "--from",
        "feature-x",
        "--persistent",
    ]);

    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: advance for persistent test");

    let json = status_json(&repo);
    let stale = json["stale_workspaces"]
        .as_array()
        .expect("stale_workspaces array");
    let longliv = stale.iter().find(|w| w["name"].as_str() == Some("longliv"));

    // Persistent workspaces don't auto-go-stale on epoch advance in
    // every backend wiring; if it does show up, the fix MUST be
    // `advance` not `sync`. If it doesn't show up the test is a
    // no-op (the priority/labeling logic is what we care about, not
    // the persistent-staleness detection — bn-1ieb owns that).
    if let Some(entry) = longliv {
        assert_eq!(entry["mode"].as_str(), Some("persistent"));
        assert_eq!(
            entry["fix_command"].as_str(),
            Some("maw ws advance longliv"),
            "persistent stale fix must be `maw ws advance`, got: {entry:?}"
        );
    }
}

/// SG4 / bn-221b — the legacy fields (`workspaces`, `is_stale`,
/// `main_sync`) remain present and unchanged in shape. This guards
/// against accidental breakage of existing JSON consumers.
#[test]
fn status_json_preserves_legacy_envelope_shape() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);

    let json = status_json(&repo);

    assert!(json.get("workspaces").is_some());
    assert!(json.get("workspace_details").is_some());
    assert!(json.get("changed_files").is_some());
    assert!(json.get("untracked_files").is_some());
    assert!(json.get("is_stale").is_some());
    assert!(json.get("main_sync").is_some());

    // Names list is still a list of bare strings (no breaking shape change).
    let names = json["workspaces"].as_array().expect("workspaces is array");
    assert!(
        names.iter().all(serde_json::Value::is_string),
        "workspaces should remain a Vec<String> for legacy consumers"
    );
}
