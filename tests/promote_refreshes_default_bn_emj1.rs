//! Regression test for bn-emj1: `maw merge promote` must refresh the default
//! worktree to the promoted epoch, preserving dirty state.
//!
//! Before the fix, `promote_quarantine` advanced the epoch and branch refs but
//! never touched the default worktree. The root was left at `epoch_before`
//! while the branch/epoch pointed at the promoted commit — and no command
//! refreshed it (`maw ws advance default` refuses, `maw ws sync` skips default).
//!
//! This test drives the full quarantine lifecycle end to end: a validation
//! command fails during merge (quarantine), the failure is fixed in the
//! quarantine worktree, and `maw merge promote` commits it. The assertions
//! prove the default worktree HEAD advanced to the new epoch AND that an
//! uncommitted edit in the default worktree survived the refresh.

mod manifold_common;

use manifold_common::TestRepo;

/// Return the single active quarantine's merge id by reading
/// `.manifold/quarantine/<merge_id>/`.
fn only_quarantine_id(repo: &TestRepo) -> String {
    let dir = repo.root().join(".manifold").join("quarantine");
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "expected exactly one quarantine, found {ids:?}"
    );
    ids.pop().expect("quarantine id present")
}

#[test]
fn promote_refreshes_default_worktree_and_preserves_dirty_state() {
    let repo = TestRepo::new();

    // Validation fails whenever a top-level `BROKEN` file is present. The merged
    // candidate will contain it (quarantine); removing it in the quarantine
    // worktree makes promote's re-validation pass.
    std::fs::write(
        repo.root().join(".manifold").join("config.toml"),
        "[repo]\nbranch = \"main\"\n\n[merge.validation]\ncommand = \"test ! -f BROKEN\"\non_failure = \"quarantine\"\n",
    )
    .expect("write config.toml");

    // Worker change: a real file plus the marker that trips validation.
    repo.create_workspace("worker-1");
    repo.add_file("worker-1", "hello.txt", "hi\n");
    repo.add_file("worker-1", "BROKEN", "x\n");

    // Uncommitted edit in the default worktree — must survive the refresh.
    let default_gitignore = repo.default_workspace().join(".gitignore");
    let original_gitignore =
        std::fs::read_to_string(&default_gitignore).expect("read default .gitignore");
    let dirty_gitignore = format!("{original_gitignore}\n# bn-emj1 local edit\n");
    std::fs::write(&default_gitignore, &dirty_gitignore).expect("dirty the default worktree");

    let epoch0 = repo.current_epoch();

    // Merge: validation fails, so a quarantine is created and the epoch does NOT
    // advance. (on_failure = "quarantine" does not block, so exit code is not
    // asserted here.)
    let _ = repo.maw_raw(&["ws", "merge", "worker-1", "--message", "feat: worker-1"]);
    assert_eq!(
        repo.current_epoch(),
        epoch0,
        "epoch must not advance while validation is failing"
    );

    let merge_id = only_quarantine_id(&repo);

    // Fix-forward: remove the marker in the quarantine worktree.
    let quarantine_broken = repo
        .root()
        .join("ws")
        .join(format!("merge-quarantine-{merge_id}"))
        .join("BROKEN");
    std::fs::remove_file(&quarantine_broken)
        .unwrap_or_else(|e| panic!("remove {}: {e}", quarantine_broken.display()));

    // Promote: re-validation now passes; epoch + branch advance.
    let out = repo.maw_raw_exact(&["merge", "promote", &merge_id]);
    assert!(
        out.status.success(),
        "promote failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let new_epoch = repo.current_epoch();
    assert_ne!(new_epoch, epoch0, "promote must advance the epoch");

    // THE FIX: the default worktree HEAD must track the promoted epoch, not the
    // stale pre-promotion epoch.
    assert_eq!(
        repo.workspace_head("default"),
        new_epoch,
        "default worktree must be refreshed to the promoted epoch (bn-emj1)"
    );

    // The merged content is present in the default worktree.
    assert_eq!(
        repo.read_file("default", "hello.txt").as_deref(),
        Some("hi\n"),
        "promoted merge content must be checked out into default"
    );
    // The fix-forward removal rode into the promoted epoch.
    assert!(
        !repo.file_exists("default", "BROKEN"),
        "the removed marker must not reappear in default"
    );

    // The uncommitted default edit survived the refresh.
    let refreshed_gitignore =
        std::fs::read_to_string(&default_gitignore).expect("read refreshed .gitignore");
    assert!(
        refreshed_gitignore.contains("# bn-emj1 local edit"),
        "dirty default edit must survive the promote refresh; got:\n{refreshed_gitignore}"
    );
}
