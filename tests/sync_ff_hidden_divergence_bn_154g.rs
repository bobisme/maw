//! End-to-end: `maw ws sync` must PIN hidden worktree divergence before its
//! fast-forward checkout flattens it (bn-154g).
//!
//! # The gap this pins
//!
//! Found by black-box validation 2026-08-15 (trunk 555aabc9,
//! `/tmp/maw-validate/t9`). Inject the bn-p3m9 corruption signature into a
//! *clean* workspace — stale bytes on a tracked path plus an index stat-cache
//! mask, so `git status --porcelain` reports nothing — then run
//! `maw ws sync <ws>`.
//!
//! Before this fix the outcome was **correct but silent**: the FF
//! `checkout_detach` materializes every entry of the target tree with
//! `overwrite_existing = true`, so it rewrote the stale file with the right
//! content — and the post-checkout `verify_clean_materialization` then saw a
//! clean tree, because the checkout had already destroyed the divergence it was
//! there to detect. No WARNING, no artifact, no oplog entry, and — the part
//! that matters — **no snapshot of the bytes maw overwrote**. That is the exact
//! asymmetry with the FF-absorb site, where bn-3gba pins before repairing.
//!
//! # What this test asserts
//!
//! Through the real `maw` binary, on the real `maw ws sync` command:
//!
//! 1. the loud stderr WARNING names the workspace and the divergent path,
//! 2. the pre-overwrite bytes are pinned to a `refs/manifold/recovery/<ws>/
//!    materialize-*` ref and are recoverable **verbatim**,
//! 3. a JSON artifact records the event as `preserved-before-overwrite`,
//! 4. the sync still lands the correct content at the new epoch.
//!
//! Plus the negative control that keeps the guard honest: an ordinary clean
//! sync says nothing, pins nothing and writes no artifact. A guard that fired
//! on every sync would bury the signal and litter a ref per workspace per sync.
//!
//! This runs in the DEFAULT `just check` lane (no `failpoints` feature needed):
//! the injection uses only real git plumbing, so a regression is caught the same
//! day it lands.

mod manifold_common;

use std::path::Path;

use manifold_common::{TestRepo, git_ok};

/// Tracked file the injection poisons.
const VICTIM: &str = "victim.txt";
/// The committed content.
const GOOD: &str = "pub fn answer() -> u32 { 42 }\n";
/// Stale bytes with the SAME byte length as `GOOD`, so even a size-only stat
/// comparison cannot separate them — the strongest form of the mask.
const STALE: &str = "pub fn answer() -> u32 { 17 }\n";
/// Uncommitted bytes a deliberately-dirty workspace must still hold after a
/// refused sync. If the guard ever eats these, the defense has become the bug.
const PRECIOUS: &str = "PRECIOUS uncommitted agent work\n";

/// Substring of the stderr WARNING the pre-overwrite guard prints.
const WARNING_MARKER: &str = "had hidden working-tree divergence going into";

/// The materialize-repair artifact directory for a workspace. `TestRepo` builds
/// the V2 (`.manifold/`) layout — same helper as
/// `tests/materialize_verify_bn_3gba.rs`.
fn artifact_dir(root: &Path, ws: &str) -> std::path::PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws)
        .join("materialize-repair")
}

fn artifact_files(root: &Path, ws: &str) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(artifact_dir(root, ws)) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    out.sort();
    out
}

fn recovery_refs(root: &Path, ws: &str) -> Vec<String> {
    git_ok(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/manifold/recovery/{ws}"),
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_owned)
    .collect()
}

/// Reproduce the bn-p3m9 / t9 corruption signature: stale bytes on a tracked
/// path that every status-shaped query reports as clean.
///
/// Recipe, in the order the real corrupter produces it:
///
/// 1. back-date the victim, so the index records an unambiguously old mtime (an
///    entry whose mtime equals the index's own is "racily clean" and gets
///    re-hashed, which would accidentally rescue a stat-based checker);
/// 2. rebuild the index from HEAD (`read-tree --reset -u` is too destructive
///    here, so `reset --mixed HEAD`, which is what `unstage_all` does) and then
///    `update-index --refresh`, so every entry holds HEAD's CORRECT blob OID
///    stamped with the freshly re-stat'd (back-dated) data — the same index
///    shape the corrupting call site leaves behind;
/// 3. overwrite the file with the same-length stale bytes and restore the
///    back-dated mtime, so `(size, mtime)` still match the index entry.
///
/// `core.checkStat = minimal` narrows the comparison to size+mtime. It is a
/// real, documented setting (recommended on filesystems with unstable
/// ctime/ino, e.g. some network and container mounts) and is the only part of
/// the fingerprint that cannot be reproduced portably — `ctime` cannot be set
/// from userspace.
///
/// `core.trustCTime = false` must be set ALONGSIDE it, and is load-bearing
/// here: maw's dirty check is gix, and gix compares `ctime.secs` whenever
/// `trust_ctime` is on, **independently of `check_stat`**
/// (`gix_index::entry::stat::Stat::matches`), where git's `minimal` drops ctime
/// entirely. Writing the stale bytes always bumps ctime, so without this the
/// mask survives only while the whole injection lands inside a single
/// wall-clock second — a fixture that passes on an idle machine and flakes
/// under a loaded `just check`. With it, the mask is deterministic.
///
/// Returns whether the mask actually took effect. The status query is run
/// TWICE: the first run can legitimately cause git to write back a refreshed
/// index, and it is the SECOND, settled answer that maw will see.
fn inject_hidden_divergence(ws: &Path) -> bool {
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    git_ok(ws, &["config", "core.checkStat", "minimal"]);
    git_ok(ws, &["config", "core.trustctime", "false"]);

    let victim = ws.join(VICTIM);
    let backdated = SystemTime::now() - Duration::from_mins(10);
    let times = FileTimes::new()
        .set_accessed(backdated)
        .set_modified(backdated);
    let backdate = || {
        std::fs::File::options()
            .write(true)
            .open(&victim)
            .expect("open victim")
            .set_times(times)
            .expect("back-date victim");
    };

    backdate();
    git_ok(ws, &["reset", "--mixed", "HEAD"]);
    git_ok(ws, &["update-index", "--refresh"]);

    std::fs::write(&victim, STALE).expect("write stale bytes");
    backdate();

    git_ok(ws, &["status", "--porcelain"]).trim().is_empty()
        && git_ok(ws, &["status", "--porcelain"]).trim().is_empty()
}

/// A stale-but-clean workspace whose worktree secretly disagrees with HEAD must
/// have those bytes pinned, warned about and recorded **before** the sync
/// checkout overwrites them — and the sync must still land the right content.
#[test]
fn sync_ff_pins_hidden_divergence_before_checkout() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    // Advance the epoch so `alice` is stale → `ws sync` takes the FF path.
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let ws = repo.workspace_path("alice");
    assert!(
        inject_hidden_divergence(&ws),
        "the fixture failed to produce the mask: `git status --porcelain` already reports the \
         divergence, so this run would NOT be testing what it claims. Fix the injection (see \
         inject_hidden_divergence) rather than weakening the assertion — a status-based \
         detector passing here would be a false green on the exact class this test exists to \
         catch."
    );
    assert_eq!(
        std::fs::read_to_string(ws.join(VICTIM)).expect("read victim"),
        STALE,
        "the injection must leave stale bytes on disk"
    );

    let out = repo.maw_raw(&["ws", "sync", "alice"]);
    let stderr =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "sync must still succeed (the checkout IS the repair):\n{stderr}"
    );

    // 1. The loud WARNING.
    assert!(
        stderr.contains(WARNING_MARKER),
        "sync must WARN about hidden divergence before overwriting it, got:\n{stderr}"
    );
    assert!(
        stderr.contains(VICTIM),
        "the WARNING must name the divergent path, got:\n{stderr}"
    );
    assert!(
        stderr.contains("alice"),
        "the WARNING must name the workspace, got:\n{stderr}"
    );

    // 2. The pin — the whole point of the bone. Before the fix these bytes
    //    were overwritten with no snapshot anywhere.
    let refs = recovery_refs(repo.root(), "alice");
    assert_eq!(
        refs.len(),
        1,
        "exactly one pre-overwrite pin expected, got: {refs:?}"
    );
    let pinned = &refs[0];
    assert!(
        pinned.contains("/materialize-"),
        "pin must land in the materialize namespace: {pinned}"
    );
    assert_eq!(
        git_ok(repo.root(), &["show", &format!("{pinned}:{VICTIM}")]),
        STALE,
        "the pin must hold the PRE-overwrite bytes verbatim"
    );

    // 3. The artifact, recording that this was a pre-overwrite preserve.
    let artifacts = artifact_files(repo.root(), "alice");
    assert_eq!(
        artifacts.len(),
        1,
        "exactly one artifact expected, got: {artifacts:?}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifacts[0]).expect("read artifact"))
            .expect("artifact must be valid JSON");
    assert_eq!(
        json["repair_mode"].as_str(),
        Some("preserved-before-overwrite"),
        "artifact: {json}"
    );
    assert_eq!(
        json["operation"].as_str(),
        Some("sync-fast-forward"),
        "artifact: {json}"
    );
    assert_eq!(json["paths"][0]["path"].as_str(), Some(VICTIM), "{json}");
    assert_eq!(
        json["preserved_ref"].as_str(),
        Some(pinned.as_str()),
        "the artifact must point at the pin: {json}"
    );

    // 4. The sync outcome is still correct.
    assert_eq!(
        std::fs::read_to_string(ws.join(VICTIM)).expect("read victim after sync"),
        GOOD,
        "the FF checkout must still land the committed content"
    );
    assert!(
        ws.join("advance.txt").exists(),
        "the epoch delta must be materialized"
    );
    assert_eq!(
        repo.workspace_head("alice"),
        repo.current_epoch(),
        "HEAD must be at the new epoch"
    );
}

/// The negative control. An ordinary clean sync must stay completely silent:
/// no WARNING, no artifact, no recovery ref. A guard that fires on every sync
/// buries the signal and leaves a ref per workspace per sync behind.
#[test]
fn clean_sync_pins_nothing_and_says_nothing() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let out = repo.maw_raw(&["ws", "sync", "alice"]);
    let stderr =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clean sync must succeed:\n{stderr}");

    assert!(
        !stderr.contains(WARNING_MARKER) && !stderr.contains("did not materialize cleanly"),
        "a clean sync must emit NO divergence warning, got:\n{stderr}"
    );
    assert!(
        recovery_refs(repo.root(), "alice").is_empty(),
        "a clean sync must pin nothing, got: {:?}",
        recovery_refs(repo.root(), "alice")
    );
    assert!(
        artifact_files(repo.root(), "alice").is_empty(),
        "a clean sync must write no artifact, got: {:?}",
        artifact_files(repo.root(), "alice")
    );

    let ws = repo.workspace_path("alice");
    assert_eq!(
        std::fs::read_to_string(ws.join(VICTIM)).expect("read victim"),
        GOOD
    );
}

/// A legitimately dirty workspace must still be REFUSED, with its paths named.
/// The pre-overwrite guard sits *after* that refusal on purpose: if it ever
/// moved ahead of the dirty check it would pin every uncommitted edit on every
/// refused sync, and the refusal is the behaviour agents rely on.
#[test]
fn dirty_sync_still_refuses_and_pins_nothing() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    repo.modify_file("alice", VICTIM, PRECIOUS);
    repo.add_file("alice", "scratch.txt", "untracked\n");

    let out = repo.maw_raw(&["ws", "sync", "alice"]);
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "sync must refuse a dirty workspace:\n{combined}"
    );
    assert!(
        combined.contains("uncommitted changes that would be lost by sync"),
        "the dirty refusal must be unchanged, got:\n{combined}"
    );
    assert!(
        combined.contains(&format!("M {VICTIM}")),
        "the refusal must name the modified path, got:\n{combined}"
    );
    assert!(
        combined.contains("scratch.txt"),
        "the refusal must name the untracked path, got:\n{combined}"
    );

    let ws = repo.workspace_path("alice");
    assert_eq!(
        std::fs::read_to_string(ws.join(VICTIM)).expect("read victim"),
        PRECIOUS,
        "a refused sync must not touch the worktree"
    );
    assert!(
        recovery_refs(repo.root(), "alice").is_empty(),
        "a refused sync must pin nothing, got: {:?}",
        recovery_refs(repo.root(), "alice")
    );
    assert!(
        artifact_files(repo.root(), "alice").is_empty(),
        "a refused sync must write no artifact"
    );
}
