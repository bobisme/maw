//! bn-1xmk regression tests: `maw ws merge` must never silently clobber
//! uncommitted tracked files at the trunk/default workspace, and must pin a
//! durable, `maw ws recover`-visible snapshot of the preserved dirt.
//!
//! The data-loss class: a dirty tracked trunk file that NO merged workspace
//! touched (e.g. an append-only `.bones/events/*.events` journal) was restored
//! to its committed content during a merge because the trunk preserve-and-replay
//! cycle either treated ambiguous state as clean or replayed via a bare
//! `stash_apply` that bypassed the merge=union driver.

mod manifold_common;

use manifold_common::TestRepo;

/// A tracked, uncommitted trunk file that no merged workspace touches must
/// survive the merge byte-for-byte.
#[test]
fn uncommitted_tracked_trunk_file_untouched_survives_merge_byte_for_byte() {
    let repo = TestRepo::new();
    repo.seed_files(&[("notes.txt", "committed line\n")]);

    // Trunk gets an uncommitted tracked modification.
    let dirty = "committed line\nlocal uncommitted work\n";
    repo.modify_file("default", "notes.txt", dirty);

    // A workspace touches an unrelated file and is merged (advances the epoch).
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "other.txt", "alice work\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: unrelated"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = repo
        .read_file("default", "notes.txt")
        .expect("notes.txt must still exist at trunk after merge");
    assert_eq!(
        after, dirty,
        "uncommitted trunk edit was clobbered by the merge (Prime Invariant)"
    );
    // Alice's work also landed.
    assert_eq!(
        repo.read_file("default", "other.txt").as_deref(),
        Some("alice work\n"),
        "alice's merged work missing"
    );
}

/// The exact reported class: an append-only `merge=union` journal, dirty at
/// trunk and untouched by the merged workspace, whose committed content ALSO
/// changed out-of-band (a direct trunk commit the merge absorbs). A bare
/// `stash_apply` would resolve to the committed side and drop the uncommitted
/// append; the driver-aware replay must preserve it.
#[test]
fn dirty_union_journal_untouched_by_workspace_keeps_uncommitted_append() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        (".gitattributes", ".bones/events/*.events merge=union\n"),
        (".bones/events/2026-07.events", "L1 base\n"),
    ]);

    // Out-of-maw trunk commit advances the journal's committed content to L2
    // (the merge will absorb this, so the snapshot anchor differs from the
    // post-checkout tree — the exact mode-(b) trigger).
    repo.modify_file(
        "default",
        ".bones/events/2026-07.events",
        "L1 base\nL2 trunk\n",
    );
    repo.git_in_workspace("default", &["add", "-A"]);
    repo.git_in_workspace("default", &["commit", "-m", "out-of-maw trunk append"]);

    // Now an uncommitted append (L3) rides on top — dirty at trunk.
    repo.modify_file(
        "default",
        ".bones/events/2026-07.events",
        "L1 base\nL2 trunk\nL3 uncommitted\n",
    );

    // A workspace touches an unrelated file and is merged.
    repo.maw_ok(&["ws", "create", "worker"]);
    repo.add_file("worker", "src/x.rs", "// worker\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "worker: unrelated"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "worker",
        "--destroy",
        "--message",
        "merge worker",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = repo
        .read_file("default", ".bones/events/2026-07.events")
        .expect("journal must exist after merge");
    assert!(
        after.contains("L3 uncommitted"),
        "uncommitted journal append was DROPPED (data-loss class bn-1xmk):\n{after}"
    );
    assert!(
        after.contains("L1 base"),
        "journal base line missing:\n{after}"
    );
    assert!(
        !after.contains("<<<<<<<"),
        "journal should not carry conflict markers:\n{after}"
    );
    assert_eq!(
        after.matches("L1 base").count(),
        1,
        "journal base line duplicated:\n{after}"
    );
}

/// A dirty-trunk merge must pin a durable recovery snapshot that
/// `maw ws recover` lists, and print a visibility line to stderr.
#[test]
fn dirty_trunk_merge_pins_recovery_snapshot_and_announces_it() {
    let repo = TestRepo::new();
    repo.seed_files(&[("journal.txt", "one\n")]);

    // Uncommitted tracked trunk edit.
    repo.modify_file("default", "journal.txt", "one\ntwo uncommitted\n");

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "feature\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    assert!(out.status.success(), "merge should succeed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("uncommitted trunk file"),
        "merge should announce it is preserving dirty trunk files on stderr:\n{stderr}"
    );

    // The durable recovery ref is surfaced by `maw ws recover`.
    let recover = repo.maw_raw(&["ws", "recover"]);
    let rstdout = String::from_utf8_lossy(&recover.stdout);
    assert!(
        recover.status.success(),
        "ws recover should succeed\nstdout: {rstdout}\nstderr: {}",
        String::from_utf8_lossy(&recover.stderr)
    );
    assert!(
        rstdout.contains("default"),
        "recovery listing should include the trunk snapshot for 'default':\n{rstdout}"
    );
    // The preserved uncommitted content is discoverable via content search.
    let search = repo.maw_raw(&["ws", "recover", "--search", "two uncommitted"]);
    let sstdout = String::from_utf8_lossy(&search.stdout);
    assert!(
        sstdout.contains("two uncommitted") || sstdout.contains("journal.txt"),
        "recovery search should find the preserved trunk content:\n{sstdout}\nstderr: {}",
        String::from_utf8_lossy(&search.stderr)
    );
}

/// `maw ws merge --check` should surface dirty trunk files as an informational
/// note without blocking the merge.
#[test]
fn merge_check_reports_dirty_trunk_files_informationally() {
    let repo = TestRepo::new();
    repo.seed_files(&[("cfg.toml", "a = 1\n")]);
    repo.modify_file("default", "cfg.toml", "a = 1\nb = 2\n");

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "z.txt", "z\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice"]);

    let out = repo.maw_raw(&["ws", "merge", "alice", "--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // --check with no conflicts is "ready" (exit 0) and must not be blocked by
    // dirty trunk.
    assert!(
        out.status.success(),
        "dirty trunk must not block --check\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("cfg.toml"),
        "--check should list the dirty trunk file cfg.toml:\n{stdout}"
    );
}
