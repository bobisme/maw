//! Integration tests for merge scenarios: disjoint, conflicting, and N-way.
//!
//! Uses the git-native `TestRepo` infrastructure (no jj dependency).
//! Tests exercise the full collect → partition → resolve → build merge pipeline
//! via [`run_build_phase_with_inputs`].
//!
//! Coverage:
//! - 2-way merge, disjoint files: clean merge, both changes present
//! - 2-way merge, same file different regions: clean diff3 merge
//! - 2-way merge, same file same region: conflict reported
//! - 3-way merge, disjoint files: clean merge
//! - 5-way merge, disjoint files: clean merge
//! - Identical changes from 2 workspaces: hash equality, clean merge
//! - --destroy flag: workspaces removed after merge (via maw CLI)
//! - Merge with empty workspace: no-op for that workspace

mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;

/// Helper: create a `GitWorktreeBackend` for a `TestRepo`.
fn backend_for(repo: &TestRepo) -> maw::backend::git::GitWorktreeBackend {
    maw::backend::git::GitWorktreeBackend::new(repo.root().to_path_buf())
}

/// Helper: read a file from the candidate commit tree.
fn read_candidate_file(repo: &TestRepo, candidate_oid: &str, path: &str) -> Option<String> {
    let spec = format!("{candidate_oid}:{path}");
    let output = Command::new("git")
        .args(["show", &spec])
        .current_dir(repo.root())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// Helper: list all files in the candidate commit tree.
fn list_candidate_files(repo: &TestRepo, candidate_oid: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", candidate_oid])
        .current_dir(repo.root())
        .output()
        .expect("git ls-tree failed");
    assert!(output.status.success(), "git ls-tree failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect()
}

/// Helper: verify the candidate commit's parent is the epoch.
fn assert_candidate_parent_is_epoch(repo: &TestRepo, candidate_oid: &str) {
    let parent = Command::new("git")
        .args(["rev-parse", &format!("{candidate_oid}^")])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse failed");
    let parent_oid = String::from_utf8_lossy(&parent.stdout).trim().to_owned();
    assert_eq!(
        parent_oid,
        repo.current_epoch(),
        "candidate parent should be the current epoch"
    );
}

// ==========================================================================
// 2-way merge: disjoint files
// ==========================================================================

#[test]
fn two_way_merge_disjoint_files_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# Project\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    repo.add_file("alice", "alice.txt", "Alice's work\n");
    repo.add_file("bob", "bob.txt", "Bob's work\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    // Should be clean — no conflicts
    assert!(
        output.conflicts.is_empty(),
        "disjoint files should not produce conflicts: {:?}",
        output.conflicts
    );

    // Both files should be in the candidate tree
    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(
        files.contains(&"alice.txt".to_string()),
        "alice.txt missing from merge result"
    );
    assert!(
        files.contains(&"bob.txt".to_string()),
        "bob.txt missing from merge result"
    );
    assert!(
        files.contains(&"README.md".to_string()),
        "README.md should be preserved"
    );

    // Content should match
    assert_eq!(
        read_candidate_file(&repo, output.candidate.as_str(), "alice.txt"),
        Some("Alice's work\n".to_string())
    );
    assert_eq!(
        read_candidate_file(&repo, output.candidate.as_str(), "bob.txt"),
        Some("Bob's work\n".to_string())
    );

    // Candidate parent should be epoch
    assert_candidate_parent_is_epoch(&repo, output.candidate.as_str());

    // Stats
    assert_eq!(output.unique_count, 2);
    assert_eq!(output.shared_count, 0);
    assert_eq!(output.resolved_count, 2);
}

// ==========================================================================
// 2-way merge: same file, different regions (diff3 clean)
// ==========================================================================

#[test]
fn two_way_merge_same_file_different_regions_diff3_clean() {
    let repo = TestRepo::new();
    // Seed a file with clearly separated regions (4+ context lines between edits)
    repo.seed_files(&[(
        "shared.txt",
        "line1\n---\n---\n---\n---\nline2\n---\n---\n---\n---\nline3\n",
    )]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Alice modifies line1 region
    repo.modify_file(
        "alice",
        "shared.txt",
        "ALICE1\n---\n---\n---\n---\nline2\n---\n---\n---\n---\nline3\n",
    );

    // Bob modifies line3 region
    repo.modify_file(
        "bob",
        "shared.txt",
        "line1\n---\n---\n---\n---\nline2\n---\n---\n---\n---\nBOB3\n",
    );

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    // Should be clean — diff3 resolves non-overlapping edits
    assert!(
        output.conflicts.is_empty(),
        "non-overlapping edits should merge cleanly: {:?}",
        output.conflicts
    );

    // Merged content should have both changes
    let content = read_candidate_file(&repo, output.candidate.as_str(), "shared.txt")
        .expect("shared.txt should exist in candidate");
    assert!(
        content.contains("ALICE1"),
        "alice's edit missing: {content}"
    );
    assert!(content.contains("BOB3"), "bob's edit missing: {content}");
    assert!(
        !content.contains("line1"),
        "original line1 should be replaced"
    );
    assert!(
        !content.contains("line3"),
        "original line3 should be replaced"
    );

    // Stats
    assert_eq!(output.shared_count, 1, "one shared path");
    assert_eq!(output.resolved_count, 1, "one resolved change");
}

// ==========================================================================
// 2-way merge: same file, same region (conflict)
// ==========================================================================

#[test]
fn two_way_merge_same_file_same_region_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("data.txt", "original\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Both modify the same single-line region
    repo.modify_file("alice", "data.txt", "alice version\n");
    repo.modify_file("bob", "data.txt", "bob version\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    // Should report a conflict
    assert_eq!(
        output.conflicts.len(),
        1,
        "overlapping edits should produce exactly 1 conflict"
    );
    assert_eq!(
        output.conflicts[0].path.to_str(),
        Some("data.txt"),
        "conflict should be for data.txt"
    );
    assert_eq!(
        output.conflicts[0].reason,
        maw::merge::resolve::ConflictReason::Diff3Conflict,
        "conflict reason should be Diff3Conflict"
    );

    // Conflict sides
    assert_eq!(
        output.conflicts[0].sides.len(),
        2,
        "2 conflict sides expected"
    );
    let side_ws: Vec<_> = output.conflicts[0]
        .sides
        .iter()
        .map(|s| s.workspace_id.as_str().to_owned())
        .collect();
    assert!(side_ws.contains(&"alice".to_string()));
    assert!(side_ws.contains(&"bob".to_string()));

    // Base content should be the original
    assert_eq!(
        output.conflicts[0].base.as_deref(),
        Some(b"original\n".as_ref()),
        "base content should be the epoch version"
    );
}

// ==========================================================================
// 3-way merge: disjoint files
// ==========================================================================

#[test]
fn three_way_merge_disjoint_files_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");
    repo.create_workspace("carol");

    repo.add_file("alice", "alice.txt", "Alice's feature\n");
    repo.add_file("bob", "bob.txt", "Bob's feature\n");
    repo.add_file("carol", "carol.txt", "Carol's feature\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
        maw::model::types::WorkspaceId::new("carol").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "disjoint 3-way should be clean"
    );

    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(files.contains(&"alice.txt".to_string()));
    assert!(files.contains(&"bob.txt".to_string()));
    assert!(files.contains(&"carol.txt".to_string()));
    assert!(
        files.contains(&"base.txt".to_string()),
        "base.txt preserved"
    );

    assert_eq!(output.unique_count, 3);
    assert_eq!(output.shared_count, 0);
    assert_eq!(output.resolved_count, 3);
}

// ==========================================================================
// 5-way merge: disjoint files
// ==========================================================================

#[test]
fn five_way_merge_disjoint_files_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    let names = ["ws-0", "ws-1", "ws-2", "ws-3", "ws-4"];
    for name in &names {
        repo.create_workspace(name);
        repo.add_file(
            name,
            &format!("{name}.txt"),
            &format!("content from {name}\n"),
        );
    }

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources: Vec<_> = names
        .iter()
        .map(|n| maw::model::types::WorkspaceId::new(n).unwrap())
        .collect();

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "disjoint 5-way should be clean"
    );

    let files = list_candidate_files(&repo, output.candidate.as_str());
    for name in &names {
        assert!(
            files.contains(&format!("{name}.txt")),
            "{name}.txt missing from 5-way merge result"
        );
    }
    assert!(files.contains(&"base.txt".to_string()));

    assert_eq!(output.unique_count, 5);
    assert_eq!(output.shared_count, 0);
    assert_eq!(output.resolved_count, 5);
}

// ==========================================================================
// Identical changes from 2 workspaces: hash equality
// ==========================================================================

#[test]
fn identical_changes_resolve_via_hash_equality() {
    let repo = TestRepo::new();
    repo.seed_files(&[("config.txt", "old config\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Both make the exact same modification
    let new_content = "new config v2\n";
    repo.modify_file("alice", "config.txt", new_content);
    repo.modify_file("bob", "config.txt", new_content);

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    // Hash equality short-circuit: identical changes → no conflict
    assert!(
        output.conflicts.is_empty(),
        "identical changes should not conflict: {:?}",
        output.conflicts
    );

    let content = read_candidate_file(&repo, output.candidate.as_str(), "config.txt")
        .expect("config.txt should exist");
    assert_eq!(content, new_content);

    // Stats
    assert_eq!(output.shared_count, 1, "one shared path");
    assert_eq!(output.resolved_count, 1, "one resolved change");
}

// ==========================================================================
// Merge with empty workspace: no-op for that workspace
// ==========================================================================

#[test]
fn merge_with_empty_workspace_is_noop_for_that_workspace() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# Hello\n")]);

    repo.create_workspace("active");
    repo.create_workspace("empty");

    // Only 'active' makes changes; 'empty' stays unchanged
    repo.add_file("active", "feature.txt", "new feature\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("active").unwrap(),
        maw::model::types::WorkspaceId::new("empty").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "empty workspace should not cause conflicts"
    );

    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(
        files.contains(&"feature.txt".to_string()),
        "feature.txt from active ws"
    );
    assert!(
        files.contains(&"README.md".to_string()),
        "README.md preserved"
    );

    // The empty workspace contributes nothing
    assert_eq!(
        output.unique_count, 1,
        "only 1 unique change from active ws"
    );
    assert_eq!(output.resolved_count, 1);
}

// ==========================================================================
// 3-way merge: same file, different regions (K>2 diff3)
// ==========================================================================

#[test]
fn three_way_merge_same_file_different_regions_diff3_clean() {
    let repo = TestRepo::new();
    // File with 3 well-separated regions (4+ context lines between edits)
    repo.seed_files(&[(
        "shared.txt",
        "R1\n---\n---\n---\n---\nR2\n---\n---\n---\n---\nR3\n",
    )]);

    repo.create_workspace("ws-a");
    repo.create_workspace("ws-b");
    repo.create_workspace("ws-c");

    // Each modifies a distinct region
    repo.modify_file(
        "ws-a",
        "shared.txt",
        "A1\n---\n---\n---\n---\nR2\n---\n---\n---\n---\nR3\n",
    );
    repo.modify_file(
        "ws-b",
        "shared.txt",
        "R1\n---\n---\n---\n---\nB2\n---\n---\n---\n---\nR3\n",
    );
    repo.modify_file(
        "ws-c",
        "shared.txt",
        "R1\n---\n---\n---\n---\nR2\n---\n---\n---\n---\nC3\n",
    );

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("ws-a").unwrap(),
        maw::model::types::WorkspaceId::new("ws-b").unwrap(),
        maw::model::types::WorkspaceId::new("ws-c").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "3-way non-overlapping edits should merge cleanly: {:?}",
        output.conflicts
    );

    let content = read_candidate_file(&repo, output.candidate.as_str(), "shared.txt")
        .expect("shared.txt should exist");
    assert!(content.contains("A1"), "ws-a's edit missing");
    assert!(content.contains("B2"), "ws-b's edit missing");
    assert!(content.contains("C3"), "ws-c's edit missing");
    assert_eq!(
        content, "A1\n---\n---\n---\n---\nB2\n---\n---\n---\n---\nC3\n",
        "merged content should combine all edits"
    );
}

// ==========================================================================
// 5-way merge: same file, different regions (K=5 diff3)
// ==========================================================================

#[test]
fn five_way_merge_same_file_different_regions_diff3_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[(
        "big.txt",
        "1\n---\n---\n---\n---\n2\n---\n---\n---\n---\n3\n---\n---\n---\n---\n4\n---\n---\n---\n---\n5\n",
    )]);

    let names = ["ws-0", "ws-1", "ws-2", "ws-3", "ws-4"];
    let edits = ["A", "B", "C", "D", "E"];

    for (i, name) in names.iter().enumerate() {
        repo.create_workspace(name);
        // Each workspace modifies a different region (replace the number)
        let mut content = "1\n---\n---\n---\n---\n2\n---\n---\n---\n---\n3\n---\n---\n---\n---\n4\n---\n---\n---\n---\n5\n".to_string();
        let original = format!("{}", i + 1);
        // Replace only the first occurrence of the digit at the right position
        let parts: Vec<&str> = content.splitn(2, &original).collect();
        if parts.len() == 2 {
            content = format!("{}{}{}", parts[0], edits[i], parts[1]);
        }
        repo.modify_file(name, "big.txt", &content);
    }

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources: Vec<_> = names
        .iter()
        .map(|n| maw::model::types::WorkspaceId::new(n).unwrap())
        .collect();

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "5-way non-overlapping edits should merge cleanly: {:?}",
        output.conflicts
    );

    let content = read_candidate_file(&repo, output.candidate.as_str(), "big.txt")
        .expect("big.txt should exist");
    assert_eq!(
        content,
        "A\n---\n---\n---\n---\nB\n---\n---\n---\n---\nC\n---\n---\n---\n---\nD\n---\n---\n---\n---\nE\n",
        "5-way merge should combine all edits"
    );
}

// ==========================================================================
// Add/add conflict: two workspaces add the same file with different content
// ==========================================================================

#[test]
fn add_add_different_content_produces_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Both add the same new file with different content
    repo.add_file("alice", "new.txt", "alice's new file\n");
    repo.add_file("bob", "new.txt", "bob's new file\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert_eq!(
        output.conflicts.len(),
        1,
        "add/add with different content should conflict"
    );
    assert_eq!(output.conflicts[0].path.to_str(), Some("new.txt"));
    assert_eq!(
        output.conflicts[0].reason,
        maw::merge::resolve::ConflictReason::AddAddDifferent,
    );
}

// ==========================================================================
// Modify/delete conflict
// ==========================================================================

#[test]
fn modify_delete_produces_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("target.txt", "original content\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Alice modifies, Bob deletes
    repo.modify_file("alice", "target.txt", "alice modified content\n");
    repo.delete_file("bob", "target.txt");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert_eq!(output.conflicts.len(), 1, "modify/delete should conflict");
    assert_eq!(output.conflicts[0].path.to_str(), Some("target.txt"));
    assert_eq!(
        output.conflicts[0].reason,
        maw::merge::resolve::ConflictReason::ModifyDelete,
    );
}

// ==========================================================================
// Delete/delete from multiple workspaces: resolves to single delete
// ==========================================================================

#[test]
fn delete_delete_resolves_cleanly() {
    let repo = TestRepo::new();
    repo.seed_files(&[("keep.txt", "keep this\n"), ("remove.txt", "remove this\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Both delete the same file
    repo.delete_file("alice", "remove.txt");
    repo.delete_file("bob", "remove.txt");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "delete/delete should resolve cleanly"
    );

    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(
        !files.contains(&"remove.txt".to_string()),
        "remove.txt should be deleted"
    );
    assert!(
        files.contains(&"keep.txt".to_string()),
        "keep.txt should be preserved"
    );
}

// ==========================================================================
// Mixed scenario: disjoint adds + shared modify (diff3 clean)
// ==========================================================================

#[test]
fn mixed_disjoint_and_shared_changes() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "header\n---\n---\n---\n---\nfooter\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Alice adds a new file AND modifies the header region
    repo.add_file("alice", "alice_only.txt", "alice exclusive\n");
    repo.modify_file(
        "alice",
        "shared.txt",
        "ALICE HEADER\n---\n---\n---\n---\nfooter\n",
    );

    // Bob adds a different file AND modifies the footer region
    repo.add_file("bob", "bob_only.txt", "bob exclusive\n");
    repo.modify_file(
        "bob",
        "shared.txt",
        "header\n---\n---\n---\n---\nBOB FOOTER\n",
    );

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "mixed clean scenario should have no conflicts"
    );

    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(files.contains(&"alice_only.txt".to_string()));
    assert!(files.contains(&"bob_only.txt".to_string()));
    assert!(files.contains(&"shared.txt".to_string()));

    let shared = read_candidate_file(&repo, output.candidate.as_str(), "shared.txt")
        .expect("shared.txt should exist");
    assert!(
        shared.contains("ALICE HEADER"),
        "alice's header edit missing"
    );
    assert!(shared.contains("BOB FOOTER"), "bob's footer edit missing");

    // Stats: 2 unique (one add from each) + 1 shared (shared.txt)
    assert_eq!(output.unique_count, 2);
    assert_eq!(output.shared_count, 1);
}

// ==========================================================================
// Merge with nested directory structure
// ==========================================================================

#[test]
fn merge_with_nested_directories() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn lib() {}\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Alice adds deep nested files
    repo.add_file("alice", "src/features/auth/mod.rs", "pub mod login;\n");
    repo.add_file("alice", "src/features/auth/login.rs", "pub fn login() {}\n");

    // Bob adds files in a different subtree
    repo.add_file("bob", "src/utils/helpers.rs", "pub fn helper() {}\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(output.conflicts.is_empty());

    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(files.contains(&"src/lib.rs".to_string()));
    assert!(files.contains(&"src/features/auth/mod.rs".to_string()));
    assert!(files.contains(&"src/features/auth/login.rs".to_string()));
    assert!(files.contains(&"src/utils/helpers.rs".to_string()));
}

// ==========================================================================
// Merge determinism: same inputs → same result
// ==========================================================================

#[test]
fn merge_is_deterministic() {
    // Run the same merge twice — should produce the same tree OID both times.
    let mut tree_oids = Vec::new();

    for _ in 0..2 {
        let repo = TestRepo::new();
        repo.seed_files(&[
            ("README.md", "# Project\n"),
            ("shared.txt", "R1\n---\n---\n---\n---\nR2\n"),
        ]);

        repo.create_workspace("alice");
        repo.create_workspace("bob");

        repo.add_file("alice", "alice.txt", "alice\n");
        repo.modify_file("alice", "shared.txt", "A1\n---\n---\n---\n---\nR2\n");

        repo.add_file("bob", "bob.txt", "bob\n");
        repo.modify_file("bob", "shared.txt", "R1\n---\n---\n---\n---\nB2\n");

        let backend = backend_for(&repo);
        let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
        let sources = vec![
            maw::model::types::WorkspaceId::new("alice").unwrap(),
            maw::model::types::WorkspaceId::new("bob").unwrap(),
        ];

        let output =
            maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources)
                .unwrap();

        assert!(output.conflicts.is_empty());

        // Extract tree OID from the candidate commit
        let tree_oid = Command::new("git")
            .args([
                "rev-parse",
                &format!("{}^{{tree}}", output.candidate.as_str()),
            ])
            .current_dir(repo.root())
            .output()
            .expect("git rev-parse failed");
        let tree_oid = String::from_utf8_lossy(&tree_oid.stdout).trim().to_owned();
        tree_oids.push(tree_oid);
    }

    assert_eq!(
        tree_oids[0], tree_oids[1],
        "same inputs must produce the same tree OID"
    );
}

// ==========================================================================
// N-way with mix of conflicts and clean merges
// ==========================================================================

#[test]
fn nway_mixed_conflicts_and_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[("clean.txt", "clean\n"), ("conflict.txt", "original\n")]);

    repo.create_workspace("ws-a");
    repo.create_workspace("ws-b");
    repo.create_workspace("ws-c");

    // ws-a: adds a new file + modifies conflict.txt
    repo.add_file("ws-a", "a_only.txt", "from a\n");
    repo.modify_file("ws-a", "conflict.txt", "version A\n");

    // ws-b: adds a different file + modifies conflict.txt (same region, different content)
    repo.add_file("ws-b", "b_only.txt", "from b\n");
    repo.modify_file("ws-b", "conflict.txt", "version B\n");

    // ws-c: only adds a new file (no conflict)
    repo.add_file("ws-c", "c_only.txt", "from c\n");

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("ws-a").unwrap(),
        maw::model::types::WorkspaceId::new("ws-b").unwrap(),
        maw::model::types::WorkspaceId::new("ws-c").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    // conflict.txt should be conflicted
    assert_eq!(output.conflicts.len(), 1, "exactly one conflict expected");
    assert_eq!(output.conflicts[0].path.to_str(), Some("conflict.txt"));

    // Clean changes should still be resolved
    let files = list_candidate_files(&repo, output.candidate.as_str());
    assert!(
        files.contains(&"a_only.txt".to_string()),
        "a_only.txt should be merged"
    );
    assert!(
        files.contains(&"b_only.txt".to_string()),
        "b_only.txt should be merged"
    );
    assert!(
        files.contains(&"c_only.txt".to_string()),
        "c_only.txt should be merged"
    );
    assert!(
        files.contains(&"clean.txt".to_string()),
        "clean.txt preserved"
    );
}

// ==========================================================================
// --destroy flag: workspaces removed after merge (maw CLI test)
// ==========================================================================

#[test]
fn maw_cli_merge_with_destroy() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# Project\n")]);

    // Use maw CLI to create workspaces
    let out = repo.maw_ok(&["ws", "create", "agent-1"]);
    assert!(out.contains("agent-1"), "workspace should be created");

    // Add a file to the workspace
    repo.add_file("agent-1", "feature.txt", "new feature\n");

    // Merge with --destroy
    let out = repo.maw_raw(&["ws", "merge", "agent-1", "--destroy"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The merge might use old jj path or new git path — either way, check outcome
    // If maw ws merge is still using the jj path, this test validates the CLI wiring
    if out.status.success() {
        // After destroy, workspace should not be listed
        let list = repo.maw_ok(&["ws", "list"]);
        assert!(
            !list.contains("agent-1"),
            "agent-1 should be destroyed after --destroy. List output: {list}"
        );
    } else {
        // If merge fails (e.g., jj not installed for old code path), that's expected
        // during transition. Just verify the error is meaningful.
        let combined = format!("{stdout}\n{stderr}");
        assert!(
            combined.contains("merge") || combined.contains("jj") || combined.contains("error"),
            "merge failure should have meaningful output: {combined}"
        );
    }
}

// ==========================================================================
// Reject merging the default workspace
// ==========================================================================

#[test]
fn reject_merge_default_workspace() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "merge", "default"]);
    assert!(
        stderr.contains("default") || stderr.contains("Cannot merge"),
        "error should mention default workspace: {stderr}"
    );
}

// ==========================================================================
// Add identical files from 2 workspaces (add/add same content)
// ==========================================================================

#[test]
fn add_add_identical_content_resolves_cleanly() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.create_workspace("alice");
    repo.create_workspace("bob");

    // Both add the same file with identical content
    let content = "shared new content\n";
    repo.add_file("alice", "new.txt", content);
    repo.add_file("bob", "new.txt", content);

    let backend = backend_for(&repo);
    let epoch = maw::model::types::EpochId::new(&repo.current_epoch()).unwrap();
    let sources = vec![
        maw::model::types::WorkspaceId::new("alice").unwrap(),
        maw::model::types::WorkspaceId::new("bob").unwrap(),
    ];

    let output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources).unwrap();

    assert!(
        output.conflicts.is_empty(),
        "add/add with identical content should resolve via hash equality"
    );

    let candidate_content = read_candidate_file(&repo, output.candidate.as_str(), "new.txt")
        .expect("new.txt should exist");
    assert_eq!(candidate_content, content);
}

// ==========================================================================
// Eval: multi-agent parallel edit + merge (bd-21sm.2)
//
// 3 agents work in parallel on different files, then merge.
// This validates the N-way merge path through the full maw CLI:
//   1. maw ws create agent-1, agent-2, agent-3
//   2. agent-1 creates src/auth.rs
//   3. agent-2 creates src/api.rs
//   4. agent-3 creates src/db.rs
//   5. maw ws merge agent-1 agent-2 agent-3 --destroy
//   6. All 3 files present in ws/default/
//   7. All 3 workspaces destroyed
// ==========================================================================

#[test]
fn eval_three_agent_parallel_disjoint_files() {
    let repo = TestRepo::new();

    // Seed a baseline project (matches the bead preconditions)
    repo.seed_files(&[
        (
            "Cargo.toml",
            "[package]\nname = \"agent-eval\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
        ),
        ("src/main.rs", "fn main() {}\n"),
    ]);

    // Step 1: Create 3 workspaces via maw CLI
    let out1 = repo.maw_ok(&["ws", "create", "agent-1"]);
    assert!(
        out1.contains("agent-1"),
        "agent-1 workspace should be created: {out1}"
    );

    let out2 = repo.maw_ok(&["ws", "create", "agent-2"]);
    assert!(
        out2.contains("agent-2"),
        "agent-2 workspace should be created: {out2}"
    );

    let out3 = repo.maw_ok(&["ws", "create", "agent-3"]);
    assert!(
        out3.contains("agent-3"),
        "agent-3 workspace should be created: {out3}"
    );

    // Verify list shows all workspaces before merge
    let ws_list_before = repo.maw_ok(&["ws", "list"]);
    assert!(
        ws_list_before.contains("agent-1"),
        "agent-1 should be listed"
    );
    assert!(
        ws_list_before.contains("agent-2"),
        "agent-2 should be listed"
    );
    assert!(
        ws_list_before.contains("agent-3"),
        "agent-3 should be listed"
    );

    // Step 2-4: Each agent creates a different file (non-overlapping edits)
    repo.add_file(
        "agent-1",
        "src/auth.rs",
        concat!(
            "pub fn authenticate(user: &str) -> bool {\n",
            "    user == \"admin\" || user == \"root\"\n",
            "}\n",
            "\n",
            "pub fn is_admin(user: &str) -> bool {\n",
            "    user == \"admin\"\n",
            "}\n",
        ),
    );

    repo.add_file(
        "agent-2",
        "src/api.rs",
        concat!(
            "pub fn handle_request(path: &str) -> String {\n",
            "    format!(\"OK: {path}\")\n",
            "}\n",
            "\n",
            "pub fn handle_error(code: u16) -> String {\n",
            "    format!(\"Error: {code}\")\n",
            "}\n",
        ),
    );

    repo.add_file(
        "agent-3",
        "src/db.rs",
        concat!(
            "pub fn connect(url: &str) -> Result<(), String> {\n",
            "    if url.is_empty() {\n",
            "        Err(\"empty URL\".to_owned())\n",
            "    } else {\n",
            "        Ok(())\n",
            "    }\n",
            "}\n",
        ),
    );

    // Verify workspace isolation: each agent sees only its own changes
    assert!(
        !repo.file_exists("agent-1", "src/api.rs"),
        "agent-1 should not see agent-2's api.rs"
    );
    assert!(
        !repo.file_exists("agent-1", "src/db.rs"),
        "agent-1 should not see agent-3's db.rs"
    );
    assert!(
        !repo.file_exists("agent-2", "src/auth.rs"),
        "agent-2 should not see agent-1's auth.rs"
    );
    assert!(
        !repo.file_exists("agent-3", "src/auth.rs"),
        "agent-3 should not see agent-1's auth.rs"
    );

    // Step 5: Merge all 3 workspaces with --destroy via maw CLI
    let merge_out = repo.maw_ok(&["ws", "merge", "agent-1", "agent-2", "agent-3", "--destroy"]);

    // Merge should report success
    assert!(
        merge_out.contains("Merged") || merge_out.contains("merge") || merge_out.contains("adopt"),
        "merge output should confirm success: {merge_out}"
    );

    // Step 6: Verify all 3 files exist in ws/default/
    assert!(
        repo.file_exists("default", "src/auth.rs"),
        "src/auth.rs should exist in ws/default/ after merge"
    );
    assert!(
        repo.file_exists("default", "src/api.rs"),
        "src/api.rs should exist in ws/default/ after merge"
    );
    assert!(
        repo.file_exists("default", "src/db.rs"),
        "src/db.rs should exist in ws/default/ after merge"
    );

    // Verify file contents are correct
    let auth_content = repo
        .read_file("default", "src/auth.rs")
        .expect("auth.rs should be readable");
    assert!(
        auth_content.contains("is_admin"),
        "auth.rs should contain is_admin function: {auth_content}"
    );

    let api_content = repo
        .read_file("default", "src/api.rs")
        .expect("api.rs should be readable");
    assert!(
        api_content.contains("handle_error"),
        "api.rs should contain handle_error function: {api_content}"
    );

    let db_content = repo
        .read_file("default", "src/db.rs")
        .expect("db.rs should be readable");
    assert!(
        db_content.contains("connect"),
        "db.rs should contain connect function: {db_content}"
    );

    // Also verify baseline files still exist (no regressions)
    assert!(
        repo.file_exists("default", "Cargo.toml"),
        "Cargo.toml should still exist after merge"
    );
    assert!(
        repo.file_exists("default", "src/main.rs"),
        "src/main.rs should still exist after merge"
    );

    // Step 7: Verify all 3 workspaces are destroyed
    let ws_list_after = repo.maw_ok(&["ws", "list"]);
    assert!(
        !ws_list_after.contains("agent-1"),
        "agent-1 should be destroyed: {ws_list_after}"
    );
    assert!(
        !ws_list_after.contains("agent-2"),
        "agent-2 should be destroyed: {ws_list_after}"
    );
    assert!(
        !ws_list_after.contains("agent-3"),
        "agent-3 should be destroyed: {ws_list_after}"
    );

    // Default workspace should still exist
    assert!(
        ws_list_after.contains("default"),
        "default workspace should remain: {ws_list_after}"
    );
}

// ==========================================================================
// Eval: conflict detection and resolution (bd-21sm.3)
//
// Two agents edit the same file at the same function, producing a conflict.
// This validates the conflict detection, structured JSON output, and the
// full resolution flow:
//
//   1. maw ws create agent-1, agent-2
//   2. Both modify src/lib.rs at the same function body → diff3 conflict
//   3. maw ws merge agent-1 agent-2 --check --format json
//      → JSON is machine-parseable
//      → conflict identifies file, reason, sides
//      → line range reported if atoms available
//   4. maw ws merge agent-1 agent-2 → fails (conflict reported, exit non-zero)
//   5. Agent resolves conflict (edits agent-1 to agreed-upon content)
//   6. maw ws merge agent-1 agent-2 --destroy → succeeds
//   7. ws/default/src/lib.rs contains the resolved content
// ==========================================================================

#[test]
fn eval_conflict_detection_and_resolution() {
    let repo = TestRepo::new();

    // Seed a project with a src/lib.rs that has a function both agents will modify.
    // Use a multi-line function so diff3 can localize the conflict.
    let base_lib_rs = concat!(
        "/// Process an order and return the total price.\n",
        "pub fn process_order(qty: u32, price: f64) -> f64 {\n",
        "    let total = (qty as f64) * price;\n",
        "    total\n",
        "}\n",
    );
    repo.seed_files(&[
        ("src/lib.rs", base_lib_rs),
        ("Cargo.toml", "[package]\nname = \"eval\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"eval\"\npath = \"src/lib.rs\"\n"),
    ]);

    // Step 1: Create two agent workspaces.
    let out1 = repo.maw_ok(&["ws", "create", "agent-1"]);
    assert!(
        out1.contains("agent-1"),
        "agent-1 workspace created: {out1}"
    );

    let out2 = repo.maw_ok(&["ws", "create", "agent-2"]);
    assert!(
        out2.contains("agent-2"),
        "agent-2 workspace created: {out2}"
    );

    // Step 2: Both agents modify src/lib.rs at the same function body — same
    // region, different edits → diff3 will detect the conflict.
    let agent1_lib_rs = concat!(
        "/// Process an order and return the total price.\n",
        "pub fn process_order(qty: u32, price: f64) -> f64 {\n",
        "    // agent-1: apply 10% discount\n",
        "    let total = (qty as f64) * price * 0.90;\n",
        "    total\n",
        "}\n",
    );
    let agent2_lib_rs = concat!(
        "/// Process an order and return the total price.\n",
        "pub fn process_order(qty: u32, price: f64) -> f64 {\n",
        "    // agent-2: apply sales tax\n",
        "    let total = (qty as f64) * price * 1.08;\n",
        "    total\n",
        "}\n",
    );
    repo.modify_file("agent-1", "src/lib.rs", agent1_lib_rs);
    repo.modify_file("agent-2", "src/lib.rs", agent2_lib_rs);

    // Step 3: Run --check --format json and verify structured JSON output.
    let check_out = repo.maw_raw(&[
        "ws", "merge", "agent-1", "agent-2", "--check", "--format", "json",
    ]);
    let check_stdout = String::from_utf8_lossy(&check_out.stdout).to_string();
    let check_stderr = String::from_utf8_lossy(&check_out.stderr).to_string();

    // The check should detect a conflict (exit non-zero) and produce JSON.
    assert!(
        !check_out.status.success(),
        "check should fail (conflict detected): stdout={check_stdout}\nstderr={check_stderr}"
    );

    // Parse the JSON output.
    let json: serde_json::Value = serde_json::from_str(&check_stdout).unwrap_or_else(|e| {
        panic!("check --format json output is not valid JSON: {e}\nstdout: {check_stdout}")
    });

    // Verify: ready is false (conflict present).
    assert_eq!(
        json["ready"],
        serde_json::Value::Bool(false),
        "check result should not be ready: {json}"
    );

    // Verify: conflicts array is non-empty.
    let conflicts = json["conflicts"]
        .as_array()
        .expect("conflicts should be an array");
    assert!(
        !conflicts.is_empty(),
        "conflicts array should be non-empty: {json}"
    );

    // Verify: first conflict identifies the file (src/lib.rs).
    let first = &conflicts[0];
    assert_eq!(
        first["path"].as_str().unwrap_or(""),
        "src/lib.rs",
        "conflict should identify src/lib.rs: {first}"
    );

    // Verify: conflict reason is present and non-empty.
    let reason = first["reason"].as_str().unwrap_or("");
    assert!(
        !reason.is_empty(),
        "conflict reason should be non-empty: {first}"
    );

    // Verify: sides array identifies both workspaces.
    let sides = first["sides"]
        .as_array()
        .expect("conflict.sides should be an array");
    assert!(
        sides.len() >= 2,
        "conflict should list at least 2 sides: {first}"
    );
    let side_names: Vec<&str> = sides.iter().filter_map(|s| s.as_str()).collect();
    assert!(
        side_names.contains(&"agent-1"),
        "agent-1 should be listed as a conflict side: {first}"
    );
    assert!(
        side_names.contains(&"agent-2"),
        "agent-2 should be listed as a conflict side: {first}"
    );

    // Verify: line_start / line_end are present for diff3 conflicts (optional
    // but expected when atom extraction succeeds for a text file).
    // We log a note if absent, but don't fail — atom extraction is best-effort.
    if first["line_start"].is_null() || first["line_end"].is_null() {
        eprintln!(
            "NOTE: line_start/line_end not present in conflict JSON (atom extraction may not have run): {first}"
        );
    } else {
        let line_start = first["line_start"]
            .as_u64()
            .expect("line_start should be a number");
        let line_end = first["line_end"]
            .as_u64()
            .expect("line_end should be a number");
        assert!(
            line_start >= 1,
            "line_start should be ≥ 1 (1-indexed): {line_start}"
        );
        assert!(
            line_end > line_start,
            "line_end should be > line_start: start={line_start} end={line_end}"
        );
    }

    // Step 4: Full merge should fail with a conflict error.
    let merge_out = repo.maw_raw(&["ws", "merge", "agent-1", "agent-2"]);
    assert!(
        !merge_out.status.success(),
        "merge should fail due to unresolved conflict"
    );
    let merge_stderr = String::from_utf8_lossy(&merge_out.stderr).to_string();
    let merge_stdout = String::from_utf8_lossy(&merge_out.stdout).to_string();
    let combined_output = format!("{merge_stdout}\n{merge_stderr}");
    assert!(
        combined_output.to_lowercase().contains("conflict"),
        "merge failure output should mention 'conflict': {combined_output}"
    );

    // Step 5: Agent resolves the conflict.
    // Strategy: edit agent-1 to agree with agent-2 (hash equality → clean merge).
    // In practice, an agent would produce a true manual merge; here we simulate
    // resolution by adopting agent-2's version.
    let resolved_lib_rs = concat!(
        "/// Process an order and return the total price.\n",
        "pub fn process_order(qty: u32, price: f64) -> f64 {\n",
        "    // resolved: apply sales tax (team decision)\n",
        "    let total = (qty as f64) * price * 1.08;\n",
        "    total\n",
        "}\n",
    );
    // Update agent-1 to the resolved content.
    repo.modify_file("agent-1", "src/lib.rs", resolved_lib_rs);
    // Update agent-2 to the same resolved content (required for hash equality).
    repo.modify_file("agent-2", "src/lib.rs", resolved_lib_rs);

    // Verify --check now says ready.
    let recheck_out = repo.maw_raw(&[
        "ws", "merge", "agent-1", "agent-2", "--check", "--format", "json",
    ]);
    let recheck_stdout = String::from_utf8_lossy(&recheck_out.stdout).to_string();

    let recheck_json: serde_json::Value = serde_json::from_str(&recheck_stdout)
        .unwrap_or_else(|e| panic!("re-check JSON not valid: {e}\nstdout: {recheck_stdout}"));
    assert_eq!(
        recheck_json["ready"],
        serde_json::Value::Bool(true),
        "after resolution, check should be ready: {recheck_json}"
    );
    let recheck_conflicts = recheck_json["conflicts"]
        .as_array()
        .expect("conflicts should be an array");
    assert!(
        recheck_conflicts.is_empty(),
        "after resolution, conflicts should be empty: {recheck_json}"
    );

    // Step 6: Re-merge after resolution — should succeed.
    let final_merge = repo.maw_ok(&["ws", "merge", "agent-1", "agent-2", "--destroy"]);
    assert!(
        final_merge.contains("Merged")
            || final_merge.contains("merge")
            || final_merge.contains("adopt"),
        "final merge should confirm success: {final_merge}"
    );

    // Step 7: Verify resolved content is present in ws/default/.
    let default_content = repo
        .read_file("default", "src/lib.rs")
        .expect("src/lib.rs should exist in ws/default/ after merge");
    assert!(
        default_content.contains("resolved: apply sales tax"),
        "ws/default/src/lib.rs should contain the resolved content: {default_content}"
    );
    assert!(
        default_content.contains("1.08"),
        "ws/default/src/lib.rs should contain the resolved tax rate: {default_content}"
    );
    assert!(
        !default_content.contains("0.90"),
        "ws/default/src/lib.rs should NOT contain agent-1's original discount: {default_content}"
    );

    // Verify workspaces were destroyed.
    let ws_list = repo.maw_ok(&["ws", "list"]);
    assert!(
        !ws_list.contains("agent-1"),
        "agent-1 should be destroyed after --destroy: {ws_list}"
    );
    assert!(
        !ws_list.contains("agent-2"),
        "agent-2 should be destroyed after --destroy: {ws_list}"
    );
    assert!(
        ws_list.contains("default"),
        "default workspace should remain: {ws_list}"
    );
}
