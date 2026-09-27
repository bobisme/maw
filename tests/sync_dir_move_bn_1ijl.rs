//! bn-1ijl — `maw ws sync` must replay a commit that deletes one directory
//! and adds another containing an identical subtree.
//!
//! Field report (bones repo): `maw ws sync bn-12ta` replayed the first of 4
//! commits, then died with
//!
//! ```text
//! Error: Failed to extract patchset for 21aa7debbe09: git repo error: not
//! found: blob 05c5…: Object named 05c5… was supposed to be of kind blob,
//! but was kind tree.
//! ```
//!
//! The commit deleted `.crit/` + `.critignore` and added `.seal/`; the epoch
//! it was rebasing onto already carried the same migration from a sibling
//! workspace. gix's rename tracker reported the moved subtree as a
//! directory-level rename, and the patchset extractor tried to read that
//! tree as a blob. The workspace stayed stale and `maw ws merge` refused it.

mod manifold_common;

use manifold_common::TestRepo;

fn commit_all(repo: &TestRepo, workspace: &str, message: &str) {
    repo.git_in_workspace(workspace, &["add", "-A"]);
    repo.git_in_workspace(workspace, &["commit", "-m", message]);
}

fn seed_crit(repo: &TestRepo) {
    repo.seed_files(&[
        (".crit/.gitignore", "ignored\n"),
        (".crit/reviews/r1/events.jsonl", "{\"e\":1}\n"),
        (".crit/reviews/r2/events.jsonl", "{\"e\":2}\n"),
        (".crit/version", "1\n"),
        (".critignore", "target\n"),
        ("README", "readme\n"),
        ("src/lib.rs", "pub fn a() {}\n"),
    ]);
}

/// Apply the `.crit/` -> `.seal/` migration in `ws` as one commit: the
/// review subtree moves verbatim (identical tree OID), the rest is deleted,
/// and a new config file is added.
fn migrate_crit_to_seal(repo: &TestRepo, ws: &str) {
    repo.git_in_workspace(ws, &["rm", "-rq", ".crit", ".critignore"]);
    repo.add_file(ws, ".seal/reviews/r1/events.jsonl", "{\"e\":1}\n");
    repo.add_file(ws, ".seal/reviews/r2/events.jsonl", "{\"e\":2}\n");
    repo.add_file(ws, ".seal/config.toml", "version = 2\n");
    commit_all(repo, ws, "chore: migrate crit -> seal");
}

fn assert_sealed_tree(repo: &TestRepo, ws: &str, rev: &str) {
    let mut paths: Vec<String> = repo
        .git_ls_tree(ws, rev)
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    paths.sort();
    assert!(
        !paths.iter().any(|p| p.starts_with(".crit")),
        "{ws}@{rev}: .crit must be gone, got {paths:?}"
    );
    for want in [
        ".seal/config.toml",
        ".seal/reviews/r1/events.jsonl",
        ".seal/reviews/r2/events.jsonl",
        "README",
    ] {
        assert!(
            paths.iter().any(|p| p == want),
            "{ws}@{rev}: missing {want}, got {paths:?}"
        );
    }
}

/// The field shape: two workspaces carry the same directory migration; the
/// first is merged (without auto-rebase, so the second stays stale exactly
/// like the reporter's); `maw ws sync` on the second must succeed, keep all
/// of its commits' work, and leave it mergeable.
#[test]
fn sync_replays_directory_move_already_applied_upstream() {
    let repo = TestRepo::new();
    seed_crit(&repo);

    repo.maw_ok(&["ws", "create", "first"]);
    repo.maw_ok(&["ws", "create", "second"]);

    migrate_crit_to_seal(&repo, "first");

    // `second`: an unrelated commit, the same migration, then follow-ups.
    repo.add_file("second", "notes.md", "n1\n");
    commit_all(&repo, "second", "docs: notes");
    migrate_crit_to_seal(&repo, "second");
    repo.modify_file("second", "src/lib.rs", "pub fn a() {}\npub fn b() {}\n");
    commit_all(&repo, "second", "feat: b");
    repo.add_file("second", ".seal/reviews/r3/events.jsonl", "{\"e\":3}\n");
    commit_all(&repo, "second", "chore: r3");

    repo.maw_ok(&[
        "ws",
        "merge",
        "first",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge first",
    ]);
    let epoch = repo.current_epoch();

    let out = repo.maw_raw(&["ws", "sync", "second"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "sync must replay a directory-move commit:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("was supposed to be of kind blob"),
        "tree read as blob:\n{stderr}"
    );

    // All four commits replayed onto the new epoch, work intact.
    let ahead = repo.git_in_workspace(
        "second",
        &["rev-list", "--count", &format!("{epoch}..HEAD")],
    );
    assert_eq!(ahead.trim(), "4", "all 4 commits must be replayed");
    assert_sealed_tree(&repo, "second", "HEAD");
    assert_eq!(
        repo.read_file("second", "notes.md").as_deref(),
        Some("n1\n")
    );
    assert_eq!(
        repo.read_file("second", "src/lib.rs").as_deref(),
        Some("pub fn a() {}\npub fn b() {}\n")
    );
    assert_eq!(
        repo.read_file("second", ".seal/reviews/r3/events.jsonl")
            .as_deref(),
        Some("{\"e\":3}\n")
    );
    assert!(!repo.file_exists("second", ".crit/version"));
    assert!(!repo.file_exists("second", ".critignore"));

    // The workspace is now mergeable.
    repo.maw_ok(&[
        "ws",
        "merge",
        "second",
        "--destroy",
        "--message",
        "merge second",
    ]);
    assert_sealed_tree(&repo, "default", "HEAD");
    assert_eq!(
        repo.read_file("default", "notes.md").as_deref(),
        Some("n1\n")
    );
    assert_eq!(
        repo.read_file("default", ".seal/reviews/r3/events.jsonl")
            .as_deref(),
        Some("{\"e\":3}\n")
    );
}

/// Same shape, but through the merge-time auto-rebase of siblings: after
/// `first` merges, `second` must end up current (or at worst syncable) and
/// mergeable — never stuck stale.
#[test]
fn auto_rebase_replays_directory_move_already_applied_upstream() {
    let repo = TestRepo::new();
    seed_crit(&repo);

    repo.maw_ok(&["ws", "create", "first"]);
    repo.maw_ok(&["ws", "create", "second"]);
    migrate_crit_to_seal(&repo, "first");
    migrate_crit_to_seal(&repo, "second");
    repo.modify_file("second", "README", "readme v2\n");
    commit_all(&repo, "second", "docs: readme v2");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "first",
        "--destroy",
        "--message",
        "merge first",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "merge first:\n{stdout}\n{stderr}");
    assert!(
        !stderr.contains("was supposed to be of kind blob")
            && !stdout.contains("was supposed to be of kind blob"),
        "auto-rebase read a tree as a blob:\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Whatever auto-rebase did, an explicit sync must succeed and the
    // workspace must then merge cleanly.
    repo.maw_ok(&["ws", "sync", "second"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "second",
        "--destroy",
        "--message",
        "merge second",
    ]);
    assert_sealed_tree(&repo, "default", "HEAD");
    assert_eq!(
        repo.read_file("default", "README").as_deref(),
        Some("readme v2\n")
    );
}

/// Neighbouring shape found by the bn-1ijl property test: a commit that
/// replaces a gitlink (submodule entry, mode 160000) with a regular file at
/// the same path. The patchset extractor skipped reading the new blob
/// whenever the OLD side was a gitlink, so the replayed change carried no
/// content. Replay must keep the file's bytes.
#[test]
fn sync_replays_gitlink_replaced_by_file() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README", "readme\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    // Commit a gitlink entry directly (no submodule checkout needed).
    let fake_commit = "fe00000000000000000000000000000000000001";
    repo.git_in_workspace(
        "alice",
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{fake_commit},vendor/sub"),
        ],
    );
    repo.git_in_workspace("alice", &["commit", "-m", "chore: add gitlink"]);
    // Replace the gitlink with a regular file.
    repo.git_in_workspace("alice", &["rm", "--cached", "-q", "vendor/sub"]);
    let _ = std::fs::remove_dir_all(repo.workspace_path("alice").join("vendor/sub"));
    repo.add_file("alice", "vendor/sub", "now a vendored file\n");
    commit_all(&repo, "alice", "chore: vendor sub as a file");

    // Advance the epoch unrelatedly so `alice` must replay both commits.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "unrelated.txt", "x\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    repo.maw_ok(&["ws", "sync", "alice"]);
    let tree = repo.git_ls_tree("alice", "HEAD");
    assert!(
        tree.iter().any(|(m, p)| m == "100644" && p == "vendor/sub"),
        "vendor/sub must be a regular file after replay: {tree:?}"
    );
    assert_eq!(
        repo.git_in_workspace("alice", &["show", "HEAD:vendor/sub"]),
        "now a vendored file\n"
    );
    assert_eq!(
        repo.read_file("alice", "vendor/sub").as_deref(),
        Some("now a vendored file\n")
    );
}
