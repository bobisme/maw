//! bn-mq6j: conflicted `maw ws sync` must be UNMISSABLE (WARNING-prefixed,
//! duplicated to stderr) and machine-detectable (`--format json`), while
//! still exiting 0 — the jj-style conflicts-are-data model stays: sync
//! COMMITS conflict markers and does not stop-uncommitted or change the
//! exit code (see `tests/conflict_state_truth.rs`, locked in there too).
//!
//! bn-2cvx: sibling auto-rebase must flag when the epoch range a sibling was
//! just replayed over touches a path the sibling itself also touches — a
//! textually clean rebase is not the same as a semantically safe one.

mod manifold_common;

use manifold_common::TestRepo;

fn notice_path(repo: &TestRepo, ws: &str) -> std::path::PathBuf {
    repo.root()
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws)
        .join("auto-rebase-notice.json")
}

// ---------------------------------------------------------------------------
// Shared setups
// ---------------------------------------------------------------------------

/// `a` and `b` both edit the same line of `shared.txt`. Merging `a` with
/// `--no-auto-rebase` leaves `b` stale with committed work ahead of the new
/// epoch — the next manual `maw ws sync b` will hit a brand-new textual
/// conflict (rebase.rs's "new conflicts this run" branch).
fn setup_pending_new_conflict(repo: &TestRepo) {
    repo.seed_files(&[("shared.txt", "line1\nshared\nline3\n")]);
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "shared.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "shared.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--no-auto-rebase",
        "--message",
        "merge a",
    ]);
}

/// Drive a quiet sibling auto-rebase that commits a structured conflict into
/// workspace `b` (mirrors `tests/conflict_state_truth.rs`'s helper of the
/// same shape — duplicated here since integration test binaries can't share
/// private helpers across files).
fn setup_committed_conflict_via_auto_rebase(repo: &TestRepo) {
    repo.seed_files(&[("shared.txt", "line1\nshared\nline3\n")]);

    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "shared.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "shared.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);

    let sidecar = repo
        .read_conflict_tree_sidecar("b")
        .expect("auto-rebase should write conflict-tree.json for 'b'");
    let conflicts = sidecar
        .get("conflicts")
        .and_then(|v| v.as_object())
        .expect("sidecar should have a conflicts object");
    assert!(
        !conflicts.is_empty(),
        "precondition: auto-rebase must record a conflict for 'b'"
    );
}

/// Advance the epoch past `b` without touching it, so `b` goes stale again
/// while still carrying its committed (unresolved) conflict content.
fn advance_epoch_without_touching_b(repo: &TestRepo) {
    repo.maw_ok(&["ws", "create", "c"]);
    repo.add_file("c", "other.txt", "unrelated\n");
    repo.git_in_workspace("c", &["add", "-A"]);
    repo.git_in_workspace("c", &["commit", "-qm", "c-change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "c",
        "--into",
        "default",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge c",
    ]);
}

// ---------------------------------------------------------------------------
// bn-mq6j item 1: WARNING loudness on stdout + stderr, exit code stays 0
// ---------------------------------------------------------------------------

#[test]
fn sync_new_conflict_prints_warning_to_stdout_and_stderr_and_exits_zero() {
    let repo = TestRepo::new();
    setup_pending_new_conflict(&repo);

    let out = repo.maw_raw(&["ws", "sync", "b"]);
    assert!(
        out.status.success(),
        "conflicted sync must still exit 0 (jj-style conflicts-are-data); stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("WARNING: Rebase complete"),
        "stdout must carry the WARNING-prefixed summary; got:\n{stdout}"
    );
    assert!(
        stdout.contains("WARNING: Workspace 'b' has") && stdout.contains("unresolved conflict"),
        "stdout must carry the WARNING-prefixed unresolved-conflict line; got:\n{stdout}"
    );
    assert!(
        stderr.contains("WARNING: Rebase complete"),
        "the conflict summary must be duplicated to stderr so it survives \
         stdout-swallowing wrappers; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("WARNING: Workspace 'b' has"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn sync_residual_conflict_prints_warning_to_stdout_and_stderr_and_exits_zero() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);
    advance_epoch_without_touching_b(&repo);

    // Manual sync replays b's already-conflicted commit onto the newer
    // epoch. The replay itself introduces no NEW conflict, but committed
    // conflict content is still sitting in HEAD (bn-21cj's "residual"
    // branch) — must still be loud.
    let out = repo.maw_raw(&["ws", "sync", "b"]);
    assert!(
        out.status.success(),
        "residual-conflict sync must still exit 0; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("WARNING: Rebase complete") && stdout.contains("unresolved conflict"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("shared.txt"),
        "stdout must still name the conflicted path; got:\n{stdout}"
    );
    assert!(
        stderr.contains("WARNING: Rebase complete") && stderr.contains("shared.txt"),
        "residual-conflict summary must be duplicated to stderr; stderr:\n{stderr}"
    );
}

#[test]
fn sync_up_to_date_with_residual_conflicts_warns_on_stderr() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);
    advance_epoch_without_touching_b(&repo);

    // First sync clears staleness but leaves the conflict committed.
    repo.maw_ok(&["ws", "sync", "b"]);

    // Second sync hits the "not stale, but conflicted" pre-flight branch
    // (sync/mod.rs), not the rebase machinery at all.
    let out = repo.maw_raw(&["ws", "sync", "b"]);
    assert!(
        out.status.success(),
        "up-to-date-but-conflicted sync must still exit 0; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("WARNING: Workspace 'b' is up to date, but has")
            && stdout.contains("unresolved conflict"),
        "stdout:\n{stdout}"
    );
    assert!(
        stderr.contains("WARNING: Workspace 'b' is up to date, but has"),
        "pre-flight conflict notice must be duplicated to stderr; stderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-mq6j item 4: `ws sync --format json` schema
// ---------------------------------------------------------------------------

#[test]
fn sync_format_json_reports_residual_conflicts() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);
    advance_epoch_without_touching_b(&repo);

    let out = repo.maw_raw(&["ws", "sync", "b", "--format", "json"]);
    assert!(
        out.status.success(),
        "json-format conflicted sync must still exit 0; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("WARNING"),
        "JSON stdout must not be polluted by the text-mode WARNING lines; got:\n{stdout}"
    );
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("sync --format json produced invalid JSON: {e}\n{stdout}"));

    assert_eq!(json["workspace"].as_str(), Some("b"));
    assert!(
        json["conflict_count"].as_u64().unwrap_or(0) > 0,
        "conflict_count must be > 0: {json}"
    );
    let paths = json["conflicted_paths"]
        .as_array()
        .expect("conflicted_paths must be an array");
    assert!(
        paths.iter().any(|p| p.as_str() == Some("shared.txt")),
        "conflicted_paths must name shared.txt: {json}"
    );
}

#[test]
fn sync_format_json_reports_clean_rebase() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "a\n")]);
    repo.maw_ok(&["ws", "create", "merger"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("merger", "merger.txt", "merger change\n");
    repo.git_in_workspace("merger", &["add", "-A"]);
    repo.git_in_workspace("merger", &["commit", "-qm", "merger-change"]);
    repo.add_file("b", "b_file.txt", "b change\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "b-change"]);

    // Disjoint files, no auto-rebase — 'b' stays stale with committed work,
    // then a manual `ws sync --format json` performs the rebase itself.
    repo.maw_ok(&[
        "ws",
        "merge",
        "merger",
        "--into",
        "default",
        "--no-auto-rebase",
        "--message",
        "merge merger",
    ]);

    let out = repo.maw_raw(&["ws", "sync", "b", "--format", "json"]);
    assert!(
        out.status.success(),
        "clean sync --format json failed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));

    assert_eq!(json["workspace"].as_str(), Some("b"));
    assert_eq!(json["action"].as_str(), Some("rebased"));
    assert_eq!(json["replayed"].as_u64(), Some(1));
    assert_eq!(json["conflict_count"].as_u64(), Some(0));
    assert!(
        json["conflicted_paths"]
            .as_array()
            .expect("array")
            .is_empty()
    );
}

#[test]
fn sync_format_json_up_to_date_workspace() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "a\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);

    let out = repo.maw_raw(&["ws", "sync", "alice", "--format", "json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));
    assert_eq!(json["workspace"].as_str(), Some("alice"));
    assert_eq!(json["action"].as_str(), Some("up_to_date"));
    assert_eq!(json["conflict_count"].as_u64(), Some(0));
}

// ---------------------------------------------------------------------------
// bn-mq6j item 3: merge summary NOTE when a sibling ends up conflicted
// ---------------------------------------------------------------------------

#[test]
fn merge_summary_notes_sibling_conflicts() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "line1\nshared\nline3\n")]);
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "shared.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "shared.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    // Default auto-rebase is on: merging 'a' auto-rebases 'b' straight into
    // a conflict.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);
    assert!(
        out.status.success(),
        "merge must succeed even though a sibling ends up conflicted; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("CONFLICT: rebased with"),
        "auto-rebase line must use the distinct CONFLICT: tag; got:\n{stdout}"
    );
    assert!(
        stdout.contains("resolve: maw ws resolve b --list"),
        "auto-rebase line must include the exact resolve command; got:\n{stdout}"
    );
    assert!(
        stdout.contains("NOTE: 1 sibling workspace(s) now have conflicts: b"),
        "merge's final summary must bubble up the sibling conflict; got:\n{stdout}"
    );
}

#[test]
fn merge_format_json_reports_sibling_conflicts() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "line1\nshared\nline3\n")]);
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "shared.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "shared.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
        "--format",
        "json",
    ]);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));
    let siblings = json["sibling_conflicts"]
        .as_array()
        .expect("sibling_conflicts must be an array");
    assert_eq!(siblings.len(), 1);
    assert_eq!(siblings[0].as_str(), Some("b"));
}

// ---------------------------------------------------------------------------
// bn-2cvx: auto-rebase overlap hint
// ---------------------------------------------------------------------------

#[test]
fn auto_rebase_overlap_hint_present_when_ranges_overlap() {
    let repo = TestRepo::new();
    // Both sides touch shared.txt, but on DIFFERENT lines — the rebase is
    // textually clean, but the path overlaps with what the merge changed.
    repo.seed_files(&[("shared.txt", "line1\nline2\nline3\n")]);
    repo.maw_ok(&["ws", "create", "merger"]);
    repo.maw_ok(&["ws", "create", "sib"]);
    repo.add_file("merger", "shared.txt", "CHANGED1\nline2\nline3\n");
    repo.git_in_workspace("merger", &["commit", "-aqm", "merger-change"]);
    repo.add_file("sib", "shared.txt", "line1\nline2\nCHANGED3\n");
    repo.git_in_workspace("sib", &["commit", "-aqm", "sib-change"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "merger",
        "--into",
        "default",
        "--message",
        "merge merger",
    ]);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("rebased clean"),
        "sib should rebase cleanly (no textual conflict); got:\n{stdout}"
    );
    assert!(
        !stdout.contains("CONFLICT: rebased"),
        "this scenario must not produce a textual conflict; got:\n{stdout}"
    );
    assert!(
        stdout.contains("replayed over commits touching 1 file(s) this workspace also touches"),
        "overlap hint must be appended to the clean rebase line; got:\n{stdout}"
    );
    assert!(
        stdout.contains("re-run its tests before merging"),
        "got:\n{stdout}"
    );

    // Notice JSON must carry the same hint for the sibling's owner.
    let raw = std::fs::read_to_string(notice_path(&repo, "sib")).expect("notice file exists");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(json["overlap"]["count"].as_u64(), Some(1));
    let sample = json["overlap"]["sample_paths"]
        .as_array()
        .expect("sample_paths array");
    assert!(sample.iter().any(|p| p.as_str() == Some("shared.txt")));

    // And the rendered notice (next `maw exec sib -- ...`) mentions it too.
    let exec_out = repo.maw_raw_exact(&["exec", "sib", "--", "git", "status", "--short"]);
    assert!(exec_out.status.success());
    let exec_stderr = String::from_utf8_lossy(&exec_out.stderr);
    assert!(
        exec_stderr.contains("re-run its tests before merging"),
        "exec notice must render the overlap hint; stderr:\n{exec_stderr}"
    );
}

#[test]
fn auto_rebase_overlap_hint_absent_when_disjoint() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    repo.maw_ok(&["ws", "create", "merger"]);
    repo.maw_ok(&["ws", "create", "sib"]);
    repo.add_file("merger", "a.txt", "merger change\n");
    repo.git_in_workspace("merger", &["commit", "-aqm", "merger-change"]);
    repo.add_file("sib", "b.txt", "sib change\n");
    repo.git_in_workspace("sib", &["commit", "-aqm", "sib-change"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "merger",
        "--into",
        "default",
        "--message",
        "merge merger",
    ]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.contains("rebased clean"), "got:\n{stdout}");
    assert!(
        !stdout.contains("replayed over commits touching"),
        "disjoint touched-paths must not produce an overlap hint; got:\n{stdout}"
    );

    let raw = std::fs::read_to_string(notice_path(&repo, "sib")).expect("notice file exists");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert!(
        json.get("overlap").is_none() || json["overlap"].is_null(),
        "overlap must be absent when touched-path sets are disjoint: {json}"
    );
}
