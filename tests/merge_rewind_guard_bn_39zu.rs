//! bn-39zu: merge guard that detects a source workspace's change being a
//! **pure rewind** of a path to an older epoch's blob, and warns loudly.
//!
//! Motivation (continuum field report, 2026-08-11): an agent inside a
//! workspace accidentally rewrote an append-only file
//! (`.bones/events/2026-08.events`) back to a stale, already-superseded
//! copy and committed it. `maw ws sync` and `maw ws merge` faithfully
//! replayed the rewind into trunk, silently deleting 20 event lines. maw
//! did nothing wrong mechanically — the sides really did agree, byte for
//! byte, with the workspace's stale content — but the merge engine is the
//! last chokepoint that can catch this class of accident before it lands.
//!
//! These tests pin the mechanism-independent hardening: when a source
//! workspace's new content for a changed path is byte-identical to that
//! path's content at an earlier commit in the merge base's own history
//! (and different from the merge base's current content), the merge must:
//! - print a loud `WARNING:` line naming the path and the ancestor,
//! - still succeed (warn-only, never blocks),
//! - land the workspace's (rewound) content — the warning does not alter
//!   the merge result,
//! - surface the same signal in `warnings[]` in `--format json` output.
//!
//! Non-rewind changes (genuinely new content) must never trigger the
//! warning, and a legitimate revert of only the immediately preceding
//! change is still flagged (it is indistinguishable from an accident at
//! the merge engine's level — the human decides what to do with the
//! warning).
//!
//! The second half of this file covers the opt-in, harder guard: `[merge]
//! append_only` globs (bn-39zu item 2). Unlike the rewind warning, this one
//! REFUSES the merge outright when a matched path's merged result does not
//! keep the epoch tip's bytes as an exact prefix (lines removed or
//! rewritten, or the path deleted) — bypassable only with `--force`.

mod manifold_common;

use manifold_common::TestRepo;

/// Overwrite `.manifold/config.toml` with the given `[merge] append_only`
/// globs (plus the `[repo] branch = "main"` section every `TestRepo` needs).
fn set_append_only_config(repo: &TestRepo, globs: &[&str]) {
    let globs_toml = globs
        .iter()
        .map(|g| format!("\"{g}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!("[repo]\nbranch = \"main\"\n\n[merge]\nappend_only = [{globs_toml}]\n");
    std::fs::write(repo.root().join(".manifold").join("config.toml"), toml)
        .expect("failed to write .manifold/config.toml");
}

fn parse_json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}):\n{stdout}"))
}

// ---------------------------------------------------------------------------
// 1. Rewind case: A -> B -> C across epochs, workspace restores exactly A.
// ---------------------------------------------------------------------------

#[test]
fn merge_warns_on_pure_rewind_to_older_epoch_blob() {
    let repo = TestRepo::new();

    // Trunk evolves journal.txt across three epochs: A -> B -> C.
    repo.seed_files(&[("journal.txt", "A\n")]); // epoch: content "A\n"
    repo.modify_file("default", "journal.txt", "A\nB\n");
    repo.advance_epoch("chore: append B"); // epoch: content "A\nB\n"
    repo.modify_file("default", "journal.txt", "A\nB\nC\n");
    repo.advance_epoch("chore: append C"); // epoch: content "A\nB\nC\n"

    // Workspace created at C accidentally restores the file to exactly A's
    // content (the stale-copy accident from the field report).
    repo.maw_ok(&["ws", "create", "rewinder"]);
    repo.modify_file("rewinder", "journal.txt", "A\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "rewinder",
        "--message",
        "feat: oops",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);

    assert_eq!(v["status"], "success", "merge must succeed:\n{stdout}");

    let warnings = v["warnings"]
        .as_array()
        .expect("warnings[] must be present");
    assert!(
        warnings.iter().any(|w| {
            let w = w.as_str().unwrap_or_default();
            w.contains("rewinder") && w.contains("journal.txt") && w.contains("rewinds")
        }),
        "warnings[] must name the workspace and the rewound path:\n{warnings:#?}"
    );
    // 2 first-parent commits behind the epoch (C's parent is B, B's parent is A).
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("2 commits behind")),
        "warnings[] must report the distance behind the epoch:\n{warnings:#?}"
    );

    // The warn does not alter the result: the merged content is A's content.
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("A\n"),
        "merge must still apply the workspace's (rewound) content"
    );
}

/// Same scenario in text mode: the loud `WARNING:` line must be present and
/// the merge sentinel must still show success.
#[test]
fn merge_text_output_prints_loud_warning_line_on_rewind() {
    let repo = TestRepo::new();

    repo.seed_files(&[("journal.txt", "A\n")]);
    repo.modify_file("default", "journal.txt", "A\nB\n");
    repo.advance_epoch("chore: append B");
    repo.modify_file("default", "journal.txt", "A\nB\nC\n");
    repo.advance_epoch("chore: append C");

    repo.maw_ok(&["ws", "create", "rewinder2"]);
    repo.modify_file("rewinder2", "journal.txt", "A\n");

    let stdout = repo.maw_ok(&["ws", "merge", "rewinder2", "--message", "feat: oops"]);

    assert!(
        stdout
            .lines()
            .any(|l| l.contains("WARNING:") && l.contains("rewinds") && l.contains("journal.txt")),
        "text output must contain a loud WARNING line naming the rewound path:\n{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l.starts_with("[OK] merged ")),
        "merge must still report success:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// 2. Non-rewind control: genuinely new content must not warn.
// ---------------------------------------------------------------------------

#[test]
fn merge_does_not_warn_on_genuinely_new_content() {
    let repo = TestRepo::new();

    repo.seed_files(&[("journal.txt", "A\n")]);
    repo.modify_file("default", "journal.txt", "A\nB\n");
    repo.advance_epoch("chore: append B");
    repo.modify_file("default", "journal.txt", "A\nB\nC\n");
    repo.advance_epoch("chore: append C");

    // Workspace appends genuinely new content that never existed before.
    repo.maw_ok(&["ws", "create", "adder"]);
    repo.modify_file("adder", "journal.txt", "A\nB\nC\nD\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "adder",
        "--message",
        "feat: append D",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);

    assert_eq!(v["status"], "success", "merge must succeed:\n{stdout}");
    let warnings = v["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("rewinds")),
        "genuinely new content must not trigger a rewind warning:\n{warnings:#?}"
    );
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("A\nB\nC\nD\n")
    );
}

// ---------------------------------------------------------------------------
// 3. Legitimate revert of only the immediately previous change: still warns.
// ---------------------------------------------------------------------------

#[test]
fn merge_warns_but_succeeds_on_revert_of_immediately_previous_change() {
    let repo = TestRepo::new();

    // Only one prior change: orig -> orig+changed.
    repo.seed_files(&[("config.txt", "orig\n")]);
    repo.modify_file("default", "config.txt", "orig\nchanged\n");
    repo.advance_epoch("chore: apply change");

    // Workspace reverts exactly to the immediately preceding content.
    repo.maw_ok(&["ws", "create", "reverter"]);
    repo.modify_file("reverter", "config.txt", "orig\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "reverter",
        "--message",
        "revert: back out change",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);

    assert_eq!(
        v["status"], "success",
        "a legitimate revert must not be blocked:\n{stdout}"
    );
    let warnings = v["warnings"]
        .as_array()
        .expect("warnings[] must be present");
    assert!(
        warnings.iter().any(|w| {
            let w = w.as_str().unwrap_or_default();
            w.contains("config.txt") && w.contains("1 commit behind")
        }),
        "warnings[] must flag the 1-commit-behind revert:\n{warnings:#?}"
    );
    assert_eq!(
        repo.read_file("default", "config.txt").as_deref(),
        Some("orig\n"),
        "the legitimate revert must still land"
    );
}

// ---------------------------------------------------------------------------
// 4. Deletion is not a rewind.
// ---------------------------------------------------------------------------

#[test]
fn merge_does_not_warn_on_deletion() {
    let repo = TestRepo::new();

    repo.seed_files(&[("journal.txt", "A\n")]);
    repo.modify_file("default", "journal.txt", "A\nB\n");
    repo.advance_epoch("chore: append B");

    repo.maw_ok(&["ws", "create", "deleter"]);
    repo.delete_file("deleter", "journal.txt");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "deleter",
        "--message",
        "chore: remove journal",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);

    assert_eq!(v["status"], "success");
    let warnings = v["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("rewinds")),
        "deletion must never be classified as a rewind:\n{warnings:#?}"
    );
}

// ---------------------------------------------------------------------------
// 5. append_only: deleting a middle line refuses the merge; --force overrides.
// ---------------------------------------------------------------------------

#[test]
fn merge_refuses_append_only_violation_deleting_middle_line() {
    let repo = TestRepo::new();
    set_append_only_config(&repo, &["journal.txt"]);

    repo.seed_files(&[("journal.txt", "L1\nL2\nL3\nL4\n")]);

    // Workspace removes L2 from the middle — not a byte-prefix-preserving
    // change, so this must violate append-only.
    repo.maw_ok(&["ws", "create", "middle-deleter"]);
    repo.modify_file("middle-deleter", "journal.txt", "L1\nL3\nL4\n");

    for mode in ["--check", "--plan"] {
        let stderr = repo.maw_fails(&["ws", "merge", "middle-deleter", mode]);
        assert!(
            stderr.contains("journal.txt")
                && (stderr.contains("append-only") || stderr.contains("append only")),
            "{mode} must report the same append-only refusal as the real merge:\n{stderr}"
        );
    }

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "middle-deleter",
        "--message",
        "chore: oops removed a line",
    ]);
    assert!(
        stderr.contains("journal.txt"),
        "refusal must name the violating path:\n{stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("append-only")
            || stderr.to_lowercase().contains("append only"),
        "refusal must explain this is an append-only violation:\n{stderr}"
    );
    assert!(
        stderr.contains("--force"),
        "refusal must mention the --force override:\n{stderr}"
    );

    // The trunk file must be untouched by the refused merge.
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("L1\nL2\nL3\nL4\n"),
        "a refused merge must not modify trunk"
    );

    // The exact same merge with --force must succeed and land the
    // (line-deleting) content, with a bypass warning.
    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "middle-deleter",
        "--message",
        "chore: oops removed a line",
        "--force",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);
    assert_eq!(
        v["status"], "success",
        "--force must let the merge through:\n{stdout}"
    );
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("L1\nL3\nL4\n"),
        "--force must land the workspace's content despite the violation"
    );
}

// ---------------------------------------------------------------------------
// 6. append_only: a pure append merges cleanly (no refusal).
// ---------------------------------------------------------------------------

#[test]
fn merge_allows_append_only_pure_append() {
    let repo = TestRepo::new();
    set_append_only_config(&repo, &["journal.txt"]);

    repo.seed_files(&[("journal.txt", "L1\nL2\nL3\nL4\n")]);

    repo.maw_ok(&["ws", "create", "appender"]);
    repo.modify_file("appender", "journal.txt", "L1\nL2\nL3\nL4\nL5\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "appender",
        "--message",
        "chore: append L5",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);

    assert_eq!(
        v["status"], "success",
        "a pure append must merge cleanly:\n{stdout}"
    );
    let warnings = v["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("append-only")),
        "a pure append must not trigger any append-only warning:\n{warnings:#?}"
    );
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("L1\nL2\nL3\nL4\nL5\n")
    );
}

// ---------------------------------------------------------------------------
// 7. append_only: deleting the whole matched file refuses the merge.
// ---------------------------------------------------------------------------

#[test]
fn merge_refuses_append_only_violation_on_deletion() {
    let repo = TestRepo::new();
    set_append_only_config(&repo, &["journal.txt"]);

    repo.seed_files(&[("journal.txt", "L1\nL2\n")]);

    repo.maw_ok(&["ws", "create", "file-deleter"]);
    repo.delete_file("file-deleter", "journal.txt");

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "file-deleter",
        "--message",
        "chore: remove journal (oops)",
    ]);
    assert!(
        stderr.contains("journal.txt"),
        "refusal must name the deleted append-only path:\n{stderr}"
    );
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("L1\nL2\n"),
        "a refused merge must not modify trunk"
    );
}

// ---------------------------------------------------------------------------
// 8. append_only: a brand-new file matching the glob is always fine.
// ---------------------------------------------------------------------------

#[test]
fn merge_allows_new_file_matching_append_only_glob() {
    let repo = TestRepo::new();
    set_append_only_config(&repo, &["journal.txt"]);

    repo.seed_files(&[("README.md", "hello\n")]);

    repo.maw_ok(&["ws", "create", "new-journal"]);
    repo.add_file("new-journal", "journal.txt", "first line\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "new-journal",
        "--message",
        "chore: add journal",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);
    assert_eq!(
        v["status"], "success",
        "a brand-new file matching an append-only glob must never violate:\n{stdout}"
    );
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("first line\n")
    );
}

// ---------------------------------------------------------------------------
// 9. append_only: invalid patterns fail closed instead of disabling the guard.
// ---------------------------------------------------------------------------

#[test]
fn merge_refuses_invalid_append_only_glob_instead_of_silently_ignoring_it() {
    let repo = TestRepo::new();
    set_append_only_config(&repo, &["["]);

    repo.seed_files(&[("journal.txt", "L1\nL2\n")]);
    repo.maw_ok(&["ws", "create", "rewriter"]);
    repo.modify_file("rewriter", "journal.txt", "L2\n");

    for mode in ["--check", "--plan"] {
        let stderr = repo.maw_fails(&["ws", "merge", "rewriter", mode]);
        assert!(
            stderr.contains("invalid append-only glob") && stderr.contains('['),
            "{mode} must identify the invalid safety pattern:\n{stderr}"
        );
    }

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "rewriter",
        "--message",
        "chore: rewrite protected journal",
    ]);
    assert!(
        stderr.contains("invalid append-only glob") && stderr.contains('['),
        "the refusal must identify the invalid safety pattern:\n{stderr}"
    );
    assert_eq!(
        repo.read_file("default", "journal.txt").as_deref(),
        Some("L1\nL2\n"),
        "an invalid append-only policy must fail closed before trunk changes"
    );
}
