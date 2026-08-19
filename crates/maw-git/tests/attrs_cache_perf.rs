//! Perf harness for bn-2fps: `.gitattributes` resolution cost inside
//! `write_blob_with_path`.
//!
//! Before bn-2fps every `write_blob_with_path` call rebuilt an
//! `AttrsMatcher` from scratch: a full recursive walk of the HEAD tree
//! plus (when HEAD carried no `.gitattributes`) a full recursive walk of
//! the working directory. Callers loop over files, so an N-file snapshot
//! paid 2N repo-sized walks.
//!
//! These tests are `#[ignore]`d — they are timing harnesses, not
//! assertions. Run them with:
//!
//! ```text
//! cargo test --release -p maw-git --test attrs_cache_perf -- --ignored --nocapture
//! ```

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use maw_git::{GitRepo, GixRepo};

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "git {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a repo with `dirs * per_dir` tracked files across nested
/// directories, commit them, then dirty every one.
fn build_dirty_repo(
    dirs: usize,
    per_dir: usize,
    gitattributes: bool,
) -> (tempfile::TempDir, GixRepo) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    git(&root, &["init", "-q", "--initial-branch=main"]);
    git(&root, &["config", "user.email", "t@t.com"]);
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "commit.gpgsign", "false"]);

    if gitattributes {
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("write .gitattributes");
    }

    for d in 0..dirs {
        // Nest two levels deep so the tree walk has real depth.
        let sub = root.join(format!("pkg{d}")).join("src");
        std::fs::create_dir_all(&sub).expect("mkdir");
        for f in 0..per_dir {
            std::fs::write(sub.join(format!("file{f}.txt")), format!("v0 {d} {f}\n"))
                .expect("write file");
        }
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "seed"]);

    // Dirty every tracked file.
    for d in 0..dirs {
        let sub = root.join(format!("pkg{d}")).join("src");
        for f in 0..per_dir {
            std::fs::write(sub.join(format!("file{f}.txt")), format!("v1 {d} {f}\n"))
                .expect("dirty file");
        }
    }

    let repo = GixRepo::open(&root).expect("open repo");
    (dir, repo)
}

fn time_snapshot(label: &str, dirs: usize, per_dir: usize, gitattributes: bool) {
    let (_dir, repo) = build_dirty_repo(dirs, per_dir, gitattributes);
    let start = Instant::now();
    let oid = repo
        .worktree_state_commit("bench snapshot")
        .expect("worktree_state_commit");
    let elapsed = start.elapsed();
    assert!(oid.is_some(), "expected a snapshot commit");
    println!(
        "[bn-2fps] {label}: {} files, gitattributes={gitattributes} -> {:?}",
        dirs * per_dir,
        elapsed
    );
}

#[test]
#[ignore = "timing harness (bn-2fps); run with --ignored --nocapture"]
fn perf_snapshot_300_files_no_gitattributes() {
    time_snapshot("300-file snapshot", 20, 15, false);
}

#[test]
#[ignore = "timing harness (bn-2fps); run with --ignored --nocapture"]
fn perf_snapshot_300_files_with_gitattributes() {
    time_snapshot("300-file snapshot", 20, 15, true);
}

#[test]
#[ignore = "timing harness (bn-2fps); run with --ignored --nocapture"]
fn perf_snapshot_2000_files_no_gitattributes() {
    time_snapshot("2000-file snapshot", 50, 40, false);
}

#[test]
#[ignore = "timing harness (bn-2fps); run with --ignored --nocapture"]
fn perf_snapshot_2000_files_with_gitattributes() {
    time_snapshot("2000-file snapshot", 50, 40, true);
}
