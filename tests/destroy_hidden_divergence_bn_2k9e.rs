//! End-to-end: destroying a workspace must not delete stat-cache-masked stale
//! bytes without first putting them in that workspace's recovery snapshot
//! (bn-2k9e).
//!
//! # The gap this pins
//!
//! Found by the bn-22jy DST corruption primitive (2026-08-19): at 8x16 the
//! `MaskedStalePreservation` oracle fired four times on `op=destroy`. It is
//! bn-154g's blind spot at a different site.
//!
//! `maw ws destroy` builds its recovery snapshot from *git state*:
//! `capture::list_dirty_paths` (gix `status_head_to_worktree`) decides which
//! files are worth snapshotting, and `git add -A` decides which bytes the
//! stash commit gets. Both short-circuit on the index **stat cache**. A tracked
//! file whose on-disk bytes were replaced while its `(size, mtime)` were
//! preserved — with `core.trustCTime=false` so ctime cannot betray it either —
//! is therefore reported CLEAN, is left out of the destroy record, and is
//! staged as its HEAD blob even when some *other* file drags the workspace onto
//! the dirty-capture path. The worktree is then removed and those bytes are
//! reachable from **no** `refs/manifold/recovery/<ws>/*` ref: a Prime Invariant
//! violation.
//!
//! Worse, the same mask makes `maw ws destroy` skip the "unmerged changes"
//! refusal entirely, so the loss needs no `--force` at all.
//!
//! # What this test asserts
//!
//! Through the real `maw` binary:
//!
//! 1. plain `maw ws destroy` (no `--force`) of a masked workspace snapshots the
//!    stale bytes and `maw ws recover <ws> --show <path>` returns them verbatim,
//! 2. `maw ws destroy --force` of a masked workspace that ALSO has ordinary
//!    visible dirt keeps both — the visible edit and the masked bytes,
//! 3. `maw ws merge --destroy` (a different code route) does the same,
//! 4. `maw ws recover --search` finds the masked content,
//! 5. the negative control: destroying a genuinely clean workspace still pins
//!    nothing, warns about nothing, and writes a `capture_mode: none` destroy
//!    record — byte-identical to the behaviour before this guard existed.
//!
//! This runs in the DEFAULT `just check` lane: the injection uses only real git
//! plumbing, so a regression is caught the same day it lands.

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
/// A second tracked file, edited *visibly*, to prove the masked path is added
/// to the snapshot rather than replacing what was already captured.
const SIBLING: &str = "sibling.txt";
const SIBLING_GOOD: &str = "sibling original\n";
const SIBLING_DIRTY: &str = "sibling edited by the agent\n";

/// Substring of the stderr WARNING the pre-destroy guard prints.
const WARNING_MARKER: &str = "every status query reports as CLEAN";

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

/// The destroy records written for `ws`, newest last.
fn destroy_records(root: &Path, ws: &str) -> Vec<serde_json::Value> {
    let dir = root
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws)
        .join("destroy");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| p.file_name().is_some_and(|n| n != "latest.json"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect()
}

/// Reproduce the bn-22jy / bn-p3m9 corruption signature on `path` inside `ws`:
/// stale bytes on a tracked file that every status-shaped query reports clean.
///
/// This is the `corrupt_worktree_stat_masked` recipe from
/// `tests/dst_production_tier.rs`, in the same order, with the same two
/// load-bearing config knobs:
///
/// * `core.checkStat = minimal` narrows git's comparison to size+mtime. It is a
///   real, documented setting (recommended on filesystems with unstable
///   ctime/ino) and is the only part of the fingerprint that cannot be
///   reproduced portably — `ctime` cannot be set from userspace.
/// * `core.trustCTime = false` must be set ALONGSIDE it, because gix compares
///   `ctime.secs` whenever `trust_ctime` is on, **independently of
///   `check_stat`** (`gix_index::entry::stat::Stat::matches`). Writing the
///   stale bytes always bumps ctime, so without this knob the mask survives
///   only while the whole injection lands inside a single wall-clock second — a
///   fixture that passes idle and flakes under a loaded `just check`.
///
/// Returns whether the mask actually took effect. The status query runs TWICE:
/// the first run can legitimately make git write back a refreshed index, and it
/// is the SECOND, settled answer that maw will see.
fn inject_hidden_divergence(ws: &Path, path: &str, stale: &str) -> bool {
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    git_ok(ws, &["config", "core.checkStat", "minimal"]);
    git_ok(ws, &["config", "core.trustctime", "false"]);

    let victim = ws.join(path);
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read victim").len(),
        stale.len(),
        "the stale payload must have the SAME length as the committed bytes, \
         or a size-only stat comparison would see through the mask"
    );

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

    std::fs::write(&victim, stale).expect("write stale bytes");
    backdate();

    git_ok(ws, &["status", "--porcelain"]).trim().is_empty()
        && git_ok(ws, &["status", "--porcelain"]).trim().is_empty()
}

/// The assertion that keeps the fixture honest: if the mask did not take, this
/// run is not testing the class it claims to.
fn assert_masked(ws: &Path, path: &str, stale: &str) {
    assert!(
        inject_hidden_divergence(ws, path, stale),
        "the fixture failed to produce the mask: `git status --porcelain` already reports the \
         divergence, so this run would NOT be testing what it claims. Fix the injection (see \
         inject_hidden_divergence) rather than weakening the assertion — a status-based \
         detector passing here would be a false green on the exact class this test exists to \
         catch."
    );
    assert_eq!(
        std::fs::read_to_string(ws.join(path)).expect("read victim"),
        stale,
        "the injection must leave stale bytes on disk"
    );
}

/// A plain `maw ws destroy` — no `--force`, because the mask also hides the
/// workspace from the unmerged-changes refusal — must snapshot the masked bytes
/// before removing the worktree, and `maw ws recover --show` must return them.
#[test]
fn destroy_snapshots_stat_masked_stale_bytes() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    let ws = repo.workspace_path("alice");
    assert_masked(&ws, VICTIM, STALE);

    let out = repo.maw_raw(&["ws", "destroy", "alice"]);
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "destroy must still succeed once the bytes are preserved:\n{combined}"
    );
    assert!(
        !ws.exists(),
        "the worktree must actually be gone:\n{combined}"
    );

    // 1. The loud WARNING — nothing else in the destroy output would hint that
    //    bytes were about to be lost, because the workspace looked clean.
    assert!(
        combined.contains(WARNING_MARKER),
        "destroy must WARN that it found stat-cache-masked divergence, got:\n{combined}"
    );
    assert!(
        combined.contains(VICTIM) && combined.contains("alice"),
        "the WARNING must name the path and the workspace, got:\n{combined}"
    );

    // 2. The pin — the whole point of the bone. Before the fix these bytes were
    //    deleted with the worktree and were reachable from no ref at all.
    let refs = recovery_refs(repo.root(), "alice");
    assert_eq!(
        refs.len(),
        1,
        "exactly one destroy pin expected, got: {refs:?}"
    );
    assert_eq!(
        git_ok(repo.root(), &["show", &format!("{}:{VICTIM}", refs[0])]),
        STALE,
        "the pin must hold the MASKED bytes verbatim, not HEAD's"
    );

    // 3. The front door: `maw ws recover <ws> --show <path>`. Pinning into the
    //    destroy namespace (rather than a side ref) is what makes this resolve.
    let shown = repo.maw_ok(&["ws", "recover", "alice", "--show", VICTIM]);
    assert_eq!(
        shown, STALE,
        "maw ws recover --show must return the masked bytes, not HEAD's"
    );

    // 4. The destroy record must advertise the snapshot, not claim "clean".
    let records = destroy_records(repo.root(), "alice");
    assert_eq!(records.len(), 1, "one destroy record expected: {records:?}");
    let record = &records[0];
    assert_eq!(
        record["capture_mode"].as_str(),
        Some("dirty_snapshot"),
        "record: {record}"
    );
    assert!(
        record["dirty_files"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(VICTIM))),
        "the record must name the preserved path: {record}"
    );

    // 5. `--search` is the "I don't know which workspace" entry point and must
    //    reach the same bytes.
    let hits = repo.maw_ok(&["ws", "recover", "--search", "answer() -> u32 { 17 }"]);
    assert!(
        hits.contains(VICTIM),
        "recover --search must find the masked content, got:\n{hits}"
    );
}

/// `--force` on a workspace that has BOTH a visible edit and a masked one must
/// keep both. This is the shape where the bug is subtlest: the visible edit
/// drags the destroy onto the dirty-capture path, `git add -A` runs, and the
/// masked file is still staged as its HEAD blob because `git add` trusts the
/// same forged stat cache.
#[test]
fn force_destroy_keeps_masked_bytes_alongside_visible_dirt() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD), (SIBLING, SIBLING_GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    let ws = repo.workspace_path("alice");
    assert_masked(&ws, VICTIM, STALE);
    // The visible edit lands AFTER the mask so the injection's `reset --mixed`
    // cannot undo it.
    std::fs::write(ws.join(SIBLING), SIBLING_DIRTY).expect("write sibling");

    let out = repo.maw_raw(&["ws", "destroy", "alice", "--force"]);
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "force destroy must succeed:\n{combined}"
    );
    assert!(combined.contains(WARNING_MARKER), "got:\n{combined}");

    assert_eq!(
        repo.maw_ok(&["ws", "recover", "alice", "--show", VICTIM]),
        STALE,
        "the masked bytes must survive a --force destroy that also had visible dirt"
    );
    assert_eq!(
        repo.maw_ok(&["ws", "recover", "alice", "--show", SIBLING]),
        SIBLING_DIRTY,
        "the ordinary uncommitted edit must still be captured as before"
    );
}

/// `maw ws merge --destroy` is a different code route into the same capture.
/// It must preserve the masked bytes too.
#[test]
fn merge_destroy_snapshots_stat_masked_stale_bytes() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    // Give the merge something real to land, so `--destroy` runs after a
    // successful merge rather than on an empty workspace.
    repo.add_file("alice", "feature.txt", "alice work\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "feat: alice"]);

    let ws = repo.workspace_path("alice");
    assert_masked(&ws, VICTIM, STALE);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "feat: merge alice",
    ]);
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "merge --destroy must succeed:\n{combined}"
    );
    assert!(
        !ws.exists(),
        "merge --destroy must remove the worktree:\n{combined}"
    );
    assert!(
        combined.contains(WARNING_MARKER),
        "merge --destroy must WARN about the masked divergence, got:\n{combined}"
    );

    assert_eq!(
        repo.maw_ok(&["ws", "recover", "alice", "--show", VICTIM]),
        STALE,
        "merge --destroy must preserve the masked bytes"
    );
}

/// The negative control. Destroying a genuinely clean workspace must stay
/// exactly as it was: no WARNING, no recovery ref, and a `capture_mode: none`
/// destroy record. A guard that pinned on every destroy would litter a ref per
/// workspace and bury the signal it exists to raise.
#[test]
fn clean_destroy_pins_nothing_and_says_nothing() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    let out = repo.maw_raw(&["ws", "destroy", "alice"]);
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "clean destroy must succeed:\n{combined}"
    );
    assert!(
        !combined.contains(WARNING_MARKER),
        "a clean destroy must emit NO divergence warning, got:\n{combined}"
    );
    assert_eq!(
        combined.trim(),
        "Workspace 'alice' destroyed.",
        "a clean destroy's output must be unchanged, got:\n{combined}"
    );

    let refs = recovery_refs(repo.root(), "alice");
    assert!(
        refs.is_empty(),
        "a clean destroy must pin nothing: {refs:?}"
    );

    let records = destroy_records(repo.root(), "alice");
    assert_eq!(records.len(), 1, "one destroy record expected: {records:?}");
    assert_eq!(
        records[0]["capture_mode"].as_str(),
        Some("none"),
        "the clean-destroy record shape must be unchanged: {}",
        records[0]
    );
    assert_eq!(
        records[0]["dirty_files"].as_array().map(Vec::len),
        Some(0),
        "record: {}",
        records[0]
    );
    assert!(
        records[0]["snapshot_ref"].is_null(),
        "record: {}",
        records[0]
    );
}

/// A clean `--force` destroy must also be unchanged: `--force` on a workspace
/// with nothing to preserve still reports "nothing to snapshot" and pins
/// nothing. This pins the wording the destroy output contract depends on, which
/// the guard's re-keying of that branch (on the capture rather than on
/// `--force`) could otherwise silently drop.
#[test]
fn clean_force_destroy_reports_nothing_to_snapshot() {
    let repo = TestRepo::new();
    repo.seed_files(&[(VICTIM, GOOD)]);
    repo.maw_ok(&["ws", "create", "alice"]);

    let out = repo.maw_raw(&["ws", "destroy", "alice", "--force"]);
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "clean force destroy must succeed:\n{combined}"
    );
    assert!(
        combined.contains("(nothing to snapshot)"),
        "the clean --force wording must be unchanged, got:\n{combined}"
    );
    assert!(
        recovery_refs(repo.root(), "alice").is_empty(),
        "a clean --force destroy must pin nothing"
    );
}
