//! bn-1s8d + bn-21qy: consolidated-layout default-workspace handling.
//!
//! bn-1s8d: `maw ws clean` resolves the default workspace path
//! layout-aware in consolidated repos (root, not backend worktrees-dir).
//!
//! bn-21qy: `maw ws create` refuses the reserved name 'default'; an
//! existing impostor can be cleaned up via `maw ws destroy default`.

mod manifold_common;

use manifold_common::maw_bin;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run the `maw` binary in `dir`, return (stdout, stderr, success).
fn maw_raw(dir: &Path, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(maw_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute maw {}: {e}", args.join(" ")));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let (stdout, stderr, ok) = maw_raw(dir, args);
    assert!(
        ok,
        "maw {} failed:\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" ")
    );
    stdout
}

fn maw_fails(dir: &Path, args: &[&str]) -> String {
    let (stdout, stderr, ok) = maw_raw(dir, args);
    assert!(
        !ok,
        "maw {} succeeded unexpectedly:\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" ")
    );
    stderr
}

/// Greenfield-init a fresh consolidated `.maw/` repo.
fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();

    let out = Command::new(maw_bin())
        .args(["init"])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .expect("failed to execute maw init");
    assert!(
        out.status.success(),
        "maw init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        root.join(".maw").join("manifold").is_dir(),
        "expected consolidated layout after init"
    );
    dir
}

// ---------------------------------------------------------------------------
// bn-1s8d: ws clean is layout-aware in consolidated repos
// ---------------------------------------------------------------------------

/// `maw ws clean` (no name = default) in a consolidated repo succeeds and
/// cleans the root `target/` directory. Does NOT error "Workspace 'default'
/// does not exist" and does NOT suggest "maw ws create 'default'".
#[test]
fn ws_clean_default_consolidated_cleans_root_target() {
    let dir = init_consolidated();
    let root = dir.path();

    // Fabricate a target/ directory at the repo root (the default workspace
    // in consolidated layout IS the repo root).
    let target_dir = root.join("target");
    std::fs::create_dir_all(target_dir.join("debug")).expect("create target/debug");
    std::fs::write(target_dir.join("debug").join("dummy"), b"artifact")
        .expect("write dummy artifact");
    assert!(target_dir.exists(), "target/ must exist before clean");

    let stdout = maw_ok(root, &["ws", "clean"]);

    // target/ should be gone
    assert!(
        !target_dir.exists(),
        "target/ should have been removed by ws clean\nstdout: {stdout}"
    );
    // Output should mention cleaned, not error
    assert!(
        stdout.contains("Cleaned") || stdout.contains("cleaned"),
        "expected 'Cleaned' in output, got: {stdout}"
    );
}

/// `maw ws clean` without a target/ in consolidated layout says "No target/"
/// instead of erroring about a missing workspace.
#[test]
fn ws_clean_default_consolidated_no_target_ok() {
    let dir = init_consolidated();
    let root = dir.path();

    // Ensure target/ does NOT exist
    let target_dir = root.join("target");
    if target_dir.exists() {
        std::fs::remove_dir_all(&target_dir).ok();
    }

    let stdout = maw_ok(root, &["ws", "clean"]);
    assert!(
        stdout.contains("No target/") || stdout.contains("no target"),
        "expected 'No target/' message, got: {stdout}"
    );
}

/// `maw ws clean --all` in consolidated layout includes the root target/.
#[test]
fn ws_clean_all_consolidated_includes_root() {
    let dir = init_consolidated();
    let root = dir.path();

    // Create a root target/ and a workspace target/
    let root_target = root.join("target");
    std::fs::create_dir_all(&root_target).expect("create root target/");
    std::fs::write(root_target.join("artifact"), b"data").expect("write artifact");

    // Create an agent workspace with its own target/
    maw_ok(root, &["ws", "create", "agent-a", "--from", "main"]);
    let ws_target = root
        .join(".maw")
        .join("workspaces")
        .join("agent-a")
        .join("target");
    std::fs::create_dir_all(&ws_target).expect("create ws target/");
    std::fs::write(ws_target.join("artifact"), b"data").expect("write ws artifact");

    let stdout = maw_ok(root, &["ws", "clean", "--all"]);

    assert!(
        !root_target.exists(),
        "root target/ should be cleaned\nstdout: {stdout}"
    );
    assert!(
        !ws_target.exists(),
        "agent-a target/ should be cleaned\nstdout: {stdout}"
    );
    assert!(
        stdout.contains('2') || stdout.contains("cleaned"),
        "expected cleaned count in output, got: {stdout}"
    );
}

/// The error from `maw ws clean` for a NAMED missing workspace should NOT
/// suggest "maw ws create" (confusing and harmful hint removed in bn-1s8d).
#[test]
fn ws_clean_missing_named_workspace_no_create_hint() {
    let dir = init_consolidated();
    let root = dir.path();

    let stderr = maw_fails(root, &["ws", "clean", "ghost-ws"]);
    // Must not suggest creating the workspace
    assert!(
        !stderr.contains("maw ws create 'ghost-ws'"),
        "clean error must not suggest 'maw ws create' for non-default name, got: {stderr}"
    );
    // Must not suggest "maw ws create 'default'" either
    assert!(
        !stderr.contains("maw ws create 'default'"),
        "clean error must not suggest 'maw ws create default', got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-21qy: 'default' is reserved — create refuses it
// ---------------------------------------------------------------------------

/// `maw ws create default --from main` is refused in consolidated layout.
#[test]
fn ws_create_default_refused_with_from_in_consolidated() {
    let dir = init_consolidated();
    let root = dir.path();

    let stderr = maw_fails(root, &["ws", "create", "default", "--from", "main"]);
    assert!(
        stderr.contains("reserved") || stderr.contains("'default' is reserved"),
        "expected reserved-name error, got: {stderr}"
    );
    // Must not have created the directory
    assert!(
        !root
            .join(".maw")
            .join("workspaces")
            .join("default")
            .exists(),
        ".maw/workspaces/default must not be created"
    );
}

/// `maw ws create default` (bare, no --from) is also refused — must say
/// 'reserved' rather than the create-requires-source error (which would give
/// a harmful hint suggesting `maw ws create --from main default`).
#[test]
fn ws_create_default_refused_bare_in_consolidated() {
    let dir = init_consolidated();
    let root = dir.path();

    let stderr = maw_fails(root, &["ws", "create", "default"]);
    assert!(
        stderr.contains("reserved") || stderr.contains("'default' is reserved"),
        "expected reserved-name error even without --from, got: {stderr}"
    );
}

/// `maw ws create default --from main` is refused in v2 layout too.
#[test]
fn ws_create_default_refused_in_v2() {
    // TestRepo::new() creates a v2 (bare) repo.
    let repo = manifold_common::TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", "default", "--from", "main"]);
    assert!(
        stderr.contains("reserved") || stderr.contains("'default' is reserved"),
        "expected reserved-name error in v2 layout, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-21qy: impostor zombie cleanup via ws destroy default
// ---------------------------------------------------------------------------

/// If .maw/workspaces/default exists (pre-fix zombie), `maw ws destroy default`
/// in consolidated layout removes it and prints the impostor message.
#[test]
fn ws_destroy_default_removes_impostor_in_consolidated() {
    let dir = init_consolidated();
    let root = dir.path();

    // Simulate the zombie: create the impostor directory + git worktree.
    let impostor_path = root.join(".maw").join("workspaces").join("default");
    std::fs::create_dir_all(&impostor_path).expect("create impostor dir");

    // Register it as a git worktree so that git worktree remove works.
    let out = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&impostor_path)
        .arg("HEAD")
        .current_dir(root)
        .output()
        .expect("git worktree add");
    // If git worktree add fails (e.g. no HEAD commit yet), we still proceed —
    // the destroy fallback does fs::remove_dir_all + prune.
    if !out.status.success() {
        // Just ensure the directory exists for the fs-removal path.
        std::fs::create_dir_all(&impostor_path).ok();
    }

    let (stdout, stderr, ok) = maw_raw(root, &["ws", "destroy", "default"]);
    assert!(
        ok,
        "ws destroy default should succeed for impostor\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Directory must be gone
    assert!(
        !impostor_path.exists(),
        ".maw/workspaces/default should be removed\nstdout: {stdout}"
    );

    // Output must mention impostor (not "Cannot destroy the default workspace")
    assert!(
        stdout.contains("impostor") || stdout.contains("Removed impostor"),
        "expected impostor message in output, got: {stdout}"
    );

    // Repo root (the REAL default) must be untouched
    assert!(
        root.join(".maw").is_dir(),
        "repo root must still exist after impostor destroy"
    );
    assert!(
        root.join(".git").is_dir(),
        ".git must still exist after impostor destroy"
    );
}

/// `maw ws destroy default` in consolidated layout with NO impostor says
/// "Cannot destroy the default workspace" (the real one is protected).
#[test]
fn ws_destroy_default_refuses_when_no_impostor_consolidated() {
    let dir = init_consolidated();
    let root = dir.path();

    // No impostor exists — the normal refusal should fire.
    let stderr = maw_fails(root, &["ws", "destroy", "default"]);
    assert!(
        stderr.contains("Cannot destroy the default workspace"),
        "expected refusal, got: {stderr}"
    );
    // Repo root must be untouched
    assert!(root.join(".git").is_dir(), ".git must still exist");
}

/// `maw ws destroy default` in v2 layout (where no impostor scenario applies)
/// still refuses with the standard message.
#[test]
fn ws_destroy_default_refuses_in_v2() {
    let repo = manifold_common::TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "destroy", "default"]);
    assert!(
        stderr.contains("Cannot destroy the default workspace"),
        "expected refusal in v2 layout, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-21qy: v2 init path — internal default workspace creation still works
// ---------------------------------------------------------------------------

/// `maw init --legacy-ws` on a bare directory produces a v2 layout with
/// ws/default/ correctly created internally (not blocked by the new guard).
#[test]
fn maw_init_legacy_ws_creates_default_workspace() {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();

    let out = Command::new(maw_bin())
        .args(["init", "--legacy-ws"])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .expect("failed to execute maw init --legacy-ws");
    assert!(
        out.status.success(),
        "maw init --legacy-ws failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // v2 layout: ws/default/ must exist
    assert!(
        root.join("ws").join("default").is_dir(),
        "ws/default/ must be created by maw init --legacy-ws"
    );
    // Consolidated layout marker must NOT be present
    assert!(
        !root.join(".maw").join("manifold").is_dir(),
        "maw init --legacy-ws must produce v2, not consolidated"
    );
}

// ---------------------------------------------------------------------------
// bn-3k38: `maw status --status-bar` enumerates workspaces layout-awarely
// ---------------------------------------------------------------------------

/// `maw status --status-bar` must count non-default workspaces from the
/// layout's workspaces dir (`.maw/workspaces/` in consolidated), not the
/// hardcoded legacy `ws/`. Pre-fix it read `root.join("ws")`, which does not
/// exist in a consolidated repo, so the status bar reported zero workspaces
/// even with several active (observed in ~/src/wraith after migration).
#[test]
fn status_bar_counts_workspaces_in_consolidated_layout() {
    let dir = init_consolidated();
    let root = dir.path();

    // Two agent workspaces under `.maw/workspaces/`.
    maw_ok(root, &["ws", "create", "alice", "--from", "main"]);
    maw_ok(root, &["ws", "create", "bob", "--from", "main"]);
    assert!(
        root.join(".maw").join("workspaces").join("alice").is_dir(),
        "workspaces must live under .maw/workspaces/ in consolidated layout"
    );

    let bar = maw_ok(root, &["status", "--status-bar"]);

    // The workspace marker glyph (\u{f0645}) is emitted only when the count
    // is > 0; the count itself must read 2. Pre-fix the marker was absent.
    assert!(
        bar.contains('\u{f0645}'),
        "status bar should show the workspace marker when workspaces exist; got: {bar:?}"
    );
    assert!(
        bar.contains('2'),
        "status bar should count the 2 non-default workspaces; got: {bar:?}"
    );
}

// ---------------------------------------------------------------------------
// bn-2jez: `maw ws list` shows the default workspace in consolidated layout
// ---------------------------------------------------------------------------

/// In the consolidated layout the default workspace IS the repo root, which
/// lives outside `.maw/workspaces/` and so is not returned by the backend's
/// worktree enumeration. `maw ws list` must still show `default` (it always
/// exists) — and an otherwise-empty repo must NOT report "No workspaces
/// found". Regression: it previously printed "No workspaces found".
#[test]
fn ws_list_includes_default_in_empty_consolidated_repo() {
    let dir = init_consolidated();
    let root = dir.path();

    let stdout = maw_ok(root, &["ws", "list"]);
    assert!(
        stdout.contains("default"),
        "ws list must show the default workspace in consolidated layout; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("No workspaces found"),
        "consolidated repo with a default workspace must not report 'No workspaces found'; got: {stdout:?}"
    );
}

/// With agent workspaces present, `maw ws list` shows `default` first and then
/// the agents (a single `default`, not duplicated), and the JSON form includes
/// the default entry.
#[test]
fn ws_list_shows_default_then_agents_in_consolidated() {
    let dir = init_consolidated();
    let root = dir.path();

    maw_ok(root, &["ws", "create", "alice", "--from", "main"]);

    let stdout = maw_ok(root, &["ws", "list"]);
    let default_pos = stdout.find("default");
    let alice_pos = stdout.find("alice");
    assert!(
        default_pos.is_some() && alice_pos.is_some(),
        "ws list must show both default and alice; got: {stdout:?}"
    );
    assert!(
        default_pos < alice_pos,
        "default must sort before agent workspaces; got: {stdout:?}"
    );
    assert_eq!(
        stdout.matches("default").count(),
        1,
        "default must appear exactly once (not duplicated); got: {stdout:?}"
    );

    // JSON form includes the default entry too.
    let json = maw_ok(root, &["ws", "list", "--json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&json).expect("ws list --json is valid JSON");
    let names: Vec<&str> = parsed["workspaces"]
        .as_array()
        .expect("workspaces array")
        .iter()
        .filter_map(|w| w["name"].as_str())
        .collect();
    assert!(
        names.contains(&"default"),
        "ws list --json must include the default workspace; got names: {names:?}"
    );
}
