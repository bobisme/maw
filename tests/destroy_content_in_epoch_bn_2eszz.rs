//! bn-2eszz: `maw ws destroy` refusal explains when the workspace's content
//! is already in the epoch under different commit hashes (cherry-picks).
//!
//! The refusal itself is never weakened: every case here must still refuse.
//! Only the content-already-merged cases carry the hint.

mod manifold_common;

use manifold_common::TestRepo;

const HINT: &str = "content already in epoch";

fn commit_all(repo: &TestRepo, ws: &str, msg: &str) -> String {
    repo.git_in_workspace(ws, &["add", "-A"]);
    repo.git_in_workspace(ws, &["commit", "-m", msg]);
    repo.git_in_workspace(ws, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

/// Destroy must refuse; return stderr+stdout of the refusal.
fn destroy_refuses(repo: &TestRepo, ws: &str) -> String {
    let out = repo.maw_fails(&["ws", "destroy", ws]);
    assert!(
        repo.workspace_exists(ws),
        "refused destroy must leave '{ws}' in place"
    );
    assert!(
        out.contains("Refusing destroy"),
        "expected a destroy refusal, got:\n{out}"
    );
    out
}

fn destroy_refusal_json(repo: &TestRepo, ws: &str) -> serde_json::Value {
    let out = repo.maw_fails(&["ws", "destroy", ws, "--format", "json"]);
    assert!(repo.workspace_exists(ws));
    let start = out.find('{').expect("JSON payload in refusal");
    let end = out.rfind('}').expect("JSON payload end");
    serde_json::from_str(&out[start..=end]).expect("valid refusal JSON")
}

/// Build `orig` with two commits, cherry-pick `picks` of them into `redo`,
/// and merge `redo`. Returns the orig commit OIDs.
fn cherry_pick_scenario(repo: &TestRepo, picks: usize) -> Vec<String> {
    repo.seed_files(&[
        ("README.md", "# seed\n"),
        ("shared.txt", "l1\nl2\nl3\nl4\nl5\n"),
    ]);
    repo.maw_ok(&["ws", "create", "orig"]);
    repo.add_file("orig", "a.txt", "alpha\n");
    let c1 = commit_all(repo, "orig", "feat: a");
    repo.add_file("orig", "b.txt", "beta\n");
    let c2 = commit_all(repo, "orig", "feat: b");
    let commits = vec![c1, c2];

    repo.maw_ok(&["ws", "create", "redo"]);
    for c in commits.iter().take(picks) {
        repo.git_in_workspace("redo", &["cherry-pick", c]);
    }
    // Keep orig out of the merge's sibling auto-rebase (it skips dirty
    // workspaces) so it stays on its old base with its original commits —
    // the stuck-workspace shape from the field report.
    repo.add_file("orig", "scratch.txt", "wip\n");
    repo.maw_ok(&["ws", "merge", "redo", "--destroy", "--message", "land redo"]);
    repo.delete_file("orig", "scratch.txt");

    // Precondition for the field scenario: orig's commits are NOT reachable
    // from the epoch (hashes differ), which is why destroy refuses.
    let head = repo.workspace_head("orig");
    let epoch = repo.current_epoch();
    let reachable = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &head, &epoch])
        .current_dir(repo.root())
        .status()
        .expect("git merge-base")
        .success();
    assert!(!reachable, "orig HEAD must not be reachable from the epoch");
    commits
}

#[test]
fn cherry_picked_content_still_refuses_but_explains() {
    let repo = TestRepo::new();
    cherry_pick_scenario(&repo, 2);

    let out = destroy_refuses(&repo, "orig");
    assert!(out.contains(HINT), "hint missing:\n{out}");
    assert!(
        out.contains("maw ws destroy orig --force"),
        "hint must carry the exact --force command:\n{out}"
    );
    assert!(out.contains("recovery snapshot"), "{out}");

    let json = destroy_refusal_json(&repo, "orig");
    let note = &json["content_already_in_epoch"];
    assert!(note.is_object(), "JSON must carry the diagnostic: {json}");
    assert_eq!(note["evidence"], "tree-equals-epoch");
    assert_eq!(note["epoch"], repo.current_epoch());
    assert_eq!(note["unreachable_commits"], 2);
}

#[test]
fn cherry_picked_then_unrelated_epoch_change_matches_by_content() {
    let repo = TestRepo::new();
    cherry_pick_scenario(&repo, 2);
    // Epoch gains an unrelated file; orig's tree no longer equals it, but
    // every file orig changed is identical in the epoch. Keep orig dirty so
    // the merge's sibling auto-rebase leaves it alone.
    repo.add_file("orig", "scratch.txt", "wip\n");
    repo.maw_ok(&["ws", "create", "other"]);
    repo.add_file("other", "other.txt", "unrelated\n");
    repo.maw_ok(&["ws", "merge", "other", "--destroy", "--message", "other"]);
    repo.delete_file("orig", "scratch.txt");

    let out = destroy_refuses(&repo, "orig");
    assert!(out.contains(HINT), "hint missing:\n{out}");
    let json = destroy_refusal_json(&repo, "orig");
    assert_eq!(
        json["content_already_in_epoch"]["evidence"], "changes-present-in-epoch",
        "{json}"
    );
}

#[test]
fn cherry_picked_then_epoch_moved_on_matches_by_patch_id() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("README.md", "# seed\n"),
        ("shared.txt", "l1\nl2\nl3\nl4\nl5\n"),
    ]);
    repo.maw_ok(&["ws", "create", "orig"]);
    repo.modify_file("orig", "shared.txt", "L1\nl2\nl3\nl4\nl5\n");
    let c1 = commit_all(&repo, "orig", "feat: line 1");

    repo.maw_ok(&["ws", "create", "redo"]);
    repo.git_in_workspace("redo", &["cherry-pick", &c1]);
    // Dirty orig is skipped by sibling auto-rebase (see cherry_pick_scenario).
    repo.add_file("orig", "scratch.txt", "wip\n");
    repo.maw_ok(&["ws", "merge", "redo", "--destroy", "--message", "land redo"]);

    // The epoch moves on and edits the same file further: file content no
    // longer matches orig, but orig's patch is still contained.
    repo.maw_ok(&["ws", "create", "later"]);
    repo.modify_file("later", "shared.txt", "L1\nl2\nl3\nl4\nL5\n");
    repo.maw_ok(&["ws", "merge", "later", "--destroy", "--message", "later"]);
    repo.delete_file("orig", "scratch.txt");

    let out = destroy_refuses(&repo, "orig");
    assert!(out.contains(HINT), "hint missing:\n{out}");
    let json = destroy_refusal_json(&repo, "orig");
    assert_eq!(
        json["content_already_in_epoch"]["evidence"], "patches-in-epoch",
        "{json}"
    );
}

#[test]
fn genuinely_unmerged_commit_refuses_without_hint() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# seed\n")]);
    repo.maw_ok(&["ws", "create", "work"]);
    repo.add_file("work", "new.txt", "never merged\n");
    commit_all(&repo, "work", "feat: new");

    let out = destroy_refuses(&repo, "work");
    assert!(!out.contains(HINT), "no hint for unmerged work:\n{out}");
    let json = destroy_refusal_json(&repo, "work");
    assert!(json["content_already_in_epoch"].is_null(), "{json}");
}

#[test]
fn partially_cherry_picked_refuses_without_hint() {
    let repo = TestRepo::new();
    cherry_pick_scenario(&repo, 1);

    let out = destroy_refuses(&repo, "orig");
    assert!(
        !out.contains(HINT),
        "commit b is not in the epoch — no hint allowed:\n{out}"
    );
}

#[test]
fn cherry_picked_but_dirty_refuses_without_hint() {
    let repo = TestRepo::new();
    cherry_pick_scenario(&repo, 2);
    repo.add_file("orig", "scratch.txt", "uncommitted\n");

    let out = destroy_refuses(&repo, "orig");
    assert!(
        !out.contains(HINT),
        "uncommitted edits are not in the epoch — no hint allowed:\n{out}"
    );
}
