//! Smoke tests for the Manifold v2 test infrastructure.
//!
//! Validates that [`TestRepo`] correctly sets up git-native Manifold repos
//! and that the basic workspace operations work end-to-end.

mod manifold_common;

use manifold_common::TestRepo;

#[test]
fn smoke_basic_lifecycle() {
    let repo = TestRepo::new();

    // Seed some initial files
    repo.seed_files(&[
        ("README.md", "# Test Project\n"),
        ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"),
    ]);

    // Create agent workspaces
    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Both should see the seeded files
    assert_eq!(
        repo.read_file("alice", "README.md"),
        Some("# Test Project\n".to_owned())
    );
    assert_eq!(
        repo.read_file("bob", "src/lib.rs"),
        Some("pub fn add(a: i32, b: i32) -> i32 { a + b }\n".to_owned())
    );

    // Each agent makes different changes
    repo.add_file("alice", "alice.txt", "Alice was here");
    repo.modify_file("alice", "README.md", "# Test Project (updated by Alice)\n");

    repo.add_file("bob", "bob.txt", "Bob was here");
    repo.add_file("bob", "src/main.rs", "fn main() { println!(\"hello\"); }\n");

    // Verify isolation
    assert!(!repo.file_exists("bob", "alice.txt"), "bob shouldn't see alice's file");
    assert!(!repo.file_exists("alice", "bob.txt"), "alice shouldn't see bob's file");

    // Both workspaces should show dirty files
    let alice_dirty = repo.dirty_files("alice");
    assert!(!alice_dirty.is_empty(), "alice should have dirty files");

    let bob_dirty = repo.dirty_files("bob");
    assert!(!bob_dirty.is_empty(), "bob should have dirty files");

    // List should show all three workspaces
    let ws_list = repo.list_workspaces();
    assert_eq!(ws_list.len(), 3);
    assert!(ws_list.contains(&"default".to_owned()));
    assert!(ws_list.contains(&"alice".to_owned()));
    assert!(ws_list.contains(&"bob".to_owned()));

    // Destroy workspaces
    repo.destroy_workspace("alice");
    repo.destroy_workspace("bob");

    let ws_list = repo.list_workspaces();
    assert_eq!(ws_list.len(), 1);
    assert!(ws_list.contains(&"default".to_owned()));
}

#[test]
fn smoke_epoch_advancement() {
    let repo = TestRepo::new();
    let epoch0 = repo.epoch0().to_owned();

    // Seed files advances epoch
    let epoch1 = repo.seed_files(&[("file.txt", "v1")]);
    assert_ne!(epoch0, epoch1);

    // New workspace is at epoch1
    repo.create_workspace("agent");
    assert_eq!(repo.workspace_head("agent"), epoch1);
    assert!(repo.file_exists("agent", "file.txt"));

    // Advance epoch again
    repo.add_file("default", "file2.txt", "v1");
    let epoch2 = repo.advance_epoch("add file2");
    assert_ne!(epoch1, epoch2);

    // Agent workspace is now stale
    assert_eq!(repo.workspace_head("agent"), epoch1);
    assert_ne!(repo.workspace_head("agent"), epoch2);

    // But agent doesn't see file2 (it's at the old epoch)
    assert!(!repo.file_exists("agent", "file2.txt"));
}

#[test]
fn smoke_git_operations_in_workspace() {
    let repo = TestRepo::new();
    repo.seed_files(&[("existing.txt", "original")]);
    repo.create_workspace("agent");

    // Modify a file
    repo.modify_file("agent", "existing.txt", "modified");

    // git diff in workspace should show the change
    let diff = repo.git_in_workspace("agent", &["diff", "--stat"]);
    assert!(diff.contains("existing.txt"), "diff should show existing.txt");

    // Add new file and check untracked
    repo.add_file("agent", "new.txt", "brand new");
    let status = repo.git_in_workspace("agent", &["status", "--porcelain"]);
    assert!(status.contains("new.txt"), "status should show new.txt");
}

#[test]
fn smoke_bare_mode_no_working_tree_at_root() {
    let repo = TestRepo::new();

    // The root should be bare — no working tree
    let is_bare = repo.git(&["config", "--get", "core.bare"]);
    assert_eq!(is_bare.trim(), "true");

    // Root should NOT have source files (only .git, .manifold, ws/)
    let root_entries: Vec<_> = std::fs::read_dir(repo.root())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| !name.starts_with('.'))
        .collect();

    assert!(
        root_entries.iter().all(|name| name == "ws"),
        "root should only have ws/ (no source files): {root_entries:?}"
    );
}
