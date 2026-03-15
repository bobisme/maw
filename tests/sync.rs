//! Integration tests for workspace staleness and sync behavior.

mod manifold_common;

use manifold_common::TestRepo;

fn workspace_state(repo: &TestRepo, name: &str) -> String {
    let status = repo.maw_ok(&["ws", "status", "--format", "json"]);
    let status_json: serde_json::Value =
        serde_json::from_str(&status).expect("ws status --format json should be valid JSON");
    status_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .find(|w| w["name"].as_str() == Some(name))
        .and_then(|w| w["state"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn stale_workspace_detected_and_sync_clears_it() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    assert!(workspace_state(&repo, "alice").contains("stale"));

    let out = repo.maw_ok(&["ws", "sync", "alice"]);
    assert!(
        out.contains("Workspace synced successfully.")
            && out.contains("maw ws sync alice --rebase"),
        "expected post-sync rebase hint, got stdout: {out}"
    );

    assert!(!workspace_state(&repo, "alice").contains("stale"));
}

#[test]
fn exec_auto_syncs_stale_workspace_before_running_command() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let old_head = repo.workspace_head("alice");
    assert_ne!(old_head, repo.current_epoch());

    repo.maw_ok(&["exec", "alice", "--", "git", "rev-parse", "HEAD"]);

    let new_head = repo.workspace_head("alice");
    assert_eq!(new_head, repo.current_epoch());
}

#[test]
fn exec_skips_auto_sync_for_non_git_commands() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let old_head = repo.workspace_head("alice");
    assert_ne!(old_head, repo.current_epoch());

    let out = repo.maw_raw(&["exec", "alice", "--", "cargo", "--version"]);
    assert!(
        out.status.success(),
        "exec should still run non-git command\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let new_head = repo.workspace_head("alice");
    assert_eq!(new_head, old_head);
}

#[test]
fn sync_all_updates_multiple_stale_workspaces() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    repo.maw_ok(&["ws", "sync", "--all"]);

    assert!(!workspace_state(&repo, "alice").contains("stale"));
    assert!(!workspace_state(&repo, "bob").contains("stale"));
}

#[test]
fn sync_refuses_stale_workspace_with_untracked_changes() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    repo.add_file("alice", "scratch.txt", "untracked\n");

    let out = repo.maw_raw(&["ws", "sync", "alice"]);
    assert!(
        !out.status.success(),
        "sync should fail when stale workspace has untracked changes\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("has uncommitted changes that would be lost by sync")
            && stderr.contains("git -C")
            && stderr.contains("status"),
        "expected actionable dirty-sync refusal, got: {stderr}"
    );

    assert!(
        workspace_state(&repo, "alice").contains("stale"),
        "workspace should remain stale after refused sync"
    );
}

#[test]
fn sync_all_returns_non_zero_when_any_workspace_fails_sync() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "clean"]);
    repo.maw_ok(&["ws", "create", "dirty"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    repo.add_file("dirty", "scratch.txt", "untracked\n");

    let out = repo.maw_raw(&["ws", "sync", "--all"]);
    assert!(
        !out.status.success(),
        "sync --all should fail when any workspace fails\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("Results:")
            && stdout.contains("Errors:")
            && stderr.contains("sync --all failed"),
        "expected summary + non-zero error reason\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        !workspace_state(&repo, "clean").contains("stale"),
        "clean stale workspace should still sync"
    );
    assert!(
        workspace_state(&repo, "dirty").contains("stale"),
        "dirty stale workspace should remain stale"
    );
}

#[test]
fn sync_all_returns_non_zero_when_stale_workspace_is_skipped_for_commits_ahead() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "ahead"]);
    repo.maw_ok(&["ws", "create", "advancer"]);

    repo.add_file("ahead", "ahead.txt", "keep me\n");
    repo.git_in_workspace("ahead", &["add", "ahead.txt"]);
    repo.git_in_workspace("ahead", &["commit", "-m", "feat: ahead work"]);

    repo.add_file("advancer", "epoch.txt", "epoch advance\n");
    repo.git_in_workspace("advancer", &["add", "epoch.txt"]);
    repo.git_in_workspace("advancer", &["commit", "-m", "feat: advance epoch"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--message",
        "merge advancer",
    ]);

    let out = repo.maw_raw(&["ws", "sync", "--all"]);
    assert!(
        !out.status.success(),
        "sync --all should fail when stale workspace is skipped\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("Skipped (committed work not yet merged")
            && stdout.contains("ahead (1 commit(s) ahead)")
            && stdout.contains("Results:")
            && stdout.contains("1 skipped")
            && stdout.contains("Result: INCOMPLETE")
            && stderr.contains("sync --all incomplete"),
        "expected skipped-work summary + incomplete failure\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        workspace_state(&repo, "ahead").contains("stale"),
        "ahead workspace should remain stale after skipped sync"
    );
}

#[test]
fn sync_rebase_replays_commits_ahead_of_workspace_epoch() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "feature"]);
    repo.add_file("feature", "kept.txt", "workspace change\n");
    repo.git_in_workspace("feature", &["add", "kept.txt"]);
    repo.git_in_workspace("feature", &["commit", "-m", "feat: workspace commit"]);
    let original_commit = repo.workspace_head("feature");

    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "epoch.txt", "epoch advance\n");
    repo.git_in_workspace("advancer", &["add", "epoch.txt"]);
    repo.git_in_workspace("advancer", &["commit", "-m", "feat: advance epoch"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--message",
        "merge advancer",
    ]);

    let new_epoch = repo.current_epoch();
    assert_ne!(original_commit, new_epoch, "epoch should have advanced");

    let out = repo.maw_raw(&["ws", "sync", "feature", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Rebasing workspace 'feature' (1 commit(s)) onto epoch")
            && !stdout.contains("No commits to replay")
            && stdout.contains("Replayed")
            && stdout.contains("Rebase complete: 1 commit(s) replayed cleanly."),
        "expected rebase replay output, got stdout: {stdout}"
    );

    let rebased_head = repo.workspace_head("feature");
    assert_ne!(
        rebased_head, new_epoch,
        "rebased workspace head should stay ahead of epoch with replayed commit"
    );
    assert_eq!(
        repo.read_file("feature", "kept.txt").as_deref(),
        Some("workspace change\n"),
        "workspace file should still be present after rebase"
    );
    assert_eq!(
        repo.git_in_workspace(
            "feature",
            &["rev-list", "--count", &format!("{new_epoch}..HEAD")]
        )
        .trim(),
        "1",
        "rebased workspace should still have one commit ahead of the new epoch"
    );
    assert_eq!(
        repo.git_in_workspace("feature", &["log", "-1", "--format=%s"])
            .trim(),
        "feat: workspace commit",
        "rebased commit should preserve the original message"
    );
    assert!(
        !workspace_state(&repo, "feature").contains("stale"),
        "rebased workspace should no longer be stale"
    );
}

#[test]
fn exec_does_not_auto_sync_unbound_workspace_to_active_change_epoch() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# app\n")]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-sync",
        "--workspace",
        "ch-sync",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-sync", "worker"]);
    repo.add_file("worker", "src/feature.rs", "pub fn feature() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: worker change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-sync",
        "--destroy",
        "--message",
        "merge worker",
    ]);

    // Simulate legacy drift shape (epoch tracking active change branch) so the
    // cross-target guard path stays covered even after bn-3092.
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-sync-flow"])
        .trim()
        .to_owned();
    repo.git(&["update-ref", "refs/manifold/epoch/current", &change_head]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let old_head = repo.workspace_head("hotfix");
    assert_ne!(old_head, repo.current_epoch());

    let out = repo.maw_raw(&["exec", "hotfix", "--", "git", "rev-parse", "HEAD"]);
    assert!(
        out.status.success(),
        "exec should still run command\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let new_head = repo.workspace_head("hotfix");
    assert_eq!(
        new_head, old_head,
        "auto-sync should be skipped for risky cross-target stale workspace"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Skipping auto-sync for this unbound workspace"),
        "expected explicit cross-target skip warning, got stderr: {stderr}"
    );
}

#[test]
fn sync_refuses_cross_target_update_for_unbound_workspace() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# app\n")]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-sync2",
        "--workspace",
        "ch-sync2",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-sync2", "worker"]);
    repo.add_file("worker", "src/feature.rs", "pub fn feature() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: worker change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-sync2",
        "--destroy",
        "--message",
        "merge worker",
    ]);

    // Simulate legacy drift shape (epoch tracking active change branch) so the
    // cross-target guard path stays covered even after bn-3092.
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-sync2-flow"])
        .trim()
        .to_owned();
    repo.git(&["update-ref", "refs/manifold/epoch/current", &change_head]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let old_head = repo.workspace_head("hotfix");

    let out = repo.maw_raw(&["ws", "sync", "hotfix"]);
    assert!(
        out.status.success(),
        "sync command should return success with safety refusal\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let new_head = repo.workspace_head("hotfix");
    assert_eq!(
        new_head, old_head,
        "cross-target safety should leave workspace base unchanged"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Refusing to sync this unbound workspace"),
        "expected explicit sync refusal message, got stdout: {stdout}"
    );
}
