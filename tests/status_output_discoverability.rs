//! bn-242l (SG4 / `read_from_stale_workspace` cluster):
//! integration tests for the status-output-discoverability fix.
//!
//! The cluster fires when an agent reads `maw ws status`, `maw ws list`,
//! `maw status`, or `maw ws diff` output and its next op is inconsistent
//! with a stale workspace (e.g. tries to commit on a stale base then
//! merge). Each test asserts the JSON / prose surface now carries the
//! same named safe-cleanup vocabulary slug + `fix_command` so the
//! misread is mechanical to avoid — the canonical token is literally
//! present in the output, and the recovery command is paste-able.
//!
//! Cluster `MawVerbAttribution::ReadFromStaleWorkspace` is what these
//! tests target; the target metric delta is "≥ 50% reduction in
//! attributed `read_from_stale_workspace` `total_cost_turns` at next
//! SG4 re-benchmark" (practical: reaches 0 given pilot cost=1).
//!
//! Per the bone's mitigation class: "machine-readable workspace
//! manifest, `maw status --json`, safe-cleanup vocabulary".

mod manifold_common;

use manifold_common::TestRepo;
use serde_json::Value;

fn ws_status_json(repo: &TestRepo) -> Value {
    let out = repo.maw_ok(&["ws", "status", "--format", "json"]);
    serde_json::from_str(&out).expect("maw ws status --format json should be valid JSON")
}

fn ws_list_json(repo: &TestRepo) -> Value {
    let out = repo.maw_ok(&["ws", "list", "--format", "json"]);
    serde_json::from_str(&out).expect("maw ws list --format json should be valid JSON")
}

fn ws_diff_json(repo: &TestRepo, workspace: &str) -> Value {
    // `maw ws diff` uses a `--json` shorthand flag (not `--format json`);
    // distinct from `maw ws status`/`ws list` which take `--format`.
    let out = repo.maw_ok(&["ws", "diff", workspace, "--json"]);
    serde_json::from_str(&out).expect("maw ws diff --json should be valid JSON")
}

// ----------------------------------------------------------------------
// `maw ws status --format json`: per-workspace lifecycle_state + fix_command
// ----------------------------------------------------------------------

/// SG4 / bn-242l — `maw ws status --format json` now exposes a named
/// `lifecycle_state` slug per non-default workspace. The cluster fires
/// when the agent reads this surface and misclassifies the state; the
/// slug is the canonical token the agent should branch on, mirroring
/// `maw status --json`'s `workspace_details[].lifecycle_state`.
#[test]
fn ws_status_json_carries_lifecycle_state_for_stale_workspace() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    // Advance the epoch under alice's feet — alice is now stale.
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: bn-242l advance epoch for stale test");

    let json = ws_status_json(&repo);

    let workspaces = json["workspaces"]
        .as_array()
        .expect("workspaces array must be present");
    let alice = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .expect("alice should appear in ws status workspaces array");

    assert_eq!(
        alice["lifecycle_state"].as_str(),
        Some("stale"),
        "alice should carry lifecycle_state=stale, got: {alice}"
    );
    assert_eq!(
        alice["fix_command"].as_str(),
        Some("maw ws sync alice"),
        "alice should carry the exact `sync` fix command, got: {alice}"
    );
    assert!(
        alice["behind_epochs"].as_u64().unwrap_or(0) >= 1,
        "alice should carry behind_epochs >= 1, got: {alice}"
    );
}

/// SG4 / bn-242l — committed-unintegrated workspaces get the named
/// slug `committed-unintegrated` and a paste-able `maw ws merge ...
/// --check` fix command, even when there is nothing stale. This
/// closes the "agent diffed/listed but missed the merge-ready signal"
/// branch of the cluster.
#[test]
fn ws_status_json_marks_committed_unintegrated_with_merge_check_fix() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "alice feature\n");
    let ws_path = repo.workspace_path("alice");
    manifold_common::git_ok(&ws_path, &["add", "-A"]);
    manifold_common::git_ok(&ws_path, &["commit", "-m", "feat: bn-242l alice commit"]);

    let json = ws_status_json(&repo);
    let workspaces = json["workspaces"]
        .as_array()
        .expect("workspaces array");
    let alice = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .expect("alice should appear in ws status workspaces array");

    assert_eq!(
        alice["lifecycle_state"].as_str(),
        Some("committed-unintegrated"),
        "alice should be classified committed-unintegrated, got: {alice}"
    );
    let fix = alice["fix_command"].as_str().unwrap_or_default();
    assert!(
        fix.contains("maw ws merge alice") && fix.contains("--check"),
        "fix_command should be `maw ws merge alice --into default --check`, got: {fix}"
    );
    assert_eq!(
        alice["commits_ahead"].as_u64(),
        Some(1),
        "commits_ahead should be 1, got: {alice}"
    );
}

/// SG4 / bn-242l — clean workspaces omit the `fix_command` (no
/// actionable next step) but keep `lifecycle_state=clean`. The
/// absence vs the explicit slug is the signal an agent uses to
/// distinguish "no action needed" from "no signal at all".
#[test]
fn ws_status_json_marks_clean_workspace_with_lifecycle_but_no_fix() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);

    let json = ws_status_json(&repo);
    let workspaces = json["workspaces"].as_array().expect("workspaces array");
    let alice = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .expect("alice should appear");

    assert_eq!(
        alice["lifecycle_state"].as_str(),
        Some("clean"),
        "fresh alice should be clean, got: {alice}"
    );
    assert!(
        alice.get("fix_command").is_none(),
        "clean workspace must omit fix_command (no action needed), got: {alice}"
    );
}

/// SG4 / bn-242l — the default workspace MUST NOT carry a
/// `lifecycle_state` slug. It is a permanent fixture, not a
/// candidate for the stale/integrate-ready vocabulary. Adding one
/// would mislead an agent into thinking `default` could be
/// destroyed / synced like any other ephemeral workspace.
#[test]
fn ws_status_json_omits_lifecycle_state_for_default_workspace() {
    let repo = TestRepo::new();

    let json = ws_status_json(&repo);
    let workspaces = json["workspaces"].as_array().expect("workspaces array");
    let default_entry = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("default"))
        .expect("default workspace should always appear");

    assert!(
        default_entry.get("lifecycle_state").is_none(),
        "default must NOT carry lifecycle_state, got: {default_entry}"
    );
    assert!(
        default_entry.get("fix_command").is_none(),
        "default must NOT carry fix_command, got: {default_entry}"
    );
}

// ----------------------------------------------------------------------
// `maw ws list --format json`: per-workspace lifecycle_state + fix_command
// ----------------------------------------------------------------------

/// SG4 / bn-242l — `maw ws list --format json` mirrors the same
/// vocabulary. Agents that `ws list` to choose what to merge get the
/// same `lifecycle_state` + `fix_command` shape they would have got
/// from `ws status` / `maw status --json`. The three discovery
/// surfaces cannot disagree.
#[test]
fn ws_list_json_carries_lifecycle_state_for_stale_workspace() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: bn-242l advance for ws list stale test");

    let json = ws_list_json(&repo);
    let workspaces = json["workspaces"]
        .as_array()
        .expect("workspaces array");
    let alice = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .expect("alice should appear in ws list workspaces array");

    assert_eq!(
        alice["lifecycle_state"].as_str(),
        Some("stale"),
        "alice should be lifecycle_state=stale in ws list, got: {alice}"
    );
    assert_eq!(
        alice["fix_command"].as_str(),
        Some("maw ws sync alice"),
        "alice should carry the same sync fix command as ws status, got: {alice}"
    );
}

/// SG4 / bn-242l — vocabulary parity across all three discovery
/// surfaces is the load-bearing invariant. If `maw status --json`,
/// `maw ws status --format json`, and `maw ws list --format json`
/// can disagree on a workspace's named state, the agent that reads
/// only one of them gets the wrong cue. Assert all three agree.
#[test]
fn lifecycle_state_agrees_across_status_ws_status_ws_list() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: bn-242l vocabulary parity test");

    let status_json: Value = {
        let out = repo.maw_ok(&["status", "--format", "json"]);
        serde_json::from_str(&out).expect("maw status --format json")
    };
    let ws_status = ws_status_json(&repo);
    let ws_list = ws_list_json(&repo);

    // maw status --json
    let status_alice = status_json["workspace_details"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["name"].as_str() == Some("alice")))
        .expect("maw status --json should list alice");

    // maw ws status --format json
    let ws_status_alice = ws_status["workspaces"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["name"].as_str() == Some("alice")))
        .expect("ws status --json should list alice");

    // maw ws list --format json
    let ws_list_alice = ws_list["workspaces"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["name"].as_str() == Some("alice")))
        .expect("ws list --json should list alice");

    let s1 = status_alice["lifecycle_state"].as_str();
    let s2 = ws_status_alice["lifecycle_state"].as_str();
    let s3 = ws_list_alice["lifecycle_state"].as_str();

    assert_eq!(
        s1,
        Some("stale"),
        "maw status --json should report alice=stale"
    );
    assert_eq!(
        s1, s2,
        "maw status --json and ws status disagree on lifecycle_state: {s1:?} vs {s2:?}"
    );
    assert_eq!(
        s2, s3,
        "ws status and ws list disagree on lifecycle_state: {s2:?} vs {s3:?}"
    );

    // fix_command parity too — all three should hand the agent the same exact verb.
    let f1 = status_alice["fix_command"].as_str();
    let f2 = ws_status_alice["fix_command"].as_str();
    let f3 = ws_list_alice["fix_command"].as_str();
    assert_eq!(
        f1, f2,
        "maw status --json and ws status disagree on fix_command: {f1:?} vs {f2:?}"
    );
    assert_eq!(
        f2, f3,
        "ws status and ws list disagree on fix_command: {f2:?} vs {f3:?}"
    );
}

// ----------------------------------------------------------------------
// `maw ws diff --format json`: lifecycle_state on the diffed workspace
// ----------------------------------------------------------------------

/// SG4 / bn-242l — `maw ws diff --format json` now embeds the
/// workspace's `lifecycle_state` and a `fix_command` when stale.
/// Pre-bn-242l, an agent that diffed a stale workspace to decide "is
/// this ready to merge?" got back a file list with no liveness
/// signal, then issued a doomed `maw ws merge` and burned a turn on
/// the cluster.
#[test]
fn ws_diff_json_carries_lifecycle_state_for_stale_workspace() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    // Commit a file in alice so the diff has something to show.
    repo.add_file("alice", "feature.txt", "alice feature\n");
    let ws_path = repo.workspace_path("alice");
    manifold_common::git_ok(&ws_path, &["add", "-A"]);
    manifold_common::git_ok(&ws_path, &["commit", "-m", "feat: bn-242l alice diff target"]);

    // Advance the epoch so alice is now stale.
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: bn-242l advance for ws diff stale test");

    let json = ws_diff_json(&repo, "alice");
    assert_eq!(json["workspace"].as_str(), Some("alice"));
    assert_eq!(
        json["lifecycle_state"].as_str(),
        Some("stale"),
        "ws diff --format json should carry lifecycle_state=stale, got: {json}"
    );
    assert_eq!(
        json["fix_command"].as_str(),
        Some("maw ws sync alice"),
        "ws diff --format json should carry the sync fix command, got: {json}"
    );
    assert!(
        json["behind_epochs"].as_u64().unwrap_or(0) >= 1,
        "ws diff --format json should carry behind_epochs >= 1, got: {json}"
    );
}

/// SG4 / bn-242l — when the diff target is clean and current, the
/// `lifecycle_state` is `clean` and the `fix_command` is absent. The
/// presence-of-slug + absence-of-fix combo lets the agent branch
/// `"if lifecycle_state == 'clean' && fix_command is None: merge"`
/// without re-parsing prose.
#[test]
fn ws_diff_json_marks_clean_workspace_lifecycle_no_fix() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    // No epoch advance, no commits — alice is clean and current.

    let json = ws_diff_json(&repo, "alice");
    assert_eq!(json["workspace"].as_str(), Some("alice"));
    assert_eq!(
        json["lifecycle_state"].as_str(),
        Some("clean"),
        "ws diff --format json on a clean workspace should report clean, got: {json}"
    );
    assert!(
        json.get("fix_command").is_none(),
        "clean workspace ws diff must omit fix_command, got: {json}"
    );
}

// ----------------------------------------------------------------------
// Prose output carries the same vocabulary
// ----------------------------------------------------------------------

/// SG4 / bn-242l — prose output (`maw ws status` without `--format
/// json`) carries the named lifecycle slug as a `[lifecycle:<slug>]`
/// tag. The cluster fires when an agent reads the prose and
/// misclassifies; carrying the canonical token in the prose closes
/// the misread loop for agents that don't parse JSON.
#[test]
fn ws_status_text_includes_named_lifecycle_slug() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "advance\n");
    repo.advance_epoch("chore: bn-242l advance for ws status text test");

    let out = repo.maw_ok(&["ws", "status"]);
    assert!(
        out.contains("[lifecycle:stale]"),
        "ws status prose must carry `[lifecycle:stale]` for stale alice, got:\n{out}"
    );
    assert!(
        out.contains("Fix: maw ws sync alice"),
        "ws status prose must carry the paste-able `Fix:` line for alice, got:\n{out}"
    );
}

/// SG4 / bn-242l — `maw ws list` prose carries the same
/// `[lifecycle:<slug>]` token. Agents that read `ws list` to choose a
/// workspace and then act get the canonical vocabulary in the line
/// they're already reading.
#[test]
fn ws_list_text_includes_named_lifecycle_slug() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "alice feature\n");
    let ws_path = repo.workspace_path("alice");
    manifold_common::git_ok(&ws_path, &["add", "-A"]);
    manifold_common::git_ok(&ws_path, &["commit", "-m", "feat: bn-242l alice for list text"]);

    let out = repo.maw_ok(&["ws", "list"]);
    assert!(
        out.contains("[lifecycle:committed-unintegrated]"),
        "ws list prose must carry `[lifecycle:committed-unintegrated]` for alice with work, got:\n{out}"
    );
}

// ----------------------------------------------------------------------
// Backward compat: legacy fields stay present
// ----------------------------------------------------------------------

/// SG4 / bn-242l — legacy JSON envelope fields (`current_workspace`,
/// `workspaces`, `is_stale`) must remain present. Adding the
/// vocabulary fields is purely additive — pre-bn-242l agents and
/// downstream tooling continue to parse the same shape.
#[test]
fn ws_status_json_preserves_legacy_envelope_shape() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);

    let json = ws_status_json(&repo);

    assert!(json.get("current_workspace").is_some());
    assert!(json.get("workspaces").is_some());
    assert!(json.get("is_stale").is_some());
    assert!(json.get("has_changes").is_some());

    let workspaces = json["workspaces"].as_array().expect("workspaces array");
    assert!(workspaces.iter().any(|w| w["name"].as_str() == Some("default")));
    assert!(workspaces.iter().any(|w| w["name"].as_str() == Some("alice")));
}

/// SG4 / bn-242l — same backward-compat guarantee for `ws list`. The
/// per-workspace `name`/`epoch`/`state`/`mode` fields stay in their
/// existing shape; `lifecycle_state` and `fix_command` ride alongside
/// without displacing anything.
#[test]
fn ws_list_json_preserves_legacy_per_workspace_shape() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);

    let json = ws_list_json(&repo);
    let workspaces = json["workspaces"].as_array().expect("workspaces array");
    let alice = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("alice"))
        .expect("alice present");

    assert!(alice.get("name").is_some());
    assert!(alice.get("epoch").is_some());
    assert!(alice.get("state").is_some());
    assert!(alice.get("mode").is_some());
    assert!(alice.get("path").is_some());
}
