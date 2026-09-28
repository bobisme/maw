//! Production-code DST tier (bn-2byw step 2, increment 1).
//!
//! Drives maw's **real** workspace operations — via the actual `maw` binary
//! through [`TestRepo`] — over a deterministic, seed-generated op-sequence
//! produced by the shared `maw-scenario` generator, and runs the **authoritative
//! SG1 oracles** after every op:
//!
//! - **Oracle A** (`maw::assurance::oracle_a::OracleA`) — content-reachability
//!   no-work-lost. It accumulates witness blobs across steps (incremental) and
//!   fires `ReachabilityLost` if any previously-committed blob becomes
//!   unreachable. This is the load-bearing SG1 work-loss gate (SP2), NOT the
//!   demoted commit-ancestry proxy in `oracle::check_all`.
//! - **Oracle B** (`maw::assurance::oracle_b::check`) — state-coherence: dangling
//!   workspace head/owned refs and merge-state orphans (the bn-cm63 class). It
//!   reuses maw's PRODUCTION live-merge classification, so it understands maw's
//!   real ref shapes.
//! - **`CleanMaterialization`** (`maw::assurance::oracle_worktree`, bn-3gba) —
//!   after every op, every live, expected-clean, non-default workspace's
//!   WORKING TREE must equal its own HEAD tree. This is the bn-p3m9 class
//!   (`ws create` produced HEAD/index at the right commit but stale-epoch blobs
//!   on disk) that Oracles A and B are blind to by construction: every ref was
//!   correct and the stale blobs were perfectly reachable.
//!
//! These are the same oracles the in-proc soak (`maw-assurance::in_proc`) gates
//! on. Because they reason about CONTENT (blob reachability) and maw's real
//! refs, none of the demoted-proxy false-positives apply — so this tier needs
//! **no** snapshot relaxation: a violation here is a candidate REAL maw bug.
//!
//! # Why this exists
//!
//! The existing in-process soak (`maw-assurance::in_proc` + `tests/dst_harness.rs`'s
//! crash-simulation traces) drives a *plumbing model*: it writes merge-state
//! JSON directly and reasons about an abstract model. This tier instead spawns
//! the production `maw` binary for every op, so the oracles get statistical
//! coverage of the **real** CLI / merge-engine / workspace code path.
//!
//! Unlike the in-proc driver, this tier has REAL per-workspace git worktrees
//! (`TestRepo`), so `capture_state` reads real HEADs directly — we do NOT
//! override `state.workspaces` the way the in-proc driver must.
//!
//! # Determinism / scope
//!
//! - Uses the SAME `maw-scenario` generator and `ScenarioPlan` as the in-proc
//!   soak (`maw::assurance::scenario`). The generator carries a **gated**
//!   `Advance` op (`maw ws advance`): `ConditionProfile::advance_weight`
//!   defaults to 0, so the default-profile seed→plan byte stream — the bn-2yzz
//!   in-proc campaign — is UNCHANGED (verified by the maw-scenario determinism
//!   and corpus tests). This tier opts in via `with_advance_weight`, so it
//!   exercises the production `ws advance` HEAD-movement path (bn-8flz) that
//!   the in-proc model cannot reach.
//! - The default test (`dst_production_tier_no_work_lost`) replays only the op
//!   stream — each `PlannedStep.fault` / `.git_time` is ignored — and stays
//!   fast for the default gate. A SEPARATE `#[ignore]` variant
//!   (`dst_production_tier_survives_faults`, run via
//!   `just sg1-production-tier-faults`) DOES honor `PlannedStep.fault`: every
//!   step the generator marks with a `FaultSpec::Failpoint` is executed via a
//!   `--features failpoints` `maw` binary with `MAW_FP=<name>=abort` (or
//!   `=error` for the handled error-style sites in
//!   `maw_assurance::fault::HANDLED_ERROR_FAILPOINTS`), crashing
//!   the op mid-flight, and the oracles must still hold on the post-crash repo
//!   (maw's merge-state recovery is what makes this true). A violation under
//!   faults is a candidate REAL maw recovery/work-loss bug.
//! - The oracle is the judge of correctness, NOT the maw exit code: we use
//!   `maw_raw_exact` (which does not panic on non-zero) and let the oracles
//!   decide whether an op broke an invariant. Many ops legitimately exit
//!   non-zero (e.g. a `git commit` with nothing staged, a merge of a workspace
//!   with no committed work) — that is expected and is not, by itself, a
//!   violation.
//!
//! # Running
//!
//! ```sh
//! cargo test --features assurance --test dst_production_tier -- --nocapture
//! # or: just sg1-production-tier
//! ```
//!
//! Knobs: `DST_TRACES` (seed count, default 16), `DST_STEPS` (steps per seed,
//! default 24). Deep runs are clean: e.g. `DST_TRACES=64 DST_STEPS=80` →
//! 5120 op-steps, 0 violations (the depth ceiling that needed bn-3g6o — Oracle
//! A recognizing content preserved inside conflict-marker rewrites — is fixed).
//!
//! bn-286g: `DST_TRACES=48 DST_STEPS=48` surfaced the SAME conflict-as-data
//! gap in the later-added `SiblingRefFaithfulness` escape oracle (seeds
//! 0/15/18). The default 16x24 budget never reaches a *conflicting* sibling
//! auto-rebase, which is why CI stayed green — so the shape is now pinned at
//! the default budget by `bn_286g_conflicted_sibling_replay_is_green` below.

mod manifold_common;

#[cfg(feature = "assurance")]
use manifold_common::TestRepo;
#[cfg(feature = "assurance")]
use maw::assurance::oracle::capture_state as capture_oracle_state;
#[cfg(feature = "assurance")]
use maw::assurance::oracle_a::OracleA;
#[cfg(feature = "assurance")]
use maw::assurance::oracle_b;
#[cfg(feature = "assurance")]
use maw::assurance::oracle_escape::{
    SiblingRefFaithfulness, TrunkDirtyPreservation, check_record_ref_coherence,
};
#[cfg(feature = "assurance")]
use maw::assurance::oracle_worktree::{CleanMaterialization, MaskedStalePreservation};
#[cfg(feature = "assurance")]
use maw::assurance::scenario::{BaseRef, ConditionProfile, FaultSpec, Op, Target, generate_plan};

/// Read a `u64` count from `var`, defaulting to `default`.
#[cfg_attr(not(feature = "assurance"), allow(dead_code))]
fn env_count(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Build (once per test process) and locate a `--features failpoints` `maw`
/// binary, returning its absolute path.
///
/// The default `manifold_common::maw_bin()` returns the plain binary, which has
/// no failpoint machinery (`MAW_FP` and every `fp!()` site are gated behind
/// `--features failpoints`). The faulted DST variant needs a binary that
/// actually honors `MAW_FP=<name>=abort` to crash mid-op, so we build the
/// failpoints variant into a *separate* target dir — we never clobber the plain
/// `target/<profile>/maw` the rest of the suite (and `just check`) rely on.
/// Memoized so repeated calls in one test process build at most once.
///
/// Mirrors `tests/flock_mutual_exclusion_bn_2byw.rs::failpoints_maw_bin`.
#[cfg(feature = "assurance")]
fn failpoints_maw_bin() -> &'static std::path::Path {
    use std::process::Command;
    use std::sync::OnceLock;

    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Dedicated target dir so the failpoints build does not overwrite the
        // plain binary used by the rest of the suite.
        let target_dir = manifest_dir.join("target").join("dst-prod-fp-bn-2byw");

        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "maw-cli",
                "--features",
                "failpoints",
                "--target-dir",
            ])
            .arg(&target_dir)
            .current_dir(&manifest_dir)
            .status()
            .expect("failed to spawn `cargo build` for the failpoints binary");
        assert!(
            status.success(),
            "`cargo build -p maw-cli --features failpoints` failed; cannot run \
             the faulted production-tier DST"
        );

        let bin = target_dir.join("debug").join("maw");
        assert!(
            bin.exists(),
            "failpoints maw binary not found at {} after build",
            bin.display()
        );
        bin
    })
    .as_path()
}

// ---------------------------------------------------------------------------
// Op → real maw command mapping
// ---------------------------------------------------------------------------

/// Counters tracking how much real work actually happened during a run, used
/// for the liveness guard (so a passing test is never vacuously green).
#[cfg(feature = "assurance")]
#[derive(Default)]
struct Liveness {
    /// Total ops attempted (== plan steps executed).
    ops_attempted: u64,
    /// Ops whose maw invocation exited 0.
    ops_succeeded: u64,
    /// `ws create` invocations that exited 0.
    ws_created: u64,
    /// Number of times the epoch ref advanced across the run.
    epoch_advances: u64,
    /// Total witness blobs Oracle A accumulated across all seeds. This is the
    /// definitive non-vacuity signal: if it is 0 the oracle never saw any
    /// committed content (e.g. `capture_state` failed to enumerate the real
    /// worktrees), so a green result would be meaningless.
    oracle_a_witnesses: u64,
    /// `ws advance` invocations that exited 0 — proves the production
    /// HEAD-movement advance path (bn-8flz) was actually exercised.
    advances_run: u64,
    /// Ops where a `FaultSpec::Failpoint` was armed and executed via the
    /// failpoints binary with `MAW_FP=<name>=abort`. The decisive non-vacuity
    /// signal for the faulted variant: if 0, no mid-op crash was ever injected,
    /// so the oracle never judged a post-crash repo and a green run is
    /// meaningless.
    faults_injected: u64,
    /// bn-2bcx: out-of-maw trunk commits made (arms FF-absorb).
    out_of_maw_commits: u64,
    /// bn-2bcx: uncommitted dirty-trunk writes performed.
    dirty_trunk_writes: u64,
    /// bn-2bcx: successful `maw gc` runs.
    gc_runs: u64,
    /// bn-3gba: workspace-level `worktree == HEAD` assertions the
    /// `CleanMaterialization` oracle actually performed. The non-vacuity signal
    /// for the bn-p3m9 gate: 0 checks means the oracle judged nothing and a
    /// green run proves nothing about the class.
    clean_materialization_checks: u64,
    /// bn-22jy: `CorruptWorktreeStatMasked` ops the driver attempted.
    masked_corruptions: u64,
    /// bn-22jy: attempts where the stat-cache mask actually took — i.e. the
    /// workspace really did carry stale bytes that `git status` called clean.
    /// The decisive non-vacuity signal for the corruption tier: if it is 0 the
    /// primitive silently degraded to a no-op and the bn-154g guard was never
    /// armed.
    masked_corruptions_effective: u64,
    /// bn-22jy: `MaskedStalePreservation` judgements — masked files that were
    /// actually overwritten by a later op. 0 means nothing ever reached the
    /// preserve-before-overwrite path, so a green run proves nothing.
    masked_overwrites_judged: u64,
    /// bn-22jy: masked-stale workspaces later observed clean at their own HEAD
    /// again — maw re-materialized them, so `CleanMaterialization` re-armed.
    masked_resolutions: u64,
}

/// Short human-readable name for an op (for oracle-violation context strings).
#[cfg(feature = "assurance")]
const fn op_name(op: &Op) -> &'static str {
    match op {
        Op::WsCreate { .. } => "ws_create",
        Op::EditFiles { .. } => "edit_files",
        Op::Commit { .. } => "commit",
        Op::Merge { .. } => "merge",
        Op::Sync { .. } => "sync",
        Op::Destroy { .. } => "destroy",
        Op::Recover { .. } => "recover",
        Op::Advance { .. } => "advance",
        Op::OutOfMawCommit { .. } => "out_of_maw_commit",
        Op::DirtyTrunkWrite { .. } => "dirty_trunk_write",
        Op::Gc { .. } => "gc",
        Op::CorruptWorktreeStatMasked { .. } => "corrupt_worktree_stat_masked",
    }
}

/// Map `BaseRef` to a `--from` value.
///
/// `maw ws create --from` accepts a workspace/branch/revision but has NO
/// dedicated "epoch" keyword (verified via `maw ws create --help`). The repo's
/// epoch ref `refs/manifold/epoch/current` is at-or-ahead of `main`, but the
/// public CLI surface does not expose a stable name for it. So for increment 1
/// we map BOTH `Main` and `Epoch` to `"main"` — a real, always-resolvable base.
/// This is conservative: it never widens divergence, and the oracle still sees
/// real workspace creation either way.
#[cfg(feature = "assurance")]
const fn base_ref_arg(base: &BaseRef) -> &'static str {
    match base {
        BaseRef::Main | BaseRef::Epoch => "main",
    }
}

/// Execute one planned op against the real `maw` binary. Returns whether the
/// primary maw invocation exited 0 (for liveness accounting). The oracle — not
/// this return value — is the judge of correctness.
#[cfg(feature = "assurance")]
#[allow(
    clippy::too_many_lines,
    reason = "one flat Op -> CLI mapping table; splitting it would hide which op maps to which invocation"
)]
fn execute_op(repo: &TestRepo, op: &Op) -> OpOutcome {
    OpOutcome::from(match op {
        Op::WsCreate { ws, from } => {
            // Create as --persistent so the `Advance` op (maw ws advance) has a
            // valid target: advance refuses non-persistent workspaces
            // (advance.rs "Only persistent workspaces can be advanced"). All
            // other ops (edit/commit/merge/sync/destroy/recover) work
            // identically on a persistent workspace.
            let out = repo.maw_raw_exact(&[
                "ws",
                "create",
                &ws.0,
                "--from",
                base_ref_arg(from),
                "--persistent",
            ]);
            out.status.success()
        }
        Op::EditFiles { ws, files } => {
            // Edits go straight to the workspace working tree (NOT through maw).
            // Only meaningful if the workspace actually exists on disk; the
            // generator can plan edits for a ws whose create failed (e.g. a
            // duplicate name), so guard the helper which would otherwise panic.
            if repo.workspace_exists(&ws.0) {
                for fe in files {
                    repo.add_file(&ws.0, &fe.path, &fe.content);
                }
            }
            // Editing is not a maw op; count it as a no-op for liveness.
            false
        }
        Op::Commit { ws, msg } => {
            // `git add -A` then `git commit -m <msg>` inside the workspace, via
            // `maw exec`. A commit with nothing staged exits non-zero — fine.
            let _ = repo.maw_raw_exact(&["exec", &ws.0, "--", "git", "add", "-A"]);
            let out = repo.maw_raw_exact(&["exec", &ws.0, "--", "git", "commit", "-m", &msg.0]);
            out.status.success()
        }
        Op::Merge {
            srcs,
            into,
            destroy,
        } => {
            // Always merge into default for increment 1.
            let _ = into; // Target is recorded for completeness; we pin `default`.
            let mut args: Vec<&str> = vec!["ws", "merge"];
            for src in srcs {
                args.push(&src.0);
            }
            args.push("--into");
            args.push(merge_target(into));
            // `maw ws merge` requires an explicit --message (it refuses to
            // read from a non-tty stdin), so supply a deterministic one. The
            // generator's Op::Merge carries no message, so this is purely a
            // mapping detail; the content does not affect what the oracle sees.
            args.push("--message");
            args.push("dst: production-tier merge");
            if *destroy {
                args.push("--destroy");
            }
            let out = repo.maw_raw_exact(&args);
            out.status.success()
        }
        Op::Sync { ws } => {
            let out = repo.maw_raw_exact(&["ws", "sync", &ws.0]);
            out.status.success()
        }
        Op::Destroy { ws, force } => {
            let mut args: Vec<&str> = vec!["ws", "destroy", &ws.0];
            if *force {
                args.push("--force");
            }
            let out = repo.maw_raw_exact(&args);
            out.status.success()
        }
        Op::Recover { ws, to } => {
            let out = repo.maw_raw_exact(&["ws", "recover", &ws.0, "--to", &to.0]);
            out.status.success()
        }
        Op::Advance { ws } => {
            // `maw ws advance <ws>` — routes committed-ahead work through the
            // guarded rebase path (bn-8flz). This is the production
            // HEAD-movement code the in-proc model can't reach.
            let out = repo.maw_raw_exact(&["ws", "advance", &ws.0]);
            out.status.success()
        }
        Op::OutOfMawCommit { files, msg } => {
            // Advance refs/heads/main with a real commit made ENTIRELY outside
            // maw (pure git plumbing against a scratch index — no worktree, no
            // epoch update). This leaves main ahead of
            // refs/manifold/epoch/current, arming reconcile_epoch_with_branch
            // (the FF-absorb path, bn-11ip/bn-rah2) for the next merge.
            out_of_maw_commit(repo, files, &msg.0)
        }
        Op::DirtyTrunkWrite { files } => {
            // Uncommitted writes into the default workspace's working tree.
            // Not committed, not staged — pure dirty bytes (bn-1xmk). Only
            // meaningful if the default worktree exists.
            let default_ws = repo.default_workspace();
            if default_ws.is_dir() {
                for fe in files {
                    let path = default_ws.join(&fe.path);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&path, &fe.content);
                }
            }
            // Not a maw op; count as no-op for liveness.
            false
        }
        Op::Gc {
            recovery_snapshots,
            older_than_days,
        } => {
            let older = older_than_days.to_string();
            let mut args: Vec<&str> = vec!["gc"];
            if *recovery_snapshots {
                args.push("--recovery-snapshots");
                args.push("--older-than");
                args.push(&older);
            }
            let out = repo.maw_raw_exact(&args);
            out.status.success()
        }
        Op::CorruptWorktreeStatMasked { ws, path, nonce } => {
            // bn-22jy: handled out-of-band because it is the one op whose
            // outcome the oracles need in DETAIL (which path, which bytes), not
            // just as a boolean. `execute_op_recording` is the entry point that
            // both runs it and feeds `MaskedStalePreservation`; this arm exists
            // so the match stays exhaustive and so a caller that only wants the
            // side effect still gets it.
            return OpOutcome {
                succeeded: false,
                masked: corrupt_worktree_stat_masked(repo, &ws.0, path.as_deref(), *nonce),
            };
        }
    })
}

/// What one executed op did: the maw exit verdict, plus (bn-22jy) the resolved
/// victim of a `CorruptWorktreeStatMasked` op when the stat mask actually took.
#[cfg(feature = "assurance")]
#[derive(Debug, Default)]
struct OpOutcome {
    /// `true` iff the op's maw invocation exited 0. Non-maw ops (edits, dirty
    /// trunk writes, the corruption primitive) report `false` — they are not
    /// maw invocations and must not inflate the liveness counters.
    succeeded: bool,
    /// `(resolved_path, stale_bytes)` iff a corruption op established a mask
    /// that `git status` genuinely reports as clean.
    masked: Option<(String, String)>,
}

#[cfg(feature = "assurance")]
impl From<bool> for OpOutcome {
    fn from(succeeded: bool) -> Self {
        Self {
            succeeded,
            masked: None,
        }
    }
}

/// **bn-22jy: the stat-cache-masked worktree corruption primitive.**
///
/// Poison one tracked file in `ws` with stale bytes of the SAME length, then
/// forge the index stat cache so `git status`, `git diff HEAD` and gix's
/// `status_head_to_worktree` all report the workspace CLEAN. Returns
/// `Some((path, stale_bytes))` only when the mask is confirmed to have taken;
/// `None` means the primitive degraded to a no-op and nothing was recorded.
///
/// # The recipe, and why each step is load-bearing
///
/// This reproduces the bn-p3m9 signature in the order the real corrupter
/// produced it. It is the same recipe the e2e fixture
/// `tests/sync_ff_hidden_divergence_bn_154g.rs::inject_hidden_divergence` uses;
/// see that file for the incident narrative.
///
/// 1. `core.checkStat = minimal` narrows git's comparison to size+mtime. It is
///    a real, documented setting (recommended on filesystems with unstable
///    ctime/ino), and it is the only part of the fingerprint that cannot be
///    reproduced portably: `ctime` cannot be set from userspace on Linux.
/// 2. `core.trustCTime = false` must be set ALONGSIDE it, and is load-bearing
///    for maw specifically. maw's dirty check is **gix**, and
///    `gix_index::entry::stat::Stat::matches` compares `ctime.secs` whenever
///    `trust_ctime` is on, **independently of `check_stat`** — where git's
///    `minimal` drops ctime entirely. Writing the stale bytes always bumps
///    ctime, so without this the mask survives only while the whole injection
///    lands inside a single wall-clock second: green on an idle machine, flaky
///    under a loaded `just check`. With it, the mask is deterministic.
/// 3. Back-date the victim BEFORE rebuilding the index: an entry whose mtime
///    equals the index's own is "racily clean" and gets re-hashed, which would
///    accidentally rescue a stat-based checker.
/// 4. `reset --mixed HEAD` + `update-index --refresh` leave every entry holding
///    HEAD's CORRECT blob OID stamped with freshly re-stat'd (back-dated)
///    data — the index shape the corrupting call site leaves behind.
/// 5. Write the same-length stale bytes and re-apply the back-dated mtime, so
///    `(size, mtime)` still match the index entry.
///
/// # Scope guards (why this may return `None`)
///
/// * The workspace must exist and be **status-clean**. On a genuinely dirty
///   workspace the confirmation in step 6 could not tell "the mask failed" from
///   "this file was already modified", and `maw ws sync` would refuse on the
///   visible dirt anyway — so there would be nothing to arm.
/// * The victim must be a non-empty regular tracked file whose bytes currently
///   EQUAL its HEAD blob, so the recorded stale bytes are exactly the ones maw
///   is about to destroy.
/// * If the mask does not hold, the original bytes are put back and `None` is
///   returned. A half-corrupted workspace would be a fixture bug masquerading
///   as a maw bug.
///
/// # `git config` writes to the SHARED config
///
/// In a linked worktree `git config` writes `.git/config`, which every
/// workspace in the repo shares. That is intended: maw's gix must read
/// `trustCTime = false` for the mask to hold, and a per-worktree config would
/// not reach it. It only ever weakens *stat-based* comparisons — every oracle
/// here, and both production detectors this op exists to arm
/// (`divergent_paths_by_tree`, `CleanMaterialization`), compare TREES.
#[cfg(feature = "assurance")]
fn corrupt_worktree_stat_masked(
    repo: &TestRepo,
    ws: &str,
    hint: Option<&str>,
    nonce: u64,
) -> Option<(String, String)> {
    use std::fs::FileTimes;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    if !repo.workspace_exists(ws) {
        return None;
    }
    let ws_path = repo.workspace_path(ws);

    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&ws_path)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let status_clean = || git(&["status", "--porcelain"]).is_some_and(|s| s.trim().is_empty());
    let refresh_index = || {
        // Exits non-zero when some entry "needs update"; informational here.
        let _ = Command::new("git")
            .args(["update-index", "--refresh"])
            .current_dir(&ws_path)
            .output();
    };

    // Only a status-clean workspace can be masked (see the doc comment).
    if !status_clean() {
        return None;
    }

    // Resolve the victim: the hint if it holds, else the first tracked path
    // that does. `ls-tree` output is sorted, so the fallback is deterministic.
    let tracked = git(&["ls-tree", "-r", "--name-only", "-z", "HEAD"])?;
    let candidates: Vec<&str> = hint
        .into_iter()
        .chain(tracked.split('\0').filter(|p| !p.is_empty()))
        .collect();
    let mut victim: Option<(std::path::PathBuf, String, String)> = None;
    for rel in candidates {
        let abs = ws_path.join(rel);
        // Must be a regular file (not a symlink or directory) with content.
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            continue;
        };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        // Non-UTF-8 files are skipped: the payload generator below is textual.
        let Ok(disk) = std::fs::read_to_string(&abs) else {
            continue;
        };
        // The bytes must currently equal HEAD's, so the stale bytes recorded
        // for the oracle are exactly what maw is about to destroy.
        if git(&["show", &format!("HEAD:{rel}")]).as_deref() != Some(disk.as_str()) {
            continue;
        }
        victim = Some((abs, rel.to_owned(), disk));
        break;
    }
    let (abs, rel, original) = victim?;

    // Same-length stale payload, deterministic in `nonce`. Same length so even
    // a size-only stat comparison cannot separate the two — the strongest form
    // of the mask.
    let stale = same_length_stale_bytes(&original, nonce);
    if stale == original {
        return None;
    }

    // 1 + 2. The two config knobs (see the doc comment).
    git(&["config", "core.checkStat", "minimal"])?;
    git(&["config", "core.trustctime", "false"])?;

    let backdated = SystemTime::now() - Duration::from_mins(10);
    let times = FileTimes::new()
        .set_accessed(backdated)
        .set_modified(backdated);
    let backdate = || -> Option<()> {
        std::fs::File::options()
            .write(true)
            .open(&abs)
            .ok()?
            .set_times(times)
            .ok()
    };

    // 3 + 4. Back-date, then rebuild the index from HEAD and re-stat it.
    backdate()?;
    git(&["reset", "--mixed", "HEAD"])?;
    refresh_index();

    // 5. Plant the stale bytes and re-apply the back-dated mtime.
    std::fs::write(&abs, &stale).ok()?;
    backdate()?;

    // 6. Confirm the mask. Run the status query TWICE: the first run can
    // legitimately cause git to write back a refreshed index, and it is the
    // SECOND, settled answer that maw will see.
    if status_clean() && status_clean() {
        return Some((rel, stale));
    }

    // The mask did not hold — put the original bytes back rather than leave a
    // half-corrupted workspace that every downstream oracle would read as a maw
    // bug.
    let _ = std::fs::write(&abs, &original);
    refresh_index();
    None
}

/// A deterministic stale payload with **exactly** `original.len()` bytes.
///
/// A `nonce`-derived ASCII banner, then padded with `.` or truncated to length.
/// ASCII-only (so truncation can never split a `char`) keeps the result valid
/// UTF-8, and the banner makes a stray copy instantly identifiable in a failing
/// trace.
#[cfg(feature = "assurance")]
fn same_length_stale_bytes(original: &str, nonce: u64) -> String {
    let want = original.len();
    let mut out = format!("STALE-bn-22jy-{nonce:016x}\n");
    if out.len() > want {
        out.truncate(want);
        return out;
    }
    while out.len() < want {
        out.push('.');
    }
    out
}

/// Make a commit directly on `refs/heads/main` outside of maw, via git
/// plumbing against a throwaway index — no worktree touched, no epoch ref
/// updated. This is the faithful "someone `git commit`ed on trunk" arming
/// condition for FF-absorb (`reconcile_epoch_with_branch`).
///
/// Returns `true` (a real commit landed) for liveness accounting.
#[cfg(feature = "assurance")]
fn out_of_maw_commit(
    repo: &TestRepo,
    files: &[maw::assurance::scenario::FileEdit],
    msg: &str,
) -> bool {
    use std::process::Command;

    let root = repo.root();
    let git = |args: &[&str]| -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git spawn")
    };
    let git_env = |args: &[&str], index: &std::path::Path| -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_INDEX_FILE", index)
            .output()
            .expect("git spawn")
    };

    let parent = String::from_utf8_lossy(&git(&["rev-parse", "refs/heads/main"]).stdout)
        .trim()
        .to_owned();
    if parent.is_empty() {
        return false;
    }

    // Scratch index seeded from main's tree so the commit is a superset.
    let index = root
        .join(".git")
        .join(format!("dst-oom-index-{}", std::process::id()));
    let _ = std::fs::remove_file(&index);
    let _ = git_env(&["read-tree", "refs/heads/main"], &index);

    for fe in files {
        // Write the blob, then stage it at the desired path in the scratch index.
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("git hash-object");
        {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(fe.content.as_bytes())
                .expect("write blob");
        }
        let blob = String::from_utf8_lossy(&child.wait_with_output().expect("hash-object").stdout)
            .trim()
            .to_owned();
        let cacheinfo = format!("100644,{blob},{}", fe.path);
        let _ = git_env(
            &["update-index", "--add", "--cacheinfo", &cacheinfo],
            &index,
        );
    }

    let tree = String::from_utf8_lossy(&git_env(&["write-tree"], &index).stdout)
        .trim()
        .to_owned();
    let _ = std::fs::remove_file(&index);
    if tree.is_empty() {
        return false;
    }

    let commit =
        String::from_utf8_lossy(&git(&["commit-tree", &tree, "-p", &parent, "-m", msg]).stdout)
            .trim()
            .to_owned();
    if commit.is_empty() {
        return false;
    }
    let out = git(&["update-ref", "refs/heads/main", &commit]);
    out.status.success()
}

/// Execute one planned op while ARMING a `FaultSpec::Failpoint` as
/// `MAW_FP=<name>=abort` on the **failpoints** binary, so the op can crash
/// mid-flight. The generator only attaches faults to `Op::Merge` (any phase)
/// and `Op::Commit` (commit-phase sites), so only those two arms route through
/// the failpoints binary here; any other op (defensively) falls back to the
/// plain unfaulted path.
///
/// Returns `(outcome, crashed, output)`:
/// - `outcome.succeeded` is `true` iff the maw invocation exited 0 (liveness),
/// - `crashed` is `true` iff the process did not exit cleanly (non-zero or
///   killed by a signal — the realistic "mid-op kill"). A crash is EXPECTED,
///   not a failure: the post-crash repo state is what the oracle must judge.
///
/// The `FP_COMMIT_*` / `FP_BUILD_*_MERGE_COMPUTE` sites fire inside maw's
/// real merge engine (`maw::merge::commit` / `maw::merge::build_phase`, called
/// from `maw ws merge`), so a Merge op armed with any of them aborts mid-merge.
/// A commit-phase fault attached to an `Op::Commit` arms the same env on the
/// `git commit` shell-out; the `FP_COMMIT_*` sites are not on the `git commit`
/// path, so it typically will not crash there — that is fine (no crash, oracle
/// still runs), and matches the bn-18mv model where the armed env trips a later
/// merge.
#[cfg(feature = "assurance")]
fn execute_op_faulted(repo: &TestRepo, op: &Op, fp_name: &str) -> (OpOutcome, bool, String) {
    use std::process::Command;

    let bin = failpoints_maw_bin();
    // bn-1sbjf: `abort` for every crash site, `error` for the handled
    // error-style sites (FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT drives the bn-3jqfk
    // snapshot-failed fallback, which an abort would skip).
    let maw_fp = maw::assurance::fault::production_fp_spec(fp_name);

    // Build the argv for the op, matching `execute_op`'s mapping exactly.
    let run = |args: &[&str]| -> std::process::Output {
        Command::new(bin)
            .args(args)
            .current_dir(repo.root())
            .env("MAW_FP", &maw_fp)
            .output()
            .expect("failed to execute failpoints maw binary")
    };

    let out = match op {
        Op::Merge {
            srcs,
            into,
            destroy,
        } => {
            let mut args: Vec<&str> = vec!["ws", "merge"];
            for src in srcs {
                args.push(&src.0);
            }
            args.push("--into");
            args.push(merge_target(into));
            args.push("--message");
            args.push("dst: production-tier merge");
            if *destroy {
                args.push("--destroy");
            }
            run(&args)
        }
        Op::Commit { ws, msg } => {
            // `git add -A` is benign; only the commit carries the armed env.
            let _ = run(&["exec", &ws.0, "--", "git", "add", "-A"]);
            run(&["exec", &ws.0, "--", "git", "commit", "-m", &msg.0])
        }
        // The generator never attaches a fault to any other op; if that ever
        // changes, fall back to the unfaulted plain-binary path rather than
        // silently dropping the op.
        _ => return (execute_op(repo, op), false, String::new()),
    };

    let succeeded = out.status.success();
    let crashed = !out.status.success();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (OpOutcome::from(succeeded), crashed, text)
}

/// Merge target name. Increment 1 always merges into `default`.
#[cfg(feature = "assurance")]
const fn merge_target(into: &Target) -> &'static str {
    // The generator only emits `Target::Default` (see maw-scenario try_emit),
    // but `Target::Change` would also route to `default` for increment 1 since
    // we deliberately pin a single, always-valid merge target here.
    match into {
        Target::Default | Target::Change(_) => "default",
    }
}

// ---------------------------------------------------------------------------
// Seeded run
// ---------------------------------------------------------------------------

/// Run a single seed's plan against a fresh real repo, returning oracle
/// violations and (via `live`) liveness accounting.
///
/// After each op we capture the real repo state (`capture_state` reads the
/// real per-ws worktree HEADs) and run BOTH authoritative oracles:
/// - Oracle A is INCREMENTAL — `check_step(state, step_index)` accumulates
///   witness blobs across steps, so it must be called once per step, in order,
///   with the running `step_index`. A `StepReport.violation` is a real
///   work-loss signal; an `Err(_)` is a plumbing failure (reported as
///   inconclusive, never silently swallowed).
/// - Oracle B is STATELESS — `oracle_b::check(root)` returns a `Vec` of
///   state-coherence violations (dangling head/owned refs, merge-state orphans
///   — the bn-cm63 class). Non-empty == violation.
///
/// `inject_faults`: when `true`, any step carrying a `FaultSpec::Failpoint` is
/// executed via the **failpoints** binary with `MAW_FP=<name>=abort`, crashing
/// the op mid-flight (the realistic "mid-op kill"). The oracle then judges the
/// post-crash repo: maw's merge-state recovery is what must keep it coherent
/// and lose no committed work. Unfaulted ops always take the plain-binary path,
/// so the failpoints binary is never paid for ops that carry no fault.
#[cfg(feature = "assurance")]
#[allow(clippy::too_many_lines)]
fn run_seed(
    seed: u64,
    n_steps: usize,
    inject_faults: bool,
    corrupt_weight: u32,
    live: &mut Liveness,
) -> Vec<String> {
    let mut violations = Vec::new();

    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);

    // Enable the Advance op (weight 8, comparable to the other op weights) so
    // this tier exercises the production `ws advance` HEAD-movement path. The
    // DEFAULT profile keeps advance_weight=0, so the bn-2yzz in-proc campaign's
    // seed→plan stream is unaffected (proven by the maw-scenario determinism +
    // corpus tests). This tier regenerates plans each run, so a different
    // byte stream here is fine.
    // bn-2bcx: enable the escape-path ops (OutOfMawCommit / DirtyTrunkWrite /
    // Gc) at a LOW weight relative to advance (8) and the core op weights, so
    // the default (fast) tier stays within budget while still exercising the
    // FF-absorb / dirty-trunk / gc-recover code. The DEFAULT profile keeps both
    // weights 0, so the bn-2yzz in-proc campaign's seed→plan stream is
    // untouched; this tier regenerates plans each run so a different byte
    // stream here is fine. Soak campaigns raise `DST_ESCAPE_WEIGHT`.
    let escape_weight = u32::try_from(env_count("DST_ESCAPE_WEIGHT", 3)).unwrap_or(3);
    // bn-22jy: the stat-cache-masked corruption op is a SEPARATE opt-in knob,
    // defaulting to 0 here as well. `dst_production_tier_no_work_lost` (the
    // default gate) therefore generates exactly the plans it generated before
    // bn-22jy; only `dst_production_tier_masked_stale_corruption` turns it on.
    // Keeping it off by default matters beyond determinism: a poisoned
    // workspace is excluded from `CleanMaterialization` until an op rewrites
    // its worktree wholesale, so a high corruption rate would quietly erode the
    // bn-p3m9 gate's coverage.
    let plan = generate_plan(
        seed,
        &ConditionProfile::default()
            .with_advance_weight(8)
            .with_escape_weight(escape_weight)
            .with_corrupt_weight(corrupt_weight),
        n_steps,
    );

    // Oracle A is incremental: ONE instance per seed/repo, fed every step.
    let mut oracle_a = OracleA::new(repo.root());
    // bn-2bcx escape oracles: SiblingRefFaithfulness is incremental (one per
    // seed, fed every step in order); TrunkDirtyPreservation accumulates
    // recorded dirty writes; RecordRefCoherence is stateless.
    let mut sibling_oracle = SiblingRefFaithfulness::new();
    let mut trunk_oracle = TrunkDirtyPreservation::new();
    // bn-3gba: CleanMaterialization is incremental (one per seed, fed every
    // step in order with the op's success verdict).
    let mut clean_materialization = CleanMaterialization::new();
    // bn-22jy: MaskedStalePreservation watches every path the corruption
    // primitive actually poisoned. Inert until `record_masked` is called, so
    // corrupt_weight=0 runs pay nothing.
    let mut masked_oracle = MaskedStalePreservation::new();
    let mut last_epoch = repo.current_epoch();

    for (i, step) in plan.steps.iter().enumerate() {
        let op = &step.op;
        let name = op_name(op);

        live.ops_attempted += 1;
        // Decide whether this step is faulted. Faults only attach to Merge/Commit
        // ops (generator invariant), and only when fault injection is enabled.
        let fault_name = if inject_faults {
            match &step.fault {
                FaultSpec::Failpoint { name, .. } => Some(name.as_str()),
                FaultSpec::None => None,
            }
        } else {
            None
        };

        let outcome = if let Some(fp_name) = fault_name {
            // Arm MAW_FP=<name>=abort on the failpoints binary; the op will
            // likely crash mid-flight. That is EXPECTED — the oracle judges the
            // post-crash state below.
            live.faults_injected += 1;
            let (out, _crashed, _text) = execute_op_faulted(&repo, op, fp_name);
            out
        } else {
            execute_op(&repo, op)
        };
        let succeeded = outcome.succeeded;
        // bn-22jy: feed the corruption oracle the RESOLVED victim (the op
        // carries only a hint), so it never judges a path that was not actually
        // poisoned.
        if matches!(op, Op::CorruptWorktreeStatMasked { .. }) {
            live.masked_corruptions += 1;
        }
        if let Op::CorruptWorktreeStatMasked { ws, .. } = op
            && let Some((path, stale)) = &outcome.masked
        {
            live.masked_corruptions_effective += 1;
            masked_oracle.record_masked(&ws.0, path, stale);
        }
        if succeeded {
            live.ops_succeeded += 1;
        }
        if matches!(op, Op::WsCreate { .. }) && succeeded {
            live.ws_created += 1;
        }
        if matches!(op, Op::Advance { .. }) && succeeded {
            live.advances_run += 1;
        }
        // bn-2bcx escape-op bookkeeping + liveness.
        match op {
            Op::OutOfMawCommit { files, .. } => {
                live.out_of_maw_commits += 1;
                // A deliberate trunk re-commit supersedes any dirty-write
                // expectation on the same paths.
                trunk_oracle.note_trunk_overwrite(files.iter().map(|f| f.path.as_str()));
            }
            Op::DirtyTrunkWrite { files } => {
                live.dirty_trunk_writes += 1;
                for fe in files {
                    trunk_oracle.record_dirty(&fe.path, &fe.content);
                }
            }
            Op::Gc { .. } if succeeded => live.gc_runs += 1,
            _ => {}
        }

        // Detect epoch advance (a merge committing into default).
        let now_epoch = repo.current_epoch();
        if now_epoch != last_epoch {
            live.epoch_advances += 1;
            last_epoch = now_epoch;
        }

        // Capture the REAL post-op state (real worktree HEADs).
        let state = match capture_oracle_state(repo.root()) {
            Ok(s) => s,
            Err(err) => {
                violations.push(format!(
                    "seed={seed} step={i} op={name}: capture_state failed (plumbing): {err}"
                ));
                continue;
            }
        };

        // Oracle A (incremental content-reachability no-work-lost).
        match oracle_a.check_step(&state, i) {
            Ok(report) => {
                if let Some(v) = report.violation {
                    violations.push(format!("seed={seed} step={i} op={name} OracleA: {v}"));
                }
            }
            Err(err) => {
                // A plumbing error (e.g. git rev-list failed), NOT a clean
                // pass. Report it so the run is never vacuously green.
                violations.push(format!(
                    "seed={seed} step={i} op={name} OracleA plumbing error: {err}"
                ));
            }
        }

        // Oracle B (stateless state-coherence).
        for v in oracle_b::check(repo.root()) {
            violations.push(format!("seed={seed} step={i} op={name} OracleB: {v:?}"));
        }

        // --- bn-2bcx escape-path oracles ---
        // SiblingRefFaithfulness (bn-rah2): a committed-ahead sibling's work
        // must not be orphaned by an op that did not target it (FF-absorb).
        for v in sibling_oracle.check_step(repo.root(), op) {
            violations.push(format!(
                "seed={seed} step={i} op={name} SiblingRefFaithfulness: {v}"
            ));
        }
        // TrunkDirtyPreservation (bn-1xmk): recorded uncommitted trunk bytes
        // must survive on disk or be surfaced in a recovery ref.
        for v in trunk_oracle.check(repo.root()) {
            violations.push(format!(
                "seed={seed} step={i} op={name} TrunkDirtyPreservation: {v}"
            ));
        }
        // RecordRefCoherence (bn-3uou): no destroy record may claim a recovery
        // ref that does not exist.
        for v in check_record_ref_coherence(repo.root()) {
            violations.push(format!(
                "seed={seed} step={i} op={name} RecordRefCoherence: {v}"
            ));
        }
        // --- bn-3gba clean-materialization oracle ---
        // After every create/sync/absorb/auto-rebase (in fact after EVERY op),
        // every live, expected-clean, non-default workspace's worktree must
        // equal its own HEAD tree. This is the bn-p3m9 class that Oracle A
        // (blob reachability) and Oracle B (refs + merge-state) are blind to by
        // construction. Fed the op's success verdict so a FAILED commit does
        // not clear the workspace's expected-dirty bit.
        for v in clean_materialization.check_step(repo.root(), op, succeeded) {
            violations.push(format!(
                "seed={seed} step={i} op={name} CleanMaterialization: {v}"
            ));
        }
        // --- bn-22jy / bn-154g preserve-before-overwrite oracle ---
        // Once maw overwrites a stat-cache-masked stale file, the pre-overwrite
        // bytes must be reachable from that workspace's own recovery namespace.
        for v in masked_oracle.check_step(repo.root(), op) {
            violations.push(format!(
                "seed={seed} step={i} op={name} MaskedStalePreservation: {v}"
            ));
        }
    }

    // Record how much content Oracle A actually witnessed this seed (the
    // non-vacuity signal — accumulated across seeds by the caller).
    live.oracle_a_witnesses = live
        .oracle_a_witnesses
        .saturating_add(oracle_a.witness_count() as u64);
    // bn-3gba: same discipline for the clean-materialization oracle.
    live.clean_materialization_checks = live
        .clean_materialization_checks
        .saturating_add(clean_materialization.checks_run());
    // bn-22jy: same discipline for the preserve-before-overwrite oracle.
    live.masked_overwrites_judged = live
        .masked_overwrites_judged
        .saturating_add(masked_oracle.overwrites_judged());
    live.masked_resolutions = live
        .masked_resolutions
        .saturating_add(clean_materialization.masked_resolutions());

    violations
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Drive the production-code DST tier over `count` seeds × `n_steps` steps,
/// running both authoritative oracles after every op. Shared by the fast
/// unfaulted default test and the heavyweight faulted variant; `inject_faults`
/// selects whether `FaultSpec::Failpoint` steps are armed with
/// `MAW_FP=<name>=abort` on the failpoints binary.
///
/// Returns the accumulated `(Liveness, all_violations, failing_seeds)` so the
/// caller can apply variant-specific guards (e.g. `faults_injected > 0`) and
/// the final violation assertion.
#[cfg(feature = "assurance")]
fn drive_tier(
    label: &str,
    count: u64,
    n_steps: usize,
    inject_faults: bool,
    corrupt_weight: u32,
) -> (Liveness, Vec<String>, Vec<u64>) {
    let mut live = Liveness::default();
    let mut all_violations: Vec<String> = Vec::new();
    let mut failing_seeds: Vec<u64> = Vec::new();

    for seed in 0..count {
        let v = run_seed(seed, n_steps, inject_faults, corrupt_weight, &mut live);
        if !v.is_empty() {
            failing_seeds.push(seed);
            for line in &v {
                all_violations.push(format!("[seed={seed}] {line}"));
            }
        }
    }

    // Accrual + statistical reporting (mirrors the bn-2yzz in-proc floor's
    // rule). Each op-step is one oracle TRIAL (capture_state + Oracle A
    // check_step + Oracle B check). With X=0 observed violations over N
    // trials, the one-sided Wilson 95% upper bound on the per-op-step
    // violation rate is ≈ z²/N = 3.8416/N (z=1.96; the X=0 closed form). This
    // is the SAME discipline the SG1 soak campaign publishes (every "0/N" cell
    // reports its Wilson UB). Raise DST_TRACES / DST_STEPS to accrue toward a
    // production-code op-step floor; this tier is the production-code analog of
    // the in-proc volume soak, so its evidence is reported the same way.
    let n_trials = live.ops_attempted;
    // n_trials is a trial COUNT; the f64 widening for the Wilson 95% bound is
    // exact for any realistic soak volume and harmless for a confidence bound
    // (matches the in-proc soak's own cast_precision_loss allow).
    #[allow(clippy::cast_precision_loss)]
    let wilson_ub = if n_trials > 0 {
        3.8416_f64 / n_trials as f64
    } else {
        1.0
    };
    eprintln!(
        "{label}: ran {} op-steps across {count} seeds ({} steps/seed); \
         {} ops succeeded, {} workspaces created, {} epoch advances, \
         {} Oracle-A witness blobs, {} ws-advances, {} faults injected; \
         {} out-of-maw-commits, {} dirty-trunk-writes, {} gc-runs (bn-2bcx); \
         {} clean-materialization checks (bn-3gba); \
         {} masked corruptions ({} effective), {} masked-removal judgements \
         (overwrite or destroy, bn-2k9e), {} masks re-materialized (bn-22jy); \
         {} violations over N={} trials \
         (Wilson 95% UB on per-op-step violation rate = {:.3e})",
        live.ops_attempted,
        n_steps,
        live.ops_succeeded,
        live.ws_created,
        live.epoch_advances,
        live.oracle_a_witnesses,
        live.advances_run,
        live.faults_injected,
        live.out_of_maw_commits,
        live.dirty_trunk_writes,
        live.gc_runs,
        live.clean_materialization_checks,
        live.masked_corruptions,
        live.masked_corruptions_effective,
        live.masked_overwrites_judged,
        live.masked_resolutions,
        all_violations.len(),
        n_trials,
        wilson_ub,
    );

    (live, all_violations, failing_seeds)
}

/// Apply the liveness guards shared by both variants (workspaces created, epoch
/// advanced, Oracle A witnessed content, advance path exercised).
#[cfg(feature = "assurance")]
fn assert_shared_liveness(live: &Liveness, count: u64, n_steps: usize) {
    // ----- Liveness guard: the test must not be vacuously green. -----
    // If essentially nothing happened, the Op→CLI mapping is broken and this
    // would be a false pass — fail loudly so it gets fixed, not papered over.
    assert!(
        live.ws_created > 0,
        "LIVENESS FAILURE: zero workspaces were created across {count} seeds \
         ({} op-steps). The Op->CLI mapping is wrong (maw ws create never \
         succeeded), so the oracle never saw real production state. Fix the \
         mapping rather than trusting this as a pass.",
        live.ops_attempted,
    );
    assert!(
        live.epoch_advances > 0,
        "LIVENESS FAILURE: the epoch never advanced across {count} seeds \
         ({} op-steps, {} ws created). No merge committed into default, so the \
         no-work-lost oracle was never exercised against a real epoch bump. \
         Fix the merge mapping (or raise DST_STEPS) rather than trusting this \
         as a pass.",
        live.ops_attempted,
        live.ws_created,
    );
    // The decisive non-vacuity guard: Oracle A must have actually witnessed
    // committed content. If it saw zero blobs, `capture_state` did not observe
    // the real worktrees and the oracle was never exercised — a green result
    // would be meaningless.
    assert!(
        live.oracle_a_witnesses > 0,
        "LIVENESS FAILURE: Oracle A accumulated ZERO witness blobs across \
         {count} seeds ({} ops succeeded, {} ws created, {} epoch advances). \
         The oracle never saw committed content — capture_state likely did not \
         enumerate the real worktrees — so 'no violations' is vacuous. \
         Investigate capture_state / layout before trusting this as a pass.",
        live.ops_succeeded,
        live.ws_created,
        live.epoch_advances,
    );

    // bn-3gba non-vacuity: the CleanMaterialization oracle must have actually
    // judged live workspaces. If it never ran a single `worktree == HEAD`
    // assertion, the bn-p3m9 gate is vacuous — a silent regression in the
    // expected-dirty model (or in the workspace enumeration) would look green.
    assert!(
        live.clean_materialization_checks > 0,
        "LIVENESS FAILURE (bn-3gba): the CleanMaterialization oracle ran ZERO \
         worktree==HEAD assertions across {count} seeds ({n_steps} steps/seed, \
         {} ws created). Either no workspace was ever expected-clean or the \
         workspace enumeration is broken — investigate before trusting a pass.",
        live.ws_created,
    );

    // Non-vacuity for the Advance path: with advance_weight>0 enabled, the
    // production `ws advance` HEAD-movement code (bn-8flz) must actually have
    // run at least once — otherwise this tier silently stops covering it.
    assert!(
        live.advances_run > 0,
        "LIVENESS FAILURE: zero successful `ws advance` ops across {count} seeds \
         ({n_steps} steps/seed). The Advance op was enabled (advance_weight>0) but never \
         executed against a persistent committed-ahead workspace — the production \
         advance/rebase path was not exercised. Raise DST_STEPS or check the \
         WsCreate --persistent mapping.",
    );
}

/// Production-code DST tier: drive real `maw` over seed-generated op streams
/// and assert the SG1 oracles hold after every op.
#[cfg(feature = "assurance")]
#[test]
fn dst_production_tier_no_work_lost() {
    let count = env_count("DST_TRACES", 16);
    // Default 24 steps/seed: long enough that a healthy fraction of seeds reach
    // Edit -> Commit -> Merge and actually advance the epoch (so the
    // epoch-advance liveness guard is comfortably satisfied, not marginal),
    // while keeping the default run well under a minute. Soak campaigns raise
    // both knobs via DST_TRACES / DST_STEPS.
    let n_steps = usize::try_from(env_count("DST_STEPS", 24)).expect("DST_STEPS fits usize");

    let (live, all_violations, failing_seeds) =
        drive_tier("dst-production-tier", count, n_steps, false, 0);

    assert_shared_liveness(&live, count, n_steps);

    // bn-2bcx non-vacuity: the escape ops must actually have been exercised,
    // otherwise the FF-absorb / dirty-trunk / gc-recover oracles judged nothing
    // new. With escape_weight=3 over the default 16x24 budget this is
    // comfortably satisfied; if it ever fails, raise DST_STEPS/DST_TRACES or
    // DST_ESCAPE_WEIGHT rather than trusting a vacuous pass.
    assert!(
        live.out_of_maw_commits + live.dirty_trunk_writes + live.gc_runs > 0,
        "LIVENESS FAILURE (bn-2bcx): zero escape-path ops (out-of-maw-commit / \
         dirty-trunk-write / gc) ran across {count} seeds ({} op-steps). The \
         FF-absorb / dirty-trunk / gc-recover oracles were never exercised.",
        live.ops_attempted,
    );

    assert!(
        all_violations.is_empty(),
        "Oracle violations across {} failing seed(s) {:?}:\n{}",
        failing_seeds.len(),
        failing_seeds,
        all_violations.join("\n"),
    );
}

/// FAULTED production-code DST tier: same op streams, but every step the
/// generator marks with a `FaultSpec::Failpoint` is executed via the
/// **failpoints** binary with `MAW_FP=<name>=abort`, crashing the op mid-flight
/// (the realistic "mid-op kill"). After EVERY op — crashed or not — both
/// authoritative oracles judge the on-disk repo. The load-bearing assertion:
/// even after an abort mid-merge/mid-commit, maw's merge-state recovery keeps
/// the repo coherent and loses no committed work (the oracle is the judge).
///
/// `#[ignore]` because it builds a `--features failpoints` `maw` binary
/// (~minutes cold) and runs every faulted op as a separate crashing process —
/// far heavier than the default tier. The fast unfaulted
/// `dst_production_tier_no_work_lost` stays the default-gate test; this variant
/// runs on demand via `just sg1-production-tier-faults` (`--ignored`).
///
/// A real oracle violation here is a candidate REAL maw recovery/work-loss bug:
/// the test is left RED with the seed + op + fault + violation, never suppressed.
///
/// bn-38vw RESOLVED: previously this reproduced a real finding — under
/// `FP_COMMIT_BETWEEN_CAS_OPS=abort` mid-merge, Oracle A (no-work-lost) stayed
/// GREEN but Oracle B fired `MergeStateBadEpoch` (epoch advanced past the
/// point-of-no-return before `epoch_after` was journaled). The fix records
/// `epoch_after` into the merge-state journal BEFORE the ref-advancing CAS, so
/// the journal is coherent at every post-build crash point. This test now
/// PASSES; it remains `#[ignore]` solely for its weight (see above).
#[cfg(feature = "assurance")]
#[test]
#[ignore = "heavyweight: builds a --features failpoints maw binary and runs every faulted op as a separate crashing process. Run via just sg1-production-tier-faults"]
fn dst_production_tier_survives_faults() {
    let count = env_count("DST_TRACES", 16);
    // Same 24-step window as the unfaulted `dst_production_tier_no_work_lost`
    // test, which is GREEN at this budget. Pinning the same window means any
    // violation this variant surfaces is attributable to the INJECTED FAULTS,
    // not to a fault-independent issue that only appears at deeper step counts.
    // (At 16 seeds × 24 steps the default profile arms ~10 Merge/Commit faults —
    // comfortably above the `faults_injected > 0` non-vacuity guard.) Soak
    // campaigns raise both knobs via DST_TRACES / DST_STEPS.
    let n_steps = usize::try_from(env_count("DST_STEPS", 24)).expect("DST_STEPS fits usize");

    let (live, all_violations, failing_seeds) =
        drive_tier("dst-production-tier-faults", count, n_steps, true, 0);

    assert_shared_liveness(&live, count, n_steps);

    // Non-vacuity for the WHOLE POINT of this variant: faults must actually have
    // been injected. With the default profile's mid_op_kill_prob=0.15, faults
    // attach to a healthy fraction of Merge/Commit ops over enough steps; if
    // none fired, this variant degenerates into the unfaulted tier and "no
    // violations" says nothing about recovery under crashes.
    assert!(
        live.faults_injected > 0,
        "LIVENESS FAILURE: ZERO faults were injected across {count} seeds \
         ({n_steps} steps/seed). The default profile arms faults on Merge/Commit \
         ops at mid_op_kill_prob=0.15, so over enough steps at least one should \
         fire. Raise DST_STEPS / DST_TRACES (more Merge/Commit ops) — a faulted \
         tier that injects no faults is vacuously green.",
    );

    // A violation here under faults = a candidate REAL maw recovery/work-loss
    // bug. Leave it RED with full detail; do NOT suppress.
    assert!(
        all_violations.is_empty(),
        "ORACLE VIOLATION UNDER FAULT INJECTION across {} failing seed(s) {:?} \
         — candidate REAL maw recovery/work-loss bug (post-crash state failed \
         the oracle). DO NOT suppress; investigate the seed + op + fault:\n{}",
        failing_seeds.len(),
        failing_seeds,
        all_violations.join("\n"),
    );
}

// ---------------------------------------------------------------------------
// bn-2bcx: named regression scenarios for the 2026-07 escapes
// ---------------------------------------------------------------------------

/// Drive a hand-built regression [`ScenarioPlan`] against a fresh real repo,
/// running ALL five oracles (Oracle A, Oracle B, and the three bn-2bcx escape
/// oracles) after every op. Returns the collected violation strings.
///
/// This is the acceptance harness for the escape-path oracles: it drives the
/// REAL maw binary over the exact incident shapes, so reverting a fix makes the
/// corresponding oracle turn red here within the plan's bounded step count.
#[cfg(feature = "assurance")]
fn drive_regression_plan(plan: &maw::assurance::scenario::ScenarioPlan) -> Vec<String> {
    drive_regression_plan_reported(plan).violations
}

/// What [`drive_regression_plan_reported`] observed while driving a regression
/// plan: the oracle violations plus the NON-VACUITY evidence a caller needs to
/// prove the plan actually reached the state it exists to cover.
#[cfg(feature = "assurance")]
struct RegressionRun {
    /// Oracle violation strings (empty == the plan is green).
    violations: Vec<String>,
    /// Workspaces that ended the run with a `rebase-conflicts.json` sidecar —
    /// i.e. whose auto-rebase actually produced conflict-as-data (bn-286g).
    conflicted_workspaces: Vec<String>,
    /// `true` if some blob reachable at the end of the run bears maw's diff3
    /// conflict markers — the marker rewrite the bn-286g carveout is about.
    saw_conflict_marker_blob: bool,
    /// bn-22jy: `(workspace, path)` pairs the corruption primitive actually
    /// masked. Empty means the primitive degraded to a no-op, so a green run
    /// says nothing about the bn-154g guard.
    masked_paths: Vec<(String, String)>,
    /// bn-22jy: how many masked paths were later overwritten and judged. 0
    /// means nothing ever reached the preserve-before-overwrite site.
    masked_overwrites_judged: u64,
    /// bn-22jy: every `refs/manifold/recovery/*` ref present at the end of the
    /// run, so a test can pin the EXACT observable bn-154g produces
    /// (`refs/manifold/recovery/<ws>/materialize-<ts>`) rather than settling
    /// for "the bytes turned up somewhere recoverable".
    recovery_refs: Vec<String>,
}

/// [`drive_regression_plan`] plus the non-vacuity evidence.
#[cfg(feature = "assurance")]
fn drive_regression_plan_reported(plan: &maw::assurance::scenario::ScenarioPlan) -> RegressionRun {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);
    drive_plan_on(&repo, plan, false, &mut |_, _| {}).0
}

/// One faulted step of a regression plan driven with faults honored.
#[cfg(feature = "assurance")]
struct FaultedStep {
    index: usize,
    /// The `MAW_FP` spec armed on the failpoints binary.
    spec: String,
    /// Whether the faulted `maw` invocation exited 0.
    succeeded: bool,
    /// Its combined stdout + stderr.
    output: String,
    /// The merge journal was on disk right after the faulted op.
    journal_after: bool,
    /// A target-checkout intent (bn-15fzo) was on disk right after it.
    checkout_intent_after: bool,
}

/// Drive `plan` on an already-seeded `repo`, running every oracle after every
/// op. With `inject_faults`, a step carrying a `FaultSpec::Failpoint` runs on
/// the failpoints binary with
/// [`maw::assurance::fault::production_fp_spec`] (exactly as the faulted
/// tier does). `before_step(repo, i)` runs before step `i` executes, for
/// harness-side setup the op vocabulary cannot express (bn-1sbjf: a trunk
/// symlink).
#[cfg(feature = "assurance")]
fn drive_plan_on(
    repo: &TestRepo,
    plan: &maw::assurance::scenario::ScenarioPlan,
    inject_faults: bool,
    before_step: &mut dyn FnMut(&TestRepo, usize),
) -> (RegressionRun, Vec<FaultedStep>) {
    let mut faulted_steps = Vec::new();

    let mut oracle_a = OracleA::new(repo.root());
    let mut sibling_oracle = SiblingRefFaithfulness::new();
    let mut trunk_oracle = TrunkDirtyPreservation::new();
    let mut clean_materialization = CleanMaterialization::new();
    let mut masked_oracle = MaskedStalePreservation::new();
    let mut masked_paths: Vec<(String, String)> = Vec::new();
    let mut violations = Vec::new();

    for (i, step) in plan.steps.iter().enumerate() {
        let op = &step.op;
        let name = op_name(op);
        before_step(repo, i);
        let outcome = match (&step.fault, inject_faults) {
            (FaultSpec::Failpoint { name: fp, .. }, true) => {
                let (outcome, _crashed, output) = execute_op_faulted(repo, op, fp);
                faulted_steps.push(FaultedStep {
                    index: i,
                    spec: maw::assurance::fault::production_fp_spec(fp),
                    succeeded: outcome.succeeded,
                    output,
                    journal_after: merge_journal_path(repo.root()).exists(),
                    checkout_intent_after: checkout_intent_path(repo.root(), "default").exists(),
                });
                outcome
            }
            _ => execute_op(repo, op),
        };
        let succeeded = outcome.succeeded;

        if let Op::CorruptWorktreeStatMasked { ws, .. } = op
            && let Some((path, stale)) = &outcome.masked
        {
            masked_paths.push((ws.0.clone(), path.clone()));
            masked_oracle.record_masked(&ws.0, path, stale);
        }

        match op {
            Op::OutOfMawCommit { files, .. } => {
                trunk_oracle.note_trunk_overwrite(files.iter().map(|f| f.path.as_str()));
            }
            Op::DirtyTrunkWrite { files } => {
                for fe in files {
                    trunk_oracle.record_dirty(&fe.path, &fe.content);
                }
            }
            _ => {}
        }

        let state = match capture_oracle_state(repo.root()) {
            Ok(s) => s,
            Err(err) => {
                violations.push(format!("step={i} op={name}: capture_state failed: {err}"));
                continue;
            }
        };
        if let Ok(report) = oracle_a.check_step(&state, i)
            && let Some(v) = report.violation
        {
            violations.push(format!("step={i} op={name} OracleA: {v}"));
        }
        for v in oracle_b::check(repo.root()) {
            violations.push(format!("step={i} op={name} OracleB: {v:?}"));
        }
        for v in sibling_oracle.check_step(repo.root(), op) {
            violations.push(format!("step={i} op={name} SiblingRefFaithfulness: {v}"));
        }
        for v in trunk_oracle.check(repo.root()) {
            violations.push(format!("step={i} op={name} TrunkDirtyPreservation: {v}"));
        }
        for v in check_record_ref_coherence(repo.root()) {
            violations.push(format!("step={i} op={name} RecordRefCoherence: {v}"));
        }
        for v in masked_oracle.check_step(repo.root(), op) {
            violations.push(format!("step={i} op={name} MaskedStalePreservation: {v}"));
        }
        for v in clean_materialization.check_step(repo.root(), op, succeeded) {
            violations.push(format!("step={i} op={name} CleanMaterialization: {v}"));
        }
    }

    let conflicted_workspaces = workspaces_with_conflict_sidecar(repo.root());
    let saw_conflict_marker_blob = repo_has_conflict_marker_blob(repo.root());
    (
        RegressionRun {
            violations,
            conflicted_workspaces,
            saw_conflict_marker_blob,
            masked_paths,
            masked_overwrites_judged: masked_oracle.overwrites_judged(),
            recovery_refs: recovery_ref_names(repo.root()),
        },
        faulted_steps,
    )
}

/// The merge journal (`merge-state.json`) path for `root`'s layout.
#[cfg(feature = "assurance")]
fn merge_journal_path(root: &std::path::Path) -> std::path::PathBuf {
    maw_core::merge_state::MergeStateFile::default_path(
        &maw_core::model::layout::LayoutFlavor::detect(root).manifold_dir(root),
    )
}

/// The bn-15fzo target-checkout intent record for workspace `ws`.
#[cfg(feature = "assurance")]
fn checkout_intent_path(root: &std::path::Path, ws: &str) -> std::path::PathBuf {
    maw_core::model::layout::LayoutFlavor::detect(root)
        .manifold_dir(root)
        .join(format!("target-checkout-{ws}.json"))
}

/// Every `refs/manifold/recovery/*` ref name in the repo, sorted.
#[cfg(feature = "assurance")]
fn recovery_ref_names(root: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/manifold/recovery",
        ])
        .current_dir(root)
        .output();
    let mut names: Vec<String> = out
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Names of workspaces whose `artifacts/ws/<name>/rebase-conflicts.json`
/// sidecar exists — the on-disk proof that maw's auto-rebase produced
/// conflict-as-data for that workspace (bn-286g non-vacuity).
#[cfg(feature = "assurance")]
fn workspaces_with_conflict_sidecar(root: &std::path::Path) -> Vec<String> {
    let dir = maw_core::model::layout::LayoutFlavor::detect(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("rebase-conflicts.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// `true` if ANY object in the repo is a blob bearing maw's diff3 conflict
/// markers — the marker rewrite that made the sibling's original blob OID
/// unreachable (bn-286g non-vacuity).
#[cfg(feature = "assurance")]
fn repo_has_conflict_marker_blob(root: &std::path::Path) -> bool {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--objects", "--all"])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return false };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(oid) = line.split_whitespace().next() else {
            continue;
        };
        let Ok(blob) = std::process::Command::new("git")
            .args(["cat-file", "blob", oid])
            .current_dir(root)
            .output()
        else {
            continue;
        };
        if !blob.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&blob.stdout);
        if text.contains("<<<<<<<") && text.contains(">>>>>>>") {
            return true;
        }
    }
    false
}

/// The bn-rah2 regression scenario (FF-absorb orphaned committed-ahead
/// sibling) must be GREEN under all oracles with the fix (68abe479) in place.
///
/// This is the acceptance proof: locally reverting the bn-rah2 sibling-replay
/// classification in `reconcile_epoch_with_branch` turns this RED via
/// `SiblingRefFaithfulness` within the plan's bounded step count.
#[cfg(feature = "assurance")]
#[test]
fn bn_rah2_regression_is_green() {
    let plan = maw::assurance::scenario::bn_rah2_regression_plan();
    let violations = drive_regression_plan(&plan);
    assert!(
        violations.is_empty(),
        "bn-rah2 regression must be clean with the fix in place; oracle violations:\n{}",
        violations.join("\n"),
    );
}

/// bn-286g: a post-merge sibling auto-rebase that CONFLICTS must be GREEN.
///
/// This is the default-budget regression for the deep-DST failure
/// (`DST_TRACES=48 DST_STEPS=48`, seeds 0/15/18): maw rewrites the sibling's
/// committed blob into a diff3 conflict-marker blob and pins the original OID
/// in `rebase-conflicts.json`. The bytes survive verbatim and the state is
/// recoverable (`maw ws resolve`), so this is conflict-as-data, not an
/// orphaned sibling — `SiblingRefFaithfulness` must not fire. Reverting the
/// bn-286g carveout in `oracle_escape.rs` turns this test RED.
///
/// The two non-vacuity assertions are load-bearing: without a real conflict
/// (sidecar written AND a marker blob in the object store) the plan would
/// exercise nothing and a green result would say nothing.
#[cfg(feature = "assurance")]
#[test]
fn bn_286g_conflicted_sibling_replay_is_green() {
    let plan = maw::assurance::scenario::bn_286g_regression_plan();
    let run = drive_regression_plan_reported(&plan);

    assert!(
        run.conflicted_workspaces.iter().any(|w| w == "ws-sibling"),
        "NON-VACUITY (bn-286g): the sibling auto-rebase did not produce a \
         conflict sidecar, so the conflict-as-data path was never exercised. \
         Workspaces with sidecars: {:?}",
        run.conflicted_workspaces,
    );
    assert!(
        run.saw_conflict_marker_blob,
        "NON-VACUITY (bn-286g): no conflict-marker blob exists in the repo, \
         so no committed blob was ever rewritten — the oracle carveout under \
         test was never reached."
    );

    assert!(
        run.violations.is_empty(),
        "bn-286g: a conflicting sibling replay is conflict-as-data, not work \
         loss; oracle violations:\n{}",
        run.violations.join("\n"),
    );
}

/// The bn-1xmk regression scenario (dirty tracked trunk file clobbered by
/// preserve-and-replay) must be GREEN under all oracles with the fix in place.
#[cfg(feature = "assurance")]
#[test]
fn bn_1xmk_regression_is_green() {
    let plan = maw::assurance::scenario::bn_1xmk_regression_plan();
    let violations = drive_regression_plan(&plan);
    assert!(
        violations.is_empty(),
        "bn-1xmk regression must be clean with the fix in place; oracle violations:\n{}",
        violations.join("\n"),
    );
}

// ---------------------------------------------------------------------------
// bn-22jy: stat-cache-masked worktree corruption
// ---------------------------------------------------------------------------

/// The bn-154g scenario — a stat-cache-masked stale file flattened by the
/// `ws sync` fast-forward checkout — must be GREEN, and must actually happen.
///
/// This is the deliberately-seeded trace the generator op exists to make
/// reachable: `ws-victim` goes stale behind a merge it did not join, gets
/// poisoned behind a forged index stat cache, and is then synced. The FF
/// `checkout_detach` overwrites every entry of the target tree, so the stale
/// bytes are destroyed by design — and `preserve_divergence_before_overwrite`
/// must have pinned them to `refs/manifold/recovery/ws-victim/materialize-*`
/// first. Reverting the bn-154g guard turns this test RED.
///
/// The two non-vacuity assertions are load-bearing. Without a confirmed mask
/// the sync would simply refuse on visible dirt, and without a judged overwrite
/// the pin was never needed — either way a green result would say nothing.
#[cfg(feature = "assurance")]
#[test]
fn bn_154g_masked_stale_pin_is_green() {
    let plan = maw::assurance::scenario::bn_154g_regression_plan();
    let run = drive_regression_plan_reported(&plan);

    assert!(
        run.masked_paths
            .iter()
            .any(|(ws, path)| ws == "ws-victim" && path == "base.txt"),
        "NON-VACUITY (bn-22jy): the stat-cache mask never took, so `maw ws sync` \
         would have refused on visible dirt and the bn-154g guard was never \
         armed. Masked paths: {:?}",
        run.masked_paths,
    );
    assert_eq!(
        run.masked_overwrites_judged, 1,
        "NON-VACUITY (bn-154g): expected the sync's fast-forward checkout to \
         destroy the masked bytes exactly once; the oracle judged {} overwrite(s). \
         0 means the sync never overwrote the poisoned path (did it refuse?), so \
         the preserve-before-overwrite site was never reached.",
        run.masked_overwrites_judged,
    );

    assert!(
        run.violations.is_empty(),
        "bn-154g: the epoch-bump fast-forward must pin the hidden divergence \
         before its checkout flattens it; oracle violations:\n{}",
        run.violations.join("\n"),
    );

    // The EXACT bn-154g observable, not merely "recoverable somewhere":
    // `preserve_divergence_before_overwrite` pins to
    // `refs/manifold/recovery/<ws>/materialize-<ts>`.
    assert!(
        run.recovery_refs
            .iter()
            .any(|r| r.starts_with("refs/manifold/recovery/ws-victim/materialize-")),
        "bn-154g: expected a `refs/manifold/recovery/ws-victim/materialize-*` pin \
         from preserve_divergence_before_overwrite; recovery refs present: {:?}",
        run.recovery_refs,
    );
}

/// The corruption-enabled production tier: a modest budget over seed-generated
/// plans with `corrupt_weight > 0`, so the DST explores the bn-154g /
/// bn-3gba interleavings it could not previously reach.
///
/// Deliberately a SEPARATE test from `dst_production_tier_no_work_lost` rather
/// than a knob on it: the default gate keeps `corrupt_weight = 0` so its plans
/// (and the `CleanMaterialization` coverage a poisoned workspace suppresses)
/// are exactly what they were before bn-22jy.
///
/// Budget: 8 seeds x 16 steps by default, raisable via the same `DST_TRACES` /
/// `DST_STEPS` knobs. The corruption weight itself is `DST_CORRUPT_WEIGHT`
/// (default 10 — high relative to the core op weights, because a corruption is
/// only interesting when a LATER op overwrites it, and short plans need the
/// density).
#[cfg(feature = "assurance")]
#[test]
fn dst_production_tier_masked_stale_corruption() {
    let count = env_count("DST_TRACES", 8);
    let n_steps = usize::try_from(env_count("DST_STEPS", 16)).expect("DST_STEPS fits usize");
    let corrupt_weight = u32::try_from(env_count("DST_CORRUPT_WEIGHT", 10)).unwrap_or(10);

    let (live, violations, failing_seeds) = drive_tier(
        "dst-production-tier-masked-stale",
        count,
        n_steps,
        false,
        corrupt_weight,
    );

    // ----- Liveness: the primitive must have actually fired. -----
    assert!(
        live.masked_corruptions > 0,
        "LIVENESS FAILURE (bn-22jy): the generator emitted ZERO \
         CorruptWorktreeStatMasked ops across {count} seeds ({n_steps} steps/seed) \
         with corrupt_weight={corrupt_weight}. The profile gating is wrong."
    );
    assert!(
        live.masked_corruptions_effective > 0,
        "LIVENESS FAILURE (bn-22jy): {} corruption ops were attempted but the \
         stat-cache mask NEVER held, so every one degraded to a no-op and no \
         hidden divergence was ever created. This is a FIXTURE failure, not a \
         maw pass: investigate `corrupt_worktree_stat_masked` (core.checkStat / \
         core.trustCTime, mtime back-dating, filesystem timestamp granularity) \
         rather than trusting this as green.",
        live.masked_corruptions,
    );

    assert!(
        violations.is_empty(),
        "bn-22jy corruption tier: {} oracle violation(s) across seeds {:?}:\n{}",
        violations.len(),
        failing_seeds,
        violations.join("\n"),
    );
}

// ---------------------------------------------------------------------------
// bn-1sbjf: targeted crashes inside the target update
// ---------------------------------------------------------------------------

/// The `(path, bytes)` the bn-1sbjf plan's `DirtyTrunkWrite` step wrote.
#[cfg(feature = "assurance")]
fn bn_1sbjf_dirty_writes(plan: &maw::assurance::scenario::ScenarioPlan) -> Vec<(String, String)> {
    plan.steps
        .iter()
        .find_map(|s| match &s.op {
            Op::DirtyTrunkWrite { files } => Some(
                files
                    .iter()
                    .map(|f| (f.path.clone(), f.content.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .expect("the bn-1sbjf plan dirties the trunk")
}

/// Strict end-state judgement of a bn-1sbjf run, on top of the oracles.
///
/// `TrunkDirtyPreservation` accepts dirty bytes that survive only inside a
/// recovery ref, which is exactly where both bugs left them (bn-15fzo: the
/// user's edits "survived only in a recovery ref"; bn-3jqfk: the pin is still
/// written). So after a successful recovery the harness also demands the
/// end state the live merge produces: every dirty write back on disk, both
/// merges' work present, the journal and checkout intent gone, and nothing
/// but the user's own paths showing as local changes (the merge's own
/// changes must not be recorded as "user edits").
#[cfg(feature = "assurance")]
fn assert_bn_1sbjf_target_state(
    repo: &TestRepo,
    plan: &maw::assurance::scenario::ScenarioPlan,
    extra_local_paths: &[&str],
    context: &str,
) {
    let default_ws = repo.default_workspace();
    let dirty = bn_1sbjf_dirty_writes(plan);
    for (path, content) in &dirty {
        assert_eq!(
            std::fs::read_to_string(default_ws.join(path))
                .ok()
                .as_deref(),
            Some(content.as_str()),
            "bn-1sbjf: uncommitted trunk bytes at '{path}' are not back on disk after \
             recovery\n{context}"
        );
    }
    for ws in ["ws-a", "ws-b"] {
        assert_eq!(
            std::fs::read_to_string(default_ws.join(ws).join("merged.txt"))
                .ok()
                .as_deref(),
            Some(format!("{ws} committed work (bn-1sbjf)\n").as_str()),
            "bn-1sbjf: {ws}'s merged work is missing from the target\n{context}"
        );
    }
    assert!(
        !merge_journal_path(repo.root()).exists(),
        "bn-1sbjf: the interrupted merge's journal was never recovered\n{context}"
    );
    assert!(
        !checkout_intent_path(repo.root(), "default").exists(),
        "bn-1sbjf: the target-checkout intent was left behind\n{context}"
    );
    let status = manifold_common::git_ok(
        &default_ws,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let mut local: Vec<String> = status
        .lines()
        .filter_map(|l| l.get(3..))
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.starts_with(".maw") && !p.starts_with(".manifold"))
        .collect();
    local.sort();
    let mut expected: Vec<String> = dirty
        .iter()
        .map(|(p, _)| p.clone())
        .chain(extra_local_paths.iter().map(|p| (*p).to_owned()))
        .collect();
    expected.sort();
    assert_eq!(
        local, expected,
        "bn-1sbjf: local changes vs the merged commit must be exactly the user's \
         own edits\n{status}\n{context}"
    );
}

/// bn-1sbjf / bn-15fzo: the DST harness crashes a dirty-trunk merge at
/// `FP_CLEANUP_AFTER_DEFAULT_CHECKOUT` (real `abort` on the failpoints
/// binary), then the next planned merge's start recovers it.
///
/// Non-vacuity: the crash must land INSIDE the window — journal on disk,
/// checkout intent on disk, and (observed right before the recovering merge)
/// the target already holding the merged tree while the user's tracked edit is
/// only in the snapshot. Then every oracle must be green and the strict end
/// state must match the live merge. Neutering the bn-15fzo resume (ignoring
/// the checkout intent in `update_default_workspace`) turns this RED.
#[cfg(feature = "assurance")]
#[test]
#[ignore = "heavyweight: builds a --features failpoints maw binary. Run via just sg1-production-tier-faults"]
fn bn_1sbjf_crash_after_default_checkout_is_recovered() {
    let plan = maw::assurance::scenario::bn_1sbjf_target_update_crash_plan(
        "FP_CLEANUP_AFTER_DEFAULT_CHECKOUT",
    );
    let (tracked_path, tracked_dirty) = bn_1sbjf_dirty_writes(&plan)
        .into_iter()
        .next()
        .expect("tracked dirty write");
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);

    let mut in_window: Option<(bool, bool)> = None;
    let (run, faulted) = drive_plan_on(&repo, &plan, true, &mut |repo, i| {
        if i == 8 {
            let ws = repo.default_workspace();
            let merged_on_disk = ws.join("ws-a").join("merged.txt").is_file();
            let user_edit_on_disk =
                std::fs::read_to_string(ws.join(&tracked_path)).is_ok_and(|c| c == tracked_dirty);
            in_window = Some((merged_on_disk, user_edit_on_disk));
        }
    });

    assert_eq!(faulted.len(), 1, "exactly one faulted step");
    let f = &faulted[0];
    let ctx = format!(
        "faulted step {} ({}), exit ok={}\n{}",
        f.index, f.spec, f.succeeded, f.output
    );
    assert_eq!(f.spec, "FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort", "{ctx}");
    assert!(!f.succeeded, "NON-VACUITY: the merge must crash\n{ctx}");
    assert!(
        f.journal_after,
        "NON-VACUITY: the crash must leave the merge journal\n{ctx}"
    );
    assert!(
        f.checkout_intent_after,
        "NON-VACUITY: the crash must land after the checkout intent was written\n{ctx}"
    );
    assert_eq!(
        in_window,
        Some((true, false)),
        "NON-VACUITY: before recovery the target must hold the merged tree \
         (ws-a/merged.txt) with the user's edit only in the snapshot\n{ctx}"
    );
    assert!(
        run.violations.is_empty(),
        "bn-1sbjf: oracle violations after a crash at FP_CLEANUP_AFTER_DEFAULT_CHECKOUT:\n{}\n{ctx}",
        run.violations.join("\n"),
    );
    assert_bn_1sbjf_target_state(&repo, &plan, &[], &ctx);
}

/// bn-1sbjf / bn-3jqfk: the DST harness injects `error` at
/// `FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT` into a dirty-trunk merge whose trunk
/// also carries a retargeted tracked SYMLINK (planted by the harness: the op
/// vocabulary has no symlink write). The merge must take the snapshot-failed
/// fallback, still succeed, pin the symlink AS a symlink, and put it back on
/// disk. Neutering the bn-3jqfk symlink-preserving capture (`DiskSide::capture`
/// following the link) turns this RED.
#[cfg(all(feature = "assurance", unix))]
#[test]
#[ignore = "heavyweight: builds a --features failpoints maw binary. Run via just sg1-production-tier-faults"]
fn bn_1sbjf_failed_snapshot_fallback_keeps_trunk_edits_and_symlink() {
    let plan = maw::assurance::scenario::bn_1sbjf_target_update_crash_plan(
        "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT",
    );
    let untracked = maw::assurance::scenario::BN_1SBJF_DIRTY_UNTRACKED_PATH;
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);
    // A tracked symlink in the epoch the workspaces are created from.
    std::os::unix::fs::symlink("base.txt", repo.default_workspace().join("link"))
        .expect("seed symlink");
    repo.advance_epoch("chore: seed tracked symlink (bn-1sbjf)");
    let pins_before = recovery_ref_names(repo.root());

    let (run, faulted) = drive_plan_on(&repo, &plan, true, &mut |repo, i| {
        if i == 7 {
            // Retarget the tracked link right before the faulted merge.
            let link = repo.default_workspace().join("link");
            std::fs::remove_file(&link).expect("rm link");
            std::os::unix::fs::symlink(untracked, &link).expect("retarget link");
        }
    });

    assert_eq!(faulted.len(), 1, "exactly one faulted step");
    let f = &faulted[0];
    let ctx = format!(
        "faulted step {} ({}), exit ok={}\n{}",
        f.index, f.spec, f.succeeded, f.output
    );
    assert_eq!(
        f.spec, "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:dst-injected",
        "{ctx}"
    );
    assert!(
        f.succeeded,
        "the snapshot-failed fallback is handled: the merge must succeed\n{ctx}"
    );
    assert!(
        f.output.contains("snapshot_working_copy failed"),
        "NON-VACUITY: the injected error must force the fallback path\n{ctx}"
    );
    assert!(!f.journal_after, "the handled merge must finish\n{ctx}");
    assert!(
        run.violations.is_empty(),
        "bn-1sbjf: oracle violations after the snapshot-failed fallback:\n{}\n{ctx}",
        run.violations.join("\n"),
    );

    let link = repo.default_workspace().join("link");
    assert!(
        std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()),
        "bn-3jqfk: the user's symlink must be back on disk AS a symlink\n{ctx}"
    );
    assert_eq!(
        std::fs::read_link(&link).ok(),
        Some(std::path::PathBuf::from(untracked)),
        "bn-3jqfk: the user's symlink retarget was lost\n{ctx}"
    );
    // The fallback's recovery pin records the link as a symlink.
    let new_pins: Vec<String> = recovery_ref_names(repo.root())
        .into_iter()
        .filter(|r| !pins_before.contains(r))
        .collect();
    let pinned_as_link = new_pins.iter().any(|r| {
        let entry = manifold_common::git_ok(repo.root(), &["ls-tree", r, "link"]);
        entry.starts_with("120000 ")
            && entry.split_whitespace().nth(2).is_some_and(|oid| {
                manifold_common::git_ok(repo.root(), &["cat-file", "blob", oid]) == untracked
            })
    });
    assert!(
        pinned_as_link,
        "bn-3jqfk: no new recovery pin records `link` as a symlink to {untracked}: \
         {new_pins:?}\n{ctx}"
    );
    assert_bn_1sbjf_target_state(&repo, &plan, &["link"], &ctx);
}
