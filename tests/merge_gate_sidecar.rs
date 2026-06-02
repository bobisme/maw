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
//!
//! bn-28d1: the sidecar-only gate is vulnerable to tampering — if the
//! sidecar is deleted or its `conflicts` map is emptied after rebase wrote
//! a placeholder blob into HEAD, the gate silently lets the placeholder
//! through. A tamper-resistance tripwire now cross-checks HEAD-tree blobs
//! against the small set of tool-authored placeholder byte prefixes
//! (`# structured conflict at `, `# BINARY CONFLICT at `). The tripwire is
//! specific-prefix, not generic-marker, so the bn-m6ad tutorial case still
//! merges cleanly.

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
    let tutorial = r"# Merge conflicts

<<<<<<< mine
my version
||||||| base
original
=======
their version
>>>>>>> theirs
";
    std::fs::write(ws_path.join("tutorial.md"), tutorial).expect("operation should succeed");
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
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge a",
    ]);

    // Rebase b — this creates a structured conflict and writes
    // conflict-tree.json.
    repo.maw_raw(&["ws", "sync", "b", "--rebase"]);

    // Sidecar must be non-empty now.
    let sidecar = repo.read_conflict_tree_sidecar("b");
    assert!(
        sidecar.is_some(),
        "rebase should have written conflict-tree.json"
    );
    let tree = sidecar.expect("operation should succeed");
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
    .expect("operation should succeed");
    repo.git_in_workspace("default", &["add", "-A"]);
    repo.git_in_workspace("default", &["commit", "-m", "seed binary"]);
    repo.maw_ok(&["epoch", "sync"]);

    // Workspace "a" modifies the binary
    repo.maw_ok(&["ws", "create", "a"]);
    let ws_a_bin = repo.root().join("ws").join("a").join("logo.png");
    let mut a_bytes = binary_bytes.clone();
    a_bytes.extend_from_slice(b"A_SIDE_SUFFIX\x00\x01");
    std::fs::write(&ws_a_bin, &a_bytes).expect("operation should succeed");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "a: tweak logo"]);

    // Workspace "b" modifies the same binary differently
    repo.maw_ok(&["ws", "create", "b"]);
    let ws_b_logo = repo.root().join("ws").join("b").join("logo.png");
    let mut b_bytes = binary_bytes;
    b_bytes.extend_from_slice(b"B_SIDE_SUFFIX\x02\x03");
    std::fs::write(&ws_b_logo, &b_bytes).expect("operation should succeed");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "b: tweak logo differently"]);

    // Merge a first
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge a",
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
        "ws",
        "merge",
        "alpha",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge alpha",
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

// ---------------------------------------------------------------------------
// bn-28d1: tamper-resistance tripwire
// ---------------------------------------------------------------------------

/// Drive a real rebase conflict so materialize.rs writes a
/// `# structured conflict at` placeholder blob into HEAD and produces a
/// non-empty `conflict-tree.json` sidecar. Returns the path of the
/// conflicted file (relative to the workspace root).
///
/// Uses the bn-3oau "two workspaces, merge one, rebase the other" pattern
/// which is what actually drives the materialize.rs placeholder commit
/// into the stale workspace's HEAD.
fn setup_rebase_conflict(repo: &TestRepo) -> &'static str {
    // Base commit: shared.txt in the epoch.
    repo.seed_files(&[("shared.txt", "original\n")]);

    // Workspace "alpha" modifies shared.txt and we merge it to advance the
    // epoch. This gives beta a divergent ancestor.
    repo.maw_ok(&["ws", "create", "alpha"]);
    repo.add_file("alpha", "shared.txt", "alpha\n");
    repo.git_in_workspace("alpha", &["add", "-A"]);
    repo.git_in_workspace("alpha", &["commit", "-m", "alpha"]);

    // Workspace "feat" modifies shared.txt differently — this is the
    // workspace we'll tamper with.
    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "shared.txt", "ws\n");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "ws"]);

    // Merge alpha into default, advancing the epoch past feat. Destroy
    // alpha so it doesn't clutter the remaining flow.
    repo.maw_ok(&[
        "ws",
        "merge",
        "alpha",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge alpha",
    ]);

    // Rebase feat — conflict produced, sidecar written, HEAD blob is a
    // materialize.rs text-conflict placeholder.
    let _ = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);

    "shared.txt"
}

/// Assert that the workspace's HEAD blob for `path` actually starts with a
/// tool-authored placeholder byte prefix. Without this precondition the
/// test would pass vacuously if some other code path re-materialized the
/// HEAD between `sync --rebase` and the gate.
///
/// Reads from the workspace's detached-HEAD commit in `ws/<name>/` —
/// that's where rebase commits the placeholder-bearing tree, and that's
/// what the gate scans.
fn assert_head_blob_has_placeholder_prefix(repo: &TestRepo, ws: &str, path: &str) {
    use std::process::Command;
    let ws_dir = repo.root().join("ws").join(ws);
    let spec = format!("HEAD:{path}");
    let out = Command::new("git")
        .args(["cat-file", "blob", &spec])
        .current_dir(&ws_dir)
        .output()
        .expect("git cat-file");
    assert!(
        out.status.success(),
        "cat-file failed in {}: {}\n{}",
        ws_dir.display(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let head = &out.stdout[..out.stdout.len().min(64)];
    assert!(
        head.starts_with(b"# structured conflict at ")
            || head.starts_with(b"# BINARY CONFLICT at "),
        "precondition: HEAD blob for {path} must start with a tool placeholder \
         prefix; got first 64 bytes: {:?}",
        String::from_utf8_lossy(head)
    );
}

#[test]
fn merge_gate_refuses_when_sidecar_emptied_but_head_has_placeholder() {
    // bn-28d1 core case: rebase produced a placeholder blob in HEAD AND
    // wrote a non-empty sidecar. An attacker (or a buggy tool) empties the
    // sidecar's `conflicts` map. The bn-m6ad/bn-3pgl sidecar-only gate
    // would wave this through; the tripwire must refuse.
    let repo = TestRepo::new();
    let conflicted_path = setup_rebase_conflict(&repo);
    assert_head_blob_has_placeholder_prefix(&repo, "feat", conflicted_path);

    // Empty the sidecar's conflicts map in place.
    let sidecar_path = repo
        .root()
        .join(".manifold/artifacts/ws/feat/conflict-tree.json");
    assert!(sidecar_path.exists(), "precondition: sidecar must exist");
    let text = std::fs::read_to_string(&sidecar_path).expect("operation should succeed");
    let mut value: serde_json::Value =
        serde_json::from_str(&text).expect("operation should succeed");
    value
        .as_object_mut()
        .expect("sidecar top-level is an object")
        .insert(
            "conflicts".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    std::fs::write(
        &sidecar_path,
        serde_json::to_string_pretty(&value).expect("operation should succeed"),
    )
    .expect("operation should succeed");

    // Confirm the sidecar conflicts map is now empty.
    let tampered = repo
        .read_conflict_tree_sidecar("feat")
        .expect("operation should succeed");
    assert!(
        tampered
            .get("conflicts")
            .and_then(|v| v.as_object())
            .is_some_and(serde_json::Map::is_empty),
        "precondition: sidecar conflicts map must be empty after tampering"
    );

    // Merge must refuse because HEAD still carries the placeholder blob.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "tampered",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse when sidecar is emptied but HEAD has placeholder\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(conflicted_path),
        "error should name the tainted path '{conflicted_path}'; got: {stderr}"
    );
    assert!(
        stderr.contains("placeholder") || stderr.contains("tool-authored"),
        "error should mention placeholder blobs; got: {stderr}"
    );
}

#[test]
fn merge_gate_refuses_when_both_sidecars_deleted_but_head_has_placeholder() {
    // bn-28d1: even more aggressive tampering — delete both sidecar files
    // entirely. The gate falls back to "no sidecar, assume clean" under
    // bn-m6ad, but the tripwire still refuses because HEAD is corrupt.
    let repo = TestRepo::new();
    let conflicted_path = setup_rebase_conflict(&repo);
    assert_head_blob_has_placeholder_prefix(&repo, "feat", conflicted_path);

    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/feat");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));
    assert!(repo.read_conflict_tree_sidecar("feat").is_none());

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "both sidecars deleted",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse when both sidecars are deleted but HEAD has placeholder\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(conflicted_path),
        "error should name the tainted path '{conflicted_path}'; got: {stderr}"
    );
}

#[test]
fn merge_gate_tripwire_ignores_legitimate_content() {
    // The tripwire matches only the exact byte *prefix* at column 0 of the
    // blob. A file that merely mentions the placeholder string further in
    // its body (e.g. documentation, a test fixture, a release note) must
    // NOT trip the gate.
    //
    // This also guards against a regression where the scan accidentally
    // falls back to generic `<<<<<<<` matching and starts flagging
    // tutorials — the bn-m6ad failure mode.
    let repo = TestRepo::new();
    repo.seed_files(&[("noop.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);

    // File whose *body* contains the exact placeholder string, but not at
    // position 0. Also throws in a raw `<<<<<<<` diff3 marker block for
    // good measure (the bn-m6ad tutorial case).
    let doc = "# Release notes\n\n\
               When a rebase conflicts, maw writes blobs starting with\n\
               `# structured conflict at <path>` or `# BINARY CONFLICT at <path>`.\n\
               Below is an example diff3 marker block from a fixture:\n\n\
               <<<<<<< mine\n\
               alpha\n\
               ||||||| base\n\
               zero\n\
               =======\n\
               beta\n\
               >>>>>>> theirs\n";
    std::fs::write(repo.root().join("ws").join("feat").join("doc.md"), doc)
        .expect("operation should succeed");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace(
        "feat",
        &["commit", "-m", "release notes with placeholder mention"],
    );

    // No sidecar ever written — this workspace is legitimately clean.
    assert!(repo.read_conflict_tree_sidecar("feat").is_none());

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "docs with placeholder mention",
    ]);
    assert!(
        out.status.success(),
        "legitimate doc mentioning the placeholder string must merge cleanly\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ---------------------------------------------------------------------------
// bn-qw4i: --check honors the source-conflict precondition
// ---------------------------------------------------------------------------

#[test]
fn merge_check_refuses_workspace_with_unresolved_rebase_conflict() {
    // bn-qw4i repro: two workspaces edit the same line. Merge the first to
    // advance the epoch; the second auto-rebases into a conflicted state
    // (sidecar non-empty, HEAD blob holds a tool-authored placeholder).
    //
    // `maw ws merge <second> --into default --check` must refuse with the
    // same diagnostic the real merge would produce — not "Ready to merge".
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
        "ws",
        "merge",
        "alpha",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge alpha",
    ]);

    let _ = repo.maw_raw(&["ws", "sync", "beta", "--rebase"]);

    let sidecar = repo
        .read_conflict_tree_sidecar("beta")
        .expect("rebase should have written conflict-tree.json");
    let conflicts = sidecar
        .get("conflicts")
        .and_then(|v| v.as_object())
        .expect("tree should have a `conflicts` object");
    assert!(
        !conflicts.is_empty(),
        "precondition: sidecar should list at least one conflict"
    );

    let out = repo.maw_raw(&["ws", "merge", "beta", "--into", "default", "--check"]);
    assert!(
        !out.status.success(),
        "--check must refuse when sidecar has entries — \
         the real merge would refuse, so --check must too\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("unresolved conflict") || combined.contains("shared.txt"),
        "error should cite the sidecar-reported conflict; got: {combined}"
    );
    assert!(
        !combined.contains("Ready to merge"),
        "--check must NOT report 'Ready to merge' when sidecar has entries; got: {combined}"
    );
}

#[test]
fn merge_check_with_force_bypasses_sidecar_gate_like_real_merge() {
    // bn-qw4i: --force bypasses the sidecar gate on the real merge path,
    // so it must also bypass on --check. Without --force, the gate refuses.
    // With --force, the sidecar gate is skipped and --check proceeds to the
    // build phase — which (for this scenario) produces structured engine
    // conflicts, so --check still reports BLOCKED but with a different,
    // post-gate diagnostic. The point: --force changes the failure mode in
    // the same way it changes it on the real merge.
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
        "ws",
        "merge",
        "alpha",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge alpha",
    ]);

    let _ = repo.maw_raw(&["ws", "sync", "beta", "--rebase"]);

    let no_force = repo.maw_raw(&["ws", "merge", "beta", "--into", "default", "--check"]);
    let no_force_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&no_force.stdout),
        String::from_utf8_lossy(&no_force.stderr)
    );
    assert!(
        !no_force.status.success(),
        "without --force, --check must refuse: {no_force_combined}"
    );
    assert!(
        no_force_combined.contains("unresolved conflict")
            || no_force_combined.contains("shared.txt"),
        "without --force, --check must cite the sidecar conflict; got: {no_force_combined}"
    );

    let with_force = repo.maw_raw(&[
        "ws", "merge", "beta", "--into", "default", "--check", "--force",
    ]);
    let with_force_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&with_force.stdout),
        String::from_utf8_lossy(&with_force.stderr)
    );
    assert!(
        !with_force_combined.contains("unresolved conflict"),
        "with --force, --check must bypass the sidecar gate (no 'unresolved conflict' \
         diagnostic); got: {with_force_combined}"
    );
}

// ---------------------------------------------------------------------------
// bn-16x2: STATUS / LIFECYCLE / RESOLVE path must agree with the gate
// ---------------------------------------------------------------------------
//
// The merge GATE was fixed (bn-m6ad/3pgl/3oau, tests above) to read conflict
// state from the sidecar, never from a content scan. But `ws list`,
// `ws status`, `lifecycle:conflicted`, and `ws resolve --list` regressed —
// they still scanned tracked-file CONTENT for `<<<<<<<` lines via
// `find_conflicted_files`, so a brand-new workspace born from a repo whose
// committed history legitimately contains marker literals (this very test
// file, merge-tool source, git tutorials, diff docs, conflict fixtures) was
// mislabeled "conflicted: N — resolve before merge / lifecycle:conflicted"
// while `merge --check` said "[OK] Ready to merge". The two DISAGREED.
//
// These tests assert all four surfaces key off the recorded-conflict sidecar
// (matching the gate): a fresh workspace with marker-literal content is NOT
// conflicted, and a genuine RECORDED rebase conflict still IS.

/// A fresh workspace whose default-branch history contains a file with
/// literal diff3 markers must NOT be reported conflicted by `ws list`,
/// `lifecycle`, or `ws resolve --list` — and must agree with `merge --check`.
#[test]
fn status_list_resolve_ignore_marker_literal_content() {
    let repo = TestRepo::new();
    // Commit a file whose bytes legitimately contain a full diff3 marker
    // block into the DEFAULT branch (epoch), then advance the epoch — so the
    // markers are part of the base history every fresh workspace inherits.
    let marker_doc = "# conflict tutorial\n\
        <<<<<<< ours\n\
        a\n\
        =======\n\
        b\n\
        >>>>>>> theirs\n";
    repo.seed_files(&[("conflicts_doc.md", marker_doc)]);

    repo.maw_ok(&["ws", "create", "solo"]);

    // Precondition: no structured sidecar (nothing rebased).
    let sidecar = repo
        .root()
        .join(".manifold/artifacts/ws/solo/conflict-tree.json");
    assert!(
        !sidecar.exists(),
        "precondition: a fresh workspace has no conflict sidecar"
    );

    // 1. `ws list` must NOT classify it conflicted.
    let list = repo.maw_ok(&["ws", "list"]);
    assert!(
        !list.contains("conflicted") && !list.contains("lifecycle:conflicted"),
        "fresh workspace with marker-literal history must not be 'conflicted' in \
         `ws list`; got:\n{list}"
    );

    // 2. `merge --check` must say ready (the gate already agreed) — confirm
    //    the status path now matches it.
    let check = repo.maw_ok(&["ws", "merge", "solo", "--into", "default", "--check"]);
    assert!(
        check.contains("Ready to merge"),
        "merge --check must report ready; got:\n{check}"
    );

    // 3. `ws resolve solo --list` must report no conflicts — NOT offer to
    //    --keep a side (which would mangle the legitimate file).
    let resolve = repo.maw_ok(&["ws", "resolve", "solo", "--list"]);
    assert!(
        resolve.contains("No conflicted files"),
        "resolve --list must report no conflicts for a fresh workspace; got:\n{resolve}"
    );
}

/// A genuine RECORDED rebase conflict must still be detected and shown by
/// `ws list` / `ws resolve --list` (do not weaken real detection — bn-16x2
/// caution: 3 prior fixes regressed this class).
#[test]
fn status_list_resolve_still_flag_genuine_recorded_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "line1\nshared\nline3\n")]);

    // Two workspaces edit the SAME line.
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "f.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "f.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    // Merge `a` — advances the epoch and auto-rebases sibling `b` into a
    // REAL recorded conflict (sidecar written).
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);

    // Sidecar must now exist for `b`.
    let sidecar = repo
        .root()
        .join(".manifold/artifacts/ws/b/conflict-tree.json");
    assert!(
        sidecar.exists(),
        "a real auto-rebase conflict must record a structured sidecar for 'b'"
    );

    // `ws list` must classify `b` conflicted.
    let list = repo.maw_ok(&["ws", "list"]);
    assert!(
        list.contains("conflicted") || list.contains("lifecycle:conflicted"),
        "genuine recorded conflict must still show 'conflicted' in `ws list`; got:\n{list}"
    );

    // `merge b --check` must refuse.
    let check = repo.maw_raw(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        !check.status.success(),
        "merge --check must refuse a workspace with a recorded conflict"
    );

    // `resolve b --list` must surface the conflicted file and offer --keep.
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("f.txt") && resolve.contains("--keep"),
        "resolve --list must surface the real conflict and offer --keep; got:\n{resolve}"
    );

    // And `--keep` must actually clear it.
    repo.maw_ok(&["ws", "resolve", "b", "--keep", "b"]);
    let after = repo.maw_ok(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        after.contains("Ready to merge"),
        "after `resolve --keep`, the workspace must be mergeable; got:\n{after}"
    );
}
