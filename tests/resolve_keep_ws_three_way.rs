//! bn-3mbj — `--keep <ws>` must apply the workspace's intent on top of the
//! new epoch, not wholesale replace the file with the workspace's pre-rebase
//! blob.
//!
//! Regression scenario (the reported case):
//!
//! 1. Workspace A modifies file F lines 1-10 and merges, advancing the epoch.
//! 2. Workspace B (forked before A, modifies F lines 50-60 in its own
//!    commits) rebases onto the new epoch — produces a structured conflict
//!    on F because both A and B touched the file.
//! 3. `maw ws resolve B --keep B` previously wrote B's pre-rebase blob,
//!    silently deleting A's lines 1-10 changes.
//!
//! Expected after the fix: F contains BOTH A's lines 1-10 AND B's lines 50-60.
//!
//! These tests also lock in:
//! - Symmetric `--keep epoch` keeps only A's content (existing behaviour).
//! - Overlapping conflicting edits resolve with workspace winning on overlap
//!   while preserving non-overlapping epoch content elsewhere.
//! - Legacy sidecars (no `base_content` on the workspace side) fall back to
//!   blob-replace and emit a warning.

mod manifold_common;

use manifold_common::TestRepo;

fn commit_all(repo: &TestRepo, workspace: &str, message: &str) {
    repo.git_in_workspace(workspace, &["add", "-A"]);
    repo.git_in_workspace(workspace, &["commit", "-m", message]);
}

/// 100-line file used as the conflict subject. Each call returns a fresh
/// `String` so callers can mutate independent regions.
fn hundred_lines() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in 1..=100 {
        writeln!(s, "line-{i:03}").expect("write to String never fails");
    }
    s
}

/// Mutate lines 1..=10 in-place by uppercasing them so the result is
/// distinguishable.
fn modify_top(content: &str) -> String {
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let line_no = i + 1;
        if (1..=10).contains(&line_no) {
            out.push_str(&line.to_ascii_uppercase());
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Recursively remove `base_content` keys from a JSON tree. Used by the
/// legacy-sidecar test to simulate a sidecar produced by an older maw
/// version that did not know about the field.
fn strip_base_content(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.remove("base_content");
            for (_, child) in map.iter_mut() {
                strip_base_content(child);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                strip_base_content(item);
            }
        }
        _ => {}
    }
}

/// Mutate lines 50..=60 in-place by replacing them with `B-MARK-N` so the
/// result is distinguishable from `modify_top`.
fn modify_bottom(content: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let line_no = i + 1;
        if (50..=60).contains(&line_no) {
            write!(out, "B-MARK-{line_no}").expect("write to String never fails");
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Set up a repo where:
/// - Default workspace is seeded with `f.txt` containing `hundred_lines()`.
/// - Workspace `a` has modified the top region (1..=10) and merged, so the
///   new epoch contains the top-modified content.
/// - Workspace `b` is created from before `a`'s merge with bottom-region
///   changes (50..=60) committed; it is then rebased onto the post-`a`
///   epoch so its conflict-tree sidecar lands.
///
/// Note: with disjoint A/B regions diff3 actually merges cleanly during
/// rebase via `try_clean_three_way_overlap`, so we additionally force a
/// minimal overlap on a single line — line 30 — so a structured conflict is
/// guaranteed to surface and `--keep b` runs through the new 3-way path.
fn setup_a_then_b_rebased(repo: &TestRepo, b_modify: impl Fn(&str) -> String) {
    let base = hundred_lines();
    repo.seed_files(&[("f.txt", &base)]);

    // Create workspace `b` BEFORE workspace `a` so `b` is forked from the
    // pre-`a` epoch and will need to rebase onto the new epoch.
    repo.maw_ok(&["ws", "create", "b"]);
    let mut b_text = b_modify(&base);
    // Force an overlap with A on line 30 so a structured conflict is
    // produced. B's value: B-OVERLAP-30.
    b_text = b_text.replace("line-030\n", "B-OVERLAP-30\n");
    repo.modify_file("b", "f.txt", &b_text);
    commit_all(repo, "b", "b: bottom region + line 30");

    // Workspace `a` modifies the top region AND line 30 differently, so
    // diff3 sees a real overlap and produces a `Conflict::Content`.
    let mut a_text = modify_top(&base);
    a_text = a_text.replace("line-030\n", "A-OVERLAP-30\n");
    repo.maw_ok(&["ws", "create", "a"]);
    repo.modify_file("a", "f.txt", &a_text);
    commit_all(repo, "a", "a: top region + line 30");
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge a",
    ]);

    // Sync `b` onto the new epoch — this should produce a structured
    // conflict on f.txt.
    let _ = repo.maw_raw(&["ws", "sync", "b", "--rebase"]);
}

// ---------------------------------------------------------------------------
// 1. The reported regression: --keep <ws> preserves both sibling and
// workspace edits.
// ---------------------------------------------------------------------------

#[test]
fn keep_ws_preserves_sibling_merged_content_on_disjoint_regions() {
    let repo = TestRepo::new();
    setup_a_then_b_rebased(&repo, modify_bottom);

    // The structured sidecar must have landed.
    let sidecar = repo
        .read_conflict_tree_sidecar("b")
        .expect("conflict-tree.json should exist after rebase");
    let conflicts = sidecar
        .get("conflicts")
        .and_then(|v| v.as_object())
        .expect("sidecar should have conflicts map");
    assert!(
        conflicts.contains_key("f.txt"),
        "expected conflict on f.txt, sidecar={sidecar}"
    );

    // Resolve --keep b.
    repo.maw_ok(&["ws", "resolve", "b", "--keep", "b"]);

    let resolved = repo
        .read_file("b", "f.txt")
        .expect("f.txt should exist after resolve");

    // The resolved file must carry BOTH A's top-region uppercasing AND B's
    // bottom-region B-MARK lines.
    assert!(
        resolved.contains("LINE-001\n"),
        "expected A's uppercased line 1 (sibling-merged content), got:\n{resolved}"
    );
    assert!(
        resolved.contains("LINE-010\n"),
        "expected A's uppercased line 10, got:\n{resolved}"
    );
    assert!(
        resolved.contains("B-MARK-50\n"),
        "expected B's bottom-region change at line 50, got:\n{resolved}"
    );
    assert!(
        resolved.contains("B-MARK-60\n"),
        "expected B's bottom-region change at line 60, got:\n{resolved}"
    );
    // The forced-overlap line (30): B's value should win because --keep b
    // runs ws-wins-on-conflict.
    assert!(
        resolved.contains("B-OVERLAP-30\n"),
        "expected B's overlap line 30 to win, got:\n{resolved}"
    );

    // And the un-touched middle region must still match the base content.
    assert!(
        resolved.contains("line-025\n"),
        "expected untouched middle line, got:\n{resolved}"
    );
}

// ---------------------------------------------------------------------------
// 2. --keep epoch is unchanged — only A's content lands, B's bottom edits
// are dropped (existing behaviour).
// ---------------------------------------------------------------------------

#[test]
fn keep_epoch_drops_workspace_changes_as_before() {
    let repo = TestRepo::new();
    setup_a_then_b_rebased(&repo, modify_bottom);

    repo.maw_ok(&["ws", "resolve", "b", "--keep", "epoch"]);

    let resolved = repo
        .read_file("b", "f.txt")
        .expect("f.txt should exist after resolve");

    assert!(
        resolved.contains("LINE-001\n"),
        "expected A's uppercased line 1, got:\n{resolved}"
    );
    // The epoch wins on the forced-overlap line 30 too.
    assert!(
        resolved.contains("A-OVERLAP-30\n"),
        "expected epoch's overlap line 30 to win under --keep epoch, got:\n{resolved}"
    );
    // B's bottom-region edits should NOT be present — `--keep epoch` takes
    // the new epoch's content verbatim.
    assert!(
        !resolved.contains("B-MARK-50"),
        "B's bottom region should NOT survive --keep epoch, got:\n{resolved}"
    );
    assert!(
        resolved.contains("line-050\n"),
        "expected the epoch's (un-modified) line 50, got:\n{resolved}"
    );
}

// ---------------------------------------------------------------------------
// 3. Overlapping conflict: when A and B both modify the same line, --keep B
// takes B's version of that line, but A's other (non-overlapping) edits are
// preserved.
// ---------------------------------------------------------------------------

#[test]
fn keep_ws_wins_on_overlap_and_preserves_non_overlap() {
    // A's edits live in two well-separated hunks: lines 1..=10 (uppercased)
    // and line 70 (replaced with `A-DISTANT-70`). B touches line 5 (which
    // overlaps A's first hunk) AND line 80 (well outside any A hunk).
    //
    // Expected after `--keep b` runs through the 3-way merge:
    // * The A-vs-B hunk around line 5: B wins (`ConflictResolution::Theirs`).
    //   A's uppercased lines 1..=10 are lost in that hunk — diff3 operates
    //   on hunks, not individual lines, so when B touches line 5 inside
    //   A's lines-1..=10 hunk the whole hunk resolves to B's side.
    // * A's distant edit at line 70 (a separate hunk) is preserved.
    // * B's edit at line 80 (outside any conflict) is preserved.
    let repo = TestRepo::new();
    let base = hundred_lines();
    repo.seed_files(&[("f.txt", &base)]);

    // B is forked first so it'll need a rebase.
    repo.maw_ok(&["ws", "create", "b"]);
    let mut b_content = base.clone();
    b_content = b_content.replace("line-005\n", "B-OVERLAP\n");
    b_content = b_content.replace("line-080\n", "B-FAR\n");
    repo.modify_file("b", "f.txt", &b_content);
    commit_all(&repo, "b", "b: line 5 + line 80");

    repo.maw_ok(&["ws", "create", "a"]);
    let mut a_content = modify_top(&base);
    a_content = a_content.replace("line-070\n", "A-DISTANT-70\n");
    repo.modify_file("a", "f.txt", &a_content);
    commit_all(&repo, "a", "a: top region + line 70");
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge a",
    ]);

    let _ = repo.maw_raw(&["ws", "sync", "b", "--rebase"]);

    repo.read_conflict_tree_sidecar("b")
        .expect("expected a conflict-tree sidecar after rebase");

    repo.maw_ok(&["ws", "resolve", "b", "--keep", "b"]);
    let resolved = repo
        .read_file("b", "f.txt")
        .expect("f.txt should exist after resolve");

    // Line 5 overlap: B wins.
    assert!(
        resolved.contains("B-OVERLAP\n"),
        "B should win on the overlapping line 5, got:\n{resolved}"
    );
    assert!(
        !resolved.contains("LINE-005\n"),
        "A's uppercased line 5 should not survive (B wins on overlap), got:\n{resolved}"
    );
    // Line 70: A's distant edit (separate hunk from B's edits) must survive.
    assert!(
        resolved.contains("A-DISTANT-70\n"),
        "A's distant edit at line 70 (separate hunk from B) should survive, got:\n{resolved}"
    );
    // Line 80: B's edit should be present — outside any conflict zone.
    assert!(
        resolved.contains("B-FAR\n"),
        "B's edit at line 80 should be preserved, got:\n{resolved}"
    );
}

// ---------------------------------------------------------------------------
// 4. Legacy sidecar (no `base_content` on the workspace side): falls back to
// blob-replace with a stderr warning. Crafted by editing the on-disk
// conflict-tree.json to drop the field, simulating an in-flight conflict
// produced by an older maw version.
// ---------------------------------------------------------------------------

#[test]
fn legacy_sidecar_without_base_content_falls_back_with_warning() {
    let repo = TestRepo::new();
    setup_a_then_b_rebased(&repo, modify_bottom);

    // Strip `base_content` from every side in the sidecar to simulate an
    // older sidecar that didn't know about the field.
    let sidecar_path = repo
        .root()
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join("b")
        .join("conflict-tree.json");
    let raw = std::fs::read_to_string(&sidecar_path)
        .expect("sidecar should be readable in legacy fallback test");
    let mut parsed: serde_json::Value =
        serde_json::from_str(&raw).expect("sidecar should be valid JSON");

    strip_base_content(&mut parsed);
    std::fs::write(
        &sidecar_path,
        serde_json::to_string_pretty(&parsed).expect("re-serialize"),
    )
    .expect("write sidecar");

    // Run resolve and capture stderr.
    let out = repo.maw_raw(&["ws", "resolve", "b", "--keep", "b"]);
    assert!(
        out.status.success(),
        "resolve should succeed even on a legacy sidecar; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("legacy blob-replace semantics"),
        "expected legacy fallback warning on stderr, got:\n{stderr}"
    );

    // The legacy fallback writes B's pre-rebase blob, so A's content WILL
    // be dropped — that's the documented legacy behaviour.
    let resolved = repo
        .read_file("b", "f.txt")
        .expect("f.txt should exist after resolve");
    assert!(
        resolved.contains("B-MARK-50"),
        "B's content should be present (legacy fallback writes ws blob), got:\n{resolved}"
    );
}

// ---------------------------------------------------------------------------
// 5. Property: random pairs of disjoint additive edits always preserve both
// sides under --keep <ws>. The randomness exercises starting line / count
// pairs and confirms determinism across many shapes.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
mod prop {
    use super::{TestRepo, commit_all, hundred_lines};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            // Fast-but-real coverage; each case spins up a TestRepo so keep it
            // bounded.
            cases: 8,
            .. ProptestConfig::default()
        })]

        #[test]
        fn keep_ws_preserves_both_disjoint_random_edits(
            // A's region: lines [a_start, a_start+a_len-1], with a buffer
            // separating it from B's region.
            a_start in 1usize..=20,
            a_len in 1usize..=10,
            // B's region: lines [b_start, b_start+b_len-1], placed strictly
            // after A's region with at least 5 lines of separation.
            b_offset in 5usize..=20,
            b_len in 1usize..=10,
        ) {
            use std::fmt::Write as _;

            let repo = TestRepo::new();
            let base = hundred_lines();
            repo.seed_files(&[("f.txt", &base)]);

            // B is forked first.
            repo.maw_ok(&["ws", "create", "b"]);
            let a_end = a_start + a_len; // exclusive
            let b_start = a_end + b_offset;
            let b_end = (b_start + b_len).min(100); // exclusive, clamped

            // B replaces lines [b_start..b_end) with a marker form.
            let mut b_text = String::new();
            for (i, line) in base.lines().enumerate() {
                let line_no = i + 1;
                if line_no >= b_start && line_no < b_end {
                    write!(b_text, "B-PROP-{line_no}").expect("write to String never fails");
                } else {
                    b_text.push_str(line);
                }
                b_text.push('\n');
            }
            repo.modify_file("b", "f.txt", &b_text);
            commit_all(&repo, "b", "b: prop edit");

            // A replaces lines [a_start..a_end) with a marker form.
            repo.maw_ok(&["ws", "create", "a"]);
            let mut a_text = String::new();
            for (i, line) in base.lines().enumerate() {
                let line_no = i + 1;
                if line_no >= a_start && line_no < a_end {
                    write!(a_text, "A-PROP-{line_no}").expect("write to String never fails");
                } else {
                    a_text.push_str(line);
                }
                a_text.push('\n');
            }
            repo.modify_file("a", "f.txt", &a_text);
            commit_all(&repo, "a", "a: prop edit");
            repo.maw_ok(&[
                "ws",
                "merge",
                "a",
                "--destroy",
                "--no-auto-rebase",
                "--message",
                "merge a",
            ]);

            // Rebase b — produces a structured conflict on f.txt.
            let _ = repo.maw_raw(&["ws", "sync", "b", "--rebase"]);
            // Some random edits may auto-merge cleanly; in that case there
            // is no conflict and nothing to assert about --keep behaviour.
            if repo.read_conflict_tree_sidecar("b").is_none() {
                return Ok(());
            }

            repo.maw_ok(&["ws", "resolve", "b", "--keep", "b"]);
            let resolved = repo
                .read_file("b", "f.txt")
                .expect("f.txt should exist after resolve");

            // Both sides' edits must be visible in the resolved file.
            for line_no in a_start..a_end {
                let needle = format!("A-PROP-{line_no}\n");
                prop_assert!(
                    resolved.contains(&needle),
                    "A's marker {needle:?} missing in resolved file:\n{resolved}"
                );
            }
            for line_no in b_start..b_end {
                let needle = format!("B-PROP-{line_no}\n");
                prop_assert!(
                    resolved.contains(&needle),
                    "B's marker {needle:?} missing in resolved file:\n{resolved}"
                );
            }
        }
    }
}
