//! Integration tests for `.gitattributes` merge driver support.
//!
//! Regression tests for the bug where `maw ws merge` ignored `.gitattributes`
//! merge drivers, producing diff3 conflict markers for append-only files like
//! `.bones/events/*.events` that were marked `merge=union`.

mod manifold_common;

use manifold_common::TestRepo;

/// Set up a repo with a `.gitattributes` file and seed an initial file so
/// the merge base has known content.
fn seed_repo_with_gitattributes(
    repo: &TestRepo,
    attrs_content: &str,
    seed_file: &str,
    seed_content: &str,
) {
    repo.seed_files(&[(".gitattributes", attrs_content), (seed_file, seed_content)]);
}

// ---------------------------------------------------------------------------
// merge=union — append-only files concatenate without conflict markers.
// ---------------------------------------------------------------------------

#[test]
fn merge_union_concatenates_both_sides_without_markers() {
    let repo = TestRepo::new();
    seed_repo_with_gitattributes(&repo, "*.log merge=union\n", "events.log", "header\n");

    // Two workspaces append different events.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "events.log", "header\nalice event 1\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: append"]);

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.add_file("bob", "events.log", "header\nbob event 1\n");
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "bob: append"]);

    // Merge both.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--destroy",
        "--message",
        "merge alice + bob",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The merged file should contain both events, no diff3 markers.
    let merged = repo
        .read_file("default", "events.log")
        .expect("events.log should exist after merge");
    assert!(
        merged.contains("alice event 1"),
        "merged file missing alice's event: {merged:?}"
    );
    assert!(
        merged.contains("bob event 1"),
        "merged file missing bob's event: {merged:?}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "merged file should NOT contain diff3 markers (merge=union):\n{merged}"
    );
    assert!(
        !merged.contains("======="),
        "merged file should NOT contain diff3 markers (merge=union):\n{merged}"
    );
}

#[test]
fn merge_union_handles_three_workspaces() {
    let repo = TestRepo::new();
    seed_repo_with_gitattributes(&repo, "*.log merge=union\n", "events.log", "header\n");

    for (ws, line) in &[("alice", "A"), ("bob", "B"), ("carol", "C")] {
        repo.maw_ok(&["ws", "create", ws]);
        repo.add_file(ws, "events.log", &format!("header\n{line} event\n"));
        repo.git_in_workspace(ws, &["add", "-A"]);
        repo.git_in_workspace(ws, &["commit", "-m", &format!("{ws}: append")]);
    }

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "carol",
        "--destroy",
        "--message",
        "3-way union",
    ]);
    assert!(out.status.success(), "3-way merge should succeed");

    let merged = repo
        .read_file("default", "events.log")
        .expect("operation should succeed");
    assert!(merged.contains("A event"), "missing A: {merged}");
    assert!(merged.contains("B event"), "missing B: {merged}");
    assert!(merged.contains("C event"), "missing C: {merged}");
    assert!(
        !merged.contains("<<<<<<<"),
        "no conflict markers expected: {merged}"
    );
}

#[test]
fn merge_union_applies_to_nested_gitattributes() {
    let repo = TestRepo::new();
    // Root .gitattributes has a default; a subdirectory overrides it.
    repo.seed_files(&[
        (".gitattributes", "*.txt merge=union\n"),
        ("notes/x.txt", "base\n"),
    ]);

    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "notes/x.txt", "base\nalice\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a: append"]);

    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "notes/x.txt", "base\nbob\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b: append"]);

    let out = repo.maw_raw(&["ws", "merge", "a", "b", "--destroy", "--message", "merge"]);
    assert!(out.status.success(), "merge should succeed");

    let merged = repo
        .read_file("default", "notes/x.txt")
        .expect("operation should succeed");
    assert!(merged.contains("alice"), "missing alice: {merged}");
    assert!(merged.contains("bob"), "missing bob: {merged}");
    assert!(!merged.contains("<<<<<<<"), "no markers: {merged}");
}

#[test]
fn merge_union_preserves_base_content() {
    // Regression: union merge with a base that has existing content. Both
    // sides append; base content should not be duplicated.
    let repo = TestRepo::new();
    seed_repo_with_gitattributes(
        &repo,
        "*.log merge=union\n",
        "app.log",
        "line1\nline2\nline3\n",
    );

    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "app.log", "line1\nline2\nline3\nline_a\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a"]);

    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "app.log", "line1\nline2\nline3\nline_b\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b"]);

    let out = repo.maw_raw(&["ws", "merge", "a", "b", "--destroy", "--message", "merge"]);
    assert!(out.status.success());

    let merged = repo
        .read_file("default", "app.log")
        .expect("operation should succeed");
    // base lines should appear exactly once
    assert_eq!(
        merged.matches("line1\n").count(),
        1,
        "base line1 duplicated:\n{merged}"
    );
    assert_eq!(
        merged.matches("line2\n").count(),
        1,
        "base line2 duplicated:\n{merged}"
    );
    assert!(merged.contains("line_a"), "missing line_a: {merged}");
    assert!(merged.contains("line_b"), "missing line_b: {merged}");
    assert!(!merged.contains("<<<<<<<"));
}

// ---------------------------------------------------------------------------
// merge=binary — refuses text merge, always a conflict when sides differ.
// ---------------------------------------------------------------------------

#[test]
fn merge_binary_produces_conflict_when_both_sides_differ() {
    let repo = TestRepo::new();
    seed_repo_with_gitattributes(&repo, "*.db merge=binary\n", "data.db", "v1\n");

    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "data.db", "va\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a"]);

    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "data.db", "vb\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b"]);

    // merge should fail due to conflict (merge=binary prevents text merge).
    let out = repo.maw_raw(&["ws", "merge", "a", "b", "--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.to_lowercase().contains("conflict"),
        "expected conflict report for merge=binary with divergent sides\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn merge_binary_clean_when_only_one_side_changes() {
    // If one side equals base, the resolve phase's hash-equality short-circuit
    // kicks in before the binary driver — so the other side wins cleanly.
    let repo = TestRepo::new();
    seed_repo_with_gitattributes(&repo, "*.db merge=binary\n", "data.db", "v1\n");

    // a modifies; b leaves data.db alone (but touches something else so the
    // workspace has a commit).
    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "data.db", "va\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a"]);

    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "other.txt", "unrelated\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b"]);

    let out = repo.maw_raw(&["ws", "merge", "a", "b", "--destroy", "--message", "merge"]);
    assert!(
        out.status.success(),
        "single-side modify on merge=binary should merge cleanly\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let merged = repo
        .read_file("default", "data.db")
        .expect("operation should succeed");
    assert_eq!(merged, "va\n");
}

// ---------------------------------------------------------------------------
// Unknown drivers fall back to default diff3 behavior.
// ---------------------------------------------------------------------------

#[test]
fn unknown_merge_driver_falls_back_to_diff3() {
    let repo = TestRepo::new();
    seed_repo_with_gitattributes(
        &repo,
        "*.conf merge=my-custom-driver\n",
        "app.conf",
        "key=original\n",
    );

    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "app.conf", "key=alice\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a"]);

    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "app.conf", "key=bob\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b"]);

    // Should behave like normal diff3 — conflict on overlapping edit.
    let out = repo.maw_raw(&["ws", "merge", "a", "b", "--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.to_lowercase().contains("conflict"),
        "unknown driver should default to diff3 conflict behavior\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Files without a merge driver still use default diff3.
// ---------------------------------------------------------------------------

#[test]
fn files_without_merge_driver_still_use_diff3() {
    let repo = TestRepo::new();
    // .gitattributes only covers *.events; *.rs gets default behavior.
    seed_repo_with_gitattributes(
        &repo,
        "*.events merge=union\n",
        "src/main.rs",
        "fn main() { println!(\"hello\"); }\n",
    );

    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "src/main.rs", "fn main() { println!(\"alice\"); }\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a"]);

    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "src/main.rs", "fn main() { println!(\"bob\"); }\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b"]);

    // Should produce a conflict since *.rs isn't merge=union.
    let out = repo.maw_raw(&["ws", "merge", "a", "b", "--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.to_lowercase().contains("conflict"),
        ".rs file without merge driver should conflict on overlap\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Append-only events file regression test — the exact scenario from the
// original bug report (.bones/events/*.events with merge=union).
// ---------------------------------------------------------------------------

#[test]
fn bones_events_files_with_merge_union_concatenate() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        (".gitattributes", ".bones/events/*.events merge=union\n"),
        (
            ".bones/events/2026-04.events",
            "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n",
        ),
    ]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file(
        "alice",
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n\
         1234567891\talice\tid2\titem.update\tbn-a\t{\"field\":\"x\"}\thash2\n",
    );
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: add event"]);

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.add_file(
        "bob",
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n\
         1234567892\tbob\tid3\titem.update\tbn-a\t{\"field\":\"y\"}\thash3\n",
    );
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "bob: add event"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--destroy",
        "--message",
        "merge events",
    ]);
    assert!(
        out.status.success(),
        "bones events merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let merged = repo
        .read_file("default", ".bones/events/2026-04.events")
        .expect("events file should exist");
    assert!(
        merged.contains("alice"),
        "merged events missing alice's event: {merged}"
    );
    assert!(
        merged.contains("bob"),
        "merged events missing bob's event: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "merged events should NOT have conflict markers:\n{merged}"
    );
    assert!(
        !merged.contains(">>>>>>>"),
        "merged events should NOT have conflict markers:\n{merged}"
    );
    // Base line appears exactly once (not duplicated).
    assert_eq!(
        merged.matches("hash1").count(),
        1,
        "base line duplicated: {merged}"
    );
}

#[test]
fn dirty_default_bones_events_use_nested_union_driver() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        (".bones/.gitattributes", "events/** merge=union\n"),
        (
            ".bones/events/2026-04.events",
            "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n",
        ),
    ]);

    // Default has an uncommitted event appended directly by `bn create`.
    repo.add_file(
        "default",
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n\
         1234567891\tdefault\tid2\titem.create\tbn-local\t{}\thash-local\n",
    );

    // A workspace also appends to the same event shard and gets merged.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file(
        "alice",
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n\
         1234567892\talice\tid3\titem.create\tbn-alice\t{}\thash-alice\n",
    );
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: add event"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let merged = repo
        .read_file("default", ".bones/events/2026-04.events")
        .expect("events file should exist");
    assert!(
        merged.contains("hash-local"),
        "dirty default event was not preserved:\n{merged}"
    );
    assert!(
        merged.contains("hash-alice"),
        "merged workspace event was not preserved:\n{merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "dirty target replay should honor .bones/.gitattributes merge=union:\n{merged}"
    );
    assert_eq!(
        merged.matches("hash1").count(),
        1,
        "base line duplicated: {merged}"
    );
}

#[test]
fn dirty_default_bones_events_use_union_driver_introduced_by_merge() {
    let repo = TestRepo::new();
    repo.seed_files(&[(
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n",
    )]);

    // Default has an uncommitted bones event, but the union driver was not
    // present at the merge anchor yet.
    repo.add_file(
        "default",
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n\
         1234567891\tdefault\tid2\titem.create\tbn-local\t{}\thash-local\n",
    );

    // The merged workspace introduces the nested .gitattributes policy and
    // appends another event to the same shard.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", ".bones/.gitattributes", "events/** merge=union\n");
    repo.add_file(
        "alice",
        ".bones/events/2026-04.events",
        "1234567890\tsetup\tid1\titem.create\tbn-a\t{}\thash1\n\
         1234567892\talice\tid3\titem.create\tbn-alice\t{}\thash-alice\n",
    );
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: add event policy"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let merged = repo
        .read_file("default", ".bones/events/2026-04.events")
        .expect("events file should exist");
    assert!(
        merged.contains("hash-local"),
        "dirty default event was not preserved:\n{merged}"
    );
    assert!(
        merged.contains("hash-alice"),
        "merged workspace event was not preserved:\n{merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "dirty target replay should honor merge-introduced .bones/.gitattributes:\n{merged}"
    );
    assert_eq!(
        merged.matches("hash1").count(),
        1,
        "base line duplicated: {merged}"
    );
}
