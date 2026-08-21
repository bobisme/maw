//! Clean-materialization oracle for the bn-p3m9 class (bn-3gba).
//!
//! # The invariant
//!
//! Several maw operations promise "the workspace ends **clean at a known
//! commit**": `ws create`, the `ws sync` fast-forward, the FF-absorb sibling
//! fast-forward/replay, and the post-merge sibling auto-rebase. After any of
//! them the workspace's **working tree must equal its own HEAD tree**.
//!
//! bn-p3m9 (continuum field report 3, 2026-08-11) is the class this catches:
//! two fresh workspaces materialized with HEAD *and index* correct at the trunk
//! tip, but a working tree carrying byte-exact stale-epoch blobs on exactly one
//! recent commit's path set. Nothing in maw noticed; the workers did, hours
//! later, as unexplained local modifications.
//!
//! # Why the existing oracles are blind to it
//!
//! * **Oracle A** ([`crate::oracle_a`]) judges blob *reachability*. In bn-p3m9
//!   every blob involved — the correct ones and the stale ones — was perfectly
//!   reachable. No work was lost. Oracle A is green by construction.
//! * **Oracle B** ([`crate::oracle_b`]) judges refs + merge-state coherence.
//!   Every ref was correct: HEAD, the index, the epoch refs. Green by
//!   construction.
//! * The **bn-2bcx escape oracles** ([`crate::oracle_escape`]) judge orphaned
//!   sibling work, dirty-trunk preservation, and record↔ref coherence. None of
//!   them looks at a working tree versus its own HEAD.
//!
//! Only the *working tree* was wrong, and nothing was checking it.
//!
//! # The expected-dirty model
//!
//! Asserting "clean" for every workspace after every op would be wrong — plenty
//! of DST ops deliberately dirty a worktree. So the oracle tracks the set of
//! workspaces the *plan* expects to be dirty and asserts every **other** live,
//! non-default workspace is clean at its own HEAD:
//!
//! | Op (outcome) | Effect |
//! |----|--------|
//! | `WsCreate` (ok) | **clean** — create's contract is clean-at-epoch |
//! | `Commit` (ok) | **clean** — the driver runs `git add -A` then commit |
//! | `Sync` / `Advance` (ok) | **clean** — both refuse a dirty worktree, so success implies clean-in / clean-out |
//! | `Sync` / `Advance` (fail) | **dirty** — refused (dirty) or crashed mid-checkout |
//! | `EditFiles` | **dirty** — deliberate |
//! | `Recover` | **dirty** — a restored worktree carries snapshot bytes by design |
//! | `Merge` (ok) | no change — siblings stay armed; this is THE assertion |
//! | `Merge` (fail) | **all live workspaces dirty** — see the fault note below |
//! | `Destroy` | forgotten |
//! | `Commit` (fail) | no change — nothing was staged |
//! | `CorruptWorktreeStatMasked` | **masked-stale** — see below (bn-22jy) |
//!
//! **Masked-stale note (bn-22jy).** [`Op::CorruptWorktreeStatMasked`] plants a
//! divergence that git itself calls clean, so the workspace is neither "dirty"
//! nor assertable. It is tracked in a separate set, excluded from the assertion
//! while the divergence persists, and re-armed the moment the worktree is
//! *observed* to equal its HEAD again. That last part is observed, never
//! inferred from the plan: none of the ops that might clear a mask does so
//! reliably (see [`CleanMaterialization::judge`]).
//!
//! **Fault note.** The faulted tier (`just sg1-production-tier-faults`) aborts
//! `maw` mid-op via `MAW_FP=<name>=abort`, and the generator only attaches
//! faults to `Merge`/`Commit`. A merge killed inside a sibling checkout can
//! legitimately leave a half-written worktree, which is a *fault artifact*, not
//! a maw defect. A failed merge therefore disarms every live workspace; the
//! next successful create/commit/sync re-arms it. This costs some coverage on
//! failing merges and buys a gate that stays actionable under fault injection.
//!
//! The **default workspace (trunk)** is never judged: `DirtyTrunkWrite` and the
//! merge preserve-and-replay path deliberately carry uncommitted trunk bytes
//! through operations (bn-1xmk's territory, covered by
//! [`crate::oracle_escape::TrunkDirtyPreservation`]).
//!
//! # Scope: tracked divergence only, detected by comparing TREES
//!
//! The check hashes the worktree into a real git tree (throwaway index +
//! `read-tree HEAD` + `add -A` + `write-tree`) and diffs it against
//! `HEAD^{tree}` — it does NOT use `git status`. Every status-shaped query
//! trusts the index **stat cache**, and this corruption class can poison it:
//! the corrupter writes stale bytes and *then* rewrites HEAD and the index, so
//! an entry can carry the correct blob OID with the stale file's stat data.
//! `git status` then reports clean on a genuinely wrong worktree — an oracle
//! built on it would be green on exactly the bug it exists to catch. Seeding a
//! throwaway index from `HEAD` gives every entry zeroed stat data, which forces
//! `add -A` to re-hash the bytes.
//!
//! Untracked files (`A` in the tree diff — present on disk, absent from HEAD)
//! are excluded for the same reason maw's production repair excludes them:
//! "repairing" a path that is not in HEAD means **deleting** it, which the Prime
//! Invariant forbids. The bn-p3m9 signature is tracked-path divergence.
//!
//! # Independent-verifier carveout
//!
//! Like [`crate::oracle_b`] and [`crate::oracle_escape`], all git access here
//! goes through the `git` CLI rather than `gix`/`maw-git`, so the verifier does
//! not share an implementation with the machinery under test. (A gix status
//! semantics trap is exactly how bn-pfh7 hid.)
//!
//! # Profile neutrality (bn-2bcx rule)
//!
//! This oracle is **read-only**: it generates no ops, changes no weights, and
//! never mutates the repo. The seed→plan byte stream is byte-identical with and
//! without it, so the default DST profile is unperturbed unless the oracle
//! fires.

#![cfg(feature = "oracles")]
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::too_long_first_doc_paragraph)]

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use maw_core::model::layout::LayoutFlavor;

use crate::scenario::Op;

/// Workspace name that always denotes trunk; never judged (see module docs).
const DEFAULT_WS: &str = "default";

// ---------------------------------------------------------------------------
// Violation type
// ---------------------------------------------------------------------------

/// A violation of the clean-materialization oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeViolation {
    /// **bn-p3m9** — a workspace that no op deliberately dirtied has tracked
    /// paths whose working-tree content differs from its own HEAD.
    WorktreeDivergedFromHead {
        /// The workspace whose worktree disagrees with its own HEAD.
        workspace: String,
        /// The op that ran immediately before the check (the suspect).
        after_op: &'static str,
        /// `diff-tree --name-status` lines (`M`/`D`/`T`), verbatim.
        status_lines: Vec<String>,
    },

    /// The oracle's own git invocation failed, so no verdict is possible.
    /// Reported as a violation so the run stops loudly rather than silently
    /// green-lighting on broken tooling (matches `oracle_b`'s `GitError`).
    GitError {
        /// The workspace being inspected.
        workspace: String,
        /// The command that failed.
        command: String,
        /// Stderr from the command.
        stderr: String,
    },
}

impl fmt::Display for WorktreeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorktreeDivergedFromHead {
                workspace,
                after_op,
                status_lines,
            } => write!(
                f,
                "CleanMaterialization (bn-p3m9): workspace '{workspace}' should be CLEAN at its \
                 own HEAD after '{after_op}', but {} tracked path(s) differ: [{}]",
                status_lines.len(),
                status_lines.join(", ")
            ),
            Self::GitError {
                workspace,
                command,
                stderr,
            } => write!(
                f,
                "CleanMaterialization: git plumbing failed for '{workspace}' \
                 (`{command}`): {stderr}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Oracle
// ---------------------------------------------------------------------------

/// Incremental oracle: after every op, assert that every live, non-default
/// workspace the plan does NOT expect to be dirty has `worktree == HEAD`.
///
/// One instance per seed/repo, fed every step **in order** with the op's
/// success verdict (the expected-dirty set is derived from the op history).
#[derive(Debug, Default)]
pub struct CleanMaterialization {
    /// Workspaces the plan has deliberately dirtied (or whose state is unknown
    /// after a crashed op). Everything else must be clean.
    expected_dirty: BTreeSet<String>,
    /// Workspaces carrying a deliberate **stat-cache-masked** divergence
    /// (bn-22jy's [`Op::CorruptWorktreeStatMasked`]). Held separately from
    /// `expected_dirty` because it clears on a *different* set of ops — see
    /// [`CleanMaterialization::absorb`].
    masked_stale: BTreeSet<String>,
    /// How many workspace-level clean assertions actually ran. The
    /// non-vacuity signal: a run where this stays 0 proved nothing.
    checks_run: u64,
    /// bn-22jy: how many times a masked-stale workspace was observed clean
    /// again and re-armed. Evidence that the corruption primitive's effect was
    /// actually repaired by maw rather than merely stopped being looked at.
    masked_resolutions: u64,
}

impl CleanMaterialization {
    /// A fresh oracle: nothing is expected dirty yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of workspace-level clean assertions performed so far.
    ///
    /// Harnesses should assert this is `> 0`: a green run with zero checks is
    /// vacuous, not evidence.
    #[must_use]
    pub const fn checks_run(&self) -> u64 {
        self.checks_run
    }

    /// Workspaces currently expected to be dirty (test/diagnostic accessor).
    #[must_use]
    pub const fn expected_dirty(&self) -> &BTreeSet<String> {
        &self.expected_dirty
    }

    /// Workspaces currently carrying a stat-cache-masked divergence
    /// (test/diagnostic accessor, bn-22jy).
    #[must_use]
    pub const fn masked_stale(&self) -> &BTreeSet<String> {
        &self.masked_stale
    }

    /// How many masked-stale workspaces were later observed clean at their own
    /// HEAD again — i.e. maw really did re-materialize them (bn-22jy).
    #[must_use]
    pub const fn masked_resolutions(&self) -> u64 {
        self.masked_resolutions
    }

    /// Fold `op` (which just executed, with outcome `succeeded`) into the
    /// expected-dirty set, then judge every live, non-default workspace that is
    /// not in it.
    ///
    /// `root` is the repo root. Returns one violation per offending workspace.
    pub fn check_step(&mut self, root: &Path, op: &Op, succeeded: bool) -> Vec<WorktreeViolation> {
        let live = live_workspaces(root);
        self.absorb(op, succeeded, &live);
        self.judge(root, &live, op_label(op))
    }

    /// Pure state-transition half of [`Self::check_step`] (unit-tested).
    ///
    /// `live` is the set of workspaces currently on disk — needed only for the
    /// "a failed merge may have crashed mid-flight" blanket disarm.
    ///
    /// The match is deliberately **exhaustive** (no `_` arm): a new generator
    /// op must force a decision here rather than silently defaulting to
    /// "asserted clean" (the bn-1nmh silent-rot class).
    pub fn absorb(&mut self, op: &Op, succeeded: bool, live: &BTreeSet<String>) {
        match op {
            // `WsCreate`: the contract is a clean worktree at the base epoch.
            // `Commit`: the driver commits with `git add -A`, so the workspace
            // ends clean at the new HEAD.
            //
            // Both only say something when they SUCCEEDED. A failed
            // duplicate-name create must not clean-arm a pre-existing (possibly
            // dirty) workspace, and a failed commit means nothing was staged.
            Op::WsCreate { ws, .. } | Op::Commit { ws, .. } => {
                if succeeded {
                    self.expected_dirty.remove(&ws.0);
                }
            }
            // Both refuse a dirty worktree, so success implies clean-in and
            // (by contract) clean-out. Failure means refused-because-dirty or
            // crashed mid-checkout: either way, stop asserting.
            Op::Sync { ws } | Op::Advance { ws } => {
                if succeeded {
                    self.expected_dirty.remove(&ws.0);
                } else {
                    self.mark_dirty(&ws.0);
                }
            }
            // Deliberately dirty.
            Op::EditFiles { ws, .. } => self.mark_dirty(&ws.0),
            // A recovered worktree materializes snapshot bytes, which differ
            // from HEAD by design.
            Op::Recover { to, .. } => self.mark_dirty(&to.0),
            Op::Destroy { ws, .. } => {
                self.expected_dirty.remove(&ws.0);
            }
            Op::Merge { .. } => {
                if !succeeded {
                    // May have been killed mid-flight (the faulted tier attaches
                    // aborts to Merge). A half-written sibling worktree is a
                    // fault artifact, not a maw defect — disarm everything and
                    // let the next successful create/commit/sync re-arm.
                    for ws in live {
                        self.mark_dirty(ws);
                    }
                }
            }
            // Trunk-only / repo-level ops; `default` is never judged anyway.
            Op::OutOfMawCommit { .. } | Op::DirtyTrunkWrite { .. } | Op::Gc { .. } => {}
            // bn-22jy: the workspace now (deliberately) disagrees with its own
            // HEAD on a tracked path, and git says otherwise. Disarm the clean
            // assertion until an op that rewrites the whole worktree clears it.
            //
            // Armed on the PLAN, not on the driver's success verdict, and
            // deliberately so: this is the one place where being conservative
            // costs only coverage, whereas being optimistic manufactures a
            // false WorktreeDivergedFromHead on maw's most sensitive oracle.
            Op::CorruptWorktreeStatMasked { ws, .. } => self.mark_masked_stale(&ws.0),
        }
    }

    fn mark_dirty(&mut self, ws: &str) {
        if ws != DEFAULT_WS {
            self.expected_dirty.insert(ws.to_owned());
        }
    }

    fn mark_masked_stale(&mut self, ws: &str) {
        if ws != DEFAULT_WS {
            self.masked_stale.insert(ws.to_owned());
        }
    }

    /// Judge every live, non-default workspace that is not expected-dirty.
    ///
    /// bn-22jy: a masked-stale workspace is not asserted on, but it IS still
    /// inspected — and the moment its worktree is observed to equal its HEAD
    /// again the mask is cleared and normal assertions resume. Deriving that
    /// from observed reality rather than from the plan is the only sound
    /// option: none of the ops that could clear a mask does so reliably.
    /// A successful `Commit` does not (`git add -A` trusts the very stat cache
    /// the primitive forged, so the stale bytes are never staged); a successful
    /// `Sync`/`Advance` does not (an already-current workspace exits 0 with
    /// "up to date" without touching the worktree); a `Destroy` may have been
    /// refused. Every one of those guesses produced a real false positive in a
    /// 16x24 corruption run before this was rewritten to observe instead.
    fn judge(
        &mut self,
        root: &Path,
        live: &BTreeSet<String>,
        after_op: &'static str,
    ) -> Vec<WorktreeViolation> {
        let flavor = LayoutFlavor::detect_with_env(root);
        let mut violations = Vec::new();
        let mut unmasked: Vec<String> = Vec::new();
        for ws in live {
            if ws == DEFAULT_WS || self.expected_dirty.contains(ws) {
                continue;
            }
            let masked = self.masked_stale.contains(ws);
            let ws_path: PathBuf = flavor.workspace_path(root, ws);
            if !masked {
                self.checks_run += 1;
            }
            match tracked_status_lines(&ws_path) {
                Ok(lines) if lines.is_empty() => {
                    if masked {
                        // The divergence is gone: some op re-materialized the
                        // worktree. Resume asserting on this workspace.
                        unmasked.push(ws.clone());
                        self.masked_resolutions += 1;
                    }
                }
                // A masked workspace is EXPECTED to differ; that is the whole
                // point of the primitive, not a maw defect.
                Ok(lines) => {
                    if !masked {
                        violations.push(WorktreeViolation::WorktreeDivergedFromHead {
                            workspace: ws.clone(),
                            after_op,
                            status_lines: lines,
                        });
                    }
                }
                Err((command, stderr)) => violations.push(WorktreeViolation::GitError {
                    workspace: ws.clone(),
                    command,
                    stderr,
                }),
            }
        }
        for ws in unmasked {
            self.masked_stale.remove(&ws);
        }
        violations
    }
}

/// Workspace directories currently on disk under the layout's workspaces dir.
///
/// Read straight off the filesystem (not via `maw ws list`) so the verifier
/// stays independent of the machinery under test.
fn live_workspaces(root: &Path) -> BTreeSet<String> {
    let dir = LayoutFlavor::detect_with_env(root).workspaces_dir(root);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return BTreeSet::new();
    };
    rd.filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != DEFAULT_WS)
        .collect()
}

/// Tracked divergence between `ws_path`'s working tree and its own `HEAD`,
/// computed by **hashing the worktree into a tree** and diffing it against
/// HEAD's tree.
///
/// # Why not `git status --porcelain` (the trap this oracle exists to avoid)
///
/// Every status-shaped query trusts the index **stat cache**: an index entry
/// whose recorded `(size, mtime, …)` matches the file on disk is reported clean
/// without the file ever being read. The bn-p3m9 corrupter writes stale bytes
/// and then rewrites HEAD and the index (`set_head_detached` + index
/// realignment), which can leave an entry holding the CORRECT blob OID stamped
/// with the STALE file's stat data — invisible to `git status`, `git diff HEAD`
/// and gix's `status_head_to_worktree` alike. An oracle built on any of those
/// would report green on exactly the corruption it was written to catch.
///
/// Seeding a THROWAWAY index from `HEAD` (`read-tree` writes zeroed stat data,
/// so nothing can be trusted-as-clean) and then `git add -A` forces git to
/// re-hash every file. The caller's real index, HEAD and stash list are never
/// touched.
///
/// Returns `diff-tree --name-status` lines for `M`/`D`/`T` entries. `A` entries
/// name paths absent from HEAD (untracked scratch) and are excluded for the
/// same reason maw's production repair excludes them: "repairing" one means
/// deleting it.
fn tracked_status_lines(ws_path: &Path) -> Result<Vec<String>, (String, String)> {
    let temp = tempfile::tempdir().map_err(|e| ("tempdir".to_owned(), e.to_string()))?;
    let index = temp.path().join("index");

    let run = |args: &[&str]| -> Result<String, (String, String)> {
        let command = format!("git -C {} {}", ws_path.display(), args.join(" "));
        let out = Command::new("git")
            .current_dir(ws_path)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .map_err(|e| (command.clone(), e.to_string()))?;
        if !out.status.success() {
            return Err((command, String::from_utf8_lossy(&out.stderr).into_owned()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    run(&["read-tree", "HEAD"])?;
    run(&["add", "-A"])?;
    let tree = run(&["write-tree"])?.trim().to_owned();
    let raw = run(&[
        "diff-tree",
        "-r",
        "--no-renames",
        "--name-status",
        "-z",
        // A commit is a tree-ish, so `diff-tree` resolves HEAD to its own tree
        // (spelling it `HEAD^{tree}` would be equivalent, but the literal trips
        // clippy's `literal_string_with_formatting_args`).
        "HEAD",
        &tree,
    ])?;

    // `-z`: NUL-separated `status\0path\0…` (never c-quoted, so odd paths
    // survive intact).
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut lines = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        if matches!(status.chars().next(), Some('M' | 'D' | 'T')) {
            lines.push(format!("{status} {path}"));
        }
    }
    lines.sort();
    Ok(lines)
}

/// Short label for the op that just ran, used in the violation message.
const fn op_label(op: &Op) -> &'static str {
    match op {
        Op::WsCreate { .. } => "ws-create",
        Op::EditFiles { .. } => "edit-files",
        Op::Commit { .. } => "commit",
        Op::Merge { .. } => "merge",
        Op::Sync { .. } => "sync",
        Op::Destroy { .. } => "destroy",
        Op::Recover { .. } => "recover",
        Op::Advance { .. } => "advance",
        Op::OutOfMawCommit { .. } => "out-of-maw-commit",
        Op::DirtyTrunkWrite { .. } => "dirty-trunk-write",
        Op::Gc { .. } => "gc",
        Op::CorruptWorktreeStatMasked { .. } => "corrupt-worktree-stat-masked",
    }
}

// ---------------------------------------------------------------------------
// MaskedStalePreservation (bn-22jy / bn-154g)
// ---------------------------------------------------------------------------

/// A violation of the preserve-before-overwrite contract (bn-154g).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskedStaleViolation {
    /// **bn-154g** — maw overwrote a stat-cache-masked stale file and the
    /// pre-overwrite bytes are recoverable from nowhere.
    MaskedBytesDestroyedWithoutPin {
        /// Workspace whose worktree held the doomed bytes.
        workspace: String,
        /// Tracked path that was poisoned and then overwritten.
        path: String,
        /// The op that ran immediately before the bytes disappeared.
        after_op: &'static str,
        /// Blob OID of the destroyed bytes, for forensics.
        blob: String,
    },
}

impl fmt::Display for MaskedStaleViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaskedBytesDestroyedWithoutPin {
                workspace,
                path,
                after_op,
                blob,
            } => write!(
                f,
                "MaskedStalePreservation (bn-154g / bn-2k9e): '{after_op}' removed the \
                 stat-cache-masked bytes at '{workspace}/{path}' (blob {blob}) and they are \
                 reachable from no refs/manifold/recovery/{workspace}/* ref — maw destroyed \
                 content it never made recoverable"
            ),
        }
    }
}

/// One workspace path this oracle is watching.
#[derive(Debug, Clone)]
struct MaskedEntry {
    workspace: String,
    path: String,
    /// The stale bytes the driver actually wrote (already confirmed masked).
    stale: String,
    /// Set once the bytes leave the working tree, so a single overwrite is
    /// judged exactly once and later ops cannot re-report it.
    settled: bool,
}

/// **Preserve-before-overwrite oracle** for the bn-154g class (bn-22jy).
///
/// # The invariant
///
/// When maw overwrites a tracked file whose working-tree bytes it believes are
/// clean but which actually disagree with HEAD, it must first PIN those bytes
/// somewhere recoverable. The Prime Invariant is unconditional: maw never
/// destroys bytes it has not first made recoverable, *even bytes it believes
/// are wrong*.
///
/// The production guard is
/// `maw_cli::workspace::materialize_verify::preserve_divergence_before_overwrite`,
/// called immediately before the fast-forward `checkout_detach` in
/// `sync::checks::sync_worktree_to_epoch_inner`. Its observable is a
/// `refs/manifold/recovery/<ws>/materialize-<ts>` ref whose tree carries the
/// pre-overwrite bytes verbatim (plus a loud stderr WARNING and a
/// `preserved-before-overwrite` JSON artifact, which the e2e test
/// `tests/sync_ff_hidden_divergence_bn_154g.rs` asserts on directly).
///
/// Note that `sync_worktree_to_epoch_inner` serves BOTH the `maw ws sync`
/// command and — via `sync_worktree_to_epoch_quiet` — the post-merge sibling
/// auto-rebase, so the same guard covers both. In practice the DST reaches it
/// far more often through the auto-rebase: a merge fast-forwards every clean
/// sibling itself, so by the time a `Sync` op runs the workspace is usually
/// already current and the command exits 0 without touching the worktree.
///
/// This oracle asserts the byte-level half of that observable — the half that
/// generalises to every op the DST can schedule, not just the one the e2e test
/// drives.
///
/// # Why the existing oracles are blind to it
///
/// * [`CleanMaterialization`] judges "worktree == HEAD". After the sync's
///   checkout that is *true* — the checkout is the repair. A silently destroyed
///   pre-overwrite snapshot is invisible to it, which is exactly how the bug
///   reached black-box validation (`/tmp/maw-validate/t9`: right outcome, no
///   WARNING, no artifact, no pin).
/// * [`crate::oracle_a`] judges blob reachability for **committed** content.
///   These bytes were never committed anywhere, so they are outside its witness
///   set by construction.
/// * [`crate::oracle_escape::TrunkDirtyPreservation`] is the same shape but for
///   the *default* workspace's visibly-dirty bytes. This oracle is its
///   counterpart for a non-default workspace's *invisibly* stale bytes.
///
/// # Scope: every way the bytes can leave the disk, destroy included (bn-2k9e)
///
/// Only paths the driver has actually poisoned and confirmed masked are
/// watched, via [`MaskedStalePreservation::record_masked`]. The oracle is
/// otherwise inert — a run that never enables `corrupt_weight` pays one
/// `is_empty()` check per step.
///
/// A watched path is judged the moment its bytes are no longer on disk — and
/// that includes the whole workspace being removed by `maw ws destroy` or
/// `ws merge --destroy`. Between bn-22jy and bn-2k9e those entries were dropped
/// unjudged (counted in a `destroyed_unjudged` field) because destroy's
/// snapshot was built from the workspace's *git state*, which the stat-cache
/// mask defeats exactly as it defeated the sync checkout before bn-154g: at
/// 8x16 this oracle fired four times on `op=destroy`, and that was a real bug,
/// not a scope error. bn-2k9e closed it — the pre-destroy capture now
/// hash-compares the worktree against HEAD and force-rehashes the divergent
/// paths into the snapshot — so the carve-out is gone and the contract is
/// uniform: *once masked bytes leave the disk, by any route, they must be
/// reachable from that workspace's own `refs/manifold/recovery/<ws>/`
/// namespace.*
///
/// # Profile neutrality (bn-2bcx rule)
///
/// Read-only: it generates no ops, changes no weights and never mutates the
/// repo.
#[derive(Debug, Default)]
pub struct MaskedStalePreservation {
    entries: Vec<MaskedEntry>,
    /// How many "these bytes left the disk" judgements actually ran — the
    /// non-vacuity signal. A run with 0 proved nothing about bn-154g.
    overwrites_judged: u64,
}

impl MaskedStalePreservation {
    /// A fresh oracle watching nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `stale` bytes were successfully planted (and confirmed
    /// masked) at `workspace/path`.
    ///
    /// The driver — not the plan — supplies these, because the corruption op
    /// carries only a *hint* and the driver resolves the real victim against
    /// the repo. Recording the resolved truth is what keeps the oracle from
    /// judging a path that was never poisoned.
    pub fn record_masked(&mut self, workspace: &str, path: &str, stale: &str) {
        // A repeat poisoning of the same path supersedes the previous bytes.
        self.entries
            .retain(|e| !(e.workspace == workspace && e.path == path));
        self.entries.push(MaskedEntry {
            workspace: workspace.to_owned(),
            path: path.to_owned(),
            stale: stale.to_owned(),
            settled: false,
        });
    }

    /// Number of overwrite judgements performed. Harnesses should assert this
    /// is `> 0` when the corruption op is enabled: a green run in which no
    /// masked file was ever overwritten never reached the bn-154g guard.
    #[must_use]
    pub const fn overwrites_judged(&self) -> u64 {
        self.overwrites_judged
    }

    /// Number of paths still being watched (still holding their stale bytes).
    #[must_use]
    pub fn watching(&self) -> usize {
        self.entries.iter().filter(|e| !e.settled).count()
    }

    /// Judge every watched path after the op labelled by `op`.
    ///
    /// A path whose stale bytes are still on disk is not yet interesting. Once
    /// they are gone — overwritten by a checkout, or taken away with the whole
    /// workspace — the bytes MUST be reachable from that workspace's own
    /// `refs/manifold/recovery/<ws>/` namespace.
    pub fn check_step(&mut self, root: &Path, op: &Op) -> Vec<MaskedStaleViolation> {
        if self.entries.iter().all(|e| e.settled) {
            return Vec::new();
        }
        let after_op = op_label(op);
        let flavor = LayoutFlavor::detect_with_env(root);
        let mut violations = Vec::new();
        // Cache the per-workspace recovery blob sets: several entries usually
        // share a workspace and `rev-list` is the expensive part.
        let mut recovery_cache: std::collections::BTreeMap<String, BTreeSet<String>> =
            std::collections::BTreeMap::new();

        for entry in &mut self.entries {
            if entry.settled {
                continue;
            }
            let ws_dir = flavor.workspace_path(root, &entry.workspace);
            // A removed workspace is judged like any other way the bytes can
            // leave the disk (bn-2k9e): `read_to_string` on a path inside a
            // deleted directory fails, so the check below sees `None` and the
            // destroy is held to the same "must be reachable from a recovery
            // ref" contract as an overwrite.
            let on_disk = std::fs::read_to_string(ws_dir.join(&entry.path)).ok();
            if on_disk.as_deref() == Some(entry.stale.as_str()) {
                // Still poisoned; nothing has been destroyed yet.
                continue;
            }
            // The bytes are gone. This is the judgement bn-154g is about.
            entry.settled = true;
            self.overwrites_judged += 1;
            let blobs = recovery_cache
                .entry(entry.workspace.clone())
                .or_insert_with(|| {
                    crate::oracle_escape::recovery_reachable_blobs(root, Some(&entry.workspace))
                });
            let Some(blob) = crate::oracle_escape::hash_blob(root, &entry.stale) else {
                // `git hash-object` failed: we cannot decide, and inventing a
                // violation from broken plumbing would be worse than silence
                // (the GitError arm of CleanMaterialization already fires loudly
                // on a broken git in the same loop).
                continue;
            };
            if !blobs.contains(&blob) {
                violations.push(MaskedStaleViolation::MaskedBytesDestroyedWithoutPin {
                    workspace: entry.workspace.clone(),
                    path: entry.path.clone(),
                    after_op,
                    blob,
                });
            }
        }
        violations
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::scenario::{BaseRef, Seeded, Target, WsId};

    fn ws(name: &str) -> WsId {
        WsId(name.to_owned())
    }

    fn create(name: &str) -> Op {
        Op::WsCreate {
            ws: ws(name),
            from: BaseRef::Main,
        }
    }

    fn live(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn nothing_is_expected_dirty_initially() {
        let o = CleanMaterialization::new();
        assert!(o.expected_dirty().is_empty());
        assert_eq!(o.checks_run(), 0);
    }

    #[test]
    fn edit_marks_dirty_and_commit_clears_it() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::EditFiles {
                ws: ws("a"),
                files: Vec::new(),
            },
            false,
            &live(&["a"]),
        );
        assert!(o.expected_dirty().contains("a"));

        o.absorb(
            &Op::Commit {
                ws: ws("a"),
                msg: Seeded("m".to_owned()),
            },
            true,
            &live(&["a"]),
        );
        assert!(
            !o.expected_dirty().contains("a"),
            "the driver commits with `git add -A`, so a successful commit ends clean"
        );
    }

    /// A failed commit (nothing staged) must not clear a deliberate dirty bit.
    #[test]
    fn failed_commit_does_not_clear_dirty() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::EditFiles {
                ws: ws("a"),
                files: Vec::new(),
            },
            false,
            &live(&["a"]),
        );
        o.absorb(
            &Op::Commit {
                ws: ws("a"),
                msg: Seeded("m".to_owned()),
            },
            false,
            &live(&["a"]),
        );
        assert!(o.expected_dirty().contains("a"));
    }

    /// A FAILED create (duplicate name) must not clean-arm a pre-existing,
    /// possibly dirty, workspace of the same name.
    #[test]
    fn failed_create_does_not_clear_dirty() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::EditFiles {
                ws: ws("a"),
                files: Vec::new(),
            },
            false,
            &live(&["a"]),
        );
        o.absorb(&create("a"), false, &live(&["a"]));
        assert!(o.expected_dirty().contains("a"));

        o.absorb(&create("a"), true, &live(&["a"]));
        assert!(!o.expected_dirty().contains("a"));
    }

    #[test]
    fn successful_sync_clears_and_failed_sync_disarms() {
        let mut o = CleanMaterialization::new();
        o.absorb(&Op::Sync { ws: ws("a") }, true, &live(&["a"]));
        assert!(o.expected_dirty().is_empty());

        o.absorb(&Op::Sync { ws: ws("a") }, false, &live(&["a"]));
        assert!(
            o.expected_dirty().contains("a"),
            "a refused/crashed sync means the worktree state is not guaranteed"
        );
    }

    #[test]
    fn recover_marks_the_destination_dirty() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::Recover {
                ws: ws("gone"),
                to: ws("restored"),
            },
            true,
            &live(&["restored"]),
        );
        assert!(o.expected_dirty().contains("restored"));
    }

    /// The load-bearing coverage case: a SUCCESSFUL merge leaves every sibling
    /// armed. The sibling auto-rebase / FF-absorb paths are exactly what
    /// bn-p3m9 poisoned.
    #[test]
    fn successful_merge_keeps_siblings_armed() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::Merge {
                srcs: vec![ws("src")],
                into: Target::Default,
                destroy: true,
            },
            true,
            &live(&["src", "sibling"]),
        );
        assert!(
            o.expected_dirty().is_empty(),
            "a clean merge must leave every sibling asserted-clean"
        );
    }

    /// A FAILED merge may have been killed mid-flight (the faulted tier aborts
    /// merges), so every live workspace is disarmed until re-armed.
    #[test]
    fn failed_merge_disarms_every_live_workspace() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::Merge {
                srcs: vec![ws("src")],
                into: Target::Default,
                destroy: false,
            },
            false,
            &live(&["src", "sibling"]),
        );
        assert_eq!(
            o.expected_dirty(),
            &live(&["src", "sibling"]),
            "a crashed merge leaves every worktree in an unknown state"
        );

        // …and a later successful create re-arms just that one.
        o.absorb(&create("sibling"), true, &live(&["src", "sibling"]));
        assert_eq!(o.expected_dirty(), &live(&["src"]));
    }

    /// Trunk is never tracked: `DirtyTrunkWrite` and merge preserve-and-replay
    /// deliberately carry uncommitted trunk bytes (bn-1xmk's territory).
    #[test]
    fn default_workspace_is_never_marked() {
        let mut o = CleanMaterialization::new();
        o.absorb(
            &Op::EditFiles {
                ws: ws(DEFAULT_WS),
                files: Vec::new(),
            },
            false,
            &live(&[DEFAULT_WS]),
        );
        assert!(o.expected_dirty().is_empty());
    }

    // -------------------------------------------------------------------
    // bn-22jy: masked-stale bookkeeping + MaskedStalePreservation
    // -------------------------------------------------------------------

    fn corrupt(name: &str) -> Op {
        Op::CorruptWorktreeStatMasked {
            ws: ws(name),
            path: Some("a.txt".to_owned()),
            nonce: 7,
        }
    }

    /// A masked workspace is excluded from the clean assertion — otherwise the
    /// primitive would make maw's own oracle red on the corruption IT injected.
    #[test]
    fn corruption_op_masks_the_workspace() {
        let mut o = CleanMaterialization::new();
        o.absorb(&corrupt("alice"), true, &live(&["alice"]));
        assert!(o.masked_stale().contains("alice"));
        // Not the same thing as "dirty": the two sets clear on different ops.
        assert!(!o.expected_dirty().contains("alice"));
    }

    /// The mask is NOT cleared by any plan-level guess — not even a successful
    /// sync, which exits 0 with "up to date" when the workspace is already
    /// current and never touches the worktree. Only observing a clean worktree
    /// clears it (exercised end-to-end by `judge`).
    #[test]
    fn no_op_clears_the_mask_from_the_plan_alone() {
        let l = live(&["alice"]);
        for (op, ok) in [
            (
                Op::Commit {
                    ws: ws("alice"),
                    msg: Seeded("m".into()),
                },
                true,
            ),
            (Op::Sync { ws: ws("alice") }, true),
            (Op::Advance { ws: ws("alice") }, true),
            (
                Op::Destroy {
                    ws: ws("alice"),
                    force: true,
                },
                true,
            ),
            (create("alice"), true),
        ] {
            let mut o = CleanMaterialization::new();
            o.absorb(&corrupt("alice"), true, &l);
            o.absorb(&op, ok, &l);
            assert!(
                o.masked_stale().contains("alice"),
                "{op:?} must not clear the mask from the plan alone"
            );
        }
    }

    /// Build a throwaway repo with one workspace holding `content` at `a.txt`.
    /// Returns `(tempdir, root, ws_path)`.
    fn masked_repo(content: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "t@e.com"]);
        git(&root, &["config", "user.name", "T"]);
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-m", "init"]);

        let ws_path = LayoutFlavor::detect_with_env(&root).workspace_path(&root, "alice");
        std::fs::create_dir_all(&ws_path).unwrap();
        std::fs::write(ws_path.join("a.txt"), content).unwrap();
        (td, root, ws_path)
    }

    /// Pin `content` under `refs/manifold/recovery/<ws>/materialize-test`,
    /// exactly as `preserve_divergence_before_overwrite` does.
    fn pin_recovery(root: &Path, ws_name: &str, content: &str) {
        let git = |args: &[&str], stdin: Option<&str>| -> String {
            use std::io::Write as _;
            let mut c = Command::new("git");
            c.current_dir(root).args(args);
            if stdin.is_some() {
                c.stdin(std::process::Stdio::piped());
            }
            c.stdout(std::process::Stdio::piped());
            let mut child = c.spawn().unwrap();
            if let Some(data) = stdin {
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(data.as_bytes())
                    .unwrap();
            }
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let blob = git(&["hash-object", "-w", "--stdin"], Some(content));
        let index = root.join(".git").join("pin-index");
        let idx = index.to_str().unwrap().to_owned();
        let run_idx = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(root)
                .env("GIT_INDEX_FILE", &idx)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        run_idx(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},a.txt"),
        ]);
        let tree = run_idx(&["write-tree"]);
        let _ = std::fs::remove_file(&index);
        let commit = git(&["commit-tree", &tree, "-m", "pin"], None);
        git(
            &[
                "update-ref",
                &format!("refs/manifold/recovery/{ws_name}/materialize-test"),
                &commit,
            ],
            None,
        );
    }

    const MASKED_STALE: &str = "STALE masked bytes\n";
    const REPAIRED: &str = "repaired from HEAD\n";

    /// bn-154g RED: the masked bytes are overwritten and nothing pinned them.
    #[test]
    fn masked_bytes_destroyed_without_pin_trips() {
        let (_td, root, ws_path) = masked_repo(MASKED_STALE);
        let mut o = MaskedStalePreservation::new();
        o.record_masked("alice", "a.txt", MASKED_STALE);

        let op = Op::Sync { ws: ws("alice") };
        assert!(
            o.check_step(&root, &op).is_empty(),
            "bytes still on disk: nothing has been destroyed yet"
        );
        assert_eq!(o.overwrites_judged(), 0);

        std::fs::write(ws_path.join("a.txt"), REPAIRED).unwrap();
        let v = o.check_step(&root, &op);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(
            &v[0],
            MaskedStaleViolation::MaskedBytesDestroyedWithoutPin { workspace, path, .. }
                if workspace == "alice" && path == "a.txt"
        ));
        assert_eq!(o.overwrites_judged(), 1);
        // Judged exactly once, so a long plan cannot re-report one overwrite.
        assert!(o.check_step(&root, &op).is_empty());
    }

    /// bn-154g GREEN: same overwrite, but the bytes were pinned first.
    #[test]
    fn masked_bytes_pinned_before_overwrite_is_green() {
        let (_td, root, ws_path) = masked_repo(MASKED_STALE);
        let mut o = MaskedStalePreservation::new();
        o.record_masked("alice", "a.txt", MASKED_STALE);

        pin_recovery(&root, "alice", MASKED_STALE);
        std::fs::write(ws_path.join("a.txt"), REPAIRED).unwrap();

        let v = o.check_step(&root, &Op::Sync { ws: ws("alice") });
        assert!(v.is_empty(), "pinned bytes must read as preserved: {v:?}");
        assert_eq!(o.overwrites_judged(), 1);
    }

    /// A pin in ANOTHER workspace's recovery namespace does not count: bn-154g
    /// pins under the workspace it is about to overwrite.
    #[test]
    fn pin_in_another_workspaces_namespace_does_not_rescue() {
        let (_td, root, ws_path) = masked_repo(MASKED_STALE);
        let mut o = MaskedStalePreservation::new();
        o.record_masked("alice", "a.txt", MASKED_STALE);

        pin_recovery(&root, "bob", MASKED_STALE);
        std::fs::write(ws_path.join("a.txt"), REPAIRED).unwrap();

        assert_eq!(o.check_step(&root, &Op::Sync { ws: ws("alice") }).len(), 1);
    }

    /// bn-2k9e: removing the whole workspace is judged, not carved out. An
    /// unpinned destroy is a violation — the RED half of the twin.
    #[test]
    fn destroyed_workspace_without_a_pin_is_a_violation() {
        let (_td, root, ws_path) = masked_repo(MASKED_STALE);
        let mut o = MaskedStalePreservation::new();
        o.record_masked("alice", "a.txt", MASKED_STALE);

        std::fs::remove_dir_all(&ws_path).unwrap();
        let v = o.check_step(
            &root,
            &Op::Destroy {
                ws: ws("alice"),
                force: true,
            },
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(o.overwrites_judged(), 1);
        assert_eq!(o.watching(), 0);
    }

    /// The GREEN half: a destroy that pinned the masked bytes into the
    /// workspace's own recovery namespace first is what bn-2k9e ships, and the
    /// oracle must accept it.
    #[test]
    fn destroyed_workspace_with_a_pin_is_clean() {
        let (_td, root, ws_path) = masked_repo(MASKED_STALE);
        let mut o = MaskedStalePreservation::new();
        o.record_masked("alice", "a.txt", MASKED_STALE);

        pin_recovery(&root, "alice", MASKED_STALE);
        std::fs::remove_dir_all(&ws_path).unwrap();
        let v = o.check_step(
            &root,
            &Op::Destroy {
                ws: ws("alice"),
                force: true,
            },
        );
        assert!(
            v.is_empty(),
            "a pinned destroy must read as preserved: {v:?}"
        );
        assert_eq!(o.overwrites_judged(), 1);
    }

    /// End-to-end plumbing sanity for the verifier's own git usage: clean →
    /// no lines; bn-p3m9-shaped tracked divergence → one line; untracked
    /// scratch → still no lines (repairing it would mean deleting it).
    #[test]
    fn tracked_status_reports_only_tracked_divergence() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} failed"
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@e.com"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", "i"]);
        assert!(tracked_status_lines(dir).unwrap().is_empty());

        std::fs::write(dir.join("a.txt"), "STALE\n").unwrap();
        let lines = tracked_status_lines(dir).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("a.txt"), "{lines:?}");

        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        std::fs::write(dir.join("scratch.tmp"), "x\n").unwrap();
        assert!(
            tracked_status_lines(dir).unwrap().is_empty(),
            "untracked scratch must never read as divergence"
        );
    }

    /// `checks_run` is the non-vacuity counter: it must actually advance when
    /// a live, armed workspace is judged.
    #[test]
    fn checks_run_counts_real_assertions() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        // Consolidated layout: the `.maw/manifold/` marker makes
        // `LayoutFlavor::detect` resolve workspaces under `.maw/workspaces/`.
        std::fs::create_dir_all(root.join(".maw").join("manifold")).unwrap();
        let ws_dir = root.join(".maw").join("workspaces").join("a");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} failed"
            );
        };
        git(&ws_dir, &["init", "-b", "main"]);
        git(&ws_dir, &["config", "user.email", "t@e.com"]);
        git(&ws_dir, &["config", "user.name", "T"]);
        std::fs::write(ws_dir.join("a.txt"), "hello\n").unwrap();
        git(&ws_dir, &["add", "-A"]);
        git(&ws_dir, &["commit", "-m", "i"]);

        let mut o = CleanMaterialization::new();
        assert!(o.check_step(root, &create("a"), true).is_empty());
        assert_eq!(o.checks_run(), 1, "the live armed workspace must be judged");

        // Plant the bn-p3m9 signature and re-check: the oracle must fire.
        std::fs::write(ws_dir.join("a.txt"), "STALE BYTES\n").unwrap();
        let v = o.check_step(root, &Op::Sync { ws: ws("zzz") }, true);
        assert_eq!(v.len(), 1, "{v:?}");
        match &v[0] {
            WorktreeViolation::WorktreeDivergedFromHead {
                workspace,
                status_lines,
                ..
            } => {
                assert_eq!(workspace, "a");
                assert_eq!(status_lines.len(), 1, "{status_lines:?}");
            }
            other @ WorktreeViolation::GitError { .. } => {
                panic!("expected WorktreeDivergedFromHead, got {other:?}")
            }
        }
    }
}
