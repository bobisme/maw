//! bn-1cth: merge quarantines must respect the consolidated `.maw/` layout,
//! and quarantine ids must be validated before any filesystem access.
//!
//! Defect 2 (layout): `create_quarantine_workspace` hardcoded
//! `<root>/ws/merge-quarantine-<id>`. In the consolidated layout the repo
//! root IS the default workspace checkout, so the quarantine worktree landed
//! inside the user's trunk checkout as an untracked `ws/` directory (visible
//! in `git status`, one `git add -A` away from being committed as an embedded
//! repo) and was invisible to `maw ws list`.
//!
//! Defect 1 (containment): `maw merge abandon <id>` joined the raw CLI id into
//! `<manifold>/quarantine/<id>` and `remove_dir_all`'d it, so
//! `maw merge abandon ../../../../victim` deleted any directory that held a
//! `state.json` (even an unparseable one).
//!
//! These tests drive the real `maw` binary on a greenfield consolidated repo.

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

fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = maw(dir, args);
    assert!(
        out.status.success(),
        "maw {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Greenfield `maw init` (empty dir) yields the consolidated layout.
fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    maw_ok(dir.path(), &["init"]);
    assert!(
        dir.path().join(".maw").join("manifold").is_dir(),
        "expected consolidated layout after greenfield init"
    );
    dir
}

/// Merge a workspace whose content fails validation, producing a quarantine.
/// Returns the quarantine merge id.
fn make_quarantine(root: &Path) -> String {
    std::fs::write(
        root.join(".maw").join("manifold").join("config.toml"),
        "[merge.validation]\ncommand = \"test ! -f BROKEN\"\non_failure = \"quarantine\"\n",
    )
    .expect("write manifold config");

    maw_ok(root, &["ws", "create", "worker", "--from", "main"]);
    maw_ok(
        root,
        &[
            "exec",
            "worker",
            "--",
            "sh",
            "-c",
            "echo hi > hello.txt && echo x > BROKEN && git add -A && git commit -qm work",
        ],
    );
    let out = maw(
        root,
        &[
            "ws",
            "merge",
            "worker",
            "--into",
            "default",
            "--message",
            "feat: work",
        ],
    );
    let text = combined(&out);
    assert!(
        text.contains("Quarantine workspace created"),
        "expected a quarantine to be created:\n{text}"
    );

    let qdir = root.join(".maw").join("manifold").join("quarantine");
    let mut ids: Vec<String> = std::fs::read_dir(&qdir)
        .unwrap_or_else(|e| panic!("read {}: {e}", qdir.display()))
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), 1, "expected one quarantine, found {ids:?}");
    ids.pop().expect("id")
}

#[test]
fn quarantine_lives_under_maw_workspaces_not_trunk_checkout() {
    let dir = init_consolidated();
    let root = dir.path();
    let merge_id = make_quarantine(root);
    let name = format!("merge-quarantine-{merge_id}");

    let expected = root.join(".maw").join("workspaces").join(&name);
    assert!(
        expected.join("BROKEN").is_file(),
        "quarantine worktree must be at {}",
        expected.display()
    );
    assert!(
        !root.join("ws").exists(),
        "quarantine created a legacy ws/ dir inside the trunk checkout (bn-1cth)"
    );

    // The trunk checkout must stay clean of quarantine artifacts.
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .expect("git status");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        !status.contains("merge-quarantine") && !status.contains("ws/"),
        "trunk checkout shows quarantine files as untracked:\n{status}"
    );

    // `maw merge list` reports the worktree as present at the new location.
    let list = maw_ok(root, &["merge", "list"]);
    assert!(
        list.contains(&expected.display().to_string()) && list.contains("(present)"),
        "merge list must point at the consolidated location:\n{list}"
    );

    // Fix forward in the quarantine and promote: the full lifecycle works.
    std::fs::remove_file(expected.join("BROKEN")).expect("remove BROKEN");
    maw_ok(root, &["merge", "promote", &merge_id]);
    assert!(
        !expected.exists(),
        "promote must clean up the quarantine worktree"
    );
    assert!(
        root.join("hello.txt").is_file() && !root.join("BROKEN").exists(),
        "promoted content must reach the default checkout"
    );
}

#[test]
fn abandon_rejects_traversal_ids_without_touching_fs() {
    let dir = init_consolidated();
    let root = dir.path();

    // A directory outside the quarantine store that holds a state.json.
    // Before the fix, `maw merge abandon ../../../victim` deleted it.
    let victim = root.join("victim");
    std::fs::create_dir(&victim).expect("mkdir victim");
    std::fs::write(victim.join("state.json"), "{}").expect("write state.json");
    std::fs::write(victim.join("data"), "keep").expect("write data");

    for bad in ["../../../victim", "..", "a/b", "ABC", "a--b"] {
        for verb in ["abandon", "promote"] {
            let out = maw(root, &["merge", verb, bad]);
            let text = combined(&out);
            assert!(
                !out.status.success(),
                "maw merge {verb} {bad:?} must fail:\n{text}"
            );
            assert!(
                text.contains("Invalid quarantine id") && text.contains("maw merge list"),
                "maw merge {verb} {bad:?} must give an actionable id error:\n{text}"
            );
        }
    }
    assert!(
        victim.join("data").is_file() && victim.join("state.json").is_file(),
        "a rejected id must not delete anything"
    );
}
