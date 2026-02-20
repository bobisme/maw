//! Integration tests: workspace isolation (bd-2hw9.3).
//!
//! Verifies that workspaces created via git worktrees are fully isolated:
//! file edits, creates, and deletes in one workspace never leak to siblings.
//!
//! These are the foundational safety guarantees for multi-agent concurrent work.

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Basic isolation: edit in A doesn't appear in B
// ---------------------------------------------------------------------------

#[test]
fn edit_in_workspace_a_does_not_appear_in_workspace_b() {
    let repo = TestRepo::new();

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Alice creates a file.
    repo.add_file("alice", "hello.txt", "Hello from Alice");

    // Bob should NOT see Alice's file.
    let bob_file = repo.workspace_path("bob").join("hello.txt");
    assert!(
        !bob_file.exists(),
        "Bob should NOT see Alice's file — workspaces must be isolated"
    );

    // Default workspace should NOT see it either.
    let default_file = repo.workspace_path("default").join("hello.txt");
    assert!(
        !default_file.exists(),
        "Default workspace should NOT see Alice's file"
    );
}

// ---------------------------------------------------------------------------
// Create isolation: new file in A doesn't exist in B
// ---------------------------------------------------------------------------

#[test]
fn create_file_in_workspace_a_not_visible_in_workspace_b() {
    let repo = TestRepo::new();

    repo.create_workspace("ws-create-a");
    repo.create_workspace("ws-create-b");

    // Create a file with nested directory in A.
    repo.add_file("ws-create-a", "src/module/new.rs", "pub fn hello() {}");

    // B should not have the directory or file.
    let b_path = repo.workspace_path("ws-create-b").join("src/module/new.rs");
    assert!(
        !b_path.exists(),
        "File created in ws-create-a must not appear in ws-create-b"
    );

    let b_dir = repo.workspace_path("ws-create-b").join("src/module");
    assert!(
        !b_dir.exists(),
        "Directory created in ws-create-a must not appear in ws-create-b"
    );
}

// ---------------------------------------------------------------------------
// Delete isolation: delete in A doesn't affect B
// ---------------------------------------------------------------------------

#[test]
fn delete_file_in_workspace_a_does_not_affect_workspace_b() {
    let repo = TestRepo::new();

    // First, commit a shared file to both workspaces via epoch advancement.
    repo.add_file("default", "shared.txt", "shared content");
    repo.advance_epoch("chore: add shared file");

    // Create workspaces — both should inherit shared.txt from the epoch.
    repo.create_workspace("ws-del-a");
    repo.create_workspace("ws-del-b");

    // Verify both have the file.
    assert!(
        repo.workspace_path("ws-del-a").join("shared.txt").exists(),
        "ws-del-a should have shared.txt"
    );
    assert!(
        repo.workspace_path("ws-del-b").join("shared.txt").exists(),
        "ws-del-b should have shared.txt"
    );

    // Delete in A.
    repo.delete_file("ws-del-a", "shared.txt");

    // B should still have it.
    assert!(
        repo.workspace_path("ws-del-b").join("shared.txt").exists(),
        "ws-del-b must still have shared.txt after delete in ws-del-a"
    );

    // Default should still have it.
    assert!(
        repo.workspace_path("default").join("shared.txt").exists(),
        "default must still have shared.txt after delete in ws-del-a"
    );
}

// ---------------------------------------------------------------------------
// Status isolation: status in A doesn't affect B's state
// ---------------------------------------------------------------------------

#[test]
fn status_in_workspace_a_does_not_affect_workspace_b() {
    let repo = TestRepo::new();

    repo.create_workspace("ws-stat-a");
    repo.create_workspace("ws-stat-b");

    // Add files to A — makes it dirty.
    repo.add_file("ws-stat-a", "dirty.txt", "dirty content");

    // Git status in A should show changes.
    let status_a = repo.git_in_workspace("ws-stat-a", &["status", "--porcelain"]);
    assert!(
        !status_a.trim().is_empty(),
        "ws-stat-a should have dirty status"
    );

    // Git status in B should be clean (or only have gitignore).
    let status_b = repo.git_in_workspace("ws-stat-b", &["status", "--porcelain"]);
    // Filter out .gitignore-related entries
    let meaningful_b: Vec<&str> = status_b
        .lines()
        .filter(|l| !l.contains(".gitignore"))
        .collect();
    assert!(
        meaningful_b.is_empty(),
        "ws-stat-b should be clean, but got: {meaningful_b:?}"
    );
}

// ---------------------------------------------------------------------------
// Modify isolation: modify in A, original content in B
// ---------------------------------------------------------------------------

#[test]
fn modify_file_in_workspace_a_original_in_workspace_b() {
    let repo = TestRepo::new();

    // Commit a shared file.
    repo.add_file("default", "config.toml", "[original]\nversion = 1\n");
    repo.advance_epoch("chore: add config.toml");

    repo.create_workspace("ws-mod-a");
    repo.create_workspace("ws-mod-b");

    // Modify in A.
    repo.modify_file("ws-mod-a", "config.toml", "[modified]\nversion = 2\n");

    // B should have original content.
    let b_content =
        std::fs::read_to_string(repo.workspace_path("ws-mod-b").join("config.toml")).unwrap();
    assert_eq!(
        b_content, "[original]\nversion = 1\n",
        "ws-mod-b must have original content"
    );

    // A should have modified content.
    let a_content =
        std::fs::read_to_string(repo.workspace_path("ws-mod-a").join("config.toml")).unwrap();
    assert_eq!(
        a_content, "[modified]\nversion = 2\n",
        "ws-mod-a should have modified content"
    );
}

// ---------------------------------------------------------------------------
// Concurrent 5-workspace test: no cross-contamination
// ---------------------------------------------------------------------------

#[test]
fn five_workspaces_concurrent_edits_no_cross_contamination() {
    let repo = TestRepo::new();

    // Commit a base file so all workspaces start with it.
    repo.add_file("default", "base.txt", "base content\n");
    repo.advance_epoch("chore: add base.txt");

    let ws_names = ["ws-1", "ws-2", "ws-3", "ws-4", "ws-5"];

    // Create all 5 workspaces.
    for name in &ws_names {
        repo.create_workspace(name);
    }

    // Each workspace creates a unique file AND modifies base.txt differently.
    for (i, name) in ws_names.iter().enumerate() {
        repo.add_file(
            name,
            &format!("unique_{i}.txt"),
            &format!("content from {name}"),
        );
        repo.modify_file(name, "base.txt", &format!("modified by {name}\n"));
    }

    // Verify: each workspace has ONLY its own unique file
    for (i, name) in ws_names.iter().enumerate() {
        // Should have its own unique file.
        let own_file = repo.workspace_path(name).join(format!("unique_{i}.txt"));
        assert!(
            own_file.exists(),
            "{name} should have its own unique_{i}.txt"
        );

        // Should NOT have any other workspace's unique file.
        for (j, other_name) in ws_names.iter().enumerate() {
            if i == j {
                continue;
            }
            let other_file = repo.workspace_path(name).join(format!("unique_{j}.txt"));
            assert!(
                !other_file.exists(),
                "{name} should NOT have {other_name}'s unique_{j}.txt"
            );
        }

        // base.txt should show THIS workspace's modification only.
        let base_content =
            std::fs::read_to_string(repo.workspace_path(name).join("base.txt")).unwrap();
        assert_eq!(
            base_content,
            format!("modified by {name}\n"),
            "{name}'s base.txt should reflect only its own modification"
        );
    }

    // Default workspace should still have original base.txt.
    let default_base =
        std::fs::read_to_string(repo.workspace_path("default").join("base.txt")).unwrap();
    assert_eq!(
        default_base, "base content\n",
        "default workspace base.txt should be untouched"
    );

    // Default workspace should NOT have any unique files.
    for i in 0..5 {
        let unique_in_default = repo
            .workspace_path("default")
            .join(format!("unique_{i}.txt"));
        assert!(
            !unique_in_default.exists(),
            "default should not have unique_{i}.txt"
        );
    }
}

// ---------------------------------------------------------------------------
// Directory creation isolation
// ---------------------------------------------------------------------------

#[test]
fn directory_creation_in_workspace_a_not_visible_in_workspace_b() {
    let repo = TestRepo::new();

    repo.create_workspace("ws-dir-a");
    repo.create_workspace("ws-dir-b");

    // Create a deep directory tree in A.
    repo.add_file(
        "ws-dir-a",
        "src/models/user/profile.rs",
        "pub struct Profile {}",
    );
    repo.add_file(
        "ws-dir-a",
        "src/models/user/settings.rs",
        "pub struct Settings {}",
    );

    // B should not have any of these directories.
    let b_models = repo.workspace_path("ws-dir-b").join("src/models");
    assert!(
        !b_models.exists(),
        "ws-dir-b should not have src/models/ directory"
    );
}

// ---------------------------------------------------------------------------
// Isolation persists after workspace destruction
// ---------------------------------------------------------------------------

#[test]
fn destroying_workspace_does_not_affect_sibling_files() {
    let repo = TestRepo::new();

    repo.create_workspace("ws-alive");
    repo.create_workspace("ws-doomed");

    repo.add_file("ws-alive", "survivor.txt", "I persist");
    repo.add_file("ws-doomed", "ephemeral.txt", "I vanish");

    // Destroy ws-doomed.
    repo.destroy_workspace("ws-doomed");

    // ws-alive should still have its file.
    let survivor = repo.workspace_path("ws-alive").join("survivor.txt");
    assert!(
        survivor.exists(),
        "ws-alive's file should survive sibling destruction"
    );
    let content = std::fs::read_to_string(&survivor).unwrap();
    assert_eq!(content, "I persist");
}

// ---------------------------------------------------------------------------
// Binary file isolation
// ---------------------------------------------------------------------------

#[test]
fn binary_file_isolation() {
    let repo = TestRepo::new();

    repo.create_workspace("ws-bin-a");
    repo.create_workspace("ws-bin-b");

    // Write binary content in A.
    let binary_content: Vec<u8> = (u8::MIN..=u8::MAX).collect();
    let file_path = repo.workspace_path("ws-bin-a").join("data.bin");
    std::fs::write(&file_path, &binary_content).unwrap();

    // B should not have it.
    let b_file = repo.workspace_path("ws-bin-b").join("data.bin");
    assert!(
        !b_file.exists(),
        "Binary file in ws-bin-a should not appear in ws-bin-b"
    );
}

// ---------------------------------------------------------------------------
// Concurrent create + delete isolation
// ---------------------------------------------------------------------------

#[test]
fn concurrent_create_and_delete_across_workspaces() {
    let repo = TestRepo::new();

    // Commit a shared file.
    repo.add_file("default", "shared.rs", "fn shared() {}");
    repo.advance_epoch("chore: add shared.rs");

    repo.create_workspace("ws-creator");
    repo.create_workspace("ws-deleter");

    // Creator adds a new file.
    repo.add_file("ws-creator", "new_feature.rs", "fn new_feature() {}");

    // Deleter removes the shared file.
    repo.delete_file("ws-deleter", "shared.rs");

    // Creator should still have shared.rs (unaffected by deleter).
    assert!(
        repo.workspace_path("ws-creator").join("shared.rs").exists(),
        "ws-creator should still have shared.rs"
    );

    // Deleter should NOT have new_feature.rs (unaffected by creator).
    assert!(
        !repo
            .workspace_path("ws-deleter")
            .join("new_feature.rs")
            .exists(),
        "ws-deleter should NOT have new_feature.rs"
    );

    // Default should still have shared.rs.
    assert!(
        repo.workspace_path("default").join("shared.rs").exists(),
        "default should still have shared.rs"
    );
}

// ---------------------------------------------------------------------------
// Large number of files isolation
// ---------------------------------------------------------------------------

#[test]
fn isolation_with_many_files() {
    let repo = TestRepo::new();

    repo.create_workspace("ws-many-a");
    repo.create_workspace("ws-many-b");

    // Create 50 files in A.
    for i in 0..50 {
        repo.add_file(
            "ws-many-a",
            &format!("file_{i:03}.txt"),
            &format!("content {i}"),
        );
    }

    // B should have exactly 0 of these files.
    let b_path = repo.workspace_path("ws-many-b");
    let leaked: Vec<String> = (0..50)
        .filter(|i| b_path.join(format!("file_{i:03}.txt")).exists())
        .map(|i| format!("file_{i:03}.txt"))
        .collect();

    assert!(
        leaked.is_empty(),
        "No files from ws-many-a should leak to ws-many-b, but found: {leaked:?}"
    );
}
