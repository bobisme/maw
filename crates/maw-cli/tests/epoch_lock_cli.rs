//! Real-subprocess coverage for the repo-level epoch lock (bn-13rc).
//!
//! These tests drive the *built* `maw` binary (via `CARGO_BIN_EXE_maw`) so they
//! exercise the actual command wiring, contention exit code, and OS-level
//! crash-safety — properties the in-crate unit tests in `epoch_lock.rs` cannot
//! reach. The test process itself holds the lock (through `maw_cli::epoch_lock`)
//! for the light cases, which is a genuine *second* process relative to the
//! spawned `maw` child.
//!
//! Rebuild `maw-cli` before trusting a run: these link the compiled binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use maw_cli::epoch_lock::{self, EPOCH_LOCK_BUSY_EXIT_CODE, EpochLock, WaitPolicy};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

const fn no_wait() -> WaitPolicy {
    WaitPolicy {
        wait: false,
        timeout: Duration::from_secs(0),
    }
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn maw(dir: &Path) -> Command {
    let mut cmd = Command::new(MAW);
    cmd.current_dir(dir);
    cmd
}

/// Initialise a git repo + `maw init` under `dir`, returning the repo root.
fn setup_repo(dir: &Path) -> PathBuf {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hi\n").expect("write readme");
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);

    let out = maw(dir).arg("init").output().expect("maw init");
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.to_path_buf()
}

/// Poll `inspect(root).held` until it matches `want`, or fail after `budget`.
fn wait_for_held(root: &Path, want: bool, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if epoch_lock::inspect(root).held == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "epoch lock did not reach held={want} within {budget:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn concurrent_mutation_fails_no_wait_with_distinct_exit_code() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    // Hold the lock in THIS process; the spawned maw is a real second process.
    let guard =
        EpochLock::acquire_with(&root, "test-holder", no_wait()).expect("test holds the lock");

    let out = maw(&root)
        .args(["epoch", "sync"])
        .env("MAW_LOCK_NO_WAIT", "1")
        .output()
        .expect("run maw epoch sync");

    assert_eq!(
        out.status.code(),
        Some(EPOCH_LOCK_BUSY_EXIT_CODE),
        "no-wait contention must exit with the distinct busy code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("epoch lock is held by another maw process"),
        "self-contained contention message missing: {stderr}"
    );
    assert!(
        stderr.contains("test-holder"),
        "holder metadata (command) must be printed: {stderr}"
    );

    // Release, then the same command succeeds.
    drop(guard);
    let out = maw(&root)
        .args(["epoch", "sync"])
        .output()
        .expect("rerun maw epoch sync");
    assert!(
        out.status.success(),
        "post-release mutation must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn read_only_command_does_not_block_on_the_lock() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    let _guard =
        EpochLock::acquire_with(&root, "test-holder", no_wait()).expect("test holds the lock");

    // `ws list` is read-only — it must not take the epoch lock, so it returns
    // promptly even while a mutation would be blocked.
    let out = maw(&root)
        .args(["ws", "list"])
        .env("MAW_LOCK_NO_WAIT", "1")
        .output()
        .expect("run maw ws list");
    assert!(
        out.status.success(),
        "read-only ws list must succeed while the lock is held: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn waiter_subprocess_blocks_then_succeeds_after_release() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    let guard =
        EpochLock::acquire_with(&root, "test-holder", no_wait()).expect("test holds the lock");

    // Default policy (wait): the child should block, not fail.
    let mut child = maw(&root)
        .args(["epoch", "sync"])
        .env("MAW_LOCK_WAIT_SECS", "10")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn waiting maw");

    std::thread::sleep(Duration::from_millis(400));
    // Still running (blocked on the lock), not exited.
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "waiter should still be blocked while the lock is held"
    );

    drop(guard);
    let status = child.wait().expect("wait child");
    assert!(
        status.success(),
        "waiter must acquire and succeed once the lock is released"
    );
}

#[test]
fn kill9_mid_merge_releases_the_lock() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    // A pre-merge hook that sleeps: `ws merge` acquires the epoch lock, then
    // pauses inside the hook — a real maw process holding the lock, killable
    // mid-merge.
    std::fs::write(
        root.join(".maw.toml"),
        "[hooks]\npre_merge = [\"sleep 30\"]\n",
    )
    .expect("write .maw.toml");
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-m", "cfg"]);

    // A workspace with a committed change so the merge has something to do.
    let out = maw(&root)
        .args(["ws", "create", "feat", "--from", "main"])
        .output()
        .expect("ws create");
    assert!(
        out.status.success(),
        "ws create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ws_path = root.join(".maw").join("workspaces").join("feat");
    std::fs::write(ws_path.join("README.md"), "hi\nchange\n").expect("edit ws file");
    run_git(&ws_path, &["add", "-A"]);
    run_git(&ws_path, &["commit", "-m", "change"]);

    let mut child = maw(&root)
        .args([
            "ws",
            "merge",
            "feat",
            "--into",
            "default",
            "-m",
            "merge feat",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn merge");

    // Wait until the merge is inside the hook holding the lock.
    wait_for_held(&root, true, Duration::from_secs(20));

    // kill -9 the merge mid-flight.
    child.kill().expect("kill merge");
    let _ = child.wait();

    // The kernel releases the flock; the lockfile becomes acquirable again.
    // (The orphaned `sleep` does not hold the epoch.lock fd — std opens are
    // O_CLOEXEC, so it was never inherited.)
    wait_for_held(&root, false, Duration::from_secs(10));
    let regained =
        EpochLock::acquire_with(&root, "post-crash", no_wait()).expect("acquire after kill -9");
    drop(regained);
}
