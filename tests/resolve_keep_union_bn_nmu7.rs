//! Integration tests for `maw ws resolve --keep union` (bn-nmu7).
//!
//! `--keep union` is `--keep both` (bn-1nwn: per-hunk 3-way union merge) with
//! an added post-processing pass: within each genuinely conflicting hunk,
//! duplicate lines shared by both sides are dropped (keeping the `ours`
//! occurrence, `ours`-first) while `theirs`-only lines keep their relative
//! order. Cleanly-merged (non-conflicting) hunks are untouched — identical to
//! `--keep both`. Binary / N>2-side / no-base (`AddAdd`) conflicts fall back to
//! the same legacy blob-concat behavior as `--keep both` (no dedup).
//!
//! These tests drive the real `maw` binary end-to-end: seed a base file,
//! diverge it in two workspaces, force a rebase conflict, then resolve with
//! `--keep union` (and, for comparison, `--keep both`) and inspect the
//! resulting worktree content.

mod manifold_common;

use manifold_common::TestRepo;

/// Commit every dirty path in `workspace` with `message`.
fn commit_all(repo: &TestRepo, workspace: &str, message: &str) {
    repo.git_in_workspace(workspace, &["add", "-A"]);
    repo.git_in_workspace(workspace, &["commit", "-m", message]);
}

/// Force a genuine rebase conflict on `path`: `ws_name` diverges from
/// `base` to `ws_content`, while a throwaway `epoch-src` workspace advances
/// the shared epoch from `base` to `epoch_content`. Rebasing `ws_name` then
/// hits a real 3-way conflict on `path` (epoch = `epoch_content`, ws =
/// `ws_content`, base = `base`).
///
/// # Panics
/// Panics (via the underlying `TestRepo` helpers) if any git/maw command
/// fails unexpectedly.
fn setup_content_conflict(
    repo: &TestRepo,
    path: &str,
    base: &str,
    epoch_content: &str,
    ws_content: &str,
    ws_name: &str,
) {
    repo.seed_files(&[(path, base)]);

    // ws_name diverges from base first, so it predates the epoch advance
    // below and will need a rebase.
    repo.maw_ok(&["ws", "create", ws_name]);
    repo.modify_file(ws_name, path, ws_content);
    commit_all(repo, ws_name, "ws change");

    // A throwaway workspace advances the epoch with a different edit to the
    // same path, then merges (fast-forwarding main / the shared epoch).
    let epoch_src = format!("{ws_name}-epoch-src");
    repo.maw_ok(&["ws", "create", &epoch_src]);
    repo.modify_file(&epoch_src, path, epoch_content);
    commit_all(repo, &epoch_src, "epoch change");
    repo.maw_ok(&[
        "ws",
        "merge",
        &epoch_src,
        "--into",
        "default",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge epoch-src",
    ]);

    // Rebase ws_name onto the new epoch. This must produce a conflict.
    let out = repo.maw_raw(&["ws", "sync", ws_name, "--rebase"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("conflict") || combined.contains("Conflict"),
        "expected rebase of '{ws_name}' to report a conflict on '{path}'\n{combined}"
    );
    assert!(
        repo.read_conflict_tree_sidecar(ws_name).is_some(),
        "expected a structured conflict sidecar for '{ws_name}' after rebase"
    );
}

/// Split file content into a `Vec<&str>` of non-empty trailing lines
/// (trailing `\n` does not produce a spurious empty final element).
fn lines(content: &str) -> Vec<&str> {
    content.lines().collect()
}

// ---------------------------------------------------------------------------
// 1. Two sides each add a distinct line -> both present once, ours-first.
// ---------------------------------------------------------------------------

#[test]
fn keep_union_includes_both_distinct_additions_ours_first() {
    let repo = TestRepo::new();
    setup_content_conflict(
        &repo,
        "list.txt",
        "apple\nbanana\ncherry\n",
        "apple\nbanana\ncherry\ndate\n", // epoch (ours) adds "date"
        "apple\nbanana\ncherry\nfig\n",  // ws (theirs) adds "fig"
        "ws-union-distinct",
    );

    let out = repo.maw_raw(&["ws", "resolve", "ws-union-distinct", "--keep", "union"]);
    assert!(
        out.status.success(),
        "resolve --keep union should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = repo
        .read_file("ws-union-distinct", "list.txt")
        .expect("list.txt should exist after resolve");
    assert!(
        !after.contains("<<<<<<<"),
        "resolved file must not contain conflict markers, got:\n{after}"
    );

    let got = lines(&after);
    assert_eq!(
        got,
        vec!["apple", "banana", "cherry", "date", "fig"],
        "expected ours' addition (date) before theirs' addition (fig), each once, got:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// 2. Identical line added by both sides -> appears once.
// ---------------------------------------------------------------------------

#[test]
fn keep_union_dedups_identical_line_added_by_both_sides() {
    let repo = TestRepo::new();
    setup_content_conflict(
        &repo,
        "list2.txt",
        "apple\nbanana\ncherry\n",
        "apple\nbanana\ncherry\nshared_addition\n", // epoch adds shared_addition only
        "apple\nbanana\ncherry\nshared_addition\nbob_only\n", // ws adds shared_addition + bob_only
        "ws-union-identical",
    );

    let out = repo.maw_raw(&["ws", "resolve", "ws-union-identical", "--keep", "union"]);
    assert!(
        out.status.success(),
        "resolve --keep union should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = repo
        .read_file("ws-union-identical", "list2.txt")
        .expect("list2.txt should exist after resolve");
    assert!(
        !after.contains("<<<<<<<"),
        "resolved file must not contain conflict markers, got:\n{after}"
    );

    let got = lines(&after);
    assert_eq!(
        got,
        vec!["apple", "banana", "cherry", "shared_addition", "bob_only"],
        "shared_addition (added identically by both sides) must appear exactly once, got:\n{after}"
    );
    assert_eq!(
        after.matches("shared_addition").count(),
        1,
        "shared_addition must appear exactly once, got:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// 3. Theirs-only lines keep their original relative order.
// ---------------------------------------------------------------------------

#[test]
fn keep_union_preserves_theirs_only_relative_order() {
    let repo = TestRepo::new();
    setup_content_conflict(
        &repo,
        "list3.txt",
        "apple\nbanana\ncherry\n",
        "apple\nbanana\ncherry\ndate\n", // epoch (ours) adds one line
        "apple\nbanana\ncherry\nfig\ngrape\nkiwi\n", // ws (theirs) adds three, in order
        "ws-union-order",
    );

    let out = repo.maw_raw(&["ws", "resolve", "ws-union-order", "--keep", "union"]);
    assert!(
        out.status.success(),
        "resolve --keep union should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = repo
        .read_file("ws-union-order", "list3.txt")
        .expect("list3.txt should exist after resolve");

    let got = lines(&after);
    assert_eq!(
        got,
        vec!["apple", "banana", "cherry", "date", "fig", "grape", "kiwi"],
        "theirs-only lines (fig, grape, kiwi) must keep their original relative order \
         after ours' addition (date), got:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// 4. Base lines deleted by one side are not resurrected.
// ---------------------------------------------------------------------------

#[test]
fn keep_union_does_not_resurrect_line_deleted_by_one_side() {
    let repo = TestRepo::new();
    setup_content_conflict(
        &repo,
        "del.txt",
        "apple\nbanana\ncherry\n",
        "apple\ncherry\n",            // epoch (ours) deletes "banana"
        "apple\nBANANA_WS\ncherry\n", // ws (theirs) modifies "banana"
        "ws-union-delete",
    );

    let out = repo.maw_raw(&["ws", "resolve", "ws-union-delete", "--keep", "union"]);
    assert!(
        out.status.success(),
        "resolve --keep union should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = repo
        .read_file("ws-union-delete", "del.txt")
        .expect("del.txt should exist after resolve");

    // The original base line "banana" (lowercase, unmodified) must not
    // resurface — only ws's edited "BANANA_WS" survives (ours dropped it).
    assert!(
        !after.lines().any(|l| l == "banana"),
        "base line 'banana' deleted by epoch must not be resurrected, got:\n{after}"
    );
    let got = lines(&after);
    assert_eq!(
        got,
        vec!["apple", "BANANA_WS", "cherry"],
        "expected ws's edit to survive and base's deleted line to stay gone, got:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// 5. Cleanly-merged (non-conflicting) hunks are identical to --keep both;
//    the conflicting region differs (dedup vs. naive concat).
// ---------------------------------------------------------------------------

#[test]
fn keep_union_clean_hunks_match_keep_both_conflicting_hunk_differs() {
    let repo = TestRepo::new();
    // Generous unchanged padding on both sides of "orig" keeps the TOP / BOTTOM
    // disjoint edits from being folded into the same diff hunk as the
    // "orig" replacement below.
    let base = "TOP\npadA\npadB\npadC\norig\npadD\npadE\npadF\nBOTTOM\n";
    // epoch: disjoint edit to TOP, plus replaces "orig" with "A/shared/B".
    let epoch_content = "TOP_EPOCH\npadA\npadB\npadC\nA\nshared\nB\npadD\npadE\npadF\nBOTTOM\n";
    // ws: disjoint edit to BOTTOM, plus replaces "orig" with "C/shared/D" —
    // a genuinely different replacement that happens to share the middle
    // "shared" line with epoch's. This is the bn-1nwn wrinkle: gix's
    // per-hunk `ResolveWithUnion` driver concatenates each side's full
    // conflicting-hunk content verbatim (no cross-side dedup), so
    // `--keep both` duplicates "shared"; `--keep union` must dedup it to a
    // single occurrence while keeping A/B (ours) and C/D (theirs, minus the
    // duplicate) and leaving the disjoint TOP/BOTTOM edits untouched.
    let ws_content = "TOP\npadA\npadB\npadC\nC\nshared\nD\npadD\npadE\npadF\nBOTTOM_WS\n";

    setup_content_conflict(
        &repo,
        "mixed.txt",
        base,
        epoch_content,
        ws_content,
        "ws-clean-union",
    );
    setup_content_conflict(
        &repo,
        "mixed.txt",
        base,
        epoch_content,
        ws_content,
        "ws-clean-both",
    );

    let union_out = repo.maw_raw(&["ws", "resolve", "ws-clean-union", "--keep", "union"]);
    assert!(
        union_out.status.success(),
        "resolve --keep union should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&union_out.stdout),
        String::from_utf8_lossy(&union_out.stderr)
    );
    let both_out = repo.maw_raw(&["ws", "resolve", "ws-clean-both", "--keep", "both"]);
    assert!(
        both_out.status.success(),
        "resolve --keep both should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&both_out.stdout),
        String::from_utf8_lossy(&both_out.stderr)
    );

    let union_after = repo
        .read_file("ws-clean-union", "mixed.txt")
        .expect("mixed.txt should exist after --keep union resolve");
    let both_after = repo
        .read_file("ws-clean-both", "mixed.txt")
        .expect("mixed.txt should exist after --keep both resolve");

    // The disjoint (cleanly-merged) edits must be present, identically, in
    // both outputs — the dedup pass must never touch non-conflicting hunks.
    for content in [&union_after, &both_after] {
        assert!(
            content.contains("TOP_EPOCH"),
            "epoch's disjoint TOP edit must survive untouched, got:\n{content}"
        );
        assert!(
            content.contains("BOTTOM_WS"),
            "ws's disjoint BOTTOM edit must survive untouched, got:\n{content}"
        );
    }
    assert_eq!(
        lines(&union_after).first(),
        lines(&both_after).first(),
        "first line (epoch's clean TOP edit) must match between --keep union and --keep both"
    );
    assert_eq!(
        lines(&union_after).last(),
        lines(&both_after).last(),
        "last line (ws's clean BOTTOM edit) must match between --keep union and --keep both"
    );

    // The conflicting region itself must differ: --keep both's naive concat
    // duplicates "shared" (present verbatim in both sides' hunk content),
    // while --keep union dedups it to a single occurrence.
    let shared_count_both = both_after.lines().filter(|l| *l == "shared").count();
    let shared_count_union = union_after.lines().filter(|l| *l == "shared").count();
    assert_eq!(
        shared_count_both, 2,
        "sanity: --keep both is expected to duplicate 'shared' (present in both \
         sides' replacement of 'orig') inside the conflict block, \
         got {shared_count_both} occurrence(s) in:\n{both_after}"
    );
    assert_eq!(
        shared_count_union, 1,
        "--keep union must dedup 'shared' to a single occurrence, got:\n{union_after}"
    );

    // Full expected union output: ours' replacement (A, shared, B) verbatim,
    // then theirs' non-duplicate lines (C, D) in their original order —
    // exactly the same dedup contract as the earlier list-based tests,
    // just via a genuinely conflicting (not auto-merged) hunk this time.
    let got = lines(&union_after);
    assert_eq!(
        got,
        vec![
            "TOP_EPOCH",
            "padA",
            "padB",
            "padC",
            "A",
            "shared",
            "B",
            "C",
            "D",
            "padD",
            "padE",
            "padF",
            "BOTTOM_WS",
        ],
        "got:\n{union_after}"
    );
    assert!(
        !union_after.contains("<<<<<<<"),
        "resolved file must not contain conflict markers, got:\n{union_after}"
    );
}

// ---------------------------------------------------------------------------
// 6. AddAdd (no base) falls back to the same behavior as --keep both.
// ---------------------------------------------------------------------------

#[test]
fn keep_union_add_add_matches_keep_both_fallback() {
    let repo = TestRepo::new();
    repo.seed_files(&[("placeholder.txt", "seed\n")]);

    // Two workspaces both ADD the same new path with no common ancestor
    // blob -- a genuine add/add conflict (no base OID available).
    repo.maw_ok(&["ws", "create", "ws-addadd-union"]);
    repo.add_file("ws-addadd-union", "new.txt", "WS_NEW\n");
    commit_all(&repo, "ws-addadd-union", "ws: new.txt");

    repo.maw_ok(&["ws", "create", "ws-addadd-both"]);
    repo.add_file("ws-addadd-both", "new.txt", "WS_NEW\n");
    commit_all(&repo, "ws-addadd-both", "ws: new.txt");

    repo.maw_ok(&["ws", "create", "addadd-epoch-src"]);
    repo.add_file("addadd-epoch-src", "new.txt", "EPOCH_NEW\n");
    commit_all(&repo, "addadd-epoch-src", "epoch: new.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "addadd-epoch-src",
        "--into",
        "default",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge addadd-epoch-src",
    ]);

    for ws in ["ws-addadd-union", "ws-addadd-both"] {
        let out = repo.maw_raw(&["ws", "sync", ws, "--rebase"]);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            combined.contains("conflict") || combined.contains("Conflict"),
            "expected rebase of '{ws}' to report an add/add conflict on new.txt\n{combined}"
        );
    }

    let union_out = repo.maw_raw(&["ws", "resolve", "ws-addadd-union", "--keep", "union"]);
    assert!(
        union_out.status.success(),
        "resolve --keep union should succeed on add/add fallback\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&union_out.stdout),
        String::from_utf8_lossy(&union_out.stderr)
    );
    let both_out = repo.maw_raw(&["ws", "resolve", "ws-addadd-both", "--keep", "both"]);
    assert!(
        both_out.status.success(),
        "resolve --keep both should succeed on add/add\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&both_out.stdout),
        String::from_utf8_lossy(&both_out.stderr)
    );

    let union_after = repo
        .read_file("ws-addadd-union", "new.txt")
        .expect("new.txt should exist after --keep union resolve");
    let both_after = repo
        .read_file("ws-addadd-both", "new.txt")
        .expect("new.txt should exist after --keep both resolve");

    assert_eq!(
        union_after, both_after,
        "no-base (add/add) fallback must behave identically for --keep union and --keep both \
         (plain concat, no dedup invented for the whole-file fallback path)"
    );
    // Both sides' content must be present (concat fallback, not a single-side pick).
    assert!(union_after.contains("WS_NEW"), "got:\n{union_after}");
    assert!(union_after.contains("EPOCH_NEW"), "got:\n{union_after}");
}
