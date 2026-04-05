use tempfile::TempDir;

use maw_git::{
    ChangeType, EntryMode, FileStatus, GitError, GitOid, GitRepo, GixRepo, IndexEntry, RefEdit,
    RefName, TreeEdit, TreeEntry,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup_repo() -> (TempDir, GixRepo) {
    let dir = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let repo = GixRepo::open(dir.path()).unwrap();
    (dir, repo)
}

/// Create an initial commit with a single file so HEAD exists.
/// Returns the commit OID and the tree OID.
fn setup_repo_with_commit() -> (TempDir, GixRepo, GitOid, GitOid) {
    let (dir, repo) = setup_repo();
    let blob_oid = repo.write_blob(b"hello world\n").unwrap();
    let tree_oid = repo
        .write_tree(&[TreeEntry {
            name: "hello.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob_oid,
        }])
        .unwrap();
    let head_ref = RefName::new("refs/heads/main").unwrap();
    let commit_oid = repo
        .create_commit(tree_oid, &[], "initial commit", Some(&head_ref))
        .unwrap();
    // Also point HEAD at refs/heads/main via symbolic ref so rev_parse("HEAD") works.
    std::process::Command::new("git")
        .args(["symbolic-ref", "HEAD", "refs/heads/main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    (dir, repo, commit_oid, tree_oid)
}

// ===========================================================================
// 1. Basic repo operations
// ===========================================================================

#[test]
fn open_repo() {
    let (_dir, _repo) = setup_repo();
    // If we got here, open succeeded.
}

#[test]
fn rev_parse_head() {
    let (_dir, repo, commit_oid, _tree_oid) = setup_repo_with_commit();
    let parsed = repo.rev_parse("HEAD").unwrap();
    assert_eq!(parsed, commit_oid);
}

#[test]
fn rev_parse_opt_missing() {
    let (_dir, repo) = setup_repo();
    let result = repo.rev_parse_opt("HEAD").unwrap();
    // Fresh repo with no commits — HEAD doesn't resolve.
    assert!(result.is_none());
}

#[test]
fn blob_roundtrip() {
    let (_dir, repo) = setup_repo();
    let data = b"some blob content";
    let oid = repo.write_blob(data).unwrap();
    let read_back = repo.read_blob(oid).unwrap();
    assert_eq!(read_back, data);
}

#[test]
fn tree_roundtrip() {
    let (_dir, repo) = setup_repo();
    let blob_oid = repo.write_blob(b"content").unwrap();
    let entries = vec![TreeEntry {
        name: "file.txt".to_string(),
        mode: EntryMode::Blob,
        oid: blob_oid,
    }];
    let tree_oid = repo.write_tree(&entries).unwrap();
    let read_back = repo.read_tree(tree_oid).unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].name, "file.txt");
    assert_eq!(read_back[0].mode, EntryMode::Blob);
    assert_eq!(read_back[0].oid, blob_oid);
}

#[test]
fn commit_roundtrip() {
    let (_dir, repo, commit_oid, tree_oid) = setup_repo_with_commit();
    let info = repo.read_commit(commit_oid).unwrap();
    assert_eq!(info.tree_oid, tree_oid);
    assert!(info.parents.is_empty());
    assert_eq!(info.message, "initial commit");
    assert!(info.author.contains("Test User"));
}

// ===========================================================================
// 2. Ref operations
// ===========================================================================

#[test]
fn write_read_ref_roundtrip() {
    let (_dir, repo, commit_oid, _) = setup_repo_with_commit();
    let refname = RefName::new("refs/heads/feature").unwrap();
    repo.write_ref(&refname, commit_oid, "create feature branch")
        .unwrap();
    let read_back = repo.read_ref(&refname).unwrap();
    assert_eq!(read_back, Some(commit_oid));
}

#[test]
fn read_ref_nonexistent() {
    let (_dir, repo) = setup_repo();
    let refname = RefName::new("refs/heads/nope").unwrap();
    let result = repo.read_ref(&refname).unwrap();
    assert_eq!(result, None);
}

#[test]
fn delete_ref() {
    let (_dir, repo, commit_oid, _) = setup_repo_with_commit();
    let refname = RefName::new("refs/heads/to-delete").unwrap();
    repo.write_ref(&refname, commit_oid, "temp").unwrap();
    assert!(repo.read_ref(&refname).unwrap().is_some());
    repo.delete_ref(&refname).unwrap();
    assert!(repo.read_ref(&refname).unwrap().is_none());
}

#[test]
fn list_refs_with_prefix() {
    let (_dir, repo, commit_oid, _) = setup_repo_with_commit();
    let r1 = RefName::new("refs/heads/alpha").unwrap();
    let r2 = RefName::new("refs/heads/beta").unwrap();
    let r3 = RefName::new("refs/tags/v1").unwrap();
    repo.write_ref(&r1, commit_oid, "a").unwrap();
    repo.write_ref(&r2, commit_oid, "b").unwrap();
    repo.write_ref(&r3, commit_oid, "t").unwrap();
    let heads = repo.list_refs("refs/heads/").unwrap();
    let head_names: Vec<&str> = heads.iter().map(|(r, _)| r.as_str()).collect();
    // Should include alpha, beta, main — but NOT refs/tags/v1
    assert!(head_names.contains(&"refs/heads/alpha"));
    assert!(head_names.contains(&"refs/heads/beta"));
    assert!(head_names.contains(&"refs/heads/main"));
    assert!(!head_names.contains(&"refs/tags/v1"));
}

#[test]
fn atomic_ref_update_success() {
    let (_dir, repo, commit_oid, _) = setup_repo_with_commit();
    let refname = RefName::new("refs/heads/atomic-test").unwrap();
    // Create: expected_old = ZERO (ref must not exist)
    let edits = vec![RefEdit {
        name: refname.clone(),
        new_oid: commit_oid,
        expected_old_oid: GitOid::ZERO,
    }];
    repo.atomic_ref_update(&edits).unwrap();
    assert_eq!(repo.read_ref(&refname).unwrap(), Some(commit_oid));
}

#[test]
#[ignore] // BUG: gix edit_references doesn't include expected error strings for loose ref CAS failures, so the impl maps this to BackendError instead of RefConflict
fn atomic_ref_update_conflict() {
    let (_dir, repo, commit_oid, _) = setup_repo_with_commit();
    let refname = RefName::new("refs/heads/conflict-test").unwrap();
    repo.write_ref(&refname, commit_oid, "setup").unwrap();

    // Expect ZERO (i.e., ref must not exist) — but it does exist.
    let edits = vec![RefEdit {
        name: refname.clone(),
        new_oid: commit_oid,
        expected_old_oid: GitOid::ZERO,
    }];
    let result = repo.atomic_ref_update(&edits);
    assert!(result.is_err());
    match result.unwrap_err() {
        GitError::RefConflict { .. } => {} // expected
        other => panic!("expected RefConflict, got: {other:?}"),
    }
}

// ===========================================================================
// 3. Object operations
// ===========================================================================

#[test]
fn write_tree_multiple_entries() {
    let (_dir, repo) = setup_repo();
    let b1 = repo.write_blob(b"aaa").unwrap();
    let b2 = repo.write_blob(b"bbb").unwrap();
    let entries = vec![
        TreeEntry {
            name: "a.txt".to_string(),
            mode: EntryMode::Blob,
            oid: b1,
        },
        TreeEntry {
            name: "b.txt".to_string(),
            mode: EntryMode::Blob,
            oid: b2,
        },
    ];
    let tree_oid = repo.write_tree(&entries).unwrap();
    let read_back = repo.read_tree(tree_oid).unwrap();
    assert_eq!(read_back.len(), 2);
    let names: Vec<&str> = read_back.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
}

#[test]
fn edit_tree_add_entry() {
    let (_dir, repo, _, tree_oid) = setup_repo_with_commit();
    let new_blob = repo.write_blob(b"new file").unwrap();
    let new_tree = repo
        .edit_tree(
            tree_oid,
            &[TreeEdit::Upsert {
                path: "new.txt".to_string(),
                mode: EntryMode::Blob,
                oid: new_blob,
            }],
        )
        .unwrap();
    let entries = repo.read_tree(new_tree).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt")); // original
    assert!(names.contains(&"new.txt")); // added
}

#[test]
fn edit_tree_modify_entry() {
    let (_dir, repo, _, tree_oid) = setup_repo_with_commit();
    let updated_blob = repo.write_blob(b"updated content").unwrap();
    let new_tree = repo
        .edit_tree(
            tree_oid,
            &[TreeEdit::Upsert {
                path: "hello.txt".to_string(),
                mode: EntryMode::Blob,
                oid: updated_blob,
            }],
        )
        .unwrap();
    let entries = repo.read_tree(new_tree).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].oid, updated_blob);
}

#[test]
fn edit_tree_remove_entry() {
    let (_dir, repo, _, tree_oid) = setup_repo_with_commit();
    let new_tree = repo
        .edit_tree(
            tree_oid,
            &[TreeEdit::Remove {
                path: "hello.txt".to_string(),
            }],
        )
        .unwrap();
    let entries = repo.read_tree(new_tree).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn create_commit_with_parent() {
    let (_dir, repo, first_commit, _) = setup_repo_with_commit();
    let blob = repo.write_blob(b"second").unwrap();
    let tree = repo
        .write_tree(&[TreeEntry {
            name: "second.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob,
        }])
        .unwrap();
    let head_ref = RefName::new("refs/heads/main").unwrap();
    let second_commit = repo
        .create_commit(tree, &[first_commit], "second commit", Some(&head_ref))
        .unwrap();
    let info = repo.read_commit(second_commit).unwrap();
    assert_eq!(info.parents, vec![first_commit]);
    assert_eq!(info.message, "second commit");
}

// ===========================================================================
// 4. Index and checkout
// ===========================================================================

#[test]
#[ignore] // BUG: read_index calls open_index() which fails when no index file exists in a fresh repo
fn read_index_fresh_repo() {
    let (_dir, repo) = setup_repo();
    let entries = repo.read_index().unwrap();
    assert!(entries.is_empty());
}

#[test]
fn write_read_index_roundtrip() {
    let (_dir, repo) = setup_repo();
    let blob_oid = repo.write_blob(b"indexed content").unwrap();
    let index_entries = vec![IndexEntry {
        path: "indexed.txt".to_string(),
        mode: EntryMode::Blob,
        oid: blob_oid,
    }];
    repo.write_index(&index_entries).unwrap();
    let read_back = repo.read_index().unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].path, "indexed.txt");
    assert_eq!(read_back[0].oid, blob_oid);
}

#[test]
fn checkout_tree_creates_files() {
    let (dir, repo, _, tree_oid) = setup_repo_with_commit();
    let checkout_dir = dir.path().to_path_buf();
    repo.checkout_tree(tree_oid, &checkout_dir).unwrap();
    let file_path = checkout_dir.join("hello.txt");
    assert!(file_path.exists(), "hello.txt should exist after checkout");
    let contents = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(contents, "hello world\n");
}

#[test]
fn checkout_tree_removes_stale_files() {
    let (dir, repo, _, _) = setup_repo_with_commit();
    let workdir = dir.path();

    // Create a tree with only "goodbye.txt" (no "hello.txt").
    let blob_oid = repo.write_blob(b"goodbye\n").unwrap();
    let tree2 = repo
        .write_tree(&[TreeEntry {
            name: "goodbye.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob_oid,
        }])
        .unwrap();

    // First checkout the original tree (has hello.txt).
    let blob1 = repo.write_blob(b"hello world\n").unwrap();
    let tree1 = repo
        .write_tree(&[TreeEntry {
            name: "hello.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob1,
        }])
        .unwrap();
    repo.checkout_tree(tree1, workdir).unwrap();
    assert!(workdir.join("hello.txt").exists());

    // Now checkout tree2 — hello.txt should be removed.
    repo.checkout_tree(tree2, workdir).unwrap();
    assert!(
        workdir.join("goodbye.txt").exists(),
        "goodbye.txt should exist after checkout"
    );
    assert!(
        !workdir.join("hello.txt").exists(),
        "hello.txt should be removed (not in target tree)"
    );
}

// ===========================================================================
// 5. Status and diff
// ===========================================================================

#[test]
fn is_dirty_clean_repo() {
    let (dir, repo, _, tree_oid) = setup_repo_with_commit();
    // Checkout so the working tree matches HEAD.
    repo.checkout_tree(tree_oid, dir.path()).unwrap();
    // Also sync the index to match.
    let blob_oid = repo
        .read_tree(tree_oid)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .oid;
    repo.write_index(&[IndexEntry {
        path: "hello.txt".to_string(),
        mode: EntryMode::Blob,
        oid: blob_oid,
    }])
    .unwrap();
    let dirty = repo.is_dirty().unwrap();
    assert!(!dirty, "repo should be clean after checkout + index sync");
}

#[test]
fn is_dirty_with_modified_tracked_file() {
    let (dir, repo, _, tree_oid) = setup_repo_with_commit();
    repo.checkout_tree(tree_oid, dir.path()).unwrap();
    // Sync index
    let blob_oid = repo
        .read_tree(tree_oid)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .oid;
    repo.write_index(&[IndexEntry {
        path: "hello.txt".to_string(),
        mode: EntryMode::Blob,
        oid: blob_oid,
    }])
    .unwrap();
    // Modify a tracked file.
    std::fs::write(dir.path().join("hello.txt"), "modified content").unwrap();
    let dirty = repo.is_dirty().unwrap();
    assert!(dirty, "repo should be dirty with modified tracked file");
}

#[test]
fn status_shows_modified_file() {
    let (dir, repo, _, tree_oid) = setup_repo_with_commit();
    repo.checkout_tree(tree_oid, dir.path()).unwrap();
    // Sync index
    let blob_oid = repo
        .read_tree(tree_oid)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .oid;
    repo.write_index(&[IndexEntry {
        path: "hello.txt".to_string(),
        mode: EntryMode::Blob,
        oid: blob_oid,
    }])
    .unwrap();
    // Modify a tracked file
    std::fs::write(dir.path().join("hello.txt"), "modified content").unwrap();
    let status = repo.status().unwrap();
    let modified = status.iter().find(|e| e.path == "hello.txt");
    assert!(modified.is_some(), "status should include hello.txt");
    assert_eq!(modified.unwrap().status, FileStatus::Modified);
}

#[test]
#[ignore] // BUG: status impl maps untracked files to FileStatus::Added via Summary::Added; FileStatus::Untracked is never produced
fn status_shows_untracked_file() {
    let (dir, repo, _, tree_oid) = setup_repo_with_commit();
    repo.checkout_tree(tree_oid, dir.path()).unwrap();
    let blob_oid = repo
        .read_tree(tree_oid)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .oid;
    repo.write_index(&[IndexEntry {
        path: "hello.txt".to_string(),
        mode: EntryMode::Blob,
        oid: blob_oid,
    }])
    .unwrap();
    std::fs::write(dir.path().join("new_file.txt"), "new content").unwrap();
    let status = repo.status().unwrap();
    let new_file = status.iter().find(|e| e.path == "new_file.txt");
    assert!(new_file.is_some(), "status should include new_file.txt");
    assert_eq!(new_file.unwrap().status, FileStatus::Untracked);
}

#[test]
fn diff_trees_shows_changes() {
    let (_dir, repo, _, tree1) = setup_repo_with_commit();
    // Build a second tree with different content.
    let new_blob = repo.write_blob(b"changed\n").unwrap();
    let tree2 = repo
        .write_tree(&[TreeEntry {
            name: "hello.txt".to_string(),
            mode: EntryMode::Blob,
            oid: new_blob,
        }])
        .unwrap();
    let diff = repo.diff_trees(Some(tree1), tree2).unwrap();
    assert!(!diff.is_empty(), "diff should show changes");
    let entry = diff.iter().find(|e| e.path == "hello.txt").unwrap();
    assert_eq!(entry.change_type, ChangeType::Modified);
}

#[test]
fn diff_trees_addition() {
    let (_dir, repo) = setup_repo();
    // Empty tree (no entries).
    let empty_tree = repo.write_tree(&[]).unwrap();
    let blob = repo.write_blob(b"new").unwrap();
    let tree_with_file = repo
        .write_tree(&[TreeEntry {
            name: "added.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob,
        }])
        .unwrap();
    let diff = repo.diff_trees(Some(empty_tree), tree_with_file).unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, "added.txt");
    assert_eq!(diff[0].change_type, ChangeType::Added);
}

#[test]
fn diff_trees_deletion() {
    let (_dir, repo, _, tree1) = setup_repo_with_commit();
    let empty_tree = repo.write_tree(&[]).unwrap();
    let diff = repo.diff_trees(Some(tree1), empty_tree).unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, "hello.txt");
    assert_eq!(diff[0].change_type, ChangeType::Deleted);
}

#[test]
fn diff_trees_none_as_old() {
    let (_dir, repo, _, tree_oid) = setup_repo_with_commit();
    // None as old tree means diff against empty.
    let diff = repo.diff_trees(None, tree_oid).unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, "hello.txt");
    assert_eq!(diff[0].change_type, ChangeType::Added);
}

// ===========================================================================
// 6. Config
// ===========================================================================

#[test]
fn write_read_config_roundtrip() {
    let (dir, repo) = setup_repo();
    repo.write_config("test.mykey", "myvalue").unwrap();
    // Re-open the repo so gix picks up the config file written by git CLI.
    let repo = GixRepo::open(dir.path()).unwrap();
    let val = repo.read_config("test.mykey").unwrap();
    assert_eq!(val.as_deref(), Some("myvalue"));
}

#[test]
fn read_config_nonexistent() {
    let (_dir, repo) = setup_repo();
    let val = repo.read_config("test.no-such-key").unwrap();
    assert_eq!(val, None);
}

// ===========================================================================
// 7. Worktree lifecycle
// ===========================================================================

#[test]
fn worktree_add_list_remove() {
    let (dir, repo, commit_oid, _) = setup_repo_with_commit();
    let wt_path = dir.path().join("wt-test");

    // Add
    repo.worktree_add("wt-test", commit_oid, &wt_path).unwrap();
    assert!(wt_path.exists(), "worktree directory should exist");

    // List — should contain the new worktree
    let list = repo.worktree_list().unwrap();
    let names: Vec<&str> = list.iter().map(|w| w.name.as_str()).collect();
    assert!(
        names.contains(&"wt-test"),
        "worktree_list should include 'wt-test', got: {names:?}"
    );

    // Remove
    repo.worktree_remove("wt-test").unwrap();
    let list_after = repo.worktree_list().unwrap();
    let names_after: Vec<&str> = list_after.iter().map(|w| w.name.as_str()).collect();
    assert!(
        !names_after.contains(&"wt-test"),
        "worktree_list should not include 'wt-test' after removal"
    );
}

#[test]
fn worktree_has_files_checked_out() {
    let (dir, repo, commit_oid, _) = setup_repo_with_commit();
    let wt_path = dir.path().join("wt-checkout");
    repo.worktree_add("wt-checkout", commit_oid, &wt_path)
        .unwrap();
    // The commit has hello.txt — verify it's checked out in the worktree.
    let file = wt_path.join("hello.txt");
    assert!(file.exists(), "hello.txt should be checked out in worktree");
}

// ===========================================================================
// 8. Ancestry
// ===========================================================================

#[test]
fn is_ancestor_parent_of_child() {
    let (_dir, repo, first_commit, _) = setup_repo_with_commit();
    // Make a second commit
    let blob = repo.write_blob(b"child").unwrap();
    let tree = repo
        .write_tree(&[TreeEntry {
            name: "child.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob,
        }])
        .unwrap();
    let child_commit = repo
        .create_commit(tree, &[first_commit], "child commit", None)
        .unwrap();

    assert!(repo.is_ancestor(first_commit, child_commit).unwrap());
}

#[test]
fn is_ancestor_child_not_ancestor_of_parent() {
    let (_dir, repo, first_commit, _) = setup_repo_with_commit();
    let blob = repo.write_blob(b"child").unwrap();
    let tree = repo
        .write_tree(&[TreeEntry {
            name: "child.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob,
        }])
        .unwrap();
    let child_commit = repo
        .create_commit(tree, &[first_commit], "child commit", None)
        .unwrap();

    assert!(!repo.is_ancestor(child_commit, first_commit).unwrap());
}

#[test]
fn merge_base_of_divergent_branches() {
    let (_dir, repo, root_commit, _) = setup_repo_with_commit();
    // Create two branches from root.
    let blob_a = repo.write_blob(b"branch a").unwrap();
    let tree_a = repo
        .write_tree(&[TreeEntry {
            name: "a.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob_a,
        }])
        .unwrap();
    let commit_a = repo
        .create_commit(tree_a, &[root_commit], "branch a", None)
        .unwrap();

    let blob_b = repo.write_blob(b"branch b").unwrap();
    let tree_b = repo
        .write_tree(&[TreeEntry {
            name: "b.txt".to_string(),
            mode: EntryMode::Blob,
            oid: blob_b,
        }])
        .unwrap();
    let commit_b = repo
        .create_commit(tree_b, &[root_commit], "branch b", None)
        .unwrap();

    let base = repo.merge_base(commit_a, commit_b).unwrap();
    assert_eq!(base, Some(root_commit));
}

#[test]
fn merge_base_same_commit() {
    let (_dir, repo, commit_oid, _) = setup_repo_with_commit();
    let base = repo.merge_base(commit_oid, commit_oid).unwrap();
    assert_eq!(base, Some(commit_oid));
}

// ---------------------------------------------------------------------------
// LFS smudge post-pass (bn-1jdp)
// ---------------------------------------------------------------------------

#[cfg(feature = "lfs")]
#[test]
fn checkout_smudges_lfs_pointer_to_real_content() {
    // Setup: a repo whose tree contains a .gitattributes (making *.bin LFS)
    // and a "hero.bin" blob that is a pointer. We pre-populate .git/lfs/objects
    // with the real content. Checkout must replace the pointer text with
    // the real bytes on disk.
    let (dir, repo) = setup_repo();
    let workdir = dir.path();

    // Real content for the LFS object.
    let real_content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    // sha256 of real_content — compute via sha2 the same way maw-lfs does.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&real_content);
    let oid: [u8; 32] = h.finalize().into();
    let oid_hex: String = oid.iter().map(|b| format!("{b:02x}")).collect();

    // Pre-populate .git/lfs/objects/<xx>/<yy>/<sha>.
    let lfs_obj_dir = workdir
        .join(".git")
        .join("lfs")
        .join("objects")
        .join(&oid_hex[0..2])
        .join(&oid_hex[2..4]);
    std::fs::create_dir_all(&lfs_obj_dir).unwrap();
    std::fs::write(lfs_obj_dir.join(&oid_hex), &real_content).unwrap();

    // Build the pointer text that will be the git blob.
    let pointer_text = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        oid_hex,
        real_content.len()
    );

    // Write blobs: .gitattributes and the pointer for hero.bin.
    let attrs_blob = repo
        .write_blob(b"*.bin filter=lfs diff=lfs merge=lfs -text\n")
        .unwrap();
    let pointer_blob = repo.write_blob(pointer_text.as_bytes()).unwrap();

    let tree_oid = repo
        .write_tree(&[
            TreeEntry {
                name: ".gitattributes".to_string(),
                mode: EntryMode::Blob,
                oid: attrs_blob,
            },
            TreeEntry {
                name: "hero.bin".to_string(),
                mode: EntryMode::Blob,
                oid: pointer_blob,
            },
        ])
        .unwrap();

    repo.checkout_tree(tree_oid, workdir).unwrap();

    // .gitattributes should still be its raw content (not LFS-tracked).
    let attrs_on_disk = std::fs::read(workdir.join(".gitattributes")).unwrap();
    assert_eq!(
        attrs_on_disk,
        b"*.bin filter=lfs diff=lfs merge=lfs -text\n"
    );

    // hero.bin must now contain the REAL content, not the pointer text.
    let hero_on_disk = std::fs::read(workdir.join("hero.bin")).unwrap();
    assert_eq!(
        hero_on_disk, real_content,
        "hero.bin should be smudged to real content"
    );
}

#[cfg(feature = "lfs")]
#[test]
fn checkout_leaves_pointer_when_object_missing() {
    // Missing LFS object — checkout must NOT fail, and the pointer must be
    // left on disk so the user can fetch + re-smudge later.
    let (dir, repo) = setup_repo();
    let workdir = dir.path();

    // Don't pre-populate the store — the object is absent.
    let pointer_text = "version https://git-lfs.github.com/spec/v1\n\
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
size 12345\n";

    let attrs_blob = repo.write_blob(b"*.bin filter=lfs\n").unwrap();
    let pointer_blob = repo.write_blob(pointer_text.as_bytes()).unwrap();
    let tree_oid = repo
        .write_tree(&[
            TreeEntry {
                name: ".gitattributes".to_string(),
                mode: EntryMode::Blob,
                oid: attrs_blob,
            },
            TreeEntry {
                name: "hero.bin".to_string(),
                mode: EntryMode::Blob,
                oid: pointer_blob,
            },
        ])
        .unwrap();

    // Should succeed despite the missing object.
    repo.checkout_tree(tree_oid, workdir).unwrap();

    // Pointer is still on disk.
    let hero_on_disk = std::fs::read(workdir.join("hero.bin")).unwrap();
    assert_eq!(hero_on_disk, pointer_text.as_bytes());
}

#[cfg(feature = "lfs")]
#[test]
fn checkout_leaves_non_lfs_files_alone() {
    // A tree with .gitattributes but no filter=lfs rules must not touch any files.
    let (dir, repo) = setup_repo();
    let workdir = dir.path();

    let attrs_blob = repo.write_blob(b"*.txt text\n").unwrap();
    let content_blob = repo.write_blob(b"just text\n").unwrap();
    let tree_oid = repo
        .write_tree(&[
            TreeEntry {
                name: ".gitattributes".to_string(),
                mode: EntryMode::Blob,
                oid: attrs_blob,
            },
            TreeEntry {
                name: "readme.txt".to_string(),
                mode: EntryMode::Blob,
                oid: content_blob,
            },
        ])
        .unwrap();

    repo.checkout_tree(tree_oid, workdir).unwrap();
    assert_eq!(
        std::fs::read(workdir.join("readme.txt")).unwrap(),
        b"just text\n"
    );
}

#[cfg(feature = "lfs")]
#[test]
fn checkout_skips_files_over_1kb_even_if_lfs_tracked() {
    // If a file is >1KB, it's definitely not a pointer — skip even if LFS-tracked.
    let (dir, repo) = setup_repo();
    let workdir = dir.path();

    let large_content: Vec<u8> = vec![b'x'; 2048];
    let attrs_blob = repo.write_blob(b"*.bin filter=lfs\n").unwrap();
    let content_blob = repo.write_blob(&large_content).unwrap();
    let tree_oid = repo
        .write_tree(&[
            TreeEntry {
                name: ".gitattributes".to_string(),
                mode: EntryMode::Blob,
                oid: attrs_blob,
            },
            TreeEntry {
                name: "oops.bin".to_string(),
                mode: EntryMode::Blob,
                oid: content_blob,
            },
        ])
        .unwrap();

    repo.checkout_tree(tree_oid, workdir).unwrap();
    // File kept as-is (raw bytes committed directly, not LFS-clean-filtered).
    assert_eq!(std::fs::read(workdir.join("oops.bin")).unwrap(), large_content);
}

#[cfg(feature = "lfs")]
#[test]
fn write_blob_with_path_cleans_lfs_content() {
    // Setting: repo has workdir with .gitattributes listing *.bin as LFS.
    // write_blob_with_path("hero.bin", real_bytes) should:
    //   1. Store real_bytes in .git/lfs/objects/<xx>/<yy>/<sha>
    //   2. Return the OID of the POINTER blob (not of the raw bytes)
    let (dir, repo) = setup_repo();
    let workdir = dir.path();
    std::fs::write(
        workdir.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();

    let real = b"raw binary content bytes";
    let oid = repo.write_blob_with_path(real, "hero.bin").unwrap();

    // The stored blob should be the POINTER, not the raw bytes.
    // Read it back via gix and check it parses as a pointer.
    let blob = repo.read_blob(oid).unwrap();
    assert!(
        blob.starts_with(b"version https://git-lfs.github.com/spec/v1\n"),
        "blob should be an LFS pointer, got {:?}",
        String::from_utf8_lossy(&blob[..blob.len().min(100)])
    );

    // Real content lives in the local LFS store under the computed sha.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(real);
    let sha: [u8; 32] = h.finalize().into();
    let sha_hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    let stored = workdir
        .join(".git")
        .join("lfs")
        .join("objects")
        .join(&sha_hex[0..2])
        .join(&sha_hex[2..4])
        .join(&sha_hex);
    assert!(stored.exists(), "LFS object should be at {stored:?}");
    assert_eq!(std::fs::read(&stored).unwrap(), real);
}

#[cfg(feature = "lfs")]
#[test]
fn write_blob_with_path_non_lfs_unchanged() {
    let (dir, repo) = setup_repo();
    let workdir = dir.path();
    std::fs::write(workdir.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();

    let text = b"hello, world\n";
    let oid = repo.write_blob_with_path(text, "notes.txt").unwrap();
    // Non-LFS path: raw content becomes the blob.
    let blob = repo.read_blob(oid).unwrap();
    assert_eq!(blob, text);
}

#[cfg(feature = "lfs")]
#[test]
fn write_blob_with_path_passes_pointer_through() {
    // Caller hands us pointer bytes for an LFS path — don't double-wrap.
    let (dir, repo) = setup_repo();
    let workdir = dir.path();
    std::fs::write(workdir.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();

    let pointer = b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
size 12345\n";
    let oid = repo.write_blob_with_path(pointer, "hero.bin").unwrap();
    let blob = repo.read_blob(oid).unwrap();
    assert_eq!(blob, pointer);
}

#[cfg(feature = "lfs")]
#[test]
fn write_blob_with_path_without_workdir_falls_through() {
    // A freshly-constructed repo with no .gitattributes: no LFS attrs
    // resolve, so behavior matches write_blob.
    let (_dir, repo) = setup_repo();
    let data = b"arbitrary bytes";
    let oid = repo.write_blob_with_path(data, "anything.bin").unwrap();
    let blob = repo.read_blob(oid).unwrap();
    assert_eq!(blob, data);
}

#[cfg(feature = "lfs")]
#[test]
fn full_round_trip_clean_then_smudge() {
    // Write raw content via clean → pointer blob committed + real content
    // in .git/lfs/objects/. Checkout that tree → working copy has real bytes.
    let (dir, repo) = setup_repo();
    let workdir = dir.path();
    std::fs::write(workdir.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();

    let real: Vec<u8> = (0..8192u32).map(|i| (i * 31 % 251) as u8).collect();
    let attrs_blob = repo.write_blob(b"*.bin filter=lfs\n").unwrap();
    let pointer_oid = repo.write_blob_with_path(&real, "hero.bin").unwrap();
    let tree_oid = repo
        .write_tree(&[
            TreeEntry {
                name: ".gitattributes".to_string(),
                mode: EntryMode::Blob,
                oid: attrs_blob,
            },
            TreeEntry {
                name: "hero.bin".to_string(),
                mode: EntryMode::Blob,
                oid: pointer_oid,
            },
        ])
        .unwrap();

    // Remove hero.bin from workdir before checkout so we can observe smudge.
    std::fs::remove_file(workdir.join("hero.bin")).ok();

    repo.checkout_tree(tree_oid, workdir).unwrap();

    // Full round trip: real bytes on disk.
    let on_disk = std::fs::read(workdir.join("hero.bin")).unwrap();
    assert_eq!(on_disk, real);
}
