//! bn-1jfui: FF-absorb must not detach the consolidated root from its branch.
//!
//! Found by the production DST tier's first run on the consolidated layout
//! (seed 4, step 21). In a `maw init` repo the root IS the default workspace
//! and is checked out ON `main`. A `git commit` there moves `main` ahead of the
//! epoch, and the next `maw ws merge` FF-absorbs it. `sync_target_worktree_to_epoch`
//! then wrote HEAD as a DETACHED OID even though `main` already pointed at that
//! commit. A successful merge re-attaches the root later, but a merge that
//! fails AFTER the absorb (here: "Nothing to merge") left the root detached.
//!
//! The user's next `git commit` in the root then landed on the detached HEAD,
//! not on `main`. The next successful merge re-attached the root to `main` and
//! left that commit with NO ref containing it. Its bytes survived only as an
//! untracked file: committed work had been silently turned into loose files.

mod manifold_common;

use manifold_common::{Layout, TestRepo, git_ok};

fn symbolic_head(repo: &TestRepo) -> Option<String> {
    let out = manifold_common::git_raw(repo.root(), &["symbolic-ref", "-q", "HEAD"]);
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
fn failed_merge_after_ff_absorb_keeps_root_on_main_and_user_commit_on_a_ref() {
    let repo = TestRepo::with_layout(Layout::Consolidated);
    repo.seed_files(&[("base.txt", "base\n")]);
    assert_eq!(symbolic_head(&repo).as_deref(), Some("refs/heads/main"));

    // A workspace with nothing to merge, then a plain trunk commit.
    repo.maw_ok(&["ws", "create", "idle", "--from", "main"]);
    std::fs::write(repo.root().join("trunk.txt"), "trunk commit\n").unwrap();
    git_ok(repo.root(), &["add", "trunk.txt"]);
    git_ok(repo.root(), &["commit", "-m", "direct trunk commit"]);

    // The merge FF-absorbs the trunk commit, then fails: nothing to merge.
    let out = repo.maw_raw(&["ws", "merge", "idle", "--message", "m"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "merge of an idle ws must fail:\n{text}"
    );
    assert_eq!(
        repo.current_epoch(),
        git_ok(repo.root(), &["rev-parse", "main"]).trim(),
        "NON-VACUITY: the failed merge must still have FF-absorbed the trunk commit:\n{text}"
    );
    assert_eq!(
        symbolic_head(&repo).as_deref(),
        Some("refs/heads/main"),
        "FF-absorb detached the consolidated root from main:\n{text}"
    );

    // The user keeps working on trunk; a later merge must keep that commit.
    std::fs::write(repo.root().join("user.txt"), "user trunk work\n").unwrap();
    git_ok(repo.root(), &["add", "user.txt"]);
    git_ok(repo.root(), &["commit", "-m", "user trunk work"]);
    let user_commit = git_ok(repo.root(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    repo.maw_ok(&["ws", "create", "worker", "--from", "main"]);
    repo.add_file("worker", "worker.txt", "worker\n");
    repo.maw_ok(&["exec", "worker", "--", "git", "add", "-A"]);
    repo.maw_ok(&["exec", "worker", "--", "git", "commit", "-m", "worker"]);
    repo.maw_ok(&["ws", "merge", "worker", "--message", "merge worker"]);

    let merge_base = manifold_common::git_raw(
        repo.root(),
        &[
            "merge-base",
            "--is-ancestor",
            &user_commit,
            "refs/heads/main",
        ],
    );
    assert!(
        merge_base.status.success(),
        "the user's trunk commit {user_commit} is no longer on main after the next merge \
         (orphaned: only reachable via reflog)"
    );
    assert_eq!(symbolic_head(&repo).as_deref(), Some("refs/heads/main"));
    let status = git_ok(repo.root(), &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "committed work must not come back as loose files:\n{status}"
    );
}
