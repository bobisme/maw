//! Shared test helpers for git repository setup (bn-5rdz).
//!
//! This module exists to consolidate the heavily-duplicated
//! `Command::new("git").args(["init"]) … config … add … commit` boilerplate
//! that appeared across dozens of `#[cfg(test)]` blocks in maw crates.
//!
//! It is gated behind the `test-support` feature so downstream crates can
//! opt in via:
//!
//! ```toml
//! [dev-dependencies]
//! maw-git = { path = "../maw-git", features = ["test-support"] }
//! ```
//!
//! # Scope
//!
//! These helpers use the `git` CLI on purpose:
//!
//! 1. Test fixtures should mirror what users actually do (init/commit via
//!    git CLI) — using gix here would couple test setup to the very
//!    implementation being tested.
//! 2. They deliberately disable gpg signing and set a stable identity so
//!    tests are hermetic regardless of the developer's `~/.gitconfig`.
//!
//! These calls are NOT counted as production debt in
//! `docs/git-subprocess-inventory.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Spawn `git <args>` with `current_dir(root)` and panic with a helpful
/// message if the call fails. Test-only.
fn run_git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `git {}`: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "`git {}` failed in {}:\nstdout: {}\nstderr: {}",
        args.join(" "),
        root.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Spawn `git <args>` and return captured stdout (trimmed). Test-only —
/// useful for `rev-parse`, `cat-file`, etc.
///
/// # Panics
/// Panics if the spawn fails or if `git` exits non-zero. Test fixtures
/// should fail loudly when their setup misbehaves.
#[must_use]
pub fn git_capture(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `git {}`: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "`git {}` failed in {}:\nstdout: {}\nstderr: {}",
        args.join(" "),
        root.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Initialise an empty git repo at `root` with deterministic identity and
/// `commit.gpgsign=false`. The directory must already exist.
///
/// Useful when the caller already owns the working directory (e.g. a
/// `tempdir().path().join("subdir")` or a worktree path).
///
/// # Panics
/// Panics if any `git` invocation fails.
pub fn init_test_repo_at(root: &Path) {
    // Always use `--initial-branch=main` so tests are deterministic regardless
    // of the system `init.defaultBranch` config (which varies between hosts
    // and CI runners).
    run_git(root, &["init", "-q", "--initial-branch=main"]);
    run_git(root, &["config", "user.email", "test@test.com"]);
    run_git(root, &["config", "user.name", "Test User"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
}

/// Initialise a brand-new git repo in a fresh `TempDir`.
///
/// - Runs `git init`
/// - Sets `user.name`, `user.email`, and disables `commit.gpgsign`
///
/// Returns `(TempDir, root)` — keep the `TempDir` alive for the duration of
/// the test, and use `root` (which is `tempdir.path().to_path_buf()`) for
/// further git operations.
///
/// # Panics
/// Panics if the `TempDir` cannot be created or any `git` invocation fails.
#[must_use]
pub fn init_test_repo() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create tempdir");
    let root = dir.path().to_path_buf();
    init_test_repo_at(&root);
    (dir, root)
}

/// Like [`init_test_repo`] but additionally seeds an initial commit:
///
/// - Writes `README.md` with `"# Test\n"`.
/// - `git add README.md && git commit -m "initial"`
///
/// Returns `(TempDir, root, HEAD oid)`.
///
/// # Panics
/// Panics if any `git` invocation fails or the seed file cannot be written.
#[must_use]
pub fn init_test_repo_with_commit() -> (TempDir, PathBuf, String) {
    let (dir, root) = init_test_repo();
    std::fs::write(root.join("README.md"), "# Test\n").expect("write README");
    run_git(&root, &["add", "README.md"]);
    run_git(&root, &["commit", "-qm", "initial"]);
    let oid = git_capture(&root, &["rev-parse", "HEAD"]);
    (dir, root, oid)
}

/// Stage every change and commit with `message`. Returns the new HEAD oid.
///
/// # Panics
/// Panics if any of `git add -A`, `git commit`, or `git rev-parse HEAD`
/// fails.
#[must_use]
pub fn commit_all(root: &Path, message: &str) -> String {
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-qm", message]);
    git_capture(root, &["rev-parse", "HEAD"])
}
