//! bn-1etl: Gate 2 (bn-28d1 tamper-resistance tripwire) message clarity.
//!
//! `assert_sources_clean_for_merge`'s Gate 2 refuses to merge a source
//! workspace whose HEAD tree contains a blob starting with a
//! `TOOL_PLACEHOLDER_PREFIXES` byte sequence (`# structured conflict at `
//! / `# BINARY CONFLICT at `). That's correct — those bytes must never land
//! on the target branch — but before bn-1etl the message always read as
//! "tool-authored conflict placeholders" / "sidecar was deleted or
//! corrupted", even for the much more common, much less alarming case: a
//! user hand-resolved the `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>`
//! markers directly in the file, picked a side, and simply forgot to also
//! delete the leading `#` header comment lines before committing.
//!
//! bn-1etl teaches Gate 2 to re-read each flagged blob and check whether it
//! still carries a `<<<<<<<` marker line:
//!
//! * markers still present → keep the original tamper-flavored message
//!   (genuine unresolved conflict, or a tampered/deleted sidecar).
//! * markers gone, header-only → a targeted message naming the exact fix
//!   ("Delete the leading '#' header lines ... commit, and re-run the
//!   merge").
//!
//! Both cases remain hard-blocking and are NOT bypassable by `--force` —
//! bn-1etl only changes wording, never the gate's refusal behavior.

mod manifold_common;

use manifold_common::TestRepo;

/// A fabricated "hand-resolved but header not deleted" blob: starts with the
/// exact `# structured conflict at ` tripwire prefix (bn-28d1) and carries
/// the mechanical-reconstruction header lines bn-36zz documents, but the
/// body below the blank separator line is plain resolved content with no
/// `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` marker anywhere.
fn header_only_leftover_content(path: &str) -> String {
    format!(
        "# structured conflict at {path}\n\
         # base blob: 0000000000000000000000000000000000000000\n\
         # side epoch blob: 1111111111111111111111111111111111111111\n\
         # side feat blob: 2222222222222222222222222222222222222222\n\
         \n\
         the user's hand-picked resolution, no markers here\n"
    )
}

/// The same header, but with the diff3 marker block still present below it —
/// the genuine unresolved-conflict / tampered-sidecar shape Gate 2 already
/// handled before bn-1etl.
fn header_with_markers_content(path: &str) -> String {
    format!(
        "# structured conflict at {path}\n\
         # base blob: 0000000000000000000000000000000000000000\n\
         # side epoch blob: 1111111111111111111111111111111111111111\n\
         # side feat blob: 2222222222222222222222222222222222222222\n\
         \n\
         <<<<<<< epoch (current)\n\
         epoch-side content\n\
         =======\n\
         feat-side content\n\
         >>>>>>> feat (workspace changes)\n"
    )
}

/// Commit `content` at `rel_path` directly into a fresh workspace's HEAD.
/// Gate 2 only inspects committed blob bytes — it does not care how the
/// commit was produced — so directly fabricating the blob (rather than
/// driving a real `sync --rebase` conflict) keeps these tests small and
/// deterministic while still exercising exactly the code path Gate 2 scans.
fn commit_placeholder_blob(repo: &TestRepo, ws: &str, rel_path: &str, content: &str) {
    repo.maw_ok(&["ws", "create", ws]);
    repo.add_file(ws, rel_path, content);
    repo.git_in_workspace(ws, &["add", "-A"]);
    repo.git_in_workspace(ws, &["commit", "-m", "commit placeholder-prefixed blob"]);
}

// ---------------------------------------------------------------------------
// bn-1etl: header-only leftover gets the targeted message
// ---------------------------------------------------------------------------

#[test]
fn merge_gate2_header_only_leftover_gets_targeted_message() {
    let repo = TestRepo::new();
    let rel_path = "conflicted.txt";
    commit_placeholder_blob(
        &repo,
        "feat",
        rel_path,
        &header_only_leftover_content(rel_path),
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse a header-only leftover blob\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(rel_path),
        "error should name the file '{rel_path}'; got: {stderr}"
    );
    assert!(
        stderr.contains("Delete the leading '#' header lines"),
        "error should give the targeted header-removal fix; got: {stderr}"
    );
    assert!(
        stderr.contains("appear manually resolved"),
        "error should say the file appears manually resolved; got: {stderr}"
    );
    // Must NOT use the tamper-flavored wording for a leftover-header file —
    // that phrasing implies deliberate tampering, which this is not.
    assert!(
        !stderr.contains("tool-authored conflict placeholders"),
        "leftover-header case must not use the tamper-flavored message; got: {stderr}"
    );
    assert!(
        !stderr.contains("sidecar was deleted or corrupted"),
        "leftover-header case must not imply the sidecar was tampered with; got: {stderr}"
    );
}

#[test]
fn merge_gate2_header_only_leftover_not_bypassable_by_force() {
    // bn-1etl must not weaken the gate: the header-only leftover case stays
    // just as blocking under --force as the original tamper message was.
    let repo = TestRepo::new();
    let rel_path = "conflicted.txt";
    commit_placeholder_blob(
        &repo,
        "feat",
        rel_path,
        &header_only_leftover_content(rel_path),
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--force",
        "--message",
        "should still fail",
    ]);
    assert!(
        !out.status.success(),
        "header-only leftover must not be bypassable by --force\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn merge_check_header_only_leftover_shares_the_targeted_message() {
    // `ws merge --check` runs the same `assert_sources_clean_for_merge`
    // gate as the real merge (bn-qw4i); it must agree on the message too.
    let repo = TestRepo::new();
    let rel_path = "conflicted.txt";
    commit_placeholder_blob(
        &repo,
        "feat",
        rel_path,
        &header_only_leftover_content(rel_path),
    );

    let out = repo.maw_raw(&["ws", "merge", "feat", "--into", "default", "--check"]);
    assert!(
        !out.status.success(),
        "merge --check must refuse a header-only leftover blob\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Delete the leading '#' header lines"),
        "merge --check should give the targeted header-removal fix; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-drk3: comment-syntax-aware header prefix — the leftover-header message
// quotes the ACTUAL prefix a known-extension file used (`//` for `.rs`),
// not the legacy `#`.
// ---------------------------------------------------------------------------

/// Same shape as [`header_only_leftover_content`] but using the `//` prefix
/// bn-drk3's `header_prefix_for` picks for a `.rs` path — a leftover
/// header-only `.rs` file after bn-drk3.
fn header_only_leftover_content_rs(path: &str) -> String {
    format!(
        "// structured conflict at {path}\n\
         // base blob: 0000000000000000000000000000000000000000\n\
         // side epoch blob: 1111111111111111111111111111111111111111\n\
         // side feat blob: 2222222222222222222222222222222222222222\n\
         \n\
         fn resolved() {{}}\n"
    )
}

#[test]
fn merge_gate2_header_only_leftover_quotes_slash_slash_prefix_for_rs_file() {
    let repo = TestRepo::new();
    let rel_path = "conflicted.rs";
    commit_placeholder_blob(
        &repo,
        "feat",
        rel_path,
        &header_only_leftover_content_rs(rel_path),
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse a header-only leftover blob\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(rel_path),
        "error should name the file '{rel_path}'; got: {stderr}"
    );
    // bn-drk3: the fix hint must quote `//`, not the legacy `#` — this is
    // the file's own comment syntax, so the guidance is directly usable.
    assert!(
        stderr.contains("Delete the leading '//' header lines"),
        "error should quote the `//` prefix this .rs file actually used; got: {stderr}"
    );
    assert!(
        stderr.contains("// structured conflict at"),
        "error should quote the `//`-prefixed header form; got: {stderr}"
    );
    assert!(
        !stderr.contains("Delete the leading '#' header lines"),
        "error must not assume the legacy `#` prefix for a `.rs` leftover header; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// bn-1etl: markers still present keeps the original tamper-flavored message
// ---------------------------------------------------------------------------

#[test]
fn merge_gate2_marker_present_keeps_original_tamper_message() {
    let repo = TestRepo::new();
    let rel_path = "conflicted.txt";
    commit_placeholder_blob(
        &repo,
        "feat",
        rel_path,
        &header_with_markers_content(rel_path),
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "feat",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail",
    ]);
    assert!(
        !out.status.success(),
        "merge must refuse a blob that still carries conflict markers\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(rel_path),
        "error should name the file '{rel_path}'; got: {stderr}"
    );
    assert!(
        stderr.contains("tool-authored conflict placeholders"),
        "marker-present case should keep the original tamper-flavored message; got: {stderr}"
    );
    // Must NOT use the new header-only wording — markers are still present,
    // so this is not a "forgot to delete the header" situation.
    assert!(
        !stderr.contains("Delete the leading '#' header lines"),
        "marker-present case must not use the header-only-leftover message; got: {stderr}"
    );
}
