//! bn-jfj2: `maw merge abandon` must pin the quarantine before deleting it.
//!
//! Since bn-3rhz, `maw merge promote` builds on the quarantine worktree's HEAD,
//! so agents are expected to COMMIT fixes inside the quarantine. Before this
//! fix, `maw merge abandon` removed the worktree (the only ref to those
//! commits) and any uncommitted edits with no recovery pin — a Prime Invariant
//! violation. Every other workspace-removal path pins under
//! `refs/manifold/recovery/<ws>/` and writes a destroy record, so
//! `maw ws recover <ws>` can find it.
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

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = maw(dir, args);
    assert!(
        out.status.success(),
        "maw {} failed:\n{}",
        args.join(" "),
        combined(&out)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    maw_ok(dir.path(), &["init"]);
    dir
}

/// Merge a workspace whose content fails validation, producing a quarantine.
fn make_quarantine(root: &Path) -> String {
    make_quarantine_with(root, "test ! -f BROKEN")
}

fn make_quarantine_with(root: &Path, command: &str) -> String {
    std::fs::write(
        root.join(".maw").join("manifold").join("config.toml"),
        format!("[merge.validation]\ncommand = \"{command}\"\non_failure = \"quarantine\"\n"),
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
        "expected a quarantine:\n{text}"
    );
    let qdir = root.join(".maw").join("manifold").join("quarantine");
    let mut ids: Vec<String> = std::fs::read_dir(&qdir)
        .expect("read quarantine dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), 1, "expected one quarantine, found {ids:?}");
    ids.pop().expect("id")
}

#[test]
fn abandon_pins_committed_fix_and_uncommitted_edit_for_recover() {
    let dir = init_consolidated();
    let root = dir.path();
    let merge_id = make_quarantine(root);
    let name = format!("merge-quarantine-{merge_id}");
    let qws = root.join(".maw").join("workspaces").join(&name);
    assert!(qws.is_dir(), "quarantine worktree at {}", qws.display());

    // A fix COMMITTED inside the quarantine (the bn-3rhz workflow)...
    std::fs::write(qws.join("fix.txt"), "committed fix\n").expect("write fix");
    git(&qws, &["add", "fix.txt"]);
    git(&qws, &["commit", "-qm", "fix inside quarantine"]);
    let fix_commit = git(&qws, &["rev-parse", "HEAD"]).trim().to_owned();
    // ...plus an uncommitted edit on top.
    std::fs::write(qws.join("wip.txt"), "uncommitted edit\n").expect("write wip");
    std::fs::write(qws.join("hello.txt"), "hi edited\n").expect("edit hello");

    let out = maw_ok(root, &["merge", "abandon", &merge_id]);
    assert!(!qws.exists(), "abandon must remove the quarantine worktree");
    assert!(
        out.contains(&format!("maw ws recover {name}")),
        "abandon must print the recovery command:\n{out}"
    );

    // The fix commit must still be reachable from a recovery ref.
    let refs = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/manifold/recovery/{name}/"),
        ],
    );
    let pin = refs
        .lines()
        .next()
        .unwrap_or_else(|| panic!("abandon left no refs/manifold/recovery/{name}/ pin (bn-jfj2)"));
    let anc = Command::new("git")
        .args(["merge-base", "--is-ancestor", &fix_commit, pin])
        .current_dir(root)
        .status()
        .expect("git merge-base");
    assert!(
        anc.success(),
        "fix commit {fix_commit} not reachable from {pin}"
    );

    // `maw ws recover` finds it and shows both the committed and uncommitted bytes.
    let list = maw_ok(root, &["ws", "recover"]);
    assert!(
        list.contains(&name),
        "recover list must include {name}:\n{list}"
    );
    assert_eq!(
        maw_ok(root, &["ws", "recover", &name, "--show", "fix.txt"]),
        "committed fix\n"
    );
    assert_eq!(
        maw_ok(root, &["ws", "recover", &name, "--show", "wip.txt"]),
        "uncommitted edit\n"
    );
    assert_eq!(
        maw_ok(root, &["ws", "recover", &name, "--show", "hello.txt"]),
        "hi edited\n"
    );

    // --to restores both into a fresh workspace.
    maw_ok(root, &["ws", "recover", &name, "--to", "restored"]);
    let restored = root.join(".maw").join("workspaces").join("restored");
    assert_eq!(
        std::fs::read_to_string(restored.join("fix.txt")).expect("restored fix.txt"),
        "committed fix\n"
    );
    assert_eq!(
        std::fs::read_to_string(restored.join("wip.txt")).expect("restored wip.txt"),
        "uncommitted edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(restored.join("hello.txt")).expect("restored hello.txt"),
        "hi edited\n"
    );
}

#[test]
fn abandon_of_untouched_quarantine_still_pins_candidate() {
    let dir = init_consolidated();
    let root = dir.path();
    let merge_id = make_quarantine(root);
    let name = format!("merge-quarantine-{merge_id}");

    maw_ok(root, &["merge", "abandon", &merge_id]);
    // The candidate commit (the merged result) stays recoverable.
    assert_eq!(
        maw_ok(root, &["ws", "recover", &name, "--show", "hello.txt"]),
        "hi\n"
    );
    // Idempotent: a second abandon reports already-abandoned.
    let again = maw_ok(root, &["merge", "abandon", &merge_id]);
    assert!(again.contains("already abandoned"), "{again}");
}

#[test]
fn promote_cleanup_pins_edits_made_after_promote_committed() {
    // Validation runs in the quarantine worktree AFTER promote committed its
    // edits; anything written then (here by the validation command itself) is
    // an uncommitted edit that promote's cleanup used to delete unpinned.
    let dir = init_consolidated();
    let root = dir.path();
    let merge_id = make_quarantine_with(root, "test ! -f BROKEN && echo late > late.txt");
    let name = format!("merge-quarantine-{merge_id}");
    let qws = root.join(".maw").join("workspaces").join(&name);
    std::fs::remove_file(qws.join("BROKEN")).expect("rm BROKEN");
    std::fs::write(qws.join("fix.txt"), "fix\n").expect("write fix");
    maw_ok(root, &["merge", "promote", &merge_id]);
    assert!(!qws.exists(), "promote must remove the quarantine worktree");
    assert_eq!(
        std::fs::read_to_string(root.join("fix.txt")).expect("fix.txt on trunk"),
        "fix\n"
    );
    assert!(!root.join("late.txt").exists(), "late edit is not promoted");
    assert_eq!(
        maw_ok(root, &["ws", "recover", &name, "--show", "late.txt"]),
        "late\n",
        "an edit made after promote committed must be pinned before cleanup"
    );
}
