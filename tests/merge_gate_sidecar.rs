//! Regression tests for the merge gate's conflict-state detection.
//!
//! bn-m6ad / bn-3pgl / bn-3oau: the merge gate must derive "workspace has
//! unresolved conflicts" exclusively from the structured
//! `conflict-tree.json` sidecar, never from a byte-level scan of the
//! worktree for `<<<<<<<` markers.
//!
//! * bn-m6ad: a workspace whose bytes legitimately contain `<<<<<<<`
//!   literals (tutorials, test fixtures, CI templates, …) and which has
//!   no sidecar must be allowed to merge. The previous marker-scan gate
//!   false-positived on these files.
//!
//! * bn-3pgl: a workspace holding a binary-conflict placeholder commit
//!   after `sync --rebase` must be refused. The sidecar lists the
//!   conflicted path; the gate refuses purely on that, independent of
//!   the materialized bytes.
//!
//! * bn-3oau: when the sidecar has entries, those paths are authoritative.
//!   The gate must refuse — regardless of whether the bytes contain
//!   markers, placeholder text, or anything else.

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// bn-m6ad: merge allowed when sidecar absent, even if bytes contain `<<<<<<<`
// ---------------------------------------------------------------------------

#[test]
fn merge_gate_allows_workspace_with_marker_content_but_empty_sidecar() {
    // Repro of bn-m6ad: a tutorial / reference file with raw `<<<<<<<`
    // content at column 0 must not block `ws merge` when the workspace
    // never went through `sync --rebase` (so no sidecar was written).
    let repo = TestRepo::new();
    repo.seed_files(&[("noop.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    let ws_path = repo.root().join("ws").join("feat");

    // Commit a file whose bytes contain a full diff3 marker block — as
    // though it were a merge-conflict tutorial, a fixture, or a
    // blog-post draft.
    let tutorial = r#"# Merge conflicts

<<<<<<< mine
my version
||||||| base
original
=======
their version
>>>>>>> theirs
"#;
    std::fs::write(ws_path.join("tutorial.md"), tutorial).unwrap();
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "ws: tutorial"]);

    // Sanity: no sidecar exists (nothing wrote one).
    let sidecar = repo
        .root()
        .join(".manifold/artifacts/ws/feat/conflict-tree.json");
    assert!(
        !sidecar.exists(),
        "precondition: no structured sidecar should exist"
    );

    // Merge must succeed — sidecar is authoritative, bytes aren't.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "bn-m6ad: merge tutorial workspace",
    ]);
    assert!(
        out.status.success(),
        "merge should proceed when no sidecar exists\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// bn-3oau invariant: sidecar with entries ⇒ merge refused
// ---------------------------------------------------------------------------

#[test]
fn merge_gate_refuses_workspace_with_structured_sidecar_entries() {
    // Force a real rebase conflict so the pipeline writes
    // `conflict-tree.json` with a non-empty `.conflicts` map. The merge
    // gate must refuse, and the error must quote the sidecar-reported
    // path rather than "marker(s) in worktree".
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "original\n")]);

    // Workspace "a" modifies shared.txt
    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "shared.txt", "alice\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "alice"]);

    // Workspace "b" modifies shared.txt differently
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "shared.txt", "bob\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "bob"]);

    // Merge a, advancing the epoch past b
    repo.maw_ok(&[
        "ws", "merge", "a", "--into", "default", "--destroy", "--message", "merge a",
    ]);

    // Rebase b — this creates a structured conflict and writes
    // conflict-tree.json.
    repo.maw_raw(&["ws", "sync", "b", "--rebase"]);

    // Sidecar must be non-empty now.
    let sidecar = repo.read_conflict_tree_sidecar("b");
    assert!(sidecar.is_some(), "rebase should have written conflict-tree.json");
    let tree = sidecar.unwrap();
    let conflicts = tree
        .get("conflicts")
        .and_then(|v| v.as_object())
        .expect("tree should have a `conflicts` object");
    assert!(
        !conflicts.is_empty(),
        "sidecar should list at least one conflicted path"
    );

    // Merge gate must refuse.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse when sidecar has entries\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unresolved conflict") || stderr.contains("shared.txt"),
        "error should cite the sidecar-reported conflict; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-3pgl: binary-conflict placeholder → merge refused
// ---------------------------------------------------------------------------

#[test]
fn merge_gate_refuses_binary_conflict_without_manual_resolve() {
    // A binary file is modified on two sides. `sync --rebase` materializes
    // a binary-conflict placeholder (textual banner + verbatim bytes) and
    // writes conflict-tree.json listing the path. Without `ws resolve`,
    // the merge gate must refuse — the structured sidecar is the
    // authority, and it has an entry.
    let repo = TestRepo::new();

    // Seed a small binary file.
    let binary_bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
    std::fs::write(
        repo.root().join("ws").join("default").join("logo.png"),
        &binary_bytes,
    )
    .unwrap();
    repo.git_in_workspace("default", &["add", "-A"]);
    repo.git_in_workspace("default", &["commit", "-m", "seed binary"]);
    repo.maw_ok(&["epoch", "sync"]);

    // Workspace "a" modifies the binary
    repo.maw_ok(&["ws", "create", "a"]);
    let ws_a_bin = repo.root().join("ws").join("a").join("logo.png");
    let mut a_bytes = binary_bytes.clone();
    a_bytes.extend_from_slice(b"A_SIDE_SUFFIX\x00\x01");
    std::fs::write(&ws_a_bin, &a_bytes).unwrap();
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a: tweak logo"]);

    // Workspace "b" modifies the same binary differently
    repo.maw_ok(&["ws", "create", "b"]);
    let ws_b_bin = repo.root().join("ws").join("b").join("logo.png");
    let mut b_bytes = binary_bytes.clone();
    b_bytes.extend_from_slice(b"B_SIDE_SUFFIX\x02\x03");
    std::fs::write(&ws_b_bin, &b_bytes).unwrap();
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b: tweak logo differently"]);

    // Merge a first
    repo.maw_ok(&[
        "ws", "merge", "a", "--into", "default", "--destroy", "--message", "merge a",
    ]);

    // Rebase b — binary conflict expected
    let _ = repo.maw_raw(&["ws", "sync", "b", "--rebase"]);

    // Sidecar should list the binary path as conflicted.
    let sidecar = repo
        .read_conflict_tree_sidecar("b")
        .expect("rebase should have written conflict-tree.json for binary conflict");
    let conflicts = sidecar
        .get("conflicts")
        .and_then(|v| v.as_object())
        .expect("tree should have a `conflicts` object");
    assert!(
        !conflicts.is_empty(),
        "sidecar should list the binary path as conflicted: {sidecar:#}"
    );

    // Merge gate must refuse.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail: unresolved binary conflict",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse when binary sidecar entry exists\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unresolved conflict") || stderr.contains("logo.png"),
        "error should reference the binary file or 'unresolved conflict'; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-3oau invariant preserved
// ---------------------------------------------------------------------------

#[test]
fn merge_gate_still_refuses_when_sidecar_lists_a_marker_path() {
    // A workspace genuinely goes through rebase and hits a text conflict.
    // The sidecar has that path listed. Merge must refuse. This is the
    // bn-3oau invariant — unchanged by the bn-m6ad/bn-3pgl refactor.
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "original\n")]);

    repo.maw_ok(&["ws", "create", "alpha"]);
    repo.add_file("alpha", "shared.txt", "alpha\n");
    repo.git_in_workspace("alpha", &["add", "-A"]);
    repo.git_in_workspace("alpha", &["commit", "-m", "alpha"]);

    repo.maw_ok(&["ws", "create", "beta"]);
    repo.add_file("beta", "shared.txt", "beta\n");
    repo.git_in_workspace("beta", &["add", "-A"]);
    repo.git_in_workspace("beta", &["commit", "-m", "beta"]);

    repo.maw_ok(&[
        "ws", "merge", "alpha", "--into", "default", "--destroy", "--message", "merge alpha",
    ]);

    // beta now stale; rebase produces a conflict on shared.txt.
    let _ = repo.maw_raw(&["ws", "sync", "beta", "--rebase"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "beta",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse when sidecar lists shared.txt\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
