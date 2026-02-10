//! Shared test helpers for maw integration tests.
//!
//! All tests use temp directories — no side effects on the real repo.
//! Each test gets its own jj repo via `setup_test_repo()` or `setup_bare_repo()`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Create a fresh jj repo in a temp directory (non-bare, simple mode).
pub fn setup_test_repo() -> TempDir {
    let dir = TempDir::new().expect("failed to create temp dir");

    let out = Command::new("jj")
        .args(["git", "init", "--colocate"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run jj git init");
    assert!(
        out.status.success(),
        "jj git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    dir
}

/// Create a bare maw repo (v2 model) with `maw init`.
///
/// Returns the temp dir. Source files live at `<dir>/ws/default/`.
pub fn setup_bare_repo() -> TempDir {
    let dir = TempDir::new().expect("failed to create temp dir");

    // Initialize jj first (maw init expects a jj repo)
    let out = Command::new("jj")
        .args(["git", "init", "--colocate"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run jj git init");
    assert!(
        out.status.success(),
        "jj git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Create an initial commit so main bookmark exists
    std::fs::write(dir.path().join("README.md"), "# test repo\n").unwrap();
    run_jj(dir.path(), &["commit", "-m", "initial commit"]);
    run_jj(dir.path(), &["bookmark", "create", "main", "-r", "@-"]);

    // Run maw init to set up bare model
    let out = maw_in(dir.path(), &["init"]);
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    dir
}

/// Create a bare maw repo with a local bare git remote for push tests.
///
/// Returns (repo_dir, remote_dir). The remote is configured as "origin".
pub fn setup_with_remote() -> (TempDir, TempDir) {
    let remote_dir = TempDir::new().expect("failed to create remote temp dir");

    // Create bare git remote
    let out = Command::new("git")
        .args(["init", "--bare"])
        .current_dir(remote_dir.path())
        .output()
        .expect("failed to create bare git remote");
    assert!(out.status.success(), "git init --bare failed");

    let repo = TempDir::new().expect("failed to create repo temp dir");

    // Clone the bare remote so we have an origin
    let out = Command::new("git")
        .args(["clone", &remote_dir.path().display().to_string(), "."])
        .current_dir(repo.path())
        .output()
        .expect("failed to clone remote");
    assert!(
        out.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Initialize jj on top of the git clone
    let out = Command::new("jj")
        .args(["git", "init", "--colocate"])
        .current_dir(repo.path())
        .output()
        .expect("failed to run jj git init");
    assert!(
        out.status.success(),
        "jj git init --colocate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Create initial commit and push so main exists on remote
    std::fs::write(repo.path().join("README.md"), "# test repo\n").unwrap();
    run_jj(repo.path(), &["commit", "-m", "initial commit"]);
    run_jj(repo.path(), &["bookmark", "create", "main", "-r", "@-"]);
    run_jj(repo.path(), &["git", "push", "--bookmark", "main"]);

    // Now run maw init for bare model
    let out = maw_in(repo.path(), &["init"]);
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    (repo, remote_dir)
}

/// Run maw with the given args in the given directory.
pub fn maw_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to execute maw")
}

/// Run maw and assert it succeeds. Returns stdout as string.
pub fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = maw_in(dir, args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "maw {} failed:\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" "),
    );
    stdout.to_string()
}

/// Run maw and assert it fails. Returns stderr as string.
pub fn maw_fails(dir: &Path, args: &[&str]) -> String {
    let out = maw_in(dir, args);
    assert!(
        !out.status.success(),
        "Expected maw {} to fail, but it succeeded.\nstdout: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Run a jj command in the given directory. Panics on failure.
pub fn run_jj(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run jj {}: {e}", args.join(" ")));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "jj {} failed:\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" "),
    );
    stdout.to_string()
}

/// Get the absolute path to the default workspace inside a bare repo.
pub fn default_ws(dir: &Path) -> PathBuf {
    dir.join("ws").join("default")
}

/// Write a file inside a workspace.
pub fn write_in_ws(repo: &Path, ws_name: &str, rel_path: &str, content: &str) {
    let ws_path = repo.join("ws").join(ws_name).join(rel_path);
    if let Some(parent) = ws_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&ws_path, content).unwrap();
}

/// Read a file from a workspace. Returns None if it doesn't exist.
pub fn read_from_ws(repo: &Path, ws_name: &str, rel_path: &str) -> Option<String> {
    let ws_path = repo.join("ws").join(ws_name).join(rel_path);
    std::fs::read_to_string(&ws_path).ok()
}
