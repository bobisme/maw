use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::manifold_common::{TestRepo, maw_bin};

#[derive(Serialize)]
struct FailureBundle {
    harness: String,
    seed: u64,
    replay_command: String,
    minimized_replay_command: Option<String>,
    trace: Vec<String>,
    violations: Vec<String>,
    snapshots: RepoSnapshots,
}

#[derive(Serialize)]
struct RepoSnapshots {
    repo_root: String,
    epoch_ref: Option<String>,
    main_ref: Option<String>,
    git_worktree_list: String,
    git_log_all: String,
    git_status_default: String,
    ws_list_json: String,
    ws_status_json: String,
    ws_recover_json: String,
}

fn artifact_root() -> PathBuf {
    std::env::var_os("DST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("maw-dst-artifacts"))
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_millis()
}

fn git_capture(repo_root: &Path, args: &[&str]) -> String {
    match Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!(
                "$ git {}\nexit={}\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                out.status.code().unwrap_or(-1),
                stdout,
                stderr
            )
        }
        Err(err) => format!("$ git {}\nspawn error: {err}", args.join(" ")),
    }
}

fn maw_capture(repo_root: &Path, args: &[&str]) -> String {
    match Command::new(maw_bin())
        .args(args)
        .current_dir(repo_root)
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!(
                "$ maw {}\nexit={}\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                out.status.code().unwrap_or(-1),
                stdout,
                stderr
            )
        }
        Err(err) => format!("$ maw {}\nspawn error: {err}", args.join(" ")),
    }
}

fn read_ref(repo_root: &Path, ref_name: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--verify", ref_name])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn collect_snapshots(repo: &TestRepo) -> RepoSnapshots {
    let repo_root = repo.root();
    RepoSnapshots {
        repo_root: repo_root.display().to_string(),
        epoch_ref: read_ref(repo_root, "refs/manifold/epoch/current"),
        main_ref: read_ref(repo_root, "refs/heads/main"),
        git_worktree_list: git_capture(repo_root, &["worktree", "list", "--porcelain"]),
        git_log_all: git_capture(
            repo_root,
            &["log", "--oneline", "--decorate", "--all", "-n", "40"],
        ),
        git_status_default: git_capture(
            &repo.default_workspace(),
            &["status", "--porcelain=v1", "--untracked-files=all"],
        ),
        ws_list_json: maw_capture(repo_root, &["ws", "list", "--format", "json"]),
        ws_status_json: maw_capture(repo_root, &["ws", "status", "--format", "json"]),
        ws_recover_json: maw_capture(repo_root, &["ws", "recover", "--format", "json"]),
    }
}

pub fn write_failure_bundle(
    harness: &str,
    seed: u64,
    replay_command: String,
    minimized_replay_command: Option<String>,
    trace_lines: Vec<String>,
    violations: &[String],
    repo: &TestRepo,
) -> PathBuf {
    let dir = artifact_root()
        .join(harness)
        .join(format!("seed-{seed}-{}", timestamp_millis()));
    fs::create_dir_all(&dir).expect("create DST artifact directory");

    let bundle = FailureBundle {
        harness: harness.to_string(),
        seed,
        replay_command,
        minimized_replay_command,
        trace: trace_lines,
        violations: violations.to_vec(),
        snapshots: collect_snapshots(repo),
    };

    let bundle_json = serde_json::to_string_pretty(&bundle).expect("serialize DST failure bundle");
    let path = dir.join("bundle.json");
    fs::write(&path, bundle_json).expect("write DST failure bundle");
    path
}
