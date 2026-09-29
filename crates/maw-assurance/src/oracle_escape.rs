//! Oracles for the 2026-07 escape paths (bn-2bcx).
//!
//! Three targeted state-coherence / content-faithfulness oracles that close
//! the gaps the 2026-07 field-report escapes slipped through. Each maps 1:1 to
//! an escaped-bug class:
//!
//! - [`SiblingRefFaithfulness`] — the **bn-rah2** class. FF-absorb
//!   (`reconcile_epoch_with_branch`) orphaned a committed-ahead *sibling*
//!   workspace by raw-resetting its HEAD to the absorbed branch tip instead of
//!   replaying it. This oracle asserts that every **live** workspace's
//!   previously-committed content stays reachable from the union of all refs
//!   (`git rev-list --all`), *unless the just-executed op legitimately moved
//!   that workspace* — so a merge that orphans a NON-target sibling's work
//!   trips it, while a sibling's own commit/advance/replay does not.
//!   bn-286g extends it with Oracle A's bn-3g6o conflict-as-data carveout: a
//!   sibling replay that CONFLICTS rewrites the sibling's blob into a
//!   diff3-marker blob (fresh OID) while preserving the bytes verbatim and
//!   pinning the original OID in the sibling's conflict sidecar — preserved,
//!   not orphaned.
//! - [`TrunkDirtyPreservation`] — the **bn-1xmk** class. Trunk
//!   preserve-and-replay clobbered uncommitted tracked trunk files whose
//!   committed content changed via a merge. This oracle asserts the recorded
//!   uncommitted trunk bytes survive on disk **or** are surfaced in a recovery
//!   ref, after any op.
//! - [`TrunkDirtyDisplacement`] — the **bn-15fzo / bn-3jqfk** class
//!   (bn-2zubk). The strict companion of `TrunkDirtyPreservation`: bytes that
//!   survive ONLY in a recovery ref uphold the Prime Invariant but are still a
//!   bug when the user was never told. This oracle flags any recorded dirty
//!   trunk entry that left the worktree unless the op's own output reported it
//!   (a conflict line or recovery command naming the path).
//! - [`check_record_ref_coherence`] — the **bn-3uou** class. `maw gc` desynced
//!   recovery refs from destroy records. This oracle asserts no destroy record
//!   claims (via `snapshot_ref` / `final_head_ref`) a recovery ref that does
//!   not exist.
//!
//! # Independent-verifier carveout
//!
//! Like [`crate::oracle_b`], all git access here uses the `git` CLI (cwd = repo
//! root) rather than `gix`/`maw-git`, so the verifier does not share code paths
//! with the machinery under test. Destroy records are parsed directly off disk
//! (their JSON schema fields, not the `maw-cli` `DestroyRecord` type) to avoid a
//! `maw-assurance -> maw-cli` dependency cycle — the same "read the artifact,
//! don't call the producer" discipline the rest of the oracle stack follows.

#![cfg(feature = "oracles")]
// Harness/verifier support code (like `in_proc` / `oracle_b`'s git plumbing):
// relax a few pedantic lints that hurt readability of the CLI plumbing without
// buying defect prevention. The production crates keep the strict workspace
// lints.
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::process::Command;

use maw_core::model::layout::LayoutFlavor;

use crate::scenario::Op;

// ---------------------------------------------------------------------------
// Violation type
// ---------------------------------------------------------------------------

/// A violation of one of the bn-2bcx escape-path oracles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapeViolation {
    /// **bn-rah2** — a live workspace's previously-committed content is no
    /// longer reachable from any ref, and the just-executed op did not target
    /// that workspace (so it was orphaned as a side effect — the FF-absorb
    /// sibling-reset class).
    SiblingWorkOrphaned {
        /// The workspace whose committed content was orphaned.
        workspace: String,
        /// A blob OID that was committed by `workspace` but is now unreachable
        /// from `git rev-list --all --objects`.
        blob: String,
    },

    /// **bn-1xmk** — uncommitted trunk bytes recorded at `path` neither survive
    /// on disk in the default workspace nor are surfaced in any recovery ref.
    TrunkDirtyLost {
        /// The tracked trunk path whose uncommitted bytes were lost.
        path: String,
    },

    /// **bn-2zubk** (bn-15fzo / bn-3jqfk class) — uncommitted trunk content
    /// recorded at `path` is no longer in the default worktree, and no op
    /// output reported that it was moved (no conflict line, no recovery
    /// command naming the path). It may well survive in a recovery ref (the
    /// Prime Invariant holds) — the user just has no way to know to look.
    TrunkDirtyDisplaced {
        /// The trunk path whose uncommitted content was displaced.
        path: String,
        /// Whether the displaced content is reachable from a recovery ref
        /// (`true` = silently displaced; `false` = also lost, which
        /// `TrunkDirtyPreservation` reports separately).
        in_recovery_ref: bool,
        /// Whether the displacement was left by a crashed op and survived the
        /// next recovering merge (the bn-15fzo shape), rather than being made
        /// by a completed op.
        after_crash: bool,
        /// Whether the op output went further and CLAIMED the user's version
        /// is back on disk (a false report, the bn-3jqfk regression shape).
        claimed_on_disk: bool,
    },

    /// **bn-3uou** — a destroy record claims a recovery ref that does not
    /// exist. `maw gc` must keep records ↔ refs coherent.
    RecordClaimsMissingRef {
        /// The destroyed workspace the record belongs to.
        workspace: String,
        /// The destroy-record file name.
        record: String,
        /// The recovery ref the record claims but that is absent from the repo.
        claimed_ref: String,
    },

    /// The oracle's own git invocation failed, so no verdict is possible.
    /// Reported as a violation so the run stops loudly rather than silently
    /// green-lighting on broken tooling (matches `oracle_b`'s `GitError`).
    GitError {
        /// Which oracle was running.
        check: &'static str,
        /// The command that failed.
        command: String,
        /// Stderr from the command.
        stderr: String,
    },
}

impl fmt::Display for EscapeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SiblingWorkOrphaned { workspace, blob } => write!(
                f,
                "SiblingRefFaithfulness (bn-rah2): live workspace '{workspace}' committed \
                 blob {blob} that is no longer reachable from any ref — its work was \
                 orphaned by an op that did not target it (FF-absorb sibling-reset class)"
            ),
            Self::TrunkDirtyLost { path } => write!(
                f,
                "TrunkDirtyPreservation (bn-1xmk): uncommitted trunk bytes at '{path}' were \
                 lost — the file no longer holds them and no recovery ref surfaces them"
            ),
            Self::TrunkDirtyDisplaced {
                path,
                in_recovery_ref,
                after_crash,
                claimed_on_disk,
            } => write!(
                f,
                "TrunkDirtyDisplacement (bn-2zubk): uncommitted trunk content at '{path}' \
                 left the default worktree {when} and {told}{where_}",
                when = if *after_crash {
                    "across a crash and the recovering merge"
                } else {
                    "during the op"
                },
                told = if *claimed_on_disk {
                    "the op output FALSELY claimed the user's version is back on disk"
                } else {
                    "the user was never told (no conflict / recovery command naming it \
                     in the op output)"
                },
                where_ = if *in_recovery_ref {
                    " — it survives only in a recovery ref"
                } else {
                    ""
                },
            ),
            Self::RecordClaimsMissingRef {
                workspace,
                record,
                claimed_ref,
            } => write!(
                f,
                "RecordRefCoherence (bn-3uou): destroy record {workspace}/{record} claims \
                 recovery ref '{claimed_ref}' but it does not exist"
            ),
            Self::GitError {
                check,
                command,
                stderr,
            } => write!(
                f,
                "OracleEscape {check}: git error running `{command}`: {stderr}"
            ),
        }
    }
}

impl std::error::Error for EscapeViolation {}

// ---------------------------------------------------------------------------
// SiblingRefFaithfulness (bn-rah2) — stateful, incremental
// ---------------------------------------------------------------------------

/// Tracks each live workspace's committed content (blob OID set) across steps
/// and, after every op, asserts that content stays reachable from the union of
/// all refs — unless the op legitimately moved that workspace.
///
/// The design deliberately tracks **blobs**, not commit OIDs: a legitimate
/// rebase/replay (including the bn-rah2 fix's sibling replay) preserves the
/// blobs in a new commit, so blob-reachability stays green; only an orphaning
/// reset (which leaves the blobs unreferenced) trips it. The op-targeting skip
/// lets a workspace's own commit legitimately drop content (e.g. deleting a
/// file) without a false positive.
///
/// # Conflict-as-data carveout (bn-3g6o, extended here by bn-286g)
///
/// Blob reachability alone is NOT the whole preservation story. When the
/// post-merge sibling auto-rebase replays a sibling whose committed path also
/// changed in the epoch range, maw produces a **diff3 conflict-marker blob**:
/// the sibling's original blob OID stops being tree-reachable, but its bytes
/// survive VERBATIM inside the marker blob and its OID is pinned in the
/// sibling's conflict sidecars. That is maw's first-class conflict-as-data
/// state — recoverable via `maw ws resolve` / `maw ws recover` — not work
/// loss. Oracle A has carried this carveout since bn-3g6o; this oracle did
/// not, so it false-positived on every conflicting sibling replay (bn-286g,
/// found by `DST_TRACES=48 DST_STEPS=48`, invisible at the 16x24 default).
/// The same two rescue tests are applied here, in the same order and via the
/// SAME shared helpers so the two oracles cannot drift:
///
///   (a) the blob OID is recorded as a conflict side in a **live** workspace's
///       `rebase-conflicts.json` / `conflict-tree.json` sidecar; or
///   (b) the blob's raw bytes appear verbatim inside some reachable blob that
///       is ITSELF a conflict-marker blob (covers arbitrarily-deep nested
///       marker wrapping, where the sidecar only names the immediately-prior
///       side OIDs).
///
/// A blob in NO reachable tree, NO live sidecar and NO reachable marker blob
/// is genuinely orphaned and still fires.
#[derive(Debug, Default, Clone)]
pub struct SiblingRefFaithfulness {
    /// `ws name -> set of blob OIDs committed by that workspace (as of the last
    /// step it was observed live)`.
    committed_blobs: BTreeMap<String, BTreeSet<String>>,
}

impl SiblingRefFaithfulness {
    /// Fresh tracker with no recorded state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check the invariant after `op` executed against `repo_root`, then
    /// refresh the tracked per-workspace committed-blob sets from the current
    /// live workspace **worktree HEADs**. Call once per step, in order.
    ///
    /// The committed tip of a maw workspace lives at its **worktree HEAD** (a
    /// plain `git commit` in the workspace advances only the detached worktree
    /// HEAD; maw does not maintain a `refs/manifold/ws/<ws>` state ref for
    /// non-default workspaces). So this oracle enumerates worktrees via
    /// `git worktree list` and reads each one's HEAD.
    ///
    /// The reachability roots are exactly Oracle A's frontier — every extant
    /// workspace's current worktree HEAD (its "current HEAD"), plus every
    /// commit-typed manifold/branch ref (epoch, `refs/heads/*`, recovery, the
    /// per-workspace epoch/state refs). A legitimate rebase/replay (whose NEW
    /// worktree HEAD still contains the blobs) stays green; an orphaning reset
    /// (which moves the workspace's HEAD to a commit that drops the blobs,
    /// leaving them reachable from no root) turns red — the bn-rah2
    /// sibling-orphan signature. Orphaned objects are excluded from `rev-list`
    /// even before `git gc` prunes them, so the reset is detected immediately.
    ///
    /// bn-286g: a blob missing from the reachable set is then run through the
    /// bn-3g6o conflict-as-data rescue tests (sidecar OID pin, then containment
    /// in a reachable conflict-marker blob) before being reported — see the
    /// type-level docs. Both tests are computed lazily, so a clean step pays
    /// nothing extra.
    pub fn check_step(&mut self, repo_root: &Path, op: &Op) -> Vec<EscapeViolation> {
        let worktrees = list_worktrees(repo_root);
        let roots = frontier_roots(repo_root, &worktrees);
        let reachable = match rev_list_objects(repo_root, &roots) {
            Ok(s) => s,
            Err(v) => return vec![v],
        };
        let targeted = op_targets(op);

        // bn-286g: the conflict-as-data rescue sets, computed LAZILY and at
        // most once per step — only when some tracked blob is missing from the
        // reachable set. The green fast path pays nothing.
        let mut sidecar_blobs: Option<std::collections::HashSet<String>> = None;
        let mut marker_blobs: Option<Vec<Vec<u8>>> = None;

        let mut violations = Vec::new();
        for (ws, blobs) in &self.committed_blobs {
            if !worktrees.contains_key(ws) {
                // Destroyed / removed workspace — its lifecycle is covered by
                // Oracle A / RecordRefCoherence, not this oracle.
                continue;
            }
            if targeted.contains(ws) {
                // The op legitimately moved this workspace (commit/advance/
                // sync/merge-source/recover) — a content change here is not an
                // orphaning side effect.
                continue;
            }
            for blob in blobs {
                if reachable.contains(blob) {
                    continue;
                }
                // (a) bn-3g6o sidecar OID match — the blob is pinned as a
                //     conflict side by a LIVE workspace, so `maw ws resolve` /
                //     `maw ws recover` can still reach it. Only live
                //     workspaces count: a stale sidecar left by a destroyed
                //     workspace must never rescue.
                let pinned = sidecar_blobs.get_or_insert_with(|| {
                    crate::oracle_a::conflict_sidecar_blobs_for(
                        repo_root,
                        worktrees.keys().map(String::as_str),
                    )
                });
                if pinned.contains(blob) {
                    continue;
                }
                // (b) bn-3g6o content containment — the bytes survive verbatim
                //     inside a reachable CONFLICT-MARKER blob (possibly nested
                //     several rebases deep, where no sidecar names the original
                //     OID any more). Bounded to marker blobs so a coincidental
                //     substring match against an ordinary file cannot mask a
                //     genuine loss.
                if let Some(bytes) = crate::oracle_a::read_blob_bytes(repo_root, blob) {
                    let markers = marker_blobs.get_or_insert_with(|| {
                        crate::oracle_a::reachable_marker_blob_bytes(
                            repo_root,
                            reachable.iter().map(String::as_str),
                        )
                    });
                    if markers
                        .iter()
                        .any(|m| crate::oracle_a::contains_subslice(m, &bytes))
                    {
                        continue;
                    }
                }
                violations.push(EscapeViolation::SiblingWorkOrphaned {
                    workspace: ws.clone(),
                    blob: blob.clone(),
                });
            }
        }

        // Refresh: record each live workspace's current worktree-HEAD blob set.
        let mut next: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (ws, head) in &worktrees {
            let blobs = blobs_of_ref(repo_root, head);
            if !blobs.is_empty() {
                next.insert(ws.clone(), blobs);
            }
        }
        self.committed_blobs = next;

        violations
    }
}

// ---------------------------------------------------------------------------
// TrunkDirtyPreservation (bn-1xmk) — stateful
// ---------------------------------------------------------------------------

/// Records uncommitted trunk (default-workspace) writes and asserts, after any
/// op, that the recorded bytes survive — either verbatim on disk in the default
/// worktree, or surfaced as a blob reachable from a recovery ref.
#[derive(Debug, Default, Clone)]
pub struct TrunkDirtyPreservation {
    /// `path -> expected uncommitted content`. Most-recent write per path wins.
    pending: BTreeMap<String, String>,
}

impl TrunkDirtyPreservation {
    /// Fresh tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an uncommitted trunk write the harness just performed (the bytes
    /// it wrote into the default workspace's working tree at `path`).
    pub fn record_dirty(&mut self, path: &str, content: &str) {
        self.pending.insert(path.to_owned(), content.to_owned());
    }

    /// Clear the expectation for `paths` — call when the default workspace
    /// legitimately (re)commits or overwrites those paths, so the oracle stops
    /// expecting the superseded dirty bytes.
    pub fn note_trunk_overwrite<I, S>(&mut self, paths: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for p in paths {
            self.pending.remove(p.as_ref());
        }
    }

    /// Verify every recorded uncommitted trunk write still survives.
    #[must_use]
    pub fn check(&self, repo_root: &Path) -> Vec<EscapeViolation> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let default_ws = LayoutFlavor::detect(repo_root).default_target_path(repo_root, "default");
        // The set of blob OIDs reachable from recovery refs (surfaced content).
        let recovery_blobs = recovery_reachable_blobs(repo_root, None);

        let mut violations = Vec::new();
        for (path, content) in &self.pending {
            // 1) Bytes survive verbatim on disk in the default worktree.
            let on_disk = std::fs::read_to_string(default_ws.join(path)).ok();
            if on_disk.as_deref() == Some(content.as_str()) {
                continue;
            }
            // 1b) Or the bytes survive VERBATIM as one side of a diff3
            //     conflict in the default worktree file (bn-m7kjy). A merge
            //     whose epoch changed a path the user had dirty on trunk
            //     writes `<<<<<<< ws … ======= <dirty bytes> >>>>>>> default`
            //     into the file (maw's conflict-as-data model); `maw ws
            //     resolve default --keep default` restores them. Bounded to
            //     files that actually carry BOTH marker kinds — the same
            //     rescue Oracle A (bn-3g6o) and SiblingRefFaithfulness
            //     (bn-286g) apply — so a coincidental substring in an
            //     ordinary file never masks a loss.
            if on_disk.as_deref().is_some_and(|disk| {
                let d = disk.as_bytes();
                crate::oracle_a::is_conflict_marker_blob(d)
                    && crate::oracle_a::contains_subslice(d, content.as_bytes())
            }) {
                continue;
            }
            // 2) Or the content is surfaced as a blob reachable from a recovery
            //    ref (the "explicitly surfaced in a recovery ref" escape hatch).
            if hash_blob(repo_root, content).is_some_and(|oid| recovery_blobs.contains(&oid)) {
                continue;
            }
            violations.push(EscapeViolation::TrunkDirtyLost { path: path.clone() });
        }
        violations
    }
}

// ---------------------------------------------------------------------------
// TrunkDirtyDisplacement (bn-2zubk) — stateful, strict
// ---------------------------------------------------------------------------

/// The uncommitted trunk entry the harness expects back on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DirtyEntry {
    /// A regular file with exactly these bytes.
    File(String),
    /// A symlink with exactly this target (bn-3jqfk: a retargeted tracked
    /// link must come back AS a link, to the user's target).
    Symlink(String),
}

/// Output markers that report the WHOLE pre-merge snapshot as displaced (no
/// per-path list follows), so they acknowledge every tracked path.
const WHOLE_SNAPSHOT_NOTICES: &[&str] = &[
    // Step-4 replay hard failure: "WARNING: replay_snapshot failed: ...",
    // followed by the snapshot ref + a `stash apply` command.
    "replay_snapshot failed",
    // bn-15fzo: a stale checkout intent for a different commit — "left its
    // pre-merge edits pinned at <ref>".
    "pre-merge edits pinned at",
];

/// bn-15fzo/bn-1bkr0 resume: edits made in the default worktree AFTER the
/// crash (or a partial replay) are pinned and cleaned before the resumed
/// replay — "held changes beyond the interrupted update ... They are pinned
/// at <ref>; the pre-merge edits are replayed from <source>." Like the
/// stale-intent notice it names the ref for the whole set, not per path
/// (bn-1h9ue triage F1; per-path restore commands filed as a UX follow-up).
///
/// It is NOT a whole-snapshot notice (bn-3adck): the same message promises
/// the pre-merge edits are REPLAYED. So it acknowledges only entries the
/// crash did not displace — edits made since the crash — never a deferred
/// pre-merge entry, which the resumed replay must put back (or report per
/// path). Treating it as whole-snapshot let a resume that silently dropped
/// pre-merge edits pass whenever the user had also edited after the crash.
const RESIDUAL_NOTICE: &str = "held changes beyond the interrupted update";

/// Phrases by which maw tells the user their version of a path is back on
/// disk (the bn-1xmk replay-divergence notice: "Your version was restored from
/// the in-memory pre-merge snapshot" / "your version is already on disk").
/// A paragraph naming the path with one of these is a claim, not a
/// displacement report: if the recorded content is NOT on disk, the user was
/// told something false (bn-3jqfk's regression printed exactly this for a
/// symlink whose retarget it had lost).
const ON_DISK_CLAIMS: &[&str] = &["already on disk", "was restored"];

/// Output markers that give the user a way back to a displaced path. A path
/// counts as reported only when some output line names it AND one of these
/// handles is present — a bare mention (e.g. a progress line) is not a report.
/// Deliberately excludes "recovery snapshot pinned", which maw prints on EVERY
/// dirty-trunk merge ("preserving N uncommitted trunk file(s) across merge
/// (recovery snapshot pinned)") whether or not anything is later displaced.
const RECOVERY_HANDLES: &[&str] = &[
    "maw ws resolve",
    "maw ws recover",
    "stash apply",
    "Snapshot preserved at",
    " show ",
];

/// **Strict** dirty-trunk oracle (bn-2zubk): every recorded uncommitted trunk
/// entry must stay in the default worktree unless the op that moved it
/// REPORTED the move.
///
/// [`TrunkDirtyPreservation`] accepts bytes that survive only in a recovery
/// ref. That is the Prime Invariant (nothing lost), but it cannot tell a
/// correct merge from one that silently parked the user's edits in a ref and
/// left the worktree without them — exactly the bn-15fzo (crash between the
/// target checkout and the replay; resume ignored the checkout intent) and
/// bn-3jqfk (snapshot capture followed a symlink) regressions, on which every
/// other oracle stayed green. A displacement is legitimate only when the user
/// was told: a conflict report naming the path (`[content] path`, a type
/// conflict with a `maw ws recover --restore-file` / `git show` command), or a
/// whole-snapshot notice ([`WHOLE_SNAPSHOT_NOTICES`]).
///
/// # Crash deferral
///
/// A crashed op (killed mid-flight) cannot report anything, and a crash inside
/// the target update legitimately leaves the tree mid-way (bn-15fzo: merged
/// tree on disk, user edits only in the pinned snapshot). So a displacement
/// observed after a crashed op is DEFERRED, not judged: it must be undone —
/// back on disk — or reported by the time the next non-crashed `Merge` (the
/// op that runs journal recovery) finishes. Other ops in between neither
/// settle nor flag it.
///
/// Each path is judged once: after a violation or an acknowledged report the
/// path stops being tracked (a later `record_dirty` re-arms it), so one
/// displacement is one finding, not one per remaining step.
#[derive(Debug, Default, Clone)]
pub struct TrunkDirtyDisplacement {
    /// `path -> expected on-disk entry`. Most-recent write per path wins.
    pending: BTreeMap<String, DirtyEntry>,
    /// Paths displaced by a crashed op, awaiting the recovering merge.
    deferred: BTreeSet<String>,
    /// Per-path verdicts rendered — still on disk after a completed merge,
    /// reported, or flagged — the non-vacuity counter.
    judged: u64,
    /// Displacements the op output reported (acknowledged, not violations).
    reported: u64,
    /// Displacements deferred across a crash (whatever their outcome).
    deferred_total: u64,
}

impl TrunkDirtyDisplacement {
    /// Fresh tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an uncommitted trunk file write (the bytes the harness just
    /// wrote into the default worktree at `path`).
    pub fn record_dirty(&mut self, path: &str, content: &str) {
        self.deferred.remove(path);
        self.pending
            .insert(path.to_owned(), DirtyEntry::File(content.to_owned()));
    }

    /// Record an uncommitted trunk symlink (the harness just pointed the link
    /// at `path` to `target`).
    pub fn record_dirty_symlink(&mut self, path: &str, target: &str) {
        self.deferred.remove(path);
        self.pending
            .insert(path.to_owned(), DirtyEntry::Symlink(target.to_owned()));
    }

    /// Stop expecting `paths` — the trunk legitimately (re)committed them.
    pub fn note_trunk_overwrite<I, S>(&mut self, paths: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for p in paths {
            self.pending.remove(p.as_ref());
            self.deferred.remove(p.as_ref());
        }
    }

    /// Entries currently expected back on disk (bn-1h9ue: the in-proc
    /// vacuity guard counts merges that ran with this `> 0`).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// `Some(on_disk)` iff `path` is still expected back: whether its
    /// recorded entry is what sits at `abs`. `None` = not tracked.
    #[must_use]
    pub fn expects_on_disk(&self, path: &str, abs: &Path) -> Option<bool> {
        self.pending.get(path).map(|e| entry_on_disk(abs, e))
    }

    /// Per-path verdicts rendered so far (0 = the oracle judged nothing).
    #[must_use]
    pub const fn judged(&self) -> u64 {
        self.judged
    }

    /// Displacements the op output reported (acknowledged).
    #[must_use]
    pub const fn reported(&self) -> u64 {
        self.reported
    }

    /// Displacements deferred across a crashed op.
    #[must_use]
    pub const fn deferred_total(&self) -> u64 {
        self.deferred_total
    }

    /// Judge every tracked entry after `op` ran. `output` is the op's combined
    /// stdout + stderr (empty for non-maw ops); `crashed` is true when the op
    /// was a fault-injected invocation that did not complete.
    pub fn check_step(
        &mut self,
        repo_root: &Path,
        op: &Op,
        output: &str,
        crashed: bool,
    ) -> Vec<EscapeViolation> {
        self.check_after(repo_root, matches!(op, Op::Merge { .. }), output, crashed)
    }

    /// [`Self::check_step`] with the op class given directly: `merge` = the op
    /// ran a merge's target update (or its recovery) — the op class that can
    /// displace an entry, restore a deferred one, and so render a verdict on
    /// one still on disk. A driver whose modelled merge was a no-op (no
    /// source tip: nothing ran) passes `false` (bn-3adck: counting those as
    /// verdicts inflated `judged`, the non-vacuity counter).
    pub fn check_after(
        &mut self,
        repo_root: &Path,
        merge: bool,
        output: &str,
        crashed: bool,
    ) -> Vec<EscapeViolation> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let default_ws = LayoutFlavor::detect(repo_root).default_target_path(repo_root, "default");
        let recovery_op = merge && !crashed;
        let mut recovery_blobs: Option<BTreeSet<String>> = None;

        let mut violations = Vec::new();
        let mut settled = Vec::new();
        for (path, expected) in &self.pending {
            let was_deferred = self.deferred.contains(path);
            if entry_on_disk(&default_ws.join(path), expected) {
                if was_deferred && recovery_op {
                    // Restored by the recovering merge.
                    self.deferred.remove(path);
                }
                if recovery_op {
                    // Survived a completed merge — the op class that can
                    // displace it — so this is a real verdict.
                    self.judged += 1;
                }
                continue;
            }
            let report = classify_report(output, path, !was_deferred);
            if report == Report::Displaced {
                self.reported += 1;
                self.judged += 1;
                settled.push(path.clone());
                continue;
            }
            if crashed && report == Report::Silent {
                if self.deferred.insert(path.clone()) {
                    self.deferred_total += 1;
                }
                continue;
            }
            if was_deferred && !recovery_op {
                // Still awaiting the recovering merge.
                continue;
            }
            let in_recovery_ref = match expected {
                DirtyEntry::File(content) => {
                    let blobs = recovery_blobs
                        .get_or_insert_with(|| recovery_reachable_blobs(repo_root, None));
                    hash_blob(repo_root, content).is_some_and(|oid| blobs.contains(&oid))
                }
                DirtyEntry::Symlink(target) => {
                    let blobs = recovery_blobs
                        .get_or_insert_with(|| recovery_reachable_blobs(repo_root, None));
                    hash_blob(repo_root, target).is_some_and(|oid| blobs.contains(&oid))
                }
            };
            self.judged += 1;
            settled.push(path.clone());
            violations.push(EscapeViolation::TrunkDirtyDisplaced {
                path: path.clone(),
                in_recovery_ref,
                after_crash: was_deferred,
                claimed_on_disk: report == Report::ClaimedOnDisk,
            });
        }
        for path in settled {
            self.pending.remove(&path);
            self.deferred.remove(&path);
        }
        violations
    }
}

/// Whether `expected` is what sits at `abs` in the default worktree.
fn entry_on_disk(abs: &Path, expected: &DirtyEntry) -> bool {
    match expected {
        DirtyEntry::File(content) => {
            std::fs::symlink_metadata(abs).is_ok_and(|m| m.file_type().is_file())
                && std::fs::read_to_string(abs).ok().as_deref() == Some(content.as_str())
        }
        DirtyEntry::Symlink(target) => {
            std::fs::symlink_metadata(abs).is_ok_and(|m| m.file_type().is_symlink())
                && std::fs::read_link(abs).is_ok_and(|t| t == Path::new(target))
        }
    }
}

/// Whether `line` names `path` as a whitespace-separated token (tolerating
/// quotes and trailing punctuation: `'link'.`, `"a b"`, `path:`).
fn line_names_path(line: &str, path: &str) -> bool {
    line.split_whitespace()
        .any(|tok| tok.trim_matches(|c| matches!(c, '\'' | '"' | ':' | ',' | '.')) == path)
}

/// Whether some output paragraph (run of non-blank lines) that names `path`
/// CLAIMS the user's version is back on disk ([`ON_DISK_CLAIMS`]).
fn claims_on_disk(output: &str, path: &str) -> bool {
    let mut para: Vec<&str> = Vec::new();
    let judge = |para: &[&str]| {
        para.iter().any(|l| line_names_path(l, path))
            && para
                .iter()
                .any(|l| ON_DISK_CLAIMS.iter().any(|c| l.contains(c)))
    };
    for line in output.lines() {
        if line.trim().is_empty() {
            if judge(&para) {
                return true;
            }
            para.clear();
        } else {
            para.push(line);
        }
    }
    judge(&para)
}

/// How the op output accounted for a displaced `path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Report {
    /// Not mentioned with any way back.
    Silent,
    /// Reported as moved, with a way back (acknowledged).
    Displaced,
    /// Claimed to be back on disk — while it is not (a false report).
    ClaimedOnDisk,
}

/// Whether `output` carries a whole-snapshot displacement notice
/// ([`WHOLE_SNAPSHOT_NOTICES`]) — shared with the in-proc replay model
/// (bn-1h9ue, `crate::trunk::judge_replay`).
pub(crate) fn has_whole_snapshot_notice(output: &str) -> bool {
    WHOLE_SNAPSHOT_NOTICES.iter().any(|m| output.contains(m))
}

/// [`classify_report`] for the in-proc replay model (bn-1h9ue): one
/// definition of "the output reported `path` with a way back".
///
/// The replay model judges the user's PRE-merge entries, which
/// [`RESIDUAL_NOTICE`] never acknowledges (bn-3adck).
pub(crate) fn report_for(output: &str, path: &str) -> Report {
    classify_report(output, path, false)
}

/// How `output` accounts for `path`, which is NOT on disk as recorded (see
/// [`WHOLE_SNAPSHOT_NOTICES`] / [`RECOVERY_HANDLES`] / [`ON_DISK_CLAIMS`]).
/// `residual_ok`: whether [`RESIDUAL_NOTICE`] may acknowledge `path` (only an
/// entry recorded after the crash the resumed update is recovering from).
fn classify_report(output: &str, path: &str, residual_ok: bool) -> Report {
    if output.is_empty() {
        return Report::Silent;
    }
    // A claim that the content is restored outranks any recovery command in
    // the same message: the user is told there is nothing to do.
    if claims_on_disk(output, path) {
        return Report::ClaimedOnDisk;
    }
    if WHOLE_SNAPSHOT_NOTICES.iter().any(|m| output.contains(m))
        || (residual_ok && output.contains(RESIDUAL_NOTICE))
    {
        return Report::Displaced;
    }
    if report_sections(output).any(|section| {
        section.iter().any(|line| line_names_path(line, path))
            && section
                .iter()
                .any(|line| RECOVERY_HANDLES.iter().any(|h| line.contains(h)))
    }) {
        Report::Displaced
    } else {
        Report::Silent
    }
}

/// bn-36chi: the output split into report sections. Every per-path report
/// the target update prints (type conflicts, local-vs-merge conflicts, the
/// stash-replay conflict list, the snapshot-failed "could not be replayed"
/// list) opens with its own `WARNING:` line and carries its recovery handles
/// inside the same section, so "reported" = some ONE section names the path
/// AND carries a handle. Before, any line naming the path anywhere plus any
/// handle anywhere counted, so a progress line naming a displaced path next
/// to an unrelated conflict's `maw ws resolve` acknowledged it.
fn report_sections(output: &str) -> impl Iterator<Item = Vec<&str>> {
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in output.lines() {
        if line.trim_start().starts_with("WARNING:")
            && let Some(last) = sections.last()
            && !last.is_empty()
        {
            sections.push(Vec::new());
        }
        if let Some(last) = sections.last_mut() {
            last.push(line);
        }
    }
    sections.into_iter()
}

// ---------------------------------------------------------------------------
// RecordRefCoherence (bn-3uou) — stateless
// ---------------------------------------------------------------------------

/// Assert no destroy record claims a recovery ref that does not exist.
///
/// Reads the destroy-record JSON artifacts directly off disk (independent
/// verifier: it does not call `maw-cli`'s writer) and checks each record's
/// claimed recovery ref (`snapshot_ref`, else `final_head_ref`) against the
/// live ref set. Every `maw gc` must leave records ↔ refs coherent.
#[must_use]
pub fn check_record_ref_coherence(repo_root: &Path) -> Vec<EscapeViolation> {
    let refs = match all_ref_names(repo_root) {
        Ok(r) => r,
        Err(v) => return vec![v],
    };
    let mut violations = Vec::new();
    for claim in destroy_record_claims(repo_root) {
        if !refs.contains(&claim.claimed_ref) {
            violations.push(EscapeViolation::RecordClaimsMissingRef {
                workspace: claim.workspace,
                record: claim.record,
                claimed_ref: claim.claimed_ref,
            });
        }
    }
    violations
}

/// One destroy record's claim on a recovery ref, read straight off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestroyRecordClaim {
    /// The destroyed workspace's name (the `artifacts/ws/<name>` dir).
    pub workspace: String,
    /// The record's file name (`<timestamp>.json`).
    pub record: String,
    /// The recovery ref it claims (`snapshot_ref`, else `final_head_ref`).
    pub claimed_ref: String,
}

/// Every destroy record that claims a recovery ref, in deterministic order.
///
/// Records live where `maw ws destroy` writes them:
/// `<manifold_dir>/artifacts/ws/<ws>/destroy/<timestamp>.json` (the
/// `latest.json` pointer is skipped — it duplicates a timestamped record).
/// Parsed as plain JSON (independent verifier; no `maw-cli` types).
///
/// bn-m7kjy: `check_record_ref_coherence` used to read
/// `<manifold_dir>/destroy/<ws>/`, a directory no production writer uses, so
/// the bn-3uou oracle was vacuously green in every DST tier.
#[must_use]
pub fn destroy_record_claims(repo_root: &Path) -> Vec<DestroyRecordClaim> {
    let ws_root = LayoutFlavor::detect(repo_root)
        .manifold_dir(repo_root)
        .join("artifacts")
        .join("ws");
    let Ok(ws_dirs) = std::fs::read_dir(&ws_root) else {
        return Vec::new(); // No destroy records at all.
    };
    let mut ws_names: Vec<String> = ws_dirs
        .flatten()
        .filter(|e| e.path().join("destroy").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ws_names.sort();

    let mut claims = Vec::new();
    for ws in ws_names {
        let dir = ws_root.join(&ws).join("destroy");
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut record_files: Vec<String> = files
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json") && n != "latest.json")
            .collect();
        record_files.sort();

        for record in record_files {
            let Ok(body) = std::fs::read_to_string(dir.join(&record)) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            // Mirror `DestroyRecord::recovery_ref`: snapshot_ref, else
            // final_head_ref.
            let claimed = json
                .get("snapshot_ref")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    json.get("final_head_ref")
                        .and_then(serde_json::Value::as_str)
                });
            if let Some(claimed_ref) = claimed {
                claims.push(DestroyRecordClaim {
                    workspace: ws.clone(),
                    record: record.clone(),
                    claimed_ref: claimed_ref.to_owned(),
                });
            }
        }
    }
    claims
}

// ---------------------------------------------------------------------------
// Recovery-snapshot sweep eligibility (bn-m7kjy) — for Oracle A's gc release
// ---------------------------------------------------------------------------

/// Every `refs/manifold/recovery/*` ref as `name -> OID`.
#[must_use]
pub fn recovery_refs(repo_root: &Path) -> BTreeMap<String, String> {
    let out = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/manifold/recovery/",
        ])
        .current_dir(repo_root)
        .output();
    let Ok(out) = out else {
        return BTreeMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(r, o)| (r.to_owned(), o.to_owned()))
        .collect()
}

/// The recovery refs a `maw gc --recovery-snapshots --older-than <days>` run
/// starting at `now_secs` is ENTITLED to delete, judged independently of
/// `ref_gc.rs` from maw's documented semantics (`maw gc --help`): a
/// *recovery snapshot* is "the pinned commit holding a DESTROYED workspace's
/// content", swept "in lockstep" with the destroy record that points at it,
/// once the pin is older than the threshold. So a ref is eligible iff ALL of:
///
/// 1. it is `refs/manifold/recovery/<ws>/<leaf>`;
/// 2. `<ws>` is NOT an extant workspace (no workspace directory on disk);
/// 3. a destroy record of `<ws>` claims exactly this ref;
/// 4. the pin-creation timestamp embedded in `<leaf>` (every production
///    writer embeds one) is `<= now_secs - days*86400`.
///
/// Anything else — a pin of a live workspace (dirty-trunk
/// `recovery/default/*`, bn-154g `materialize-*`), an unclaimed pin, a pin
/// too young, a pin whose name carries no timestamp — is NOT eligible, so if
/// gc deletes the only copy of witnessed content through such a ref, Oracle A
/// still fires.
#[must_use]
pub fn gc_eligible_recovery_snapshots(
    repo_root: &Path,
    older_than_days: u64,
    now_secs: u64,
) -> BTreeMap<String, String> {
    let cutoff = now_secs.saturating_sub(older_than_days.saturating_mul(86_400));
    let live: BTreeSet<String> = crate::workspace_dirs(repo_root)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let claimed: BTreeSet<String> = destroy_record_claims(repo_root)
        .into_iter()
        .map(|c| c.claimed_ref)
        .collect();
    recovery_refs(repo_root)
        .into_iter()
        .filter(|(name, _)| {
            let Some(rest) = name.strip_prefix("refs/manifold/recovery/") else {
                return false;
            };
            let Some((ws, leaf)) = rest.rsplit_once('/') else {
                return false;
            };
            // bn-wxg28 / bn-1h9ue: liveness through the layout-aware
            // `workspace_dirs` — the default workspace is the repo root on the
            // consolidated layout (no `.maw/workspaces/default/`).
            !ws.is_empty()
                && !live.contains(ws)
                && claimed.contains(name)
                && pin_timestamp_from_leaf(leaf).is_some_and(|ts| ts <= cutoff)
        })
        .collect()
}

/// Unix seconds of the `YYYY-MM-DDTHH-MM-SS[.frac]Z` timestamp in a recovery
/// ref leaf (optionally after a `<kind>-` prefix). Independent re-derivation
/// (not `ref_gc::pin_created_at_from_ref_name`).
fn pin_timestamp_from_leaf(leaf: &str) -> Option<u64> {
    let b = leaf.as_bytes();
    (0..b.len())
        .filter(|&i| i == 0 || b[i - 1] == b'-')
        .find_map(|i| parse_ref_timestamp(&b[i..]))
}

fn parse_ref_timestamp(bytes: &[u8]) -> Option<u64> {
    let b = bytes;
    // YYYY-MM-DDTHH-MM-SS then Z or .digits Z
    if b.len() < 20 {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<u64> {
        let s = std::str::from_utf8(&b[r]).ok()?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b'-' || b[16] != b'-' {
        return None;
    }
    let tail = &b[19..];
    let tail_ok = tail == b"Z"
        || (tail.len() > 2
            && tail[0] == b'.'
            && tail[tail.len() - 1] == b'Z'
            && tail[1..tail.len() - 1].iter().all(u8::is_ascii_digit));
    if !tail_ok {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from civil (Howard Hinnant), proleptic Gregorian, UTC.
    let y = i64::try_from(year).ok()? - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::try_from(month).ok()? + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::try_from(day).ok()? - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + i64::try_from(hour * 3600 + minute * 60 + second).ok()?;
    u64::try_from(secs).ok()
}

// ---------------------------------------------------------------------------
// git / model helpers (independent-verifier carveout: CLI, not gix)
// ---------------------------------------------------------------------------

/// Enumerate live worktrees as `basename -> HEAD OID`, via
/// `git worktree list --porcelain`. Bare entries and worktrees with no
/// resolved HEAD are skipped. The basename is the workspace name maw uses.
fn list_worktrees(repo_root: &Path) -> BTreeMap<String, String> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output();
    let Ok(out) = out else {
        return BTreeMap::new();
    };
    if !out.status.success() {
        return BTreeMap::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = BTreeMap::new();
    let mut cur_name: Option<String> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            cur_name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
        } else if let Some(oid) = line.strip_prefix("HEAD ") {
            if let Some(name) = cur_name.take() {
                map.insert(name, oid.to_owned());
            }
        } else if line == "bare" {
            cur_name = None;
        }
    }
    map
}

/// The reachability roots: every extant worktree HEAD plus every commit-typed
/// manifold/branch ref (epoch, `refs/heads/*`, recovery, per-ws epoch/state).
/// Mirrors `oracle_a::compute_frontier`. Deliberately excludes the blob-typed
/// `refs/manifold/head/<ws>` oplog refs (passing a blob to `rev-list` errors).
fn frontier_roots(repo_root: &Path, worktrees: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut roots: BTreeSet<String> = worktrees.values().cloned().collect();
    if let Ok(refs) = all_ref_names(repo_root) {
        for r in &refs {
            let is_commit_ref = r == "refs/manifold/epoch/current"
                || r.starts_with("refs/heads/")
                || r.starts_with("refs/manifold/recovery/")
                || r.starts_with("refs/manifold/epoch/ws/")
                || r.starts_with(maw_core::refs::WORKSPACE_STATE_PREFIX);
            if is_commit_ref {
                roots.insert(r.clone());
            }
        }
    }
    roots
}

/// Objects reachable from `roots` (`git rev-list --objects <roots...>`).
/// Orphaned (unreferenced) objects are excluded even before `git gc` prunes
/// them, which is what makes an orphaning reset detectable immediately.
fn rev_list_objects(
    repo_root: &Path,
    roots: &BTreeSet<String>,
) -> Result<BTreeSet<String>, EscapeViolation> {
    if roots.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut args: Vec<&str> = vec!["rev-list", "--objects"];
    args.extend(roots.iter().map(String::as_str));
    let out = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output()
        .map_err(|e| EscapeViolation::GitError {
            check: "SiblingRefFaithfulness",
            command: "git rev-list --objects".to_owned(),
            stderr: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(EscapeViolation::GitError {
            check: "SiblingRefFaithfulness",
            command: format!("git rev-list --objects [{} roots]", roots.len()),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect())
}

/// Every ref name in the repo.
fn all_ref_names(repo_root: &Path) -> Result<BTreeSet<String>, EscapeViolation> {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| EscapeViolation::GitError {
            check: "RecordRefCoherence",
            command: "git for-each-ref".to_owned(),
            stderr: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(EscapeViolation::GitError {
            check: "RecordRefCoherence",
            command: "git for-each-ref".to_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// Blob OIDs in the tree of `ref_name` (recursive). Empty if the ref does not
/// resolve or points at no commit.
fn blobs_of_ref(repo_root: &Path, ref_name: &str) -> BTreeSet<String> {
    let out = Command::new("git")
        .args(["ls-tree", "-r", ref_name])
        .current_dir(repo_root)
        .output();
    let Ok(out) = out else {
        return BTreeSet::new();
    };
    if !out.status.success() {
        return BTreeSet::new();
    }
    // Line format: "<mode> <type> <oid>\t<path>". Keep blobs only.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (meta, _path) = l.split_once('\t')?;
            let mut parts = meta.split_whitespace();
            let _mode = parts.next()?;
            let kind = parts.next()?;
            let oid = parts.next()?;
            (kind == "blob").then(|| oid.to_owned())
        })
        .collect()
}

/// The set of blob OIDs reachable from any `refs/manifold/recovery/*` ref.
///
/// `scope` narrows the ref namespace: `None` means every recovery ref in the
/// repo, `Some("alice")` means only `refs/manifold/recovery/alice/*`. bn-22jy's
/// [`crate::oracle_worktree::MaskedStalePreservation`] uses the narrow form so
/// it asserts the bn-154g observable exactly — the doomed bytes must be pinned
/// under the workspace maw was about to overwrite, not merely present in some
/// unrelated snapshot.
pub(crate) fn recovery_reachable_blobs(repo_root: &Path, scope: Option<&str>) -> BTreeSet<String> {
    let prefix = scope.map_or_else(
        || "refs/manifold/recovery/".to_owned(),
        |ws| format!("refs/manifold/recovery/{ws}/"),
    );
    // Names of every recovery ref, then their reachable blobs.
    let Ok(refs) = all_ref_names(repo_root) else {
        return BTreeSet::new();
    };
    let mut blobs = BTreeSet::new();
    for r in refs.iter().filter(|r| r.starts_with(&prefix)) {
        blobs.extend(blobs_of_ref(repo_root, r));
    }
    blobs
}

/// Compute the git blob OID for `content` WITHOUT writing it (`git hash-object
/// --stdin`), so we can test membership in a reachable set.
pub(crate) fn hash_blob(repo_root: &Path, content: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(content.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The set of workspace names an op legitimately mutates (so a content change
/// there is not an orphaning side effect).
fn op_targets(op: &Op) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    match op {
        // bn-22jy: `CorruptWorktreeStatMasked` belongs in this list — it
        // deliberately mutates `ws`'s worktree, so a content change there is
        // expected, not an orphaning side effect.
        Op::WsCreate { ws, .. }
        | Op::EditFiles { ws, .. }
        | Op::Commit { ws, .. }
        | Op::Sync { ws }
        | Op::Advance { ws }
        | Op::CorruptWorktreeStatMasked { ws, .. }
        | Op::Destroy { ws, .. } => {
            set.insert(ws.0.clone());
        }
        Op::Merge { srcs, .. } => {
            for s in srcs {
                set.insert(s.0.clone());
            }
        }
        Op::Recover { ws, to } => {
            set.insert(ws.0.clone());
            set.insert(to.0.clone());
        }
        // Trunk-level ops target no tracked workspace.
        Op::OutOfMawCommit { .. } | Op::DirtyTrunkWrite { .. } | Op::Gc { .. } => {}
    }
    set
}

// ---------------------------------------------------------------------------
// Tests — each plants exactly one escape-class violation (and its green twin).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    use crate::scenario::{Seeded, WsId};

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A V2-layout temp repo (no `.maw/manifold` marker) with one root commit.
    fn setup_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("README.md"), "# test\n").unwrap();
        git(root, &["add", "README.md"]);
        git(root, &["commit", "-q", "-m", "init"]);
        let oid = git(root, &["rev-parse", "HEAD"]);
        (dir, oid)
    }

    /// Build a commit containing a single unique file and return its OID.
    fn commit_unique_file(root: &Path, parent: &str, path: &str, content: &str) -> String {
        let blob = {
            use std::io::Write as _;
            use std::process::Stdio;
            let mut c = Command::new("git")
                .args(["hash-object", "-w", "--stdin"])
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(content.as_bytes())
                .unwrap();
            String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout)
                .trim()
                .to_owned()
        };
        let tree = {
            use std::io::Write as _;
            use std::process::Stdio;
            let mut c = Command::new("git")
                .args(["mktree"])
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            writeln!(c.stdin.as_mut().unwrap(), "100644 blob {blob}\t{path}").unwrap();
            String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout)
                .trim()
                .to_owned()
        };
        git(root, &["commit-tree", &tree, "-p", parent, "-m", "work"])
    }

    fn make_ws_dir(root: &Path, name: &str) {
        fs::create_dir_all(root.join("ws").join(name)).unwrap();
    }

    /// Register a live workspace as a real detached worktree at `ws/<name>`
    /// with HEAD at `commit` (the worktree HEAD is the committed tip the oracle
    /// tracks — matching how maw workspaces actually store their tip).
    fn make_ws(root: &Path, name: &str, commit: &str) {
        let path = root.join("ws").join(name);
        git(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                commit,
            ],
        );
    }

    /// Move a workspace's worktree HEAD — the orphaning / replay primitive.
    fn move_ws_tip(root: &Path, name: &str, commit: &str) {
        let path = root.join("ws").join(name);
        git(&path, &["checkout", "-q", "--detach", commit]);
    }

    fn gc_op() -> Op {
        Op::Gc {
            recovery_snapshots: false,
            older_than_days: 30,
        }
    }

    // ----- SiblingRefFaithfulness (bn-rah2) --------------------------------

    #[test]
    fn sibling_orphaned_by_non_targeting_op_trips() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        // A live sibling with committed-ahead work (unique blob) as a real
        // worktree whose HEAD is at the committed-ahead commit.
        let sib_commit = commit_unique_file(
            root,
            &root_oid,
            "sibling.txt",
            "SIBLING committed-ahead work\n",
        );
        make_ws(root, "sibling", &sib_commit);

        let mut oracle = SiblingRefFaithfulness::new();
        // Step 1: a non-targeting op records the sibling's blobs, no violation
        // (its work is reachable from its own worktree HEAD).
        let v1 = oracle.check_step(root, &gc_op());
        assert!(v1.is_empty(), "step 1 should be clean: {v1:?}");

        // Step 2: ORPHAN the sibling's work — reset its WORKTREE HEAD to the
        // root commit (which does NOT contain sibling.txt). The manifold state
        // ref still pins sib_commit, but the oracle (correctly) does not count
        // state refs as reachable roots. The op does not target 'sibling'.
        move_ws_tip(root, "sibling", &root_oid);
        let v2 = oracle.check_step(root, &gc_op());
        assert!(
            v2.iter().any(|v| matches!(
                v,
                EscapeViolation::SiblingWorkOrphaned { workspace, .. } if workspace == "sibling"
            )),
            "orphaned sibling work must trip SiblingRefFaithfulness: {v2:?}"
        );
    }

    #[test]
    fn sibling_own_commit_does_not_false_positive() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let c1 = commit_unique_file(root, &root_oid, "s.txt", "v1\n");
        make_ws(root, "sibling", &c1);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());

        // The sibling itself moves its HEAD to a new commit that drops the old
        // blob. Because the op TARGETS 'sibling', this legitimate self-move
        // must NOT trip the oracle.
        let c2 = commit_unique_file(root, &c1, "s.txt", "v2 replaces v1\n");
        move_ws_tip(root, "sibling", &c2);
        let op = Op::Commit {
            ws: WsId("sibling".to_owned()),
            msg: Seeded("bump".to_owned()),
        };
        let v = oracle.check_step(root, &op);
        assert!(
            v.is_empty(),
            "a workspace's own commit must not trip the oracle: {v:?}"
        );
    }

    #[test]
    fn sibling_replay_preserving_blobs_stays_green() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let sib = commit_unique_file(root, &root_oid, "keep.txt", "MUST SURVIVE\n");
        make_ws(root, "sibling", &sib);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());

        // Simulate a legitimate replay: move the worktree HEAD to a NEW commit
        // (different OID) that still contains the same blob. The old sib commit
        // is now unreferenced, but its blob survives via the new HEAD — so no
        // violation, even though the op does not target 'sibling'.
        let replayed = commit_unique_file(root, &root_oid, "keep.txt", "MUST SURVIVE\n");
        move_ws_tip(root, "sibling", &replayed);
        let v = oracle.check_step(root, &gc_op());
        assert!(
            v.is_empty(),
            "a replay preserving the blobs must stay green: {v:?}"
        );
    }

    // ----- bn-286g: conflict-as-data carveout (bn-3g6o parity) -------------

    /// The exact diff3 shape maw's rebase writes when a sibling's committed
    /// path also changed in the epoch range: the sibling's ORIGINAL bytes
    /// survive verbatim between the markers under a brand-new blob OID.
    fn marker_wrap(epoch_side: &str, ws_side: &str) -> String {
        format!(
            "<<<<<<< epoch (current)\n{epoch_side}||||||| base\n=======\n{ws_side}>>>>>>> sibling (workspace changes)\n"
        )
    }

    /// Plant a `rebase-conflicts.json` sidecar pinning `theirs` for `ws_name`,
    /// exactly as `maw`'s auto-rebase does (V2 layout: `.manifold/artifacts/`).
    fn write_rebase_conflicts_sidecar(root: &Path, ws_name: &str, path: &str, theirs: &str) {
        let dir = root
            .join(".manifold")
            .join("artifacts")
            .join("ws")
            .join(ws_name);
        fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"conflicts":[{{"path":"{path}","original_commit":"{theirs}","ours":"blob:{theirs}","theirs":"blob:{theirs}"}}],"rebase_from":"{theirs}","rebase_to":"{theirs}"}}"#
        );
        fs::write(dir.join("rebase-conflicts.json"), json).unwrap();
    }

    /// bn-286g REGRESSION (the DST 48x48 failure, seeds 0/15/18): the
    /// post-merge sibling auto-rebase replays a sibling onto the new epoch and
    /// CONFLICTS on a path the sibling committed. The sibling's original blob
    /// OID stops being tree-reachable — it is now wrapped in diff3 markers
    /// under a fresh OID — but the bytes survive verbatim and the OID is pinned
    /// in the sibling's `rebase-conflicts.json`. That is conflict-as-data, not
    /// work loss: the oracle must stay GREEN (Oracle A has done so since
    /// bn-3g6o).
    #[test]
    fn conflict_marker_rewrite_with_sidecar_stays_green_bn_286g() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let ws_side = "sibling private work\n";
        let sib = commit_unique_file(root, &root_oid, "shared.txt", ws_side);
        make_ws(root, "sibling", &sib);

        let mut oracle = SiblingRefFaithfulness::new();
        let v1 = oracle.check_step(root, &gc_op());
        assert!(v1.is_empty(), "step 1 should be clean: {v1:?}");

        // The blob the sibling committed, before the replay rewrites it.
        let original_blob = git(root, &["rev-parse", &format!("{sib}:shared.txt")]);

        // Replay-with-conflict: a NEW commit whose `shared.txt` is the marker
        // blob. The original blob OID is now in no tree.
        let wrapped = marker_wrap("epoch version of shared\n", ws_side);
        let replayed = commit_unique_file(root, &root_oid, "shared.txt", &wrapped);
        move_ws_tip(root, "sibling", &replayed);
        write_rebase_conflicts_sidecar(root, "sibling", "shared.txt", &original_blob);

        let v = oracle.check_step(root, &gc_op());
        assert!(
            v.is_empty(),
            "bn-286g: a blob pinned as a conflict side by a LIVE workspace is \
             preserved (conflict-as-data), not orphaned: {v:?}"
        );
    }

    /// bn-286g / bn-3g6o §(b): after a SECOND auto-rebase the sidecar only
    /// names the immediately-prior (already-wrapped) side OIDs, so the original
    /// blob is in no sidecar at all — but its bytes are still nested inside the
    /// reachable marker blob. Containment must rescue it.
    #[test]
    fn nested_marker_wrapped_blob_without_sidecar_stays_green_bn_286g() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let ws_side = "sibling private work\n";
        let sib = commit_unique_file(root, &root_oid, "shared.txt", ws_side);
        make_ws(root, "sibling", &sib);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());

        // Two levels of marker wrapping, NO sidecar written at all.
        let once = marker_wrap("epoch v1\n", ws_side);
        let twice = marker_wrap("epoch v2\n", &once);
        let replayed = commit_unique_file(root, &root_oid, "shared.txt", &twice);
        move_ws_tip(root, "sibling", &replayed);

        let v = oracle.check_step(root, &gc_op());
        assert!(
            v.is_empty(),
            "bn-286g: bytes nested inside a reachable marker blob are preserved: {v:?}"
        );
    }

    /// NEGATIVE CONTROL for the bn-286g carveout: the same orphaning shape, but
    /// the bytes live in NO sidecar and inside NO conflict-marker blob (the
    /// replacement blob is an ordinary file). The oracle must STILL fire —
    /// the carveout must not become a blanket amnesty.
    #[test]
    fn orphaned_blob_without_sidecar_or_marker_still_trips_bn_286g() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let sib = commit_unique_file(root, &root_oid, "shared.txt", "sibling private work\n");
        make_ws(root, "sibling", &sib);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());

        // Replacement content contains NO conflict markers and no sidecar is
        // planted: the sibling's bytes are genuinely gone.
        let replayed = commit_unique_file(root, &root_oid, "shared.txt", "epoch version only\n");
        move_ws_tip(root, "sibling", &replayed);

        let v = oracle.check_step(root, &gc_op());
        assert!(
            v.iter().any(|x| matches!(
                x,
                EscapeViolation::SiblingWorkOrphaned { workspace, .. } if workspace == "sibling"
            )),
            "bn-286g: genuinely orphaned work must still trip the oracle: {v:?}"
        );
    }

    /// A sidecar left behind by a DESTROYED workspace must not rescue a live
    /// workspace's orphaned blob — only live workspaces' sidecars count.
    #[test]
    fn stale_sidecar_of_destroyed_ws_does_not_rescue_bn_286g() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let sib = commit_unique_file(root, &root_oid, "shared.txt", "sibling private work\n");
        make_ws(root, "sibling", &sib);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());
        let original_blob = git(root, &["rev-parse", &format!("{sib}:shared.txt")]);

        let replayed = commit_unique_file(root, &root_oid, "shared.txt", "epoch version only\n");
        move_ws_tip(root, "sibling", &replayed);
        // The pin exists — but under a workspace that has no worktree.
        write_rebase_conflicts_sidecar(root, "ghost", "shared.txt", &original_blob);

        let v = oracle.check_step(root, &gc_op());
        assert!(
            v.iter().any(|x| matches!(
                x,
                EscapeViolation::SiblingWorkOrphaned { workspace, .. } if workspace == "sibling"
            )),
            "bn-286g: a stale sidecar of a non-existent workspace must not \
             rescue an orphaned blob: {v:?}"
        );
    }

    // ----- TrunkDirtyPreservation (bn-1xmk) --------------------------------

    #[test]
    fn dirty_trunk_lost_trips_and_survival_is_green() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // Default worktree in V2 layout is <root>/ws/default.
        make_ws_dir(root, "default");

        let mut oracle = TrunkDirtyPreservation::new();
        oracle.record_dirty("hot.txt", "UNCOMMITTED dirty bytes\n");

        // Not on disk, not in recovery → LOST.
        let v = oracle.check(root);
        assert!(
            v.iter().any(
                |x| matches!(x, EscapeViolation::TrunkDirtyLost { path } if path == "hot.txt")
            ),
            "missing dirty bytes must trip TrunkDirtyPreservation: {v:?}"
        );

        // Write the bytes verbatim to the default worktree → survives.
        fs::write(root.join("ws/default/hot.txt"), "UNCOMMITTED dirty bytes\n").unwrap();
        assert!(
            oracle.check(root).is_empty(),
            "dirty bytes present on disk must be green"
        );
    }

    #[test]
    fn dirty_trunk_surfaced_in_recovery_ref_is_green() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");

        let mut oracle = TrunkDirtyPreservation::new();
        let content = "dirty bytes surfaced via recovery\n";
        oracle.record_dirty("hot.txt", content);

        // Bytes NOT on disk, but surfaced as a blob reachable from a recovery
        // ref: build a commit containing exactly that blob and pin it under
        // refs/manifold/recovery/.
        let rec = commit_unique_file(root, &root_oid, "hot.txt", content);
        git(
            root,
            &["update-ref", "refs/manifold/recovery/default/snap", &rec],
        );
        let v = oracle.check(root);
        assert!(
            v.is_empty(),
            "dirty bytes surfaced in a recovery ref must be green: {v:?}"
        );
    }

    /// bn-m7kjy (seed 12 of the 48x48 escape-weight-8 run): a merge that
    /// changed a path the user had dirty on trunk leaves the user's bytes as
    /// the `default` side of a diff3 conflict in the worktree file. After an
    /// explicit gc drops the redundant recovery pin, the bytes are still on
    /// disk (conflict-as-data) — preserved, not lost.
    #[test]
    fn dirty_trunk_preserved_as_conflict_side_is_green_bn_m7kjy() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let content = "dirty-trunk\nseed-slot=8780130101953745955\nidx=0\n";
        let mut oracle = TrunkDirtyPreservation::new();
        oracle.record_dirty("shared/file-0.txt", content);
        fs::create_dir_all(root.join("ws/default/shared")).unwrap();
        fs::write(
            root.join("ws/default/shared/file-0.txt"),
            format!(
                "<<<<<<< ws-4 (merged workspace)\nws=ws-4\n||||||| base\n=======\n{content}>>>>>>> default (local edits)\n"
            ),
        )
        .unwrap();
        let v = oracle.check(root);
        assert!(
            v.is_empty(),
            "dirty bytes kept as a conflict side must be green: {v:?}"
        );
    }

    /// Negative controls for the conflict-as-data rescue: markers WITHOUT the
    /// recorded bytes, and the bytes inside a NON-conflict file that merely
    /// contains them, both still trip.
    #[test]
    fn dirty_trunk_conflict_rescue_is_narrow_bn_m7kjy() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let content = "UNCOMMITTED dirty bytes\n";
        let mut oracle = TrunkDirtyPreservation::new();
        oracle.record_dirty("hot.txt", content);
        let file = root.join("ws/default/hot.txt");

        fs::write(
            &file,
            "<<<<<<< ws-a\ntheirs\n||||||| base\n=======\nsomething else\n>>>>>>> default\n",
        )
        .unwrap();
        assert_eq!(
            oracle.check(root).len(),
            1,
            "markers without the bytes must trip"
        );

        fs::write(&file, format!("prefix\n{content}suffix\n")).unwrap();
        assert_eq!(
            oracle.check(root).len(),
            1,
            "bytes embedded in an ordinary (non-conflict) file must still trip"
        );
    }

    // ----- TrunkDirtyDisplacement (bn-2zubk) -------------------------------

    fn merge_op() -> Op {
        Op::Merge {
            srcs: vec![WsId("ws-a".to_owned())],
            into: crate::scenario::Target::Default,
            destroy: false,
        }
    }

    /// Pin `content` at `path` under a recovery ref (the "survives only in a
    /// ref" state both bn-15fzo and bn-3jqfk left behind).
    fn pin_in_recovery_ref(root: &Path, parent: &str, path: &str, content: &str) {
        let rec = commit_unique_file(root, parent, path, content);
        git(
            root,
            &["update-ref", "refs/manifold/recovery/default/snap", &rec],
        );
    }

    /// The bn-15fzo / bn-3jqfk shape: bytes survive only in a recovery ref,
    /// nothing was reported. `TrunkDirtyPreservation` is green on it (that is
    /// the gap); the strict oracle must flag it.
    #[test]
    fn silent_displacement_into_recovery_ref_trips() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let content = "user edit\n";
        pin_in_recovery_ref(root, &root_oid, "hot.txt", content);

        let mut lenient = TrunkDirtyPreservation::new();
        lenient.record_dirty("hot.txt", content);
        assert!(
            lenient.check(root).is_empty(),
            "precondition: lenient oracle is green"
        );

        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty("hot.txt", content);
        let out = "Default workspace updated to new epoch.\n  preserving 1 uncommitted \
                   trunk file(s) across merge (recovery snapshot pinned)\n";
        let v = strict.check_step(root, &merge_op(), out, false);
        assert!(
            matches!(
                v.as_slice(),
                [EscapeViolation::TrunkDirtyDisplaced {
                    path,
                    in_recovery_ref: true,
                    after_crash: false,
                    claimed_on_disk: false,
                }] if path == "hot.txt"
            ),
            "{v:?}"
        );
        // One finding per displacement, not one per remaining step.
        assert!(strict.check_step(root, &gc_op(), "", false).is_empty());
    }

    #[test]
    fn dirty_entry_on_disk_is_green() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        fs::write(root.join("ws/default/hot.txt"), "user edit\n").unwrap();
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty("hot.txt", "user edit\n");
        assert!(strict.check_step(root, &merge_op(), "", false).is_empty());
    }

    /// A conflict report naming the path with a resolve command acknowledges
    /// the displacement; a report that names ANOTHER path does not.
    #[test]
    fn reported_displacement_is_green_unreported_path_is_not() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        fs::write(root.join("ws/default/a.txt"), "<<<<<<< markers\n").unwrap();
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty("a.txt", "user a\n");
        strict.record_dirty("b.txt", "user b\n");
        let out = "  WARNING: 1 file(s) in 'default' have local-vs-merge conflicts.\n\
                   \x20   [             content] a.txt\n\
                   \x20   maw ws resolve default --keep default    # keep local edits\n";
        let v = strict.check_step(root, &merge_op(), out, false);
        assert!(
            matches!(v.as_slice(), [EscapeViolation::TrunkDirtyDisplaced { path, .. }] if path == "b.txt"),
            "{v:?}"
        );
        assert_eq!(strict.reported(), 1);
    }

    /// bn-3jqfk regression shape: the output names the path WITH a recovery
    /// command, but claims the user's version is already back on disk. When it
    /// is not, that is a false report, not an acknowledgement.
    #[cfg(unix)]
    #[test]
    fn false_on_disk_claim_is_not_an_acknowledgement() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let out = "\n  WARNING (bn-1xmk): replay did not reproduce your uncommitted edits to 'link'.\n\
                   \x20 Your version was restored from the in-memory pre-merge snapshot.\n\
                   \x20 Restore:  maw ws recover --ref refs/manifold/recovery/default/x --restore-file link\n\
                   \x20           (your version is already on disk — add --force to overwrite it)\n";
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty_symlink("link", "new-target");
        let v = strict.check_step(root, &merge_op(), out, false);
        assert!(
            matches!(
                v.as_slice(),
                [EscapeViolation::TrunkDirtyDisplaced { path, claimed_on_disk: true, .. }]
                    if path == "link"
            ),
            "{v:?}"
        );
        // The same claim is TRUE when the link is on disk: green.
        std::os::unix::fs::symlink("new-target", root.join("ws/default/link")).unwrap();
        strict.record_dirty_symlink("link", "new-target");
        assert!(strict.check_step(root, &merge_op(), out, false).is_empty());
    }

    /// A crashed op defers; the recovering merge must restore or report. A
    /// non-merge op in between neither settles nor flags.
    #[test]
    fn crash_defers_until_the_recovering_merge() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty("hot.txt", "user edit\n");

        assert!(
            strict.check_step(root, &merge_op(), "", true).is_empty(),
            "crash defers"
        );
        assert!(
            strict.check_step(root, &gc_op(), "", false).is_empty(),
            "gc does not recover"
        );
        let v = strict.check_step(root, &merge_op(), "Recovered an interrupted merge", false);
        assert!(
            matches!(
                v.as_slice(),
                [EscapeViolation::TrunkDirtyDisplaced {
                    after_crash: true,
                    ..
                }]
            ),
            "{v:?}"
        );
        assert_eq!(strict.deferred_total(), 1);

        // Same crash, but the recovering merge puts the bytes back: green.
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty("hot.txt", "user edit\n");
        assert!(strict.check_step(root, &merge_op(), "", true).is_empty());
        fs::write(root.join("ws/default/hot.txt"), "user edit\n").unwrap();
        assert!(strict.check_step(root, &merge_op(), "", false).is_empty());
        assert_eq!(strict.judged(), 1);
    }

    /// bn-3adck: the resume's residual notice ("held changes beyond the
    /// interrupted update ... pinned at <ref>; the pre-merge edits are
    /// replayed from <source>") acknowledges only edits made SINCE the crash.
    /// A pre-merge entry the crash displaced (deferred) must still come back
    /// — the same message promises it is replayed.
    #[test]
    fn residual_notice_acknowledges_only_post_crash_edits() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let residual = "  resuming an interrupted update of 'default' (its pre-merge edits: \
                        refs/manifold/recovery/default/2026-09-28T00-00-00Z)\n  WARNING: \
                        'default' held changes beyond the interrupted update (a partial \
                        replay, or edits made since). They are pinned at \
                        refs/manifold/recovery/default/2026-09-28T00-00-01Z; the pre-merge \
                        edits are replayed from refs/manifold/recovery/default/2026-09-28T00-00-00Z.\n";
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty("pre.txt", "pre-merge edit\n");
        // The crash displaced the pre-merge edit: deferred.
        assert!(strict.check_step(root, &merge_op(), "", true).is_empty());
        // The user edits another path after the crash.
        strict.record_dirty("post.txt", "post-crash edit\n");
        // The resume pins + cleans the post-crash edit (acknowledged) but
        // does NOT put the pre-merge edit back: that is a violation.
        let v = strict.check_step(root, &merge_op(), residual, false);
        assert!(
            matches!(
                v.as_slice(),
                [EscapeViolation::TrunkDirtyDisplaced { path, after_crash: true, .. }]
                    if path == "pre.txt"
            ),
            "{v:?}"
        );
        assert_eq!(strict.reported(), 1, "post.txt is acknowledged");

        // The replay model never reads the residual notice as whole-snapshot.
        assert!(!has_whole_snapshot_notice(residual));
        assert_eq!(report_for(residual, "pre.txt"), Report::Silent);
    }

    /// bn-36chi: "reported" needs the path AND a recovery handle in the same
    /// report section; a path named in one place and a handle for another
    /// path's conflict elsewhere is not a report.
    #[test]
    fn report_needs_path_and_handle_in_one_section() {
        let unrelated = "  progress: touching shared/a.txt\n\n  WARNING: 1 file(s) in 'default' have local-vs-merge conflicts.\n    [ content] shared/b.txt\n    maw ws resolve default --list\n";
        assert_eq!(report_for(unrelated, "shared/a.txt"), Report::Silent);
        assert_eq!(report_for(unrelated, "shared/b.txt"), Report::Displaced);
        let two = "  WARNING: type conflict\n    shared/a.txt\n      restore yours: maw ws recover --ref R --restore-file shared/a.txt\n  WARNING: 1 file(s) have conflicts\n    [ content] shared/b.txt\n    maw ws resolve default --list\n";
        assert_eq!(report_for(two, "shared/a.txt"), Report::Displaced);
        assert_eq!(report_for(two, "shared/b.txt"), Report::Displaced);
    }

    /// bn-3jqfk: a retargeted symlink must come back as a link to the user's
    /// target; a regular file with the same bytes, or the old target, trips.
    #[cfg(unix)]
    #[test]
    fn symlink_retarget_must_survive_as_a_symlink() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");
        let link = root.join("ws/default/link");
        std::os::unix::fs::symlink("new-target", &link).unwrap();
        let mut strict = TrunkDirtyDisplacement::new();
        strict.record_dirty_symlink("link", "new-target");
        assert!(strict.check_step(root, &merge_op(), "", false).is_empty());

        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("old-target", &link).unwrap();
        let v = strict.check_step(root, &merge_op(), "", false);
        assert!(
            matches!(v.as_slice(), [EscapeViolation::TrunkDirtyDisplaced { path, .. }] if path == "link"),
            "{v:?}"
        );

        strict.record_dirty_symlink("link", "new-target");
        fs::remove_file(&link).unwrap();
        fs::write(&link, "new-target").unwrap();
        assert_eq!(strict.check_step(root, &merge_op(), "", false).len(), 1);
    }

    // ----- RecordRefCoherence (bn-3uou) ------------------------------------

    fn write_destroy_record(root: &Path, ws: &str, filename: &str, snapshot_ref: Option<&str>) {
        let dir = LayoutFlavor::detect(root)
            .manifold_dir(root)
            .join("artifacts")
            .join("ws")
            .join(ws)
            .join("destroy");
        fs::create_dir_all(&dir).unwrap();
        let snap = snapshot_ref.map_or_else(|| "null".to_owned(), |r| format!("\"{r}\""));
        let body = format!(
            r#"{{"workspace_id":"{ws}","destroyed_at":"2026-07-09T00:00:00.000Z",
                "final_head":"{oid}","final_head_ref":null,"snapshot_oid":"{oid}",
                "snapshot_ref":{snap},"capture_mode":"dirty_snapshot","dirty_files":[],
                "base_epoch":"{oid}","destroy_reason":"destroy","tool_version":"test"}}"#,
            oid = "a".repeat(40),
        );
        fs::write(dir.join(filename), body).unwrap();
    }

    #[test]
    fn record_claiming_missing_ref_trips() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // A record claims a recovery ref that does not exist.
        write_destroy_record(
            root,
            "gone",
            "20260709-000000.json",
            Some("refs/manifold/recovery/gone/missing-snap"),
        );
        let v = check_record_ref_coherence(root);
        assert!(
            v.iter().any(|x| matches!(
                x,
                EscapeViolation::RecordClaimsMissingRef { workspace, .. } if workspace == "gone"
            )),
            "a record claiming a missing recovery ref must trip: {v:?}"
        );
    }

    #[test]
    fn record_with_present_ref_is_green() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let ref_name = "refs/manifold/recovery/kept/snap";
        git(root, &["update-ref", ref_name, &oid]);
        write_destroy_record(root, "kept", "20260709-000000.json", Some(ref_name));
        let v = check_record_ref_coherence(root);
        assert!(
            v.is_empty(),
            "a record whose claimed recovery ref exists must be green: {v:?}"
        );
    }

    #[test]
    fn record_with_no_snapshot_ref_is_green() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // capture_mode none / no snapshot_ref → nothing to check.
        write_destroy_record(root, "nosnap", "20260709-000000.json", None);
        assert!(
            check_record_ref_coherence(root).is_empty(),
            "a record with no claimed ref must be green"
        );
    }

    // ----- gc sweep eligibility (bn-m7kjy) ---------------------------------

    #[test]
    fn pin_timestamp_parses_production_shapes_bn_m7kjy() {
        // 2026-09-28T02:24:27Z == 1_790_562_267 (date -u -d ... +%s).
        let want = Some(1_790_562_267);
        assert_eq!(pin_timestamp_from_leaf("2026-09-28T02-24-27Z"), want);
        assert_eq!(
            pin_timestamp_from_leaf("2026-09-28T02-24-27.337075157Z"),
            want
        );
        assert_eq!(
            pin_timestamp_from_leaf("materialize-2026-09-28T02-24-27Z"),
            want
        );
        assert_eq!(pin_timestamp_from_leaf("1970-01-01T00-00-00Z"), Some(0));
        assert_eq!(pin_timestamp_from_leaf("snap"), None);
        assert_eq!(pin_timestamp_from_leaf("2026-09-28T02-24-27"), None);
        assert_eq!(pin_timestamp_from_leaf("2026-13-28T02-24-27Z"), None);
    }

    /// The eligibility rule admits exactly a destroyed workspace's claimed,
    /// old-enough, timestamped snapshot — and nothing else.
    #[test]
    fn gc_eligibility_is_destroyed_claimed_and_aged_only_bn_m7kjy() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let ts = "2026-09-28T02-24-27Z";
        let now = 1_790_562_267 + 10;

        // (1) eligible: gone workspace, claimed by a record, old enough.
        let ok = format!("refs/manifold/recovery/gone/{ts}");
        git(root, &["update-ref", &ok, &oid]);
        write_destroy_record(root, "gone", "a.json", Some(&ok));
        // (2) live workspace (dir exists) even though claimed.
        let live = format!("refs/manifold/recovery/alive/{ts}");
        git(root, &["update-ref", &live, &oid]);
        write_destroy_record(root, "alive", "a.json", Some(&live));
        make_ws_dir(root, "alive");
        // (3) unclaimed pin of a gone workspace (e.g. dirty-trunk / materialize).
        let unclaimed = format!("refs/manifold/recovery/default/{ts}");
        git(root, &["update-ref", &unclaimed, &oid]);
        // (4) claimed but no timestamp in the name.
        let nots = "refs/manifold/recovery/legacy/snap";
        git(root, &["update-ref", nots, &oid]);
        write_destroy_record(root, "legacy", "a.json", Some(nots));

        let e0 = gc_eligible_recovery_snapshots(root, 0, now);
        assert_eq!(e0.keys().cloned().collect::<Vec<_>>(), vec![ok], "{e0:?}");
        // (5) too young for a 1-day threshold.
        assert!(gc_eligible_recovery_snapshots(root, 1, now).is_empty());
        // Pin created AFTER the gc started is never eligible.
        assert!(gc_eligible_recovery_snapshots(root, 0, now - 20).is_empty());
    }
}
