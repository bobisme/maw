//! CLI-level tests for `maw fsck` (bn-1uot): exit codes, JSON schema, a clean
//! fresh repo, and the healthy-repo `--repair` no-op.

mod manifold_common;

use manifold_common::TestRepo;

/// A fresh, healthy repo passes clean with exit 0.
#[test]
fn fsck_clean_on_fresh_repo() {
    let repo = TestRepo::new();
    let out = repo.maw_raw(&["fsck"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "fresh repo must fsck clean (exit 0):\n{stdout}"
    );
    assert!(
        stdout.contains("0 violation(s)"),
        "clean summary expected: {stdout}"
    );
}

/// The JSON shape is versioned and stable.
#[test]
fn fsck_json_schema_shape() {
    let repo = TestRepo::new();
    let out = repo.maw_raw(&["fsck", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("fsck --json must be valid JSON");

    assert_eq!(parsed["fsck_schema"].as_u64(), Some(1));
    let invariants = parsed["invariants"].as_array().expect("invariants array");
    assert!(!invariants.is_empty(), "catalog must be non-empty");
    for inv in invariants {
        for key in [
            "id",
            "severity",
            "status",
            "description",
            "detail",
            "violations",
            "repair",
        ] {
            assert!(
                inv.get(key).is_some(),
                "invariant JSON missing '{key}': {inv}"
            );
        }
    }
    let summary = &parsed["summary"];
    for key in ["checked", "violations", "repairable", "exit_code"] {
        assert!(summary.get(key).is_some(), "summary missing '{key}'");
    }
}

/// A wrong-kind manifold ref (object exists but is not a commit) is corruption
/// → exit 2.
#[test]
#[allow(clippy::literal_string_with_formatting_args)] // "HEAD^{tree}" is a git revspec, not a format string
fn fsck_exit_2_on_corruption() {
    let repo = TestRepo::new();
    // A tree OID exists but is not a commit; a state ref must be a commit.
    let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string();
    repo.git(&["update-ref", "refs/manifold/ws/phantom", &tree]);

    let out = repo.maw_raw(&["fsck"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(2),
        "corruption must exit 2:\n{stdout}"
    );
    assert!(
        stdout.contains("refs-manifold-object"),
        "corruption should be attributed to refs-manifold-object: {stdout}"
    );
}

/// Dangling recovery snapshots for a gone workspace are warn-only → exit 1.
#[test]
fn fsck_exit_1_on_warn_only() {
    let repo = TestRepo::new();
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    // Two recovery refs (valid commits) for a workspace that does not exist.
    repo.git(&[
        "update-ref",
        "refs/manifold/recovery/ghostws/2026-01-01T00-00-00Z",
        &head,
    ]);
    repo.git(&[
        "update-ref",
        "refs/manifold/recovery/ghostws/2026-01-02T00-00-00Z",
        &head,
    ]);

    let out = repo.maw_raw(&["fsck"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "warn-only must exit 1:\n{stdout}"
    );
}

/// `--repair` on a healthy repo changes no refs (byte-level no-op) and exits 0.
#[test]
fn fsck_repair_healthy_is_noop() {
    let repo = TestRepo::new();
    let before = repo.git(&["for-each-ref", "refs/manifold/"]);

    let out = repo.maw_raw(&["fsck", "--repair"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "healthy --repair must exit 0:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let after = repo.git(&["for-each-ref", "refs/manifold/"]);
    assert_eq!(
        before, after,
        "--repair must not change refs on a healthy repo"
    );
}

/// An unreadable merge journal may belong to a live or newer-version maw
/// process. `--repair` cannot prove it stale, so it must preserve the file and
/// leave the corruption visible for manual inspection.
#[test]
fn fsck_repair_preserves_unreadable_merge_state() {
    let repo = TestRepo::new();
    let state_path = repo.root().join(".manifold/merge-state.json");
    std::fs::write(&state_path, "{ malformed merge state").expect("write malformed state");

    let out = repo.maw_raw(&["fsck", "--repair"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(2),
        "unreadable merge state must remain an error:\n{stdout}"
    );
    assert!(
        state_path.exists(),
        "automatic repair must not delete state whose owner cannot be verified"
    );
    assert!(
        stdout.contains("declined") && stdout.contains("could not be proven stale"),
        "output must explain why automatic repair was refused: {stdout}"
    );
}
