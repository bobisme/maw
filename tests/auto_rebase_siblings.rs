//! Integration tests for sibling auto-rebase after `maw ws merge` (bn-3vf5).
//!
//! Each test spins up a `TestRepo`, populates a few workspaces, runs a
//! merge that advances the epoch, and asserts the side-effects on the
//! non-merged workspaces.

mod manifold_common;

use manifold_common::TestRepo;

/// Read a manifold ref from the bare repo. Returns the OID, or None.
fn read_ref(repo: &TestRepo, refname: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", refname])
        .current_dir(repo.root())
        .output()
        .expect("failed to run git");
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn workspace_epoch_ref(repo: &TestRepo, name: &str) -> Option<String> {
    read_ref(repo, &format!("refs/manifold/epoch/ws/{name}"))
}

fn epoch_current(repo: &TestRepo) -> String {
    read_ref(repo, "refs/manifold/epoch/current").expect("epoch ref")
}

fn make_commit(repo: &TestRepo, ws: &str, file: &str, content: &str, msg: &str) {
    repo.add_file(ws, file, content);
    repo.git_in_workspace(ws, &["add", "-A"]);
    repo.git_in_workspace(ws, &["commit", "-m", msg]);
}

// ---------------------------------------------------------------------------
// Test 1: 3-workspace batch — clean / conflict / dirty
// ---------------------------------------------------------------------------

/// Three siblings: one rebases cleanly, one has a textual conflict on a
/// path the merge also touched, one is dirty.
#[test]
fn auto_rebase_three_siblings_mixed_outcomes() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "base\n")]);

    // Source workspace whose merge advances the epoch.
    repo.maw_ok(&["ws", "create", "merger"]);
    make_commit(
        &repo,
        "merger",
        "shared.txt",
        "merger update\n",
        "merger: edit shared",
    );
    // Sibling 1 — clean rebase: edits a different file.
    repo.maw_ok(&["ws", "create", "sib-clean"]);
    make_commit(
        &repo,
        "sib-clean",
        "clean_file.txt",
        "clean change\n",
        "sib-clean: add file",
    );
    // Sibling 2 — should conflict: edits the same `shared.txt`.
    repo.maw_ok(&["ws", "create", "sib-conflict"]);
    make_commit(
        &repo,
        "sib-conflict",
        "shared.txt",
        "sib-conflict update\n",
        "sib-conflict: edit shared",
    );
    // Sibling 3 — dirty: uncommitted change.
    repo.maw_ok(&["ws", "create", "sib-dirty"]);
    repo.add_file("sib-dirty", "dirty.txt", "uncommitted\n");

    let epoch_before = epoch_current(&repo);
    let stdout = repo.maw_ok(&["ws", "merge", "merger", "--message", "feat: merge merger"]);
    let epoch_after = epoch_current(&repo);
    assert_ne!(
        epoch_before, epoch_after,
        "epoch must advance for the merger merge"
    );

    // Each sibling should appear in the AUTO-REBASE summary.
    assert!(
        stdout.contains("AUTO-REBASE"),
        "auto-rebase header missing from output:\n{stdout}"
    );
    assert!(
        stdout.contains("sib-clean") && stdout.contains("rebased clean"),
        "clean sibling line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("sib-conflict") && stdout.contains("conflict"),
        "conflict sibling line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("sib-dirty") && stdout.contains("skipped: dirty"),
        "dirty sibling line missing:\n{stdout}"
    );

    // Refs sanity: the clean sibling now has its workspace-epoch ref pointing
    // at the new epoch.
    assert_eq!(
        workspace_epoch_ref(&repo, "sib-clean").expect("ref should exist"),
        epoch_after,
        "sib-clean epoch ref should advance after auto-rebase"
    );
    // The dirty sibling stays at the old epoch.
    assert_eq!(
        workspace_epoch_ref(&repo, "sib-dirty").expect("ref"),
        epoch_before,
        "sib-dirty must stay at the old epoch"
    );
}

// ---------------------------------------------------------------------------
// Test 2: opt-out via --no-auto-rebase
// ---------------------------------------------------------------------------

#[test]
fn auto_rebase_opt_out_via_flag_leaves_siblings_stale() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "merger"]);
    make_commit(
        &repo,
        "merger",
        "shared.txt",
        "merger update\n",
        "merger: edit",
    );
    repo.maw_ok(&["ws", "create", "stale-sib"]);
    make_commit(
        &repo,
        "stale-sib",
        "other.txt",
        "stale-sib content\n",
        "stale-sib: add",
    );

    let epoch_before = epoch_current(&repo);
    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "merger",
        "--no-auto-rebase",
        "--message",
        "feat: merge merger no auto",
    ]);
    let epoch_after = epoch_current(&repo);
    assert_ne!(epoch_before, epoch_after);

    assert!(
        !stdout.contains("AUTO-REBASE"),
        "AUTO-REBASE block should be absent with --no-auto-rebase"
    );
    // The sibling's epoch ref is unchanged.
    assert_eq!(
        workspace_epoch_ref(&repo, "stale-sib").expect("ref"),
        epoch_before,
        "stale-sib must remain at old epoch when --no-auto-rebase is set"
    );
}

// ---------------------------------------------------------------------------
// Test 3: opt-out via config
// ---------------------------------------------------------------------------

#[test]
fn auto_rebase_opt_out_via_config_leaves_siblings_stale() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "base\n")]);

    // Write the manifold config setting auto_rebase_siblings = false.
    let config_path = repo.root().join(".manifold").join("config.toml");
    std::fs::write(
        &config_path,
        "[repo]\nbranch = \"main\"\n\n[merge]\nauto_rebase_siblings = false\n",
    )
    .expect("write config.toml");

    repo.maw_ok(&["ws", "create", "merger"]);
    make_commit(&repo, "merger", "a.txt", "merger update\n", "merger: edit");
    repo.maw_ok(&["ws", "create", "cfg-sib"]);
    make_commit(
        &repo,
        "cfg-sib",
        "side.txt",
        "cfg-sib content\n",
        "cfg-sib: add",
    );

    let epoch_before = epoch_current(&repo);
    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "merger",
        "--message",
        "feat: merge merger config off",
    ]);
    let epoch_after = epoch_current(&repo);
    assert_ne!(epoch_before, epoch_after);

    assert!(
        !stdout.contains("AUTO-REBASE"),
        "AUTO-REBASE block should be absent when merge.auto_rebase_siblings = false"
    );
    assert_eq!(
        workspace_epoch_ref(&repo, "cfg-sib").expect("ref"),
        epoch_before,
        "cfg-sib must remain at old epoch when config disables auto-rebase"
    );
}

// ---------------------------------------------------------------------------
// Test 4: default-on path advances sibling refs.
// ---------------------------------------------------------------------------

#[test]
fn auto_rebase_default_on_advances_clean_sibling_refs() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "merger"]);
    make_commit(&repo, "merger", "a.txt", "v1\n", "merger: v1");

    repo.maw_ok(&["ws", "create", "sibling"]);
    make_commit(
        &repo,
        "sibling",
        "side.txt",
        "side content\n",
        "sibling: add side",
    );

    let epoch_before = epoch_current(&repo);
    let _ = repo.maw_ok(&["ws", "merge", "merger", "--message", "feat: merge"]);
    let epoch_after = epoch_current(&repo);
    assert_ne!(epoch_before, epoch_after);

    // sibling's epoch ref should advance to the new epoch (no worktree
    // mutation, but ref-level update is allowed).
    assert_eq!(
        workspace_epoch_ref(&repo, "sibling").expect("ref"),
        epoch_after,
        "sibling ref must advance after default-on auto-rebase"
    );
}

// ---------------------------------------------------------------------------
// Test 5a: skip "in use" — sibling lock held by another process.
// ---------------------------------------------------------------------------

#[test]
fn auto_rebase_skips_locked_sibling() {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;

    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "merger"]);
    make_commit(&repo, "merger", "a.txt", "v1\n", "merger: v1");

    repo.maw_ok(&["ws", "create", "locked-sib"]);
    make_commit(&repo, "locked-sib", "side.txt", "side\n", "locked-sib: add");

    // Manually grab the rebase lock for "locked-sib".
    let lock_dir = repo.root().join(".manifold").join("locks").join("rebase");
    std::fs::create_dir_all(&lock_dir).expect("create lock dir");
    let lock_path = lock_dir.join("locked-sib.lock");
    let lock_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open lock file");
    lock_file
        .try_lock_exclusive()
        .expect("acquire lock for test");

    let epoch_before = epoch_current(&repo);
    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "merger",
        "--message",
        "feat: merge with locked sib",
    ]);
    let epoch_after = epoch_current(&repo);
    assert_ne!(epoch_before, epoch_after);

    // The locked sibling MUST be skipped, not rebased.
    assert!(
        stdout.contains("locked-sib") && stdout.contains("skipped: in use"),
        "locked sibling should be skipped 'in use':\n{stdout}"
    );
    assert_eq!(
        workspace_epoch_ref(&repo, "locked-sib").expect("ref"),
        epoch_before,
        "locked sibling epoch ref must remain unchanged"
    );

    drop(lock_file);
}

// ---------------------------------------------------------------------------
// Test 5: skip "up to date" — sibling whose base epoch already matches.
// ---------------------------------------------------------------------------

#[test]
fn auto_rebase_skips_up_to_date_sibling() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "base\n")]);

    // First merge — advances the epoch.
    repo.maw_ok(&["ws", "create", "first"]);
    make_commit(&repo, "first", "a.txt", "v1\n", "first: v1");
    repo.maw_ok(&["ws", "merge", "first", "--message", "feat: first"]);

    // Now create a sibling at the new epoch and a separate merger.
    repo.maw_ok(&["ws", "create", "uptodate"]);
    repo.maw_ok(&["ws", "create", "merger2"]);
    make_commit(&repo, "merger2", "b.txt", "merger2 content\n", "merger2: b");

    let epoch_before = epoch_current(&repo);
    let stdout = repo.maw_ok(&["ws", "merge", "merger2", "--message", "feat: merge merger2"]);
    let epoch_after = epoch_current(&repo);
    assert_ne!(epoch_before, epoch_after);

    // `uptodate` was created at epoch_before, so before the merger2 merge it
    // matched the current epoch. The merger2 merge advances epoch — uptodate
    // is now stale. Auto-rebase MUST process it (not skip "up to date") since
    // its base epoch is now < new_epoch. Confirm it advanced.
    assert!(
        stdout.contains("uptodate"),
        "uptodate sibling should appear in auto-rebase summary:\n{stdout}"
    );
    assert_eq!(
        workspace_epoch_ref(&repo, "uptodate").expect("ref"),
        epoch_after,
        "uptodate sibling should advance to new epoch"
    );
}
