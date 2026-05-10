//! bn-2upt — defense-in-depth sanity check on clean three-way merge results
//! produced during `maw ws sync --rebase`.
//!
//! Three reports of the structured-conflict layer producing silently-wrong
//! "clean" output (bn-2ghz, bn-4c6g, and an earlier sibling) prompted this
//! check. After every successful three-way overlap merge, the rebase
//! machinery validates the merged blob against its inputs:
//!   * Size-delta: merged length ≤ `post_rebase_size_ratio_max` × max(input).
//!   * AST parse: where applicable, the merged blob must parse without
//!     error if both inputs did.
//!
//! On a sanity flag with `merge.strict_post_rebase_check = true` (default)
//! the rebase routes the path through the conflict-tree pipeline. With the
//! flag false, a stderr warning is emitted but the merge is accepted.
//!
//! These integration tests exercise the full rebase machinery end-to-end.
//! Pure-function unit tests for the size-delta and AST checks live next to
//! the implementation in `crates/maw-cli/src/workspace/sync/rebase.rs`.

mod manifold_common;

use manifold_common::TestRepo;

/// Commit every dirty path in `workspace` with `message`.
fn commit_all(repo: &TestRepo, workspace: &str, message: &str) {
    repo.git_in_workspace(workspace, &["add", "-A"]);
    repo.git_in_workspace(workspace, &["commit", "-m", message]);
}

/// Write `merge.strict_post_rebase_check` (and optionally
/// `post_rebase_size_ratio_max`) into `.manifold/config.toml`.
fn write_merge_config(repo: &TestRepo, strict: bool, size_ratio_max: Option<f64>) {
    use std::fmt::Write as _;
    let mut cfg = String::from("[repo]\nbranch = \"main\"\n\n[merge]\n");
    writeln!(cfg, "strict_post_rebase_check = {strict}").expect("writing to a String never fails");
    if let Some(r) = size_ratio_max {
        writeln!(cfg, "post_rebase_size_ratio_max = {r}").expect("writing to a String never fails");
    }
    let path = repo.root().join(".manifold").join("config.toml");
    std::fs::write(&path, cfg)
        .unwrap_or_else(|e| panic!("write config.toml at {}: {e}", path.display()));
}

/// Build a "both sides add disjoint big content to the same file" scenario.
/// The base file is small; ours and theirs each add a large block at
/// different anchor lines. With the additions-aware envelope formula
/// (`expected = max(o,t) + ours_added + theirs_added`) the resulting clean
/// merge is well within bounds — that's the legitimate-merge shape we do
/// NOT want to flag. The strict/opt-out routing tests therefore use an
/// artificially tight `post_rebase_size_ratio_max = 0.5` to force the
/// check to trip, exercising the conflict-tree-routing logic without
/// depending on an actual corruption pattern.
const BASE_CONTENT: &str = "\
section_alpha:\n\
  // alpha anchor\n\
section_beta:\n\
  // beta anchor\n\
section_gamma:\n\
  // gamma anchor\n";

const OURS_CONTENT: &str = "\
section_alpha:\n\
  // alpha anchor\n\
  ALICE_LINE_01_added_to_alpha\n\
  ALICE_LINE_02_added_to_alpha\n\
  ALICE_LINE_03_added_to_alpha\n\
  ALICE_LINE_04_added_to_alpha\n\
  ALICE_LINE_05_added_to_alpha\n\
  ALICE_LINE_06_added_to_alpha\n\
  ALICE_LINE_07_added_to_alpha\n\
  ALICE_LINE_08_added_to_alpha\n\
section_beta:\n\
  // beta anchor\n\
section_gamma:\n\
  // gamma anchor\n";

const THEIRS_CONTENT: &str = "\
section_alpha:\n\
  // alpha anchor\n\
section_beta:\n\
  // beta anchor\n\
  BOB_LINE_01_added_to_beta\n\
  BOB_LINE_02_added_to_beta\n\
  BOB_LINE_03_added_to_beta\n\
  BOB_LINE_04_added_to_beta\n\
  BOB_LINE_05_added_to_beta\n\
  BOB_LINE_06_added_to_beta\n\
  BOB_LINE_07_added_to_beta\n\
  BOB_LINE_08_added_to_beta\n\
section_gamma:\n\
  // gamma anchor\n";

// ---------------------------------------------------------------------------
// 1. Strict (default): inflated merge → conflict, sidecar present.
// ---------------------------------------------------------------------------

#[test]
fn strict_mode_routes_inflated_merge_through_conflict_tree() {
    let repo = TestRepo::new();
    repo.seed_files(&[("data.txt", BASE_CONTENT)]);

    // strict_post_rebase_check defaults to true; assert that explicitly to
    // make the test's intent clear if defaults ever change.
    write_merge_config(&repo, true, Some(0.5));

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "data.txt", OURS_CONTENT);
    commit_all(&repo, "alice", "alice: add alpha lines");

    // Advance the epoch via a sibling that modifies the same file disjointly.
    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file("bob", "data.txt", THEIRS_CONTENT);
    commit_all(&repo, "bob", "bob: add beta lines");
    repo.maw_ok(&[
        "ws",
        "merge",
        "bob",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge bob",
    ]);

    // Rebase alice. Without the sanity check this would land a clean
    // merge with both alpha and beta adds. The size check fires on the
    // 1.5x ratio and routes through the conflict-tree path.
    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "rebase invocation should not error (it just produces a conflict): \
         stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("post-rebase sanity check tripped"),
        "stderr should warn about the sanity flag: stderr={stderr} stdout={stdout}",
    );
    assert!(
        stdout.contains("with conflicts") || stdout.contains("sanity-flagged"),
        "rebase summary should reflect the conflict: {stdout}",
    );

    let sidecar = repo
        .read_conflict_tree_sidecar("alice")
        .expect("conflict sidecar must exist after sanity-triggered conflict");
    let conflicts = sidecar
        .get("conflicts")
        .expect("sidecar must have a conflicts object");
    let conflicts_map = conflicts
        .as_object()
        .expect("conflicts object should be a JSON object");
    assert!(
        conflicts_map.contains_key("data.txt"),
        "data.txt must show up as a conflict; sidecar conflicts: {conflicts:?}",
    );

    let resolve_list = repo.maw_ok(&["ws", "resolve", "alice", "--list"]);
    assert!(
        resolve_list.contains("data.txt"),
        "ws resolve --list should surface the flagged path: {resolve_list}",
    );
}

// ---------------------------------------------------------------------------
// 2. Opt-out: strict=false → warning printed, merge accepted.
// ---------------------------------------------------------------------------

#[test]
fn opt_out_accepts_inflated_merge_with_warning() {
    let repo = TestRepo::new();
    repo.seed_files(&[("data.txt", BASE_CONTENT)]);

    // Same scenario as the strict test, but with the sanity check made
    // non-blocking via config.
    write_merge_config(&repo, false, Some(0.5));

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "data.txt", OURS_CONTENT);
    commit_all(&repo, "alice", "alice: add alpha lines");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file("bob", "data.txt", THEIRS_CONTENT);
    commit_all(&repo, "bob", "bob: add beta lines");
    repo.maw_ok(&[
        "ws",
        "merge",
        "bob",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge bob",
    ]);

    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "rebase should succeed: stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("post-rebase sanity check tripped"),
        "stderr should still warn even with strict=false: stderr={stderr}",
    );

    // No conflict sidecar should land — the merge was accepted.
    assert!(
        repo.read_conflict_tree_sidecar("alice").is_none(),
        "no conflict sidecar should be present when strict_post_rebase_check is false",
    );

    // The merged content should contain BOTH sides' additions (the
    // diff3 result), confirming we accepted it as clean.
    let merged = repo
        .read_file("alice", "data.txt")
        .expect("data.txt must exist after accepted merge");
    assert!(
        merged.contains("ALICE_LINE_01"),
        "alice's adds must survive the accepted-with-warning merge: {merged}",
    );
    assert!(
        merged.contains("BOB_LINE_01"),
        "bob's adds must survive the accepted-with-warning merge: {merged}",
    );
}

// ---------------------------------------------------------------------------
// 3. Reasonable merge: not flagged.
// ---------------------------------------------------------------------------

#[test]
fn reasonable_merge_is_not_flagged() {
    let repo = TestRepo::new();
    // Tiny disjoint edits — both sides change one line each. Final
    // output is roughly the same size as the inputs. Should never trip
    // a 1.5x threshold.
    repo.seed_files(&[("notes.txt", "alpha\nbeta\ngamma\n")]);

    // Default config — strict on, ratio 1.5 (the production default).
    write_merge_config(&repo, true, Some(1.5));

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "notes.txt", "alpha-edited\nbeta\ngamma\n");
    commit_all(&repo, "alice", "alice: edit alpha");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file("bob", "notes.txt", "alpha\nbeta\ngamma-edited\n");
    commit_all(&repo, "bob", "bob: edit gamma");
    repo.maw_ok(&[
        "ws",
        "merge",
        "bob",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge bob",
    ]);

    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "rebase should succeed: stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("post-rebase sanity check tripped"),
        "no sanity flag should fire on a reasonable merge: stderr={stderr}",
    );
    assert!(
        stdout.contains("replayed cleanly"),
        "reasonable merge should report 'replayed cleanly': {stdout}",
    );

    // Both edits should be present.
    let merged = repo
        .read_file("alice", "notes.txt")
        .expect("notes.txt must exist");
    assert!(
        merged.contains("alpha-edited"),
        "alice's edit must survive: {merged}",
    );
    assert!(
        merged.contains("gamma-edited"),
        "bob's edit must survive: {merged}",
    );
}
