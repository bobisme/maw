//! Integration tests for bn-3uou: `maw gc --recovery-snapshots` prunes a
//! destroyed workspace's recovery ref and its destroy record together, and
//! `maw doctor` distinguishes still-pinned snapshots (`abandoned-with-snapshot`)
//! from desynced records whose ref is gone (`destroy-record-unpinned`).
//!
//! These prove the two coupled defects are fixed end-to-end:
//!   A. the `abandoned-with-snapshot` warn can now actually be cleared;
//!   B. `maw gc --recovery-snapshots` no longer leaves records claiming a
//!      snapshot whose ref it just swept.

mod manifold_common;

use manifold_common::TestRepo;

/// Find a doctor check by name in the `--format json` envelope.
fn check<'a>(env: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    env["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("doctor check `{name}` should be registered"))
}

fn doctor(repo: &TestRepo) -> serde_json::Value {
    let out = repo.maw_ok(&["doctor", "--format", "json"]);
    serde_json::from_str(&out).expect("doctor JSON")
}

/// (a) `gc --recovery-snapshots --older-than 0` prunes ref + record together,
/// and the `abandoned-with-snapshot` count drops to ok.
#[test]
fn gc_prunes_ref_and_record_together_and_doctor_count_drops() {
    let repo = TestRepo::new();
    repo.create_workspace("alice");
    repo.add_file("alice", "draft.md", "queued work\n");
    repo.maw_ok(&["ws", "destroy", "alice", "--force"]);

    // Before GC: a recovery ref exists and doctor warns.
    let refs_before = repo.git(&["for-each-ref", "refs/manifold/recovery/alice"]);
    assert!(
        !refs_before.trim().is_empty(),
        "destroy --force should pin a recovery ref"
    );
    let env = doctor(&repo);
    assert_eq!(check(&env, "abandoned-with-snapshot")["status"], "warn");

    // Prune everything old-or-equal (older-than 0 drains the queue).
    let out = repo.maw_ok(&["gc", "--recovery-snapshots", "--older-than", "0"]);
    assert!(
        out.contains("destroy record(s)"),
        "gc output should report record pruning, got: {out}"
    );

    // After GC: ref gone AND doctor abandoned count cleared.
    let refs_after = repo.git(&["for-each-ref", "refs/manifold/recovery/alice"]);
    assert!(
        refs_after.trim().is_empty(),
        "recovery ref must be swept, got: {refs_after}"
    );
    let env = doctor(&repo);
    assert_eq!(
        check(&env, "abandoned-with-snapshot")["status"],
        "ok",
        "abandoned-with-snapshot must clear after the recommended GC"
    );
    assert_eq!(
        check(&env, "destroy-record-unpinned")["status"],
        "ok",
        "no desynced record should remain after coherent GC"
    );
}

/// The recommended GC keeps *recent* snapshots (default 30-day window): a
/// fresh destroy is NOT swept, so the warn legitimately stays.
#[test]
fn gc_keeps_recent_snapshots_and_records() {
    let repo = TestRepo::new();
    repo.create_workspace("bob");
    repo.add_file("bob", "wip.txt", "fresh\n");
    repo.maw_ok(&["ws", "destroy", "bob", "--force"]);

    // Default older-than (30 days): the just-created snapshot is too new.
    repo.maw_ok(&["gc", "--recovery-snapshots"]);

    let refs_after = repo.git(&["for-each-ref", "refs/manifold/recovery/bob"]);
    assert!(
        !refs_after.trim().is_empty(),
        "a fresh recovery ref must be kept by the default 30-day window"
    );
    let env = doctor(&repo);
    assert_eq!(
        check(&env, "abandoned-with-snapshot")["status"],
        "warn",
        "recent snapshot still queued → still warns"
    );
}

/// (c) A manually-desynced state (recovery ref deleted, destroy record left
/// behind) is surfaced by the new `destroy-record-unpinned` check, NOT
/// silently counted in `abandoned-with-snapshot`; and the documented single
/// command clears it.
#[test]
fn manual_desync_surfaces_unpinned_check_and_gc_clears_it() {
    let repo = TestRepo::new();
    repo.create_workspace("carol");
    repo.add_file("carol", "notes.md", "content\n");
    repo.maw_ok(&["ws", "destroy", "carol", "--force"]);

    // Simulate the Defect-B residue: delete the recovery ref directly, leaving
    // the destroy record claiming a now-unpinned snapshot.
    let listing = repo.git(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/manifold/recovery/carol",
    ]);
    let ref_name = listing
        .lines()
        .next()
        .expect("a recovery ref")
        .trim()
        .to_owned();
    repo.git(&["update-ref", "-d", &ref_name]);

    // doctor: NOT abandoned-with-snapshot (no live pin); IS destroy-record-unpinned.
    let env = doctor(&repo);
    assert_eq!(
        check(&env, "abandoned-with-snapshot")["status"],
        "ok",
        "a record whose ref is gone must not inflate abandoned-with-snapshot"
    );
    let unpinned = check(&env, "destroy-record-unpinned");
    assert_eq!(
        unpinned["status"], "warn",
        "desync must be surfaced: {unpinned}"
    );
    assert!(
        unpinned["message"].as_str().expect("msg").contains("carol"),
        "unpinned check should name the workspace: {unpinned}"
    );
    let fix = unpinned["fix"].as_str().expect("fix present when warn");
    assert!(
        fix.contains("maw gc --recovery-snapshots"),
        "unpinned fix should name the cleanup command: {fix}"
    );

    // The documented single command clears it.
    repo.maw_ok(&["gc", "--recovery-snapshots", "--older-than", "0"]);
    let env = doctor(&repo);
    assert_eq!(
        check(&env, "destroy-record-unpinned")["status"],
        "ok",
        "gc --recovery-snapshots --older-than 0 must clear the unpinned records"
    );
}
