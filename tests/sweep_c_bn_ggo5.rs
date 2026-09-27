//! bn-ggo5 (pre-release sweep C): merge-quarantine names vs user workspaces.
//!
//! bn-2dyz / bn-1cth made every `merge-quarantine-<id>` workspace name mean
//! "a merge quarantine": `ws sync` / `ws merge` refuse it and point at
//! `maw merge promote|abandon <id>`, and `quarantine_workspace_path` now
//! resolves `<id>` to `.maw/workspaces/merge-quarantine-<id>` in the
//! consolidated layout.
//!
//! Defect A: `maw merge abandon <id>` deleted that directory even when no
//! quarantine was recorded for `<id>` — so on a user workspace that merely
//! has the prefix (created by an older maw, or by `maw ws create`), following
//! maw's own advice deleted the workspace and its uncommitted work with no
//! recovery snapshot.
//!
//! Defect B: `maw ws create merge-quarantine-foo` was accepted, producing a
//! workspace that `ws merge` / `ws sync` then refuse forever.

mod manifold_common;

use manifold_common::maw_bin;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn maw(dir: &Path, args: &[&str]) -> Output {
    Command::new(maw_bin())
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .env_remove("MAW_LAYOUT")
        .output()
        .expect("failed to execute maw")
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let out = maw(dir.path(), &["init"]);
    assert!(out.status.success(), "maw init: {}", combined(&out));
    assert!(dir.path().join(".maw").join("manifold").is_dir());
    dir
}

#[test]
fn abandon_without_recorded_quarantine_never_deletes_a_workspace() {
    let dir = init_consolidated();
    let root = dir.path();

    // A worktree that only *looks* like a quarantine: no quarantine state
    // was ever recorded for id `foo` (a user workspace named with the
    // prefix by an older maw).
    let ws = root
        .join(".maw")
        .join("workspaces")
        .join("merge-quarantine-foo");
    let add = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&ws)
        .current_dir(root)
        .output()
        .expect("git worktree add");
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    std::fs::write(ws.join("precious.txt"), "uncommitted work\n").expect("write");

    let out = maw(root, &["merge", "abandon", "foo"]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "abandon of an unrecorded quarantine id must refuse:\n{text}"
    );
    assert!(
        text.contains("maw ws destroy merge-quarantine-foo"),
        "refusal must point at the snapshotting destroy path:\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("precious.txt"))
            .ok()
            .as_deref(),
        Some("uncommitted work\n"),
        "abandon deleted a workspace that is not a recorded quarantine"
    );

    // An id with neither state nor workspace is still a harmless no-op.
    let out = maw(root, &["merge", "abandon", "bar"]);
    assert!(out.status.success(), "{}", combined(&out));
}

#[test]
fn create_refuses_merge_quarantine_prefix() {
    let dir = init_consolidated();
    let root = dir.path();
    let out = maw(
        root,
        &["ws", "create", "--from", "main", "merge-quarantine-foo"],
    );
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "creating a merge-quarantine-* workspace must be refused:\n{text}"
    );
    assert!(text.contains("reserved"), "{text}");
    assert!(
        !root
            .join(".maw")
            .join("workspaces")
            .join("merge-quarantine-foo")
            .exists()
    );
}
