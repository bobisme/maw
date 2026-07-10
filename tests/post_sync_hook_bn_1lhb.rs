//! Real-subprocess coverage for the `post_sync` hook (bn-1lhb).
//!
//! These drive the *built* `maw` binary through real sync/auto-rebase replays
//! with a configured `[hooks] post_sync` command, asserting the jj-model
//! contract: a hook failure NEVER changes the sync/merge exit code — it is
//! persisted per workspace and surfaced in `ws list` / `ws status` (the
//! `hook:FAIL` marker) and the triggering merge summary.
//!
//! The binary is located via `manifold_common::maw_bin()`; run
//! `cargo build -p maw-cli` first (or use `just test`, which does).

mod manifold_common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use manifold_common::maw_bin;

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn maw(dir: &Path, args: &[&str]) -> Output {
    Command::new(maw_bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run maw")
}

fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = maw(dir, args);
    assert!(
        out.status.success(),
        "maw {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn setup_repo(dir: &Path) -> PathBuf {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("base.txt"), "base\n").expect("write");
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);
    let out = maw(dir, &["init"]);
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.to_path_buf()
}

/// Write `.maw.toml` with a `[hooks] post_sync` block.
fn write_hook_config(root: &Path, commands: &[&str], timeout_seconds: Option<u64>) {
    let cmds = commands
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let timeout_line =
        timeout_seconds.map_or_else(String::new, |t| format!("hook_timeout_seconds = {t}\n"));
    let toml = format!("[hooks]\npost_sync = [{cmds}]\n{timeout_line}");
    std::fs::write(root.join(".maw.toml"), toml).expect("write .maw.toml");
}

/// Create workspace `name` and commit `content` into `file` inside it, leaving
/// the workspace with one commit ahead of its base epoch.
fn create_ws_with_commit(root: &Path, name: &str, file: &str, content: &str) {
    maw_ok(root, &["ws", "create", name, "--from", "main"]);
    let script =
        format!("printf '{content}\\n' > {file} && git add -A && git commit -m '{name} work'");
    let out = maw(root, &["exec", name, "--", "sh", "-c", &script]);
    assert!(
        out.status.success(),
        "workspace commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Advance the epoch out-of-band (direct trunk commit + `epoch sync`) so any
/// existing sibling workspace becomes stale WITHOUT being auto-rebased — this
/// isolates the direct `ws sync` replay path from the merge-triggered one.
fn advance_epoch_via_trunk(root: &Path) {
    std::fs::write(root.join("trunk.txt"), "trunk change\n").expect("write");
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-m", "direct trunk commit"]);
    maw_ok(root, &["epoch", "sync"]);
}

fn postsync_json_path(root: &Path, ws: &str) -> PathBuf {
    root.join(".maw")
        .join("manifold")
        .join("artifacts")
        .join("ws")
        .join(ws)
        .join("postsync.json")
}

// ---------------------------------------------------------------------------
// (a) sync --rebase with a passing hook: recorded + visible in ws list JSON.
// ---------------------------------------------------------------------------
#[test]
fn passing_hook_recorded_and_visible_in_list_json() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    write_hook_config(&root, &["touch hook_ran.marker"], None);
    advance_epoch_via_trunk(&root);

    // sync replays alice's commit onto the new epoch and runs the hook.
    let out = maw(&root, &["ws", "sync", "alice"]);
    assert!(out.status.success(), "sync should exit 0");

    // Hook ran with cwd = the workspace (the marker landed in alice's worktree).
    assert!(
        root.join(".maw/workspaces/alice/hook_ran.marker").exists(),
        "post_sync hook should have run in the workspace cwd"
    );

    // Result persisted.
    let record =
        std::fs::read_to_string(postsync_json_path(&root, "alice")).expect("postsync.json written");
    assert!(record.contains("\"exit_code\": 0"), "record: {record}");

    // Visible in ws list JSON, not failed.
    let list = maw_ok(&root, &["ws", "list", "--format", "json"]);
    assert!(list.contains("\"post_sync_hook\""), "list json: {list}");
    assert!(list.contains("\"failed\": false"), "list json: {list}");
}

// ---------------------------------------------------------------------------
// (b) failing hook: sync still exits 0, marker in list.
// ---------------------------------------------------------------------------
#[test]
fn failing_hook_does_not_change_exit_and_marks_list() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    write_hook_config(&root, &["exit 3"], None);
    advance_epoch_via_trunk(&root);

    // jj model: the hook fails but the sync still succeeds.
    let out = maw(&root, &["ws", "sync", "alice"]);
    assert!(
        out.status.success(),
        "sync must exit 0 even when the post_sync hook fails (signal only)"
    );

    let record =
        std::fs::read_to_string(postsync_json_path(&root, "alice")).expect("postsync.json written");
    assert!(record.contains("\"exit_code\": 3"), "record: {record}");

    // Text list carries the hook:FAIL marker.
    let list_text = maw_ok(&root, &["ws", "list"]);
    assert!(list_text.contains("hook:FAIL"), "list text: {list_text}");

    // JSON list carries failed=true with the exit code.
    let list_json = maw_ok(&root, &["ws", "list", "--format", "json"]);
    assert!(
        list_json.contains("\"failed\": true"),
        "list json: {list_json}"
    );
    assert!(
        list_json.contains("\"exit_code\": 3"),
        "list json: {list_json}"
    );
}

// ---------------------------------------------------------------------------
// (c) merge-triggered auto-rebase runs the hook for each replayed sibling; a
//     failure appears as a NOTE in the triggering merge's summary.
// ---------------------------------------------------------------------------
#[test]
fn merge_auto_rebase_runs_hook_for_sibling_and_notes_failure() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    // Sibling that will be auto-rebased by the merge.
    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    // Source workspace whose merge advances the epoch.
    create_ws_with_commit(&root, "bob", "b.txt", "bob");

    write_hook_config(&root, &["exit 1"], None);

    let out = maw(
        &root,
        &[
            "ws",
            "merge",
            "bob",
            "--into",
            "default",
            "--destroy",
            "--message",
            "feat: bob",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "merge must exit 0 even when a sibling post_sync hook fails: {stdout}\n{stderr}"
    );

    // The sibling alice was replayed and its hook ran + failed.
    assert!(
        postsync_json_path(&root, "alice").exists(),
        "sibling alice's post_sync hook result should be persisted"
    );
    // Merge summary NOTE names the sibling and the failure.
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("post-sync hook failed") && combined.contains("alice"),
        "merge summary should NOTE the sibling hook failure: {combined}"
    );
}

// ---------------------------------------------------------------------------
// (c-json) merge --format json carries per-sibling post_sync_hook entries.
// ---------------------------------------------------------------------------
#[test]
fn merge_json_includes_per_sibling_hook() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    create_ws_with_commit(&root, "bob", "b.txt", "bob");
    write_hook_config(&root, &["exit 1"], None);

    let out = maw(
        &root,
        &[
            "ws",
            "merge",
            "bob",
            "--into",
            "default",
            "--destroy",
            "--message",
            "feat: bob",
            "--format",
            "json",
        ],
    );
    assert!(out.status.success(), "merge --format json should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"post_sync_hooks\""),
        "merge json: {stdout}"
    );
    assert!(
        stdout.contains("\"workspace\": \"alice\""),
        "merge json: {stdout}"
    );
    assert!(stdout.contains("\"exit_code\": 1"), "merge json: {stdout}");
}

// ---------------------------------------------------------------------------
// (d) hook timeout handling: killed, flagged, sync still exits 0.
// ---------------------------------------------------------------------------
#[test]
fn hook_timeout_is_flagged_and_non_fatal() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    write_hook_config(&root, &["sleep 30"], Some(1));
    advance_epoch_via_trunk(&root);

    let out = maw(&root, &["ws", "sync", "alice"]);
    assert!(out.status.success(), "sync must exit 0 on hook timeout");

    let record =
        std::fs::read_to_string(postsync_json_path(&root, "alice")).expect("postsync.json written");
    assert!(record.contains("\"timed_out\": true"), "record: {record}");
}

// ---------------------------------------------------------------------------
// (e) no hook configured = zero behavior change (no postsync.json written).
// ---------------------------------------------------------------------------
#[test]
fn no_hook_configured_is_zero_behavior_change() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    advance_epoch_via_trunk(&root);

    let out = maw(&root, &["ws", "sync", "alice"]);
    assert!(out.status.success(), "sync should exit 0");
    assert!(
        !postsync_json_path(&root, "alice").exists(),
        "no post_sync hook configured — nothing should be persisted"
    );
}

// ---------------------------------------------------------------------------
// (f) an up-to-date sync does not run the hook.
// ---------------------------------------------------------------------------
#[test]
fn up_to_date_sync_does_not_run_hook() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    create_ws_with_commit(&root, "alice", "a.txt", "alice");
    // Configure a hook that would create an observable side effect if it ran.
    write_hook_config(&root, &["touch should_not_run.marker"], None);

    // No epoch advance — alice is up to date, so sync must be a no-op replay.
    let out = maw(&root, &["ws", "sync", "alice"]);
    assert!(out.status.success(), "sync should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("up to date"),
        "expected up-to-date: {stdout}"
    );

    assert!(
        !root
            .join(".maw/workspaces/alice/should_not_run.marker")
            .exists(),
        "up-to-date sync must NOT run the post_sync hook"
    );
    assert!(
        !postsync_json_path(&root, "alice").exists(),
        "up-to-date sync must not persist a hook result"
    );
}
