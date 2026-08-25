//! Regression for the post-tag auto-rebase materialization verifier.
//!
//! The verifier can snapshot and rewrite a sibling worktree. It must remain
//! inside that sibling's rebase-lock critical section. Otherwise another maw
//! process can acquire the lock after the rebase, change the workspace, and
//! have those legitimate changes mistaken for materialization corruption.

#![cfg(feature = "failpoints")]

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt as _;

const MAW: &str = env!("CARGO_BIN_EXE_maw");
const VERIFY_HOLD_MS: u64 = 2_500;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn maw(root: &Path, args: &[&str]) -> Output {
    Command::new(MAW)
        .args(args)
        .current_dir(root)
        .output()
        .expect("run maw")
}

fn maw_ok(root: &Path, args: &[&str]) {
    let output = maw(root, args);
    assert!(
        output.status.success(),
        "maw {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn workspace_path(root: &Path, name: &str) -> PathBuf {
    let consolidated = root.join(".maw/workspaces").join(name);
    if consolidated.exists() {
        consolidated
    } else {
        root.join("ws").join(name)
    }
}

fn manifold_dir(root: &Path) -> PathBuf {
    let consolidated = root.join(".maw/manifold");
    if consolidated.exists() {
        consolidated
    } else {
        root.join(".manifold")
    }
}

fn make_commit(root: &Path, ws: &str, file: &str, content: &str, msg: &str) {
    let ws_path = workspace_path(root, ws);
    std::fs::write(ws_path.join(file), content).expect("write workspace file");
    git(&ws_path, &["add", "-A"]);
    git(&ws_path, &["commit", "-m", msg]);
}

fn setup_repo() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("base.txt"), "base\n").expect("write base");
    git(&root, &["add", "base.txt"]);
    git(&root, &["commit", "-m", "chore: seed"]);
    maw_ok(&root, &["init"]);
    (temp, root)
}

#[test]
fn auto_rebase_holds_workspace_lock_through_materialization_verify() {
    let (_temp, root) = setup_repo();

    maw_ok(&root, &["ws", "create", "merger", "--from", "main"]);
    make_commit(
        &root,
        "merger",
        "epoch.txt",
        "epoch change\n",
        "merger: advance epoch",
    );

    maw_ok(&root, &["ws", "create", "sibling", "--from", "main"]);
    make_commit(
        &root,
        "sibling",
        "sibling.txt",
        "sibling work\n",
        "sibling: add work",
    );
    let sibling_path = workspace_path(&root, "sibling");
    let sibling_head_before = git(&sibling_path, &["rev-parse", "HEAD"]);

    let mut merge = Command::new(MAW)
        .args([
            "ws",
            "merge",
            "merger",
            "--into",
            "default",
            "--message",
            "feat: advance epoch",
        ])
        .current_dir(&root)
        .env(
            "MAW_FP",
            format!("FP_AUTO_REBASE_BEFORE_VERIFY=sleep:{VERIFY_HOLD_MS}"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn merge");

    // HEAD changes before the verifier starts. Once it moves, the failpoint
    // keeps the process inside the verifier boundary long enough to inspect
    // the cross-process flock deterministically.
    let deadline = Instant::now() + Duration::from_secs(10);
    while git(&sibling_path, &["rev-parse", "HEAD"]) == sibling_head_before {
        if let Some(status) = merge.try_wait().expect("poll merge") {
            let output = merge.wait_with_output().expect("collect failed merge");
            panic!(
                "merge exited before auto-rebase verification: {status}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        assert!(
            Instant::now() < deadline,
            "sibling HEAD did not advance before the verifier deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let lock_path = manifold_dir(&root).join("locks/rebase/sibling.lock");
    let contender = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open sibling lock");
    let contender_result = contender.try_lock_exclusive();
    if contender_result.is_ok() {
        fs4::fs_std::FileExt::unlock(&contender).expect("release unexpected lock acquisition");
    }

    let output = merge.wait_with_output().expect("wait for merge");
    assert!(
        output.status.success(),
        "merge failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        contender_result
            .expect_err("verifier must still hold the sibling lock")
            .kind(),
        std::io::ErrorKind::WouldBlock,
    );
}
