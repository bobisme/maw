//! bn-ila3: merge validation / quarantine layout leftovers.
//!
//! 1. VALIDATE hardcoded `<root>/.manifold/validate-tmp`, so every validation
//!    run in a consolidated repo created a stray `.manifold/` at the root.
//! 2. `[merge.validation]` in the user-editable `.maw.toml` was silently
//!    ignored (only the manifold `config.toml` was read).
//! 3. Quarantine workspaces (`merge-quarantine-<id>`) live under
//!    `.maw/workspaces/` and were treated as ordinary siblings by later
//!    merges: sibling auto-rebase and FF-absorb would re-base the candidate
//!    out from under `maw merge promote`.
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
    combined(&out)
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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    maw_ok(dir.path(), &["init"]);
    assert!(dir.path().join(".maw").join("manifold").is_dir());
    dir
}

/// Create workspace `name` with one commit writing `files`.
fn commit_in_ws(root: &Path, name: &str, script: &str) {
    maw_ok(root, &["ws", "create", name, "--from", "main"]);
    maw_ok(
        root,
        &[
            "exec",
            name,
            "--",
            "sh",
            "-c",
            &format!("{script} && git add -A && git commit -qm {name}"),
        ],
    );
}

fn merge(root: &Path, name: &str) -> Output {
    maw(
        root,
        &[
            "ws",
            "merge",
            name,
            "--into",
            "default",
            "--message",
            &format!("feat: {name}"),
        ],
    )
}

/// Items 1 + 2: `[merge.validation]` in `.maw.toml` is honored (it blocks a
/// broken merge) and running validation leaves no root `.manifold/`.
#[test]
fn maw_toml_validation_is_honored_without_stray_root_manifold_dir() {
    let dir = init_consolidated();
    let root = dir.path();
    std::fs::write(
        root.join(".maw.toml"),
        "[merge.validation]\ncommand = \"test ! -f BROKEN\"\non_failure = \"block\"\n",
    )
    .expect("write .maw.toml");

    commit_in_ws(root, "worker", "echo x > BROKEN");
    let out = merge(root, "worker");
    let text = combined(&out);
    assert!(
        !out.status.success() && text.contains("Validation FAILED"),
        ".maw.toml [merge.validation] must run and block the merge:\n{text}"
    );
    assert!(
        !root.join("BROKEN").exists(),
        "blocked merge must not land:\n{text}"
    );
    assert!(
        !root.join(".manifold").exists(),
        "validation created a stray root .manifold/ in a consolidated repo"
    );

    // A clean workspace passes the same validation and merges.
    commit_in_ws(root, "fine", "echo ok > ok.txt");
    let out = merge(root, "fine");
    assert!(out.status.success(), "{}", combined(&out));
    assert!(root.join("ok.txt").is_file());
    assert!(!root.join(".manifold").exists());
}

/// Quarantine a broken merge; returns (merge id, quarantine worktree path).
fn make_quarantine(root: &Path) -> (String, std::path::PathBuf) {
    std::fs::write(
        root.join(".maw").join("manifold").join("config.toml"),
        "[merge.validation]\ncommand = \"test ! -f BROKEN\"\non_failure = \"quarantine\"\n",
    )
    .expect("write manifold config");
    commit_in_ws(root, "worker", "echo hi > hello.txt && echo x > BROKEN");
    let text = combined(&merge(root, "worker"));
    assert!(text.contains("Quarantine workspace created"), "{text}");
    let qdir = root.join(".maw").join("manifold").join("quarantine");
    let id = std::fs::read_dir(&qdir)
        .expect("quarantine dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .expect("one quarantine");
    let qws = root
        .join(".maw")
        .join("workspaces")
        .join(format!("merge-quarantine-{id}"));
    assert!(qws.join("BROKEN").is_file());
    (id, qws)
}

/// Item 3 (auto-rebase): a later unrelated merge must leave the quarantine
/// candidate exactly where it is — HEAD, worktree and (absent) epoch ref —
/// and promote must then refuse cleanly (its base epoch moved) with both
/// refs untouched and the quarantine intact.
#[test]
fn unrelated_merge_leaves_quarantine_candidate_untouched() {
    let dir = init_consolidated();
    let root = dir.path();
    let (id, qws) = make_quarantine(root);
    let qname = format!("merge-quarantine-{id}");
    let head_before = git(&qws, &["rev-parse", "HEAD"]);

    commit_in_ws(root, "other", "echo o > other.txt");
    let out = merge(root, "other");
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains(&format!("{qname} — skipped: merge quarantine")),
        "auto-rebase must skip the quarantine:\n{text}"
    );
    assert_eq!(git(&qws, &["rev-parse", "HEAD"]), head_before);
    assert!(qws.join("BROKEN").is_file() && !qws.join("other.txt").exists());
    assert_eq!(
        git(
            root,
            &["for-each-ref", &format!("refs/manifold/epoch/ws/{qname}")]
        ),
        "",
        "no per-workspace epoch ref may be written for a quarantine"
    );

    // Promote against the moved epoch: refused, nothing moves.
    std::fs::remove_file(qws.join("BROKEN")).expect("fix forward");
    let epoch = git(root, &["rev-parse", "refs/manifold/epoch/current"]);
    let main = git(root, &["rev-parse", "refs/heads/main"]);
    let out = maw(root, &["merge", "promote", &id]);
    let text = combined(&out);
    assert!(
        !out.status.success() && text.contains("moved since this quarantine was created"),
        "{text}"
    );
    assert_eq!(
        git(root, &["rev-parse", "refs/manifold/epoch/current"]),
        epoch
    );
    assert_eq!(git(root, &["rev-parse", "refs/heads/main"]), main);
    assert!(qws.is_dir(), "quarantine must survive a refused promote");
}

/// Item 3 (FF-absorb): a direct trunk commit followed by a merge FF-absorbs
/// the trunk commit. Before the fix `reconcile_epoch_with_branch` REPLAYED
/// the quarantine sibling onto the absorbed tip (its HEAD and epoch ref
/// moved), rewriting the candidate out from under `maw merge promote`.
#[test]
fn ff_absorb_does_not_replay_quarantine_sibling() {
    let dir = init_consolidated();
    let root = dir.path();
    let (id, qws) = make_quarantine(root);
    let qname = format!("merge-quarantine-{id}");
    let head_before = git(&qws, &["rev-parse", "HEAD"]);

    // Direct trunk commit → the next merge FF-absorbs it.
    std::fs::write(root.join("trunk.txt"), "t\n").expect("write");
    git(root, &["add", "trunk.txt"]);
    git(root, &["commit", "-qm", "trunk"]);

    commit_in_ws(root, "other", "echo o > other.txt");
    let out = merge(root, "other");
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("Absorbed"), "expected an FF-absorb:\n{text}");
    assert_eq!(
        git(&qws, &["rev-parse", "HEAD"]),
        head_before,
        "FF-absorb moved the quarantine candidate:\n{text}"
    );
    assert!(!qws.join("trunk.txt").exists());
    assert_eq!(
        git(
            root,
            &["for-each-ref", &format!("refs/manifold/epoch/ws/{qname}")]
        ),
        ""
    );
}
