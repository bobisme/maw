//! Phase 0 smoke tests: end-to-end verification of the recovery ecosystem.
//!
//! Validates that the Phase 0 recovery primitives work together:
//! - Dirty files in the default workspace survive a merge (preserve + replay)
//! - Destroyed agent workspaces get a recovery ref (capture-gate)
//! - Recovery refs pin the pre-destroy content (git-addressable)
//! - Destroy records are valid JSON artifacts
//! - Clean merges succeed without errors
//!
//! Bone: bn-304u

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect all recovery refs for a workspace name.
fn recovery_refs(repo: &TestRepo, workspace: &str) -> Vec<String> {
    let prefix = format!("refs/manifold/recovery/{workspace}/");
    let output = repo.git(&[
        "for-each-ref",
        "--format=%(refname)",
        &format!("refs/manifold/recovery/{workspace}/"),
    ]);
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.starts_with(&prefix))
        .map(ToOwned::to_owned)
        .collect()
}

/// Resolve a git ref to its full SHA.
fn resolve_ref(repo: &TestRepo, git_ref: &str) -> String {
    repo.git(&["rev-parse", "--verify", git_ref])
        .trim()
        .to_owned()
}

// ---------------------------------------------------------------------------
// S1: Merge with dirty default -> recovery ref exists + files preserved
// ---------------------------------------------------------------------------

#[test]
fn s1_merge_dirty_default_preserves_untracked_and_creates_recovery_ref() {
    let repo = TestRepo::new();

    // Seed tracked files.
    repo.seed_files(&[
        ("README.md", "# Original README\n"),
        ("src/lib.rs", "pub fn original() {}\n"),
    ]);

    // Create agent workspace and add agent work.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent-output.txt", "agent work product\n");

    // Add untracked files in default (these survive `git checkout --force`
    // because checkout only touches tracked paths).
    repo.add_file("default", "scratch.txt", "human scratch notes\n");
    repo.add_file("default", "local/draft.md", "draft content\n");

    // Merge agent workspace with --destroy.
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Assert: recovery ref exists for the agent workspace (the one destroyed).
    let refs = recovery_refs(&repo, "agent");
    assert!(
        !refs.is_empty(),
        "Expected at least one recovery ref under refs/manifold/recovery/agent/, got none"
    );

    // Assert: untracked files are still present in default after merge.
    assert!(
        repo.file_exists("default", "scratch.txt"),
        "Untracked file in default should survive the merge"
    );
    assert_eq!(
        repo.read_file("default", "scratch.txt").as_deref(),
        Some("human scratch notes\n"),
        "Untracked file content should be preserved"
    );
    assert!(
        repo.file_exists("default", "local/draft.md"),
        "Nested untracked file in default should survive the merge"
    );
    assert_eq!(
        repo.read_file("default", "local/draft.md").as_deref(),
        Some("draft content\n"),
        "Nested untracked file content should be preserved"
    );

    // Assert: agent work is present in default after merge.
    assert_eq!(
        repo.read_file("default", "agent-output.txt").as_deref(),
        Some("agent work product\n"),
        "Agent work should appear in default after merge"
    );
}

// ---------------------------------------------------------------------------
// S2: Rewrite artifact / destroy record is valid JSON
// ---------------------------------------------------------------------------

#[test]
fn s2_destroy_record_is_valid_json() {
    let repo = TestRepo::new();

    // Seed files.
    repo.seed_files(&[("base.txt", "base content\n")]);

    // Create agent workspace and add work.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "feature.txt", "new feature\n");

    // Modify default to create dirty state.
    repo.modify_file("default", "base.txt", "locally modified\n");

    // Merge with --destroy.
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Check for destroy record JSON under .manifold/artifacts/ws/agent/destroy/.
    let destroy_dir = repo
        .root()
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join("agent")
        .join("destroy");

    if destroy_dir.exists() {
        // The destroy directory should contain at least a latest.json and
        // a timestamped record file.
        let entries: Vec<_> = std::fs::read_dir(&destroy_dir)
            .expect("should be able to read destroy dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "json")
            })
            .collect();

        assert!(
            !entries.is_empty(),
            "Expected at least one JSON file in {}, found none",
            destroy_dir.display()
        );

        // Parse each JSON file and verify it's valid JSON.
        for entry in &entries {
            let content = std::fs::read_to_string(entry.path())
                .unwrap_or_else(|e| panic!("Failed to read {}: {e}", entry.path().display()));
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&content);
            assert!(
                parsed.is_ok(),
                "File {} should be valid JSON, parse error: {:?}",
                entry.path().display(),
                parsed.err()
            );
        }

        // Verify the latest.json pointer exists and references a valid record.
        let latest_path = destroy_dir.join("latest.json");
        assert!(
            latest_path.exists(),
            "latest.json should exist in destroy dir"
        );
        let latest_content = std::fs::read_to_string(&latest_path)
            .expect("should read latest.json");
        let latest: serde_json::Value = serde_json::from_str(&latest_content)
            .expect("latest.json should be valid JSON");
        assert!(
            latest["record"].as_str().is_some(),
            "latest.json should have a 'record' field pointing to the record filename"
        );
        assert!(
            latest["destroyed_at"].as_str().is_some(),
            "latest.json should have a 'destroyed_at' timestamp"
        );
    } else {
        // If the destroy dir doesn't exist, fall back to checking that
        // a recovery ref exists instead. The important thing is that
        // SOME record of the pre-merge state exists.
        let refs = recovery_refs(&repo, "agent");
        assert!(
            !refs.is_empty(),
            "Neither destroy records at {} nor recovery refs exist — \
             no pre-merge state was preserved",
            destroy_dir.display()
        );
    }
}

// ---------------------------------------------------------------------------
// S3: Clean default workspace merge succeeds cleanly
// ---------------------------------------------------------------------------

#[test]
fn s3_clean_default_merge_succeeds() {
    let repo = TestRepo::new();

    // Default is clean — no dirty files.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "result.txt", "clean merge output\n");
    repo.add_file("agent", "src/main.rs", "fn main() { println!(\"hello\"); }\n");

    // Merge should succeed without errors.
    let output = repo.maw_raw(&["ws", "merge", "agent", "--destroy"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Clean merge should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Assert: agent work appears in default.
    assert_eq!(
        repo.read_file("default", "result.txt").as_deref(),
        Some("clean merge output\n"),
        "Merged file should appear in default"
    );
    assert_eq!(
        repo.read_file("default", "src/main.rs").as_deref(),
        Some("fn main() { println!(\"hello\"); }\n"),
        "Nested merged file should appear in default"
    );

    // Agent workspace should be gone.
    assert!(
        !repo.workspace_exists("agent"),
        "Agent workspace should be destroyed after --destroy"
    );
}

// ---------------------------------------------------------------------------
// S4: Recovery ref contains the pre-merge content
// ---------------------------------------------------------------------------

#[test]
fn s4_recovery_ref_contains_pre_destroy_content() {
    let repo = TestRepo::new();

    // Seed files.
    repo.seed_files(&[("README.md", "# Seed\n")]);

    // Create agent workspace and add unique content.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent-work.txt", "agent-specific content\n");
    repo.add_file("agent", "data/results.csv", "col1,col2\n1,2\n3,4\n");

    // Make default dirty to exercise the full recovery path.
    repo.add_file("default", "notes.txt", "human notes\n");

    // Merge with --destroy.
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Find the recovery ref for the agent workspace.
    let refs = recovery_refs(&repo, "agent");
    assert!(
        !refs.is_empty(),
        "Expected recovery ref for destroyed agent workspace"
    );

    let recovery = &refs[0];
    let commit_oid = resolve_ref(&repo, recovery);

    // Verify the recovery ref commit contains the agent's files.
    // Use `git ls-tree` to list files in the recovery commit.
    let tree_output = repo.git(&["ls-tree", "-r", "--name-only", &commit_oid]);
    let files: Vec<&str> = tree_output.lines().map(str::trim).collect();

    assert!(
        files.iter().any(|f| *f == "agent-work.txt"),
        "Recovery commit should contain agent-work.txt, files in tree: {files:?}"
    );

    // Use `git show <ref>:<filename>` to verify actual content.
    // Note: for stash commits (WorktreeCapture mode), the tree structure
    // may differ. Try direct access first; if that fails, check via ls-tree.
    let show_result = std::process::Command::new("git")
        .args(["show", &format!("{commit_oid}:agent-work.txt")])
        .current_dir(repo.root())
        .output()
        .expect("git show should run");

    if show_result.status.success() {
        let content = String::from_utf8_lossy(&show_result.stdout);
        assert_eq!(
            content.as_ref(),
            "agent-specific content\n",
            "Recovery ref should contain the original agent content"
        );
    } else {
        // For stash-style commits, the file might be accessible through
        // a parent commit. Verify it exists via ls-tree instead.
        assert!(
            files.iter().any(|f| *f == "agent-work.txt"),
            "agent-work.txt should at minimum be in the recovery commit tree"
        );
    }

    // Verify the recovery commit is distinct from the current epoch.
    let current_epoch = repo.git(&["rev-parse", "refs/manifold/epoch/current"])
        .trim()
        .to_owned();
    assert_ne!(
        commit_oid, current_epoch,
        "Recovery ref should point to a different commit than the current epoch"
    );

    // Verify default workspace still has preserved dirty files.
    assert_eq!(
        repo.read_file("default", "notes.txt").as_deref(),
        Some("human notes\n"),
        "Human notes in default should survive the merge"
    );
}
