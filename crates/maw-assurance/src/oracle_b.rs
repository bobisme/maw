//! Oracle B — state-coherence predicate for SG1 (bn-3ji6, T1.4).
//!
//! Implementation of `notes/oracle-ab-spec.md` §3 + §3.1. Oracle B holds iff
//! all of **B1 ∧ B2 ∧ B3 ∧ B4** hold over the post-state of a step:
//!
//! - **B1 no-dangling-oplog-head.** For every `refs/manifold/head/<ws>`:
//!   `ws/<ws>/` exists OR `<ws> ∈ LiveMergeSources`. This is *exactly* the
//!   bn-cm63 class — Oracle A (content reachability) misses it entirely
//!   because the dangling ref points at a recoverable oplog-head blob; only
//!   coherence is broken.
//! - **B2 owned-ref symmetry.** Same rule as B1 for the rest of
//!   [`maw_core::refs::workspace_owned_refs`] (`refs/manifold/epoch/ws/<ws>`,
//!   `refs/manifold/ws/<ws>`). Recovery refs are deliberately *exempt* —
//!   they must survive destroy (that is the whole point of recovery).
//! - **B3 merge-state coherence.** If `.manifold/merge-state.json` exists
//!   and the phase is non-terminal: every `sources[i]` has `ws/<src>/`
//!   on disk *or* a `refs/manifold/recovery/<src>/*` ref (a source may be
//!   legitimately destroyed mid-merge — bn-cm63's *defended* path);
//!   `epoch_before` resolves to a readable commit; if the phase is
//!   `commit` or `cleanup` (post-point-of-no-return) `epoch_after` must
//!   also resolve to a readable commit.
//! - **B4 recovery well-formed.** Every `refs/manifold/recovery/<ws>/<ts>`
//!   ref resolves to an object of type `commit` (not tree/blob/missing).
//!   Subsumes the existing G5/G6 discoverability + searchability checks.
//!
//! # Reuse contract (CRITICAL — bn-cm63 self-healing invariant)
//!
//! The `LiveMergeSources` guard (§3.1) MUST reuse the *exact same*
//! production logic that `crates/maw-cli/src/ref_gc.rs::live_merge_source_names`
//! uses, namely [`maw_core::merge_state::MergeStateFile::read`] +
//! [`maw_core::merge_state::MergeStateFile::staleness`] with
//! [`maw_core::merge_state::DEFAULT_STALE_AFTER_SECS`]. If the oracle's
//! guard ever drifts from the GC guard, one of them will reclaim a head
//! ref the other thinks is live (or vice-versa) and the bn-cm63 race
//! re-opens — silently, at the *verifier* layer. The single-source-of-
//! truth here is identical to `ref_gc.rs`'s call shape and is what makes
//! the oracle and the production GC self-consistent.
//!
//! B2 likewise uses [`maw_core::refs::workspace_owned_refs`] as the
//! single source of truth so that adding a new workspace-scoped ref kind
//! (one-line change there) automatically extends Oracle B coverage.
//!
//! # Independent-verifier carveout
//!
//! All git access in this module uses the `git` CLI on the bare
//! `repo.git` (resolved via the gitfile at the repo root, the same way
//! the existing `oracle.rs` does). This is deliberate — the oracle is the
//! independent verifier, and using `gix` here would couple the verifier
//! to the same code paths that are under test (a gix bug could mask a
//! genuine invariant violation). See `oracle.rs::read_all_refs` for the
//! prior-art `TODO(gix): assurance carveout` markers.
//!
//! # Cost
//!
//! O(#refs + |sources|) per call — bounded by extant workspaces +
//! GC-retained recovery refs + merge-state size; independent of step
//! count. B4 batches the per-recovery-ref `cat-file` probe into a single
//! `git cat-file --batch-check` round-trip (one fork instead of one per
//! ref) so the dominant cost is sub-millisecond for the typical hundreds-
//! of-recovery-refs repo. No incremental design required (contrast with
//! Oracle A, which is mandatorily incremental).
//!
//! # bn-cm63 reproduction (the canonical Oracle-B violation)
//!
//! 1. workspace `<ws>` exists, in-flight merge with `<ws>` as a source.
//! 2. `maw ws destroy <ws> --force` races the merge's
//!    `record_merge_operations` -> `ensure_workspace_oplog_head` re-bootstrap.
//! 3. After both finish: `ws/<ws>/` is gone, but
//!    `refs/manifold/head/<ws>` is still present, and merge-state is
//!    terminal (or absent — the merge completed).
//! 4. Oracle A: green. No committed work was lost; the merged content
//!    landed in `default` and a recovery snapshot was pinned.
//! 5. Oracle B: **B1 RED**. The dangling head ref is exactly the leak
//!    the bn-cm63 fix prevents at write time and `maw gc` self-heals at
//!    sweep time.

use std::collections::HashMap;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use maw_core::merge_state::{DEFAULT_STALE_AFTER_SECS, MergeStateFile, Staleness};
use maw_core::model::layout::LayoutFlavor;
use maw_core::refs;

/// Layout-aware "does workspace `<name>` exist on disk?" predicate.
///
/// Pre-T3.2 this was `repo_root.join("ws").join(name).exists()` everywhere
/// in oracle_b. T3.3 (bn-3kkl) migrates to the consolidated layout where
/// workspaces live under `.maw/workspaces/<name>/`; the privileged
/// "default" workspace IS the repo root itself. This helper centralizes
/// that lookup so the migrated repo passes the oracle without re-writing
/// every call site to pass a `LayoutFlavor`.
fn ws_dir_exists(repo_root: &Path, name: &str) -> bool {
    let flavor = LayoutFlavor::detect(repo_root);
    if matches!(flavor, LayoutFlavor::ConsolidatedMawDir) && name == "default" {
        // The root checkout IS the default workspace under the
        // consolidated layout; always treat it as present.
        return repo_root.is_dir();
    }
    flavor.workspace_path(repo_root, name).is_dir()
}

/// Layout-aware path to the `.manifold/` directory used for the merge-state
/// file lookup. Mirrors the rule applied throughout the rest of the
/// codebase (T3.2 / bn-2sw3): v2 → `<root>/.manifold/`, consolidated →
/// `<root>/.maw/manifold/`.
fn manifold_dir_path(repo_root: &Path) -> std::path::PathBuf {
    LayoutFlavor::detect(repo_root).manifold_dir(repo_root)
}

// ---------------------------------------------------------------------------
// Violation type
// ---------------------------------------------------------------------------

/// A specific Oracle-B coherence violation.
///
/// Each variant maps to one of B1..B4 and carries enough context for a
/// human (and the shrinker, T1.6) to root-cause the failing seed without
/// consulting the trace file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleBViolation {
    /// **B1** — `refs/manifold/head/<ws>` is present but `ws/<ws>/` does
    /// not exist and no *live* in-flight merge has `<ws>` as a source.
    /// This is the bn-cm63 class.
    DanglingHeadRef {
        /// Workspace name parsed from the ref (`refs/manifold/head/<ws>`).
        workspace: String,
        /// The full ref name (always `refs/manifold/head/<workspace>`).
        ref_name: String,
        /// The OID the ref pointed at, for diagnostics.
        oid: String,
    },

    /// **B2** — A workspace-owned ref (`refs/manifold/ws/<ws>` or
    /// `refs/manifold/epoch/ws/<ws>`) is present for a non-existent and
    /// non-protected workspace. Same coherence-defect class as B1 but
    /// for the rest of the owned ref set.
    DanglingOwnedRef {
        /// Workspace name the ref scopes to.
        workspace: String,
        /// The full ref name (one of the entries
        /// [`maw_core::refs::workspace_owned_refs`] returns).
        ref_name: String,
        /// The OID the ref pointed at, for diagnostics.
        oid: String,
    },

    /// **B3** — A non-terminal `merge-state.json` claims `<src>` as a
    /// frozen source, but `<src>` has neither `ws/<src>/` on disk nor a
    /// `refs/manifold/recovery/<src>/*` ref. Either the workspace was
    /// destroyed *without* recovery (Prime-Invariant adjacent) or the
    /// merge-state's source list is incoherent with reality.
    MergeStateOrphanSource {
        /// The source workspace from `merge_state.sources` that has no
        /// surviving footprint.
        source: String,
        /// Current merge phase (informational).
        phase: String,
    },

    /// **B3** — `epoch_before` (or `epoch_after` post-COMMIT) in the
    /// merge-state does not resolve to a readable commit. A merge's
    /// frozen epochs are the *only* anchors recovery has if the merge
    /// crashes; if they cannot be read, recovery is impossible.
    MergeStateBadEpoch {
        /// Which epoch field was bad (`"epoch_before"` or `"epoch_after"`).
        which: &'static str,
        /// The OID claimed by the merge-state.
        oid: String,
        /// What went wrong (object type or unresolvable).
        reason: String,
    },

    /// **B4** — A `refs/manifold/recovery/<ws>/<ts>` ref resolves to an
    /// object that is missing or is not a commit (tree, blob, tag, etc.).
    /// Such a ref cannot ever satisfy `maw ws recover` — it is an
    /// orphaned/garbage recovery and violates the Prime Invariant's
    /// recovery surface (and subsumes G5/G6).
    RecoveryRefMalformed {
        /// The recovery ref name.
        ref_name: String,
        /// The OID the ref pointed at.
        oid: String,
        /// What was wrong (`"<missing>"` or `"object is a <type>, expected commit"`).
        reason: String,
    },

    /// A git CLI invocation by the oracle itself failed in a way that
    /// prevents us from making a verdict. Reported as a violation so the
    /// run stops loudly instead of silently green-lighting on broken
    /// tooling — the harness can distinguish this from a real defect by
    /// matching on the variant.
    GitError {
        /// Which Oracle-B check was running (`"B1"`, `"B3"`, `"B4"`, ...).
        check: &'static str,
        /// The command that failed, for the failure bundle.
        command: String,
        /// Stderr from the command.
        stderr: String,
    },
}

impl fmt::Display for OracleBViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DanglingHeadRef {
                workspace,
                ref_name,
                oid,
            } => write!(
                f,
                "B1 violation: dangling {ref_name} -> {oid} for non-existent \
                 workspace '{workspace}' (no ws/{workspace}/ on disk and no \
                 live in-flight merge has it as a source) — bn-cm63 class"
            ),
            Self::DanglingOwnedRef {
                workspace,
                ref_name,
                oid,
            } => write!(
                f,
                "B2 violation: dangling owned ref {ref_name} -> {oid} for \
                 non-existent workspace '{workspace}'"
            ),
            Self::MergeStateOrphanSource { source, phase } => write!(
                f,
                "B3 violation: merge-state (phase={phase}) claims '{source}' \
                 as a frozen source but ws/{source}/ does not exist and no \
                 refs/manifold/recovery/{source}/* ref pins its content"
            ),
            Self::MergeStateBadEpoch { which, oid, reason } => write!(
                f,
                "B3 violation: merge-state {which}={oid} does not resolve to \
                 a readable commit ({reason})"
            ),
            Self::RecoveryRefMalformed {
                ref_name,
                oid,
                reason,
            } => write!(
                f,
                "B4 violation: recovery ref {ref_name} -> {oid} is malformed \
                 ({reason})"
            ),
            Self::GitError {
                check,
                command,
                stderr,
            } => write!(
                f,
                "Oracle B {check}: git error running `{command}`: {stderr}"
            ),
        }
    }
}

impl std::error::Error for OracleBViolation {}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run all four Oracle-B coherence checks (B1..B4) against the live repo.
///
/// Returns *every* violation found (not just the first), in B1→B2→B3→B4
/// order with deterministic intra-check order.
///
/// The harness typically calls this once per step and treats a non-empty
/// return as a release-blocking failure (sg1-dst-architecture.md §4.3).
/// Returning every violation rather than the first is a deliberate
/// shrinker-friendliness choice: the shrinker (T1.6 bn-32k3) prefers a
/// minimal repro that trips the *same* set of violations, so reporting
/// the full set up-front avoids extra replay rounds chasing newly-
/// exposed siblings.
///
/// # Errors
///
/// This function returns `Err` only if the oracle itself cannot run
/// (e.g. `git for-each-ref` fails to spawn). In that case the harness
/// must treat the result as inconclusive — *not* as "Oracle B green".
#[must_use]
pub fn check(repo_root: &Path) -> Vec<OracleBViolation> {
    let mut violations = Vec::new();

    // --- Gather ground-truth state once (the four checks all consult
    //     the same view; capturing once amortises the ref listing). ---
    let all_refs = match list_all_refs(repo_root) {
        Ok(r) => r,
        Err(v) => {
            violations.push(v);
            return violations; // No refs → no further verdict possible.
        }
    };

    // bn-cm63 §3.1: REUSE production live-merge classification verbatim.
    // The shape of this call is identical to
    // `crates/maw-cli/src/ref_gc.rs::live_merge_source_names`, so the
    // oracle and the GC guard can never disagree about what "live" means.
    let live_sources = live_merge_source_names(repo_root);

    // --- B1: no-dangling-oplog-head ---
    let mut head_violations: Vec<OracleBViolation> = all_refs
        .iter()
        .filter_map(|(name, oid)| {
            let ws = name.strip_prefix(refs::HEAD_PREFIX)?;
            if ws.is_empty() {
                return None;
            }
            if ws_dir_exists(repo_root, ws) {
                return None;
            }
            if live_sources.contains(ws) {
                // The bn-cm63 *defended* path: a real merge owns this oplog
                // head right now; pruning/flagging it would re-open the race.
                return None;
            }
            Some(OracleBViolation::DanglingHeadRef {
                workspace: ws.to_owned(),
                ref_name: name.clone(),
                oid: oid.clone(),
            })
        })
        .collect();
    head_violations.sort_by(|a, b| match (a, b) {
        (
            OracleBViolation::DanglingHeadRef { ref_name: x, .. },
            OracleBViolation::DanglingHeadRef { ref_name: y, .. },
        ) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    violations.extend(head_violations);

    // --- B2: owned-ref symmetry (state ref + epoch ref) ---
    // We enumerate the OTHER owned refs (not head — B1 covered it) and
    // apply the same liveness rule. The set of "other owned refs" is
    // derived from `workspace_owned_refs` per ws-name we discover so a
    // new ref kind added there is automatically covered.
    let head_prefix = refs::HEAD_PREFIX;
    let mut owned_violations: Vec<OracleBViolation> = Vec::new();
    for (name, oid) in &all_refs {
        // Identify which workspace this ref scopes to by matching against
        // the two non-head owned-ref shapes.
        let ws_name = if let Some(ws) = name.strip_prefix(refs::WORKSPACE_STATE_PREFIX) {
            ws
        } else if let Some(ws) = name.strip_prefix(refs::WORKSPACE_EPOCH_PREFIX) {
            ws
        } else {
            continue;
        };
        if ws_name.is_empty() {
            continue;
        }
        // Defence-in-depth: assert this ref is actually in the owned set.
        // (If `workspace_owned_refs` ever drops one of state/epoch we want
        // to *narrow*, not falsely flag.) Skipping silently in the
        // unlikely mismatch case keeps us false-positive-free.
        if !refs::workspace_owned_refs(ws_name).contains(name) {
            continue;
        }
        // Skip if it happens to be the head ref (already handled by B1).
        if name.starts_with(head_prefix) {
            continue;
        }
        if ws_dir_exists(repo_root, ws_name) {
            continue;
        }
        if live_sources.contains(ws_name) {
            continue;
        }
        owned_violations.push(OracleBViolation::DanglingOwnedRef {
            workspace: ws_name.to_owned(),
            ref_name: name.clone(),
            oid: oid.clone(),
        });
    }
    owned_violations.sort_by(|a, b| match (a, b) {
        (
            OracleBViolation::DanglingOwnedRef { ref_name: x, .. },
            OracleBViolation::DanglingOwnedRef { ref_name: y, .. },
        ) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    violations.extend(owned_violations);

    // --- B3: merge-state coherence ---
    violations.extend(check_b3_merge_state(repo_root, &all_refs));

    // --- B4: recovery well-formed ---
    violations.extend(check_b4_recovery(repo_root, &all_refs));

    violations
}

/// Convenience wrapper: returns `Ok(())` if Oracle B is green, or the
/// first violation if not. Used by tests that only need a pass/fail
/// verdict.
///
/// # Errors
///
/// Returns the first violation found (B1 < B2 < B3 < B4 deterministic
/// order), or `Ok(())` if all four predicates hold.
pub fn check_first(repo_root: &Path) -> Result<(), OracleBViolation> {
    check(repo_root).into_iter().next().map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// **REUSE GUARD** — call into `maw_core::merge_state` verbatim. See
/// `crates/maw-cli/src/ref_gc.rs::live_merge_source_names` for the
/// identical caller shape. Do not inline-reimplement: any divergence
/// between this and the GC guard re-opens the bn-cm63 race at the
/// verifier layer (the spike's pid-liveness approximation was the bug
/// SP2 §3.1 calls out explicitly).
fn live_merge_source_names(repo_root: &Path) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let state_path = MergeStateFile::default_path(&repo_root.join(".manifold"));
    let Ok(state) = MergeStateFile::read(&state_path) else {
        return names;
    };
    if state.phase.is_terminal() {
        return names;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if matches!(
        state.staleness(now, DEFAULT_STALE_AFTER_SECS),
        Staleness::Live
    ) {
        for s in &state.sources {
            names.insert(s.as_str().to_string());
        }
    }
    names
}

/// List every ref in the repo as `(name, oid)`. Uses the bare repo's
/// git CLI via the gitfile at `repo_root/.git` (which points at
/// `repo.git`). Independent-verifier carveout: deliberately CLI, not
/// `maw-git`/`gix`.
fn list_all_refs(repo_root: &Path) -> Result<Vec<(String, String)>, OracleBViolation> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(refname) %(objectname)"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| OracleBViolation::GitError {
            check: "B1",
            command: "git for-each-ref".to_owned(),
            stderr: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(OracleBViolation::GitError {
            check: "B1",
            command: "git for-each-ref".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, o)| (n.to_owned(), o.to_owned()))
        .collect())
}

fn check_b3_merge_state(repo_root: &Path, all_refs: &[(String, String)]) -> Vec<OracleBViolation> {
    let state_path = MergeStateFile::default_path(&manifold_dir_path(repo_root));
    let Ok(state) = MergeStateFile::read(&state_path) else {
        return Vec::new(); // No merge-state file — nothing to check.
    };
    if state.phase.is_terminal() {
        return Vec::new(); // Complete/Aborted — phase-specific invariants don't apply.
    }

    let mut violations = Vec::new();

    // --- Sources coherence ---
    let recovery_prefix = "refs/manifold/recovery/";
    // Build a fast set of "<src>" names that have at least one recovery ref.
    let recovery_ws_names: std::collections::HashSet<String> = all_refs
        .iter()
        .filter_map(|(n, _)| n.strip_prefix(recovery_prefix))
        .filter_map(|rest| rest.split('/').next().filter(|s| !s.is_empty()))
        .map(ToOwned::to_owned)
        .collect();

    // Iterate sources in their canonical (deterministic) order.
    for src in &state.sources {
        let src_name = src.as_str();
        let ws_present = ws_dir_exists(repo_root, src_name);
        let pinned_in_recovery = recovery_ws_names.contains(src_name);
        if !ws_present && !pinned_in_recovery {
            violations.push(OracleBViolation::MergeStateOrphanSource {
                source: src_name.to_owned(),
                phase: state.phase.to_string(),
            });
        }
    }

    // --- Epoch resolvability (one batched probe for both fields). ---
    let epoch_before_oid: String = state.epoch_before.as_str().to_owned();
    let mut to_probe: Vec<(&'static str, String)> = vec![("epoch_before", epoch_before_oid)];

    let post_point_of_no_return = matches!(
        state.phase,
        maw_core::merge_state::MergePhase::Commit | maw_core::merge_state::MergePhase::Cleanup
    );
    if post_point_of_no_return {
        if let Some(ea) = &state.epoch_after {
            to_probe.push(("epoch_after", ea.as_str().to_owned()));
        } else {
            // Post-COMMIT but no recorded epoch_after — also a coherence break.
            violations.push(OracleBViolation::MergeStateBadEpoch {
                which: "epoch_after",
                oid: "<missing>".to_owned(),
                reason: "phase is past the point-of-no-return but \
                         epoch_after is not recorded"
                    .to_owned(),
            });
        }
    }

    let oids: Vec<String> = to_probe.iter().map(|(_, o)| o.clone()).collect();
    match batch_object_types(repo_root, &oids) {
        Ok(types) => {
            for ((which, oid), kind) in to_probe.iter().zip(types.iter()) {
                match kind.as_deref() {
                    Some("commit") => { /* good */ }
                    Some(other) => violations.push(OracleBViolation::MergeStateBadEpoch {
                        which,
                        oid: oid.clone(),
                        reason: format!("object is a {other}, expected commit"),
                    }),
                    None => violations.push(OracleBViolation::MergeStateBadEpoch {
                        which,
                        oid: oid.clone(),
                        reason: "<missing>".to_owned(),
                    }),
                }
            }
        }
        Err(v) => violations.push(v),
    }

    violations
}

fn check_b4_recovery(repo_root: &Path, all_refs: &[(String, String)]) -> Vec<OracleBViolation> {
    let recovery_prefix = "refs/manifold/recovery/";
    // Stable order: sort by ref name so the violation list is
    // bit-deterministic across runs (matches SG1 §5 contract).
    let mut recovery: Vec<(&String, &String)> = all_refs
        .iter()
        .filter(|(n, _)| n.starts_with(recovery_prefix))
        .map(|(n, o)| (n, o))
        .collect();
    recovery.sort_by_key(|(n, _)| n.as_str());

    if recovery.is_empty() {
        return Vec::new();
    }

    let oids: Vec<String> = recovery.iter().map(|(_, o)| (*o).clone()).collect();
    let types = match batch_object_types(repo_root, &oids) {
        Ok(t) => t,
        Err(v) => return vec![v],
    };

    let mut violations = Vec::new();
    for ((name, oid), kind) in recovery.iter().zip(types.iter()) {
        match kind.as_deref() {
            Some("commit") => { /* well-formed */ }
            Some(other) => violations.push(OracleBViolation::RecoveryRefMalformed {
                ref_name: (*name).clone(),
                oid: (*oid).clone(),
                reason: format!("object is a {other}, expected commit"),
            }),
            None => violations.push(OracleBViolation::RecoveryRefMalformed {
                ref_name: (*name).clone(),
                oid: (*oid).clone(),
                reason: "<missing>".to_owned(),
            }),
        }
    }
    violations
}

/// Batched `git cat-file --batch-check` for a slice of OIDs. Returns a
/// `Vec<Option<String>>` aligned with `oids`: `Some("commit"|"tree"|...)`
/// when the object exists, `None` when git reports it as `<oid> missing`.
///
/// This is the §3.2 batching optimisation — one fork instead of one per
/// recovery ref. On a typical maw repo with ~100 recovery refs the cost
/// drops from ~30 ms (cat-file per ref) to ~1 ms (single round-trip),
/// well under the 5 ms/call budget.
fn batch_object_types(
    repo_root: &Path,
    oids: &[String],
) -> Result<Vec<Option<String>>, OracleBViolation> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }

    let mut child = Command::new("git")
        .args(["cat-file", "--batch-check=%(objecttype)"])
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| OracleBViolation::GitError {
            check: "B4",
            command: "git cat-file --batch-check".to_owned(),
            stderr: e.to_string(),
        })?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| OracleBViolation::GitError {
                check: "B4",
                command: "git cat-file --batch-check".to_owned(),
                stderr: "failed to open stdin".to_owned(),
            })?;
        for oid in oids {
            // One OID per line; git emits one type per OID in input order.
            writeln!(stdin, "{oid}").map_err(|e| OracleBViolation::GitError {
                check: "B4",
                command: "git cat-file --batch-check".to_owned(),
                stderr: format!("write {oid}: {e}"),
            })?;
        }
        // Dropping `stdin` closes it, signalling EOF to git.
    }

    let output = child
        .wait_with_output()
        .map_err(|e| OracleBViolation::GitError {
            check: "B4",
            command: "git cat-file --batch-check".to_owned(),
            stderr: e.to_string(),
        })?;
    // cat-file --batch-check exits 0 even with missing objects (it prints
    // "<oid> missing" on stdout). A non-zero status is a real failure.
    if !output.status.success() {
        return Err(OracleBViolation::GitError {
            check: "B4",
            command: "git cat-file --batch-check".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() != oids.len() {
        return Err(OracleBViolation::GitError {
            check: "B4",
            command: "git cat-file --batch-check".to_owned(),
            stderr: format!(
                "expected {} lines, got {}: {}",
                oids.len(),
                lines.len(),
                stdout.trim()
            ),
        });
    }

    let mut out = Vec::with_capacity(oids.len());
    for line in lines {
        let trimmed = line.trim();
        // Missing object format: "<oid> missing"
        if trimmed.ends_with(" missing") || trimmed == "missing" {
            out.push(None);
        } else {
            out.push(Some(trimmed.to_owned()));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Doctor-agreement helper (for tests + future shrinker triage)
// ---------------------------------------------------------------------------

/// What `maw doctor`'s coherence checks (`stale head refs` + `merge-state`)
/// say about the repo, distilled into the Oracle B verdict vocabulary.
///
/// Used by the doctor-agreement test battery to assert the oracle and
/// `maw doctor` agree on a hand-built incoherent state.
///
/// This is intentionally a *thin* abstraction over what `doctor.rs`
/// already computes (and that maintains the invariant that the two
/// reach the same verdict on the same state). It does **not** invoke
/// `maw doctor`; instead it calls into the same primitives the doctor
/// uses, so the test is fast and hermetic.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DoctorVerdict {
    /// Number of stale head refs the doctor would warn about.
    /// (Matches `ref_gc::count_stale_head_refs`.)
    pub stale_head_refs: usize,
    /// Whether `maw doctor` would consider merge-state incoherent
    /// (`Orphaned`, `Indeterminate`, terminal-leftover, or unreadable).
    /// Live or absent → `false`.
    pub merge_state_incoherent: bool,
}

/// Compute the [`DoctorVerdict`] for a repo by calling the same primitives
/// `maw doctor` uses. Tests assert this matches Oracle B's verdict on a
/// battery of hand-built states.
#[must_use]
pub fn doctor_verdict(repo_root: &Path) -> DoctorVerdict {
    // Stale head refs: replicate `count_stale_head_refs` *without* depending
    // on the maw-cli crate (avoids a circular dep). We use the same data
    // sources — for-each-ref output + ws/<name>/ existence — and apply the
    // same live-merge protection. (Note: count_stale_head_refs in ref_gc
    // does NOT exempt live-merge sources; that protection only kicks in at
    // the deletion path. We match that behaviour: a head ref for a missing
    // ws is "stale" from doctor's POV even if a live merge has it pinned.)
    let stale_head_refs = list_all_refs(repo_root).map_or(0, |refs_| {
        refs_
            .iter()
            .filter(|(name, _)| {
                let Some(ws) = name.strip_prefix(refs::HEAD_PREFIX) else {
                    return false;
                };
                !ws.is_empty() && !ws_dir_exists(repo_root, ws)
            })
            .count()
    });

    // Merge-state coherence: replicate `check_merge_state`'s incoherent
    // verdict (terminal-leftover, orphaned, indeterminate, unreadable).
    let state_path = MergeStateFile::default_path(&manifold_dir_path(repo_root));
    let merge_state_incoherent = match MergeStateFile::read(&state_path) {
        Err(_) if state_path.exists() => true,  // unreadable
        Err(_) => false,                        // legitimately absent
        Ok(s) if s.phase.is_terminal() => true, // leftover terminal
        Ok(s) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            !matches!(s.staleness(now, DEFAULT_STALE_AFTER_SECS), Staleness::Live)
        }
    };

    DoctorVerdict {
        stale_head_refs,
        merge_state_incoherent,
    }
}

/// Distil Oracle B's verdict into the same vocabulary as
/// [`DoctorVerdict`] so the agreement test is a direct comparison.
#[must_use]
pub fn oracle_b_verdict(violations: &[OracleBViolation]) -> DoctorVerdict {
    let stale_head_refs = violations
        .iter()
        .filter(|v| matches!(v, OracleBViolation::DanglingHeadRef { .. }))
        .count();
    let merge_state_incoherent = violations.iter().any(|v| {
        matches!(
            v,
            OracleBViolation::MergeStateOrphanSource { .. }
                | OracleBViolation::MergeStateBadEpoch { .. }
        )
    });
    DoctorVerdict {
        stale_head_refs,
        merge_state_incoherent,
    }
}

// ---------------------------------------------------------------------------
// Re-exports of types tests touch
// ---------------------------------------------------------------------------

#[doc(hidden)]
#[must_use]
pub fn _internal_oracle_paths(repo_root: &Path) -> (PathBuf, HashMap<String, String>) {
    // Visible only for test fixtures that want to inspect what the oracle
    // is looking at. Not part of the public surface. Intentionally
    // not exposed as `pub` outside test infra.
    let state = MergeStateFile::default_path(&manifold_dir_path(repo_root));
    let refs_map: HashMap<String, String> = list_all_refs(repo_root)
        .unwrap_or_default()
        .into_iter()
        .collect();
    (state, refs_map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::Instant;
    use tempfile::TempDir;

    use maw_core::merge_state::MergePhase;
    use maw_core::model::types::{EpochId, WorkspaceId};

    // ----- helpers ---------------------------------------------------------

    /// Set up a non-bare temp repo (mirrors the `oracle.rs` test style).
    /// Production maw repos are bare; the oracle uses git CLI either way
    /// (git CLI follows the gitfile / discovers .git, both work).
    fn setup_repo() -> (TempDir, String) {
        let dir = TempDir::new().expect("tmpdir");
        let root = dir.path();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(root)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed");
        }
        fs::write(root.join("README.md"), "# test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        fs::create_dir_all(root.join("ws")).unwrap();
        fs::create_dir_all(root.join(".manifold")).unwrap();
        (dir, oid)
    }

    fn write_ref(root: &Path, name: &str, oid: &str) {
        let out = Command::new("git")
            .args(["update-ref", name, oid])
            .current_dir(root)
            .output()
            .expect("git update-ref");
        assert!(
            out.status.success(),
            "update-ref {name} {oid} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn make_ws_dir(root: &Path, name: &str) {
        fs::create_dir_all(root.join("ws").join(name)).unwrap();
    }

    /// Build a `merge-state.json` owned by **this** process so
    /// `staleness()` classifies it as `Live`. Mirrors the helper in
    /// `crates/maw-cli/src/ref_gc.rs` tests. `epoch_oid` is used for
    /// `epoch_before` so B3's epoch-resolvability check passes for tests
    /// that aren't trying to exercise that path.
    fn write_live_merge_state_with_epoch(root: &Path, sources: &[&str], epoch_oid: &str) {
        let manifold = root.join(".manifold");
        fs::create_dir_all(&manifold).unwrap();
        let epoch = EpochId::new(epoch_oid).expect("epoch");
        let ws_ids: Vec<WorkspaceId> = sources
            .iter()
            .map(|s| WorkspaceId::new(s).expect("ws id"))
            .collect();
        let mut state = MergeStateFile::new(ws_ids, epoch, 0);
        state.stamp_owner();
        state.advance(MergePhase::Build, 1).unwrap();
        state.advance(MergePhase::Validate, 2).unwrap();
        state
            .write_atomic(&MergeStateFile::default_path(&manifold))
            .unwrap();
    }

    /// Back-compat shim for tests that don't care about epoch validity
    /// (i.e. they exercise B1/B2 only, not B3). Uses a placeholder OID.
    fn write_live_merge_state(root: &Path, sources: &[&str]) {
        write_live_merge_state_with_epoch(root, sources, &"a".repeat(40));
    }

    /// Same as `write_live_merge_state` but with a recorded foreign-host
    /// pid (so liveness is Unknown and staleness depends on age).
    fn write_dead_merge_state(root: &Path, sources: &[&str], phase: MergePhase) {
        let manifold = root.join(".manifold");
        fs::create_dir_all(&manifold).unwrap();
        let epoch = EpochId::new(&"a".repeat(40)).expect("epoch");
        let ws_ids: Vec<WorkspaceId> = sources
            .iter()
            .map(|s| WorkspaceId::new(s).expect("ws id"))
            .collect();
        let mut state = MergeStateFile::new(ws_ids, epoch, 0);
        // Owner pid recorded for a host we are not on → Liveness::Unknown.
        state.owner_pid = Some(1);
        state.owner_host = Some("definitely-not-this-host".to_owned());
        // Drive to the requested non-terminal phase.
        match phase {
            MergePhase::Prepare => {}
            MergePhase::Build => {
                state.advance(MergePhase::Build, 1).unwrap();
            }
            MergePhase::Validate => {
                state.advance(MergePhase::Build, 1).unwrap();
                state.advance(MergePhase::Validate, 2).unwrap();
            }
            MergePhase::Commit => {
                state.advance(MergePhase::Build, 1).unwrap();
                state.advance(MergePhase::Validate, 2).unwrap();
                state.advance(MergePhase::Commit, 3).unwrap();
            }
            MergePhase::Cleanup => {
                state.advance(MergePhase::Build, 1).unwrap();
                state.advance(MergePhase::Validate, 2).unwrap();
                state.advance(MergePhase::Commit, 3).unwrap();
                state.advance(MergePhase::Cleanup, 4).unwrap();
            }
            MergePhase::Complete | MergePhase::Aborted => {
                panic!("helper is for non-terminal phases")
            }
        }
        state
            .write_atomic(&MergeStateFile::default_path(&manifold))
            .unwrap();
    }

    // ----- B1: bn-cm63 reproduction ----------------------------------------

    /// **bn-cm63 reproduction (B1 RED).**
    ///
    /// Recreates the exact state the bn-cm63 incident left behind:
    /// `refs/manifold/head/ghost` is present, `ws/ghost/` does not exist,
    /// and merge-state is absent (the racing merge completed). The bn-cm63
    /// fix prevents this at write time and `maw gc` self-heals it; Oracle
    /// B must catch it independently. This is the central test for T1.4 —
    /// it is the predicate's reason for existing.
    ///
    /// See:
    ///   - `notes/maw-bones-and-prime-invariant-defects.md` (the original
    ///     incident report; root cause = `record_merge_operations` ->
    ///     `ensure_workspace_oplog_head` re-bootstrap after destroy)
    ///   - `crates/maw-cli/src/ref_gc.rs::plain_gc_prunes_dangling_head_ref`
    ///     (the GC-side regression test that catches the same class).
    #[test]
    fn b1_fires_on_bn_cm63_reproduction() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        // Dangling head ref for a workspace that no longer exists, no
        // merge-state in flight.
        write_ref(root, &refs::workspace_head_ref("ghost"), &oid);

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::DanglingHeadRef { workspace, .. } if workspace == "ghost"
            )),
            "Oracle B must fire B1 on the bn-cm63 reproduction; got: {vs:?}"
        );
    }

    /// **bn-cm63 LIVE-MERGE false-positive protection.**
    ///
    /// Same dangling-ref state as the bn-cm63 reproduction, but **a live
    /// merge owns the missing ws as a source**. This is the legitimate
    /// in-flight-merge case the bn-cm63 fix *defends* (not the case it
    /// prevents). Oracle B MUST NOT fire — pruning/flagging here would
    /// re-introduce the bn-cm63 race from the verifier side, exactly
    /// the failure SP2 §3.1 calls out.
    #[test]
    fn b1_does_not_fire_when_live_merge_protects_source() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_ref(root, &refs::workspace_head_ref("inflight"), &oid);
        // Live merge (owner pid == ours) lists `inflight` as a source.
        write_live_merge_state(root, &["inflight"]);

        let vs = check(root);
        assert!(
            !vs.iter().any(|v| matches!(
                v,
                OracleBViolation::DanglingHeadRef { workspace, .. } if workspace == "inflight"
            )),
            "Oracle B must NOT fire B1 when a LIVE merge protects the source — \
             that would re-introduce bn-cm63's race at the verifier layer. \
             Got: {vs:?}"
        );
    }

    /// Live-merge protection is *narrow*: only the specific workspaces
    /// listed in `merge_state.sources` are protected. A different
    /// dangling head ref must still trip B1 even when an unrelated merge
    /// is live.
    #[test]
    fn b1_fires_on_non_source_dangling_head_even_with_live_merge() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_ref(root, &refs::workspace_head_ref("ghost"), &oid);
        // Live merge for a *different* workspace.
        write_live_merge_state(root, &["inflight"]);

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::DanglingHeadRef { workspace, .. } if workspace == "ghost"
            )),
            "B1 must still fire on 'ghost' (not a merge source): {vs:?}"
        );
        // And not on 'inflight'.
        assert!(!vs.iter().any(|v| matches!(
            v,
            OracleBViolation::DanglingHeadRef { workspace, .. } if workspace == "inflight"
        )));
    }

    // ----- B2: owned-ref symmetry ------------------------------------------

    #[test]
    fn b2_fires_on_dangling_state_ref() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_ref(root, &refs::workspace_state_ref("ghost"), &oid);

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::DanglingOwnedRef { ref_name, .. }
                    if ref_name == "refs/manifold/ws/ghost"
            )),
            "B2 must flag a dangling state ref: {vs:?}"
        );
    }

    #[test]
    fn b2_fires_on_dangling_epoch_ref() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_ref(root, &refs::workspace_epoch_ref("ghost"), &oid);

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::DanglingOwnedRef { ref_name, .. }
                    if ref_name == "refs/manifold/epoch/ws/ghost"
            )),
            "B2 must flag a dangling per-ws epoch ref: {vs:?}"
        );
    }

    /// Recovery refs MUST survive destroy. B2 must NOT flag them under
    /// any circumstance (this is the explicit carve-out in §3 B2).
    #[test]
    fn b2_exempts_recovery_refs() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_ref(root, "refs/manifold/recovery/ghost/20260301-000000", &oid);

        let vs = check(root);
        // Recovery ref pointing at a commit is well-formed (B4 green) and
        // is intentionally outside the owned-ref set (B2 green).
        assert!(
            !vs.iter()
                .any(|v| matches!(v, OracleBViolation::DanglingOwnedRef { .. })),
            "B2 must NOT flag recovery refs (they must survive destroy): {vs:?}"
        );
        assert!(
            !vs.iter()
                .any(|v| matches!(v, OracleBViolation::RecoveryRefMalformed { .. })),
            "B4 well-formed recovery ref must not be flagged: {vs:?}"
        );
    }

    // ----- B3: merge-state coherence ---------------------------------------

    #[test]
    fn b3_fires_on_orphan_source() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        // Dead merge listing `gone` as a source; gone has no ws dir and no
        // recovery ref. The epoch is the real HEAD so the epoch check
        // passes — only the source check trips.
        write_dead_merge_state(root, &["gone"], MergePhase::Validate);
        // Use the real HEAD OID for the merge-state's epoch_before by
        // re-writing the merge-state file directly: simpler to manually
        // poke for the test.
        let manifold = root.join(".manifold");
        let state_path = MergeStateFile::default_path(&manifold);
        let mut state = MergeStateFile::read(&state_path).unwrap();
        state.epoch_before = EpochId::new(&oid).unwrap();
        state.write_atomic(&state_path).unwrap();

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::MergeStateOrphanSource { source, .. } if source == "gone"
            )),
            "B3 must flag a source with no ws/ and no recovery ref: {vs:?}"
        );
    }

    #[test]
    fn b3_accepts_destroyed_source_with_recovery_ref() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_dead_merge_state(root, &["destroyed"], MergePhase::Validate);
        // Pin the destroyed ws's content via a recovery ref — this is the
        // bn-cm63 *defended* path (the workspace was legitimately
        // destroyed mid-merge but its content lives in recovery).
        write_ref(
            root,
            "refs/manifold/recovery/destroyed/20260301-000000",
            &oid,
        );
        // Make epoch_before resolvable.
        let state_path = MergeStateFile::default_path(&root.join(".manifold"));
        let mut state = MergeStateFile::read(&state_path).unwrap();
        state.epoch_before = EpochId::new(&oid).unwrap();
        state.write_atomic(&state_path).unwrap();

        let vs = check(root);
        assert!(
            !vs.iter()
                .any(|v| matches!(v, OracleBViolation::MergeStateOrphanSource { .. })),
            "B3 must NOT flag a source whose content is pinned in recovery: {vs:?}"
        );
    }

    #[test]
    fn b3_fires_on_unreadable_epoch_before() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        // ws_dir present so source check passes.
        make_ws_dir(root, "src");
        write_dead_merge_state(root, &["src"], MergePhase::Validate);
        // Force epoch_before to an OID that doesn't exist in the repo.
        let bogus = "0123456789abcdef0123456789abcdef01234567";
        let state_path = MergeStateFile::default_path(&root.join(".manifold"));
        let mut state = MergeStateFile::read(&state_path).unwrap();
        state.epoch_before = EpochId::new(bogus).unwrap();
        state.write_atomic(&state_path).unwrap();
        // Keep oid alive so the test compiles cleanly.
        let _ = oid;

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::MergeStateBadEpoch {
                    which: "epoch_before",
                    ..
                }
            )),
            "B3 must flag an unreadable epoch_before: {vs:?}"
        );
    }

    #[test]
    fn b3_requires_epoch_after_post_commit() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "src");
        write_dead_merge_state(root, &["src"], MergePhase::Commit);
        let state_path = MergeStateFile::default_path(&root.join(".manifold"));
        let mut state = MergeStateFile::read(&state_path).unwrap();
        state.epoch_before = EpochId::new(&oid).unwrap();
        // epoch_after deliberately left as None.
        state.write_atomic(&state_path).unwrap();

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::MergeStateBadEpoch { which: "epoch_after", reason, .. }
                    if reason.contains("not recorded")
            )),
            "post-COMMIT phase without epoch_after must trip B3: {vs:?}"
        );
    }

    #[test]
    fn b3_skips_terminal_phase() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        // Hand-build a Complete merge-state with a bogus epoch_before; it
        // must NOT trip B3 because terminal phases are out of scope.
        let manifold = root.join(".manifold");
        fs::create_dir_all(&manifold).unwrap();
        let mut state = MergeStateFile::new(
            vec![WorkspaceId::new("any").unwrap()],
            EpochId::new(&"a".repeat(40)).unwrap(),
            0,
        );
        // Drive it all the way to Complete.
        for next in [
            MergePhase::Build,
            MergePhase::Validate,
            MergePhase::Commit,
            MergePhase::Cleanup,
            MergePhase::Complete,
        ] {
            state.advance(next, 0).unwrap();
        }
        state
            .write_atomic(&MergeStateFile::default_path(&manifold))
            .unwrap();
        let _ = oid;

        let vs = check(root);
        assert!(
            !vs.iter().any(|v| matches!(
                v,
                OracleBViolation::MergeStateOrphanSource { .. }
                    | OracleBViolation::MergeStateBadEpoch { .. }
            )),
            "B3 must not fire on a terminal phase: {vs:?}"
        );
    }

    // ----- B4: recovery well-formed ----------------------------------------

    #[test]
    fn b4_passes_for_commit_recovery_ref() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        write_ref(root, "refs/manifold/recovery/ws/20260301-000000", &oid);

        let vs = check(root);
        assert!(
            !vs.iter()
                .any(|v| matches!(v, OracleBViolation::RecoveryRefMalformed { .. })),
            "B4 must accept a commit-typed recovery ref: {vs:?}"
        );
    }

    #[test]
    fn b4_fires_on_tree_typed_recovery_ref() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // Tree of HEAD — definitely not a commit.
        let out = Command::new("git")
            .args(["rev-parse", "HEAD^{tree}"])
            .current_dir(root)
            .output()
            .unwrap();
        let tree_oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        write_ref(root, "refs/manifold/recovery/ws/20260301-000000", &tree_oid);

        let vs = check(root);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                OracleBViolation::RecoveryRefMalformed { reason, .. } if reason.contains("tree")
            )),
            "B4 must flag a recovery ref pointing at a tree: {vs:?}"
        );
    }

    // ----- Doctor agreement battery (≥5 hand-built states) ------------------

    /// Build a battery of hand-crafted incoherent states and assert that
    /// for each one Oracle B's coherence verdict matches what `maw doctor`
    /// would report. SP2 §6 makes the existing `doctor.rs` checks the
    /// ground-truth oracle for Oracle B's design.
    #[test]
    fn doctor_agreement_battery_geq_5_states() {
        struct Case {
            name: &'static str,
            build: fn(&Path, &str),
        }

        let cases: &[Case] = &[
            Case {
                name: "clean repo",
                build: |_root, _oid| {},
            },
            Case {
                name: "single dangling head ref (bn-cm63 class)",
                build: |root, oid| {
                    write_ref(root, &refs::workspace_head_ref("ghost"), oid);
                },
            },
            Case {
                name: "multiple dangling head refs",
                build: |root, oid| {
                    write_ref(root, &refs::workspace_head_ref("ghost-1"), oid);
                    write_ref(root, &refs::workspace_head_ref("ghost-2"), oid);
                    write_ref(root, &refs::workspace_head_ref("ghost-3"), oid);
                },
            },
            Case {
                name: "head ref + workspace present (no violation)",
                build: |root, oid| {
                    fs::create_dir_all(root.join("ws/active")).unwrap();
                    write_ref(root, &refs::workspace_head_ref("active"), oid);
                },
            },
            Case {
                name: "dangling head + live merge protecting different ws \
                       (head ref still stale from doctor POV)",
                build: |root, oid| {
                    write_ref(root, &refs::workspace_head_ref("ghost"), oid);
                    // Use the real HEAD OID for epoch_before so B3 doesn't
                    // fire spuriously — we are exercising the B1 path.
                    write_live_merge_state_with_epoch(root, &["inflight"], oid);
                    // The live merge claims `inflight` as a source — but
                    // ws/inflight/ doesn't exist and has no recovery ref,
                    // which WOULD trip B3 MergeStateOrphanSource. Create
                    // the ws dir so the source check passes; we are only
                    // exercising the B1 path here.
                    fs::create_dir_all(root.join("ws/inflight")).unwrap();
                },
            },
            Case {
                name: "orphaned non-terminal merge-state (foreign-host pid \
                       at Validate; both Oracle B and doctor flag incoherence)",
                build: |root, oid| {
                    // Doctor: `[WARN/FAIL] merge-state: ORPHANED/Indeterminate`.
                    // Oracle B: B3 MergeStateOrphanSource (source 'gone' has
                    // no ws dir + no recovery ref).
                    write_dead_merge_state(root, &["gone"], MergePhase::Validate);
                    let manifold = root.join(".manifold");
                    let sp = MergeStateFile::default_path(&manifold);
                    let mut state = MergeStateFile::read(&sp).unwrap();
                    state.epoch_before = EpochId::new(oid).unwrap();
                    state.write_atomic(&sp).unwrap();
                },
            },
            Case {
                name: "non-terminal merge-state with orphan source AND \
                       dangling head ref (compound violation)",
                build: |root, oid| {
                    write_ref(root, &refs::workspace_head_ref("ghost"), oid);
                    write_dead_merge_state(root, &["gone"], MergePhase::Validate);
                    let manifold = root.join(".manifold");
                    let sp = MergeStateFile::default_path(&manifold);
                    let mut state = MergeStateFile::read(&sp).unwrap();
                    state.epoch_before = EpochId::new(oid).unwrap();
                    state.write_atomic(&sp).unwrap();
                },
            },
        ];
        // Note: a `Complete`/`Aborted` terminal-leftover merge-state is
        // *intentionally* outside Oracle B's scope (SP2 §3 B3: "if phase
        // ∉ {complete, aborted}"). `maw doctor` flags it as hygiene
        // (`[WARN] leftover terminal state`); Oracle B treats it as out-
        // of-scope by spec, so we do not include it in the agreement
        // battery — that would test divergence the spec asks for, not
        // agreement.

        assert!(
            cases.len() >= 5,
            "doctor-agreement battery must have ≥5 hand-built states"
        );

        for case in cases {
            let (dir, oid) = setup_repo();
            let root = dir.path();
            (case.build)(root, &oid);

            let vs = check(root);
            let oracle_v = oracle_b_verdict(&vs);
            let doctor_v = doctor_verdict(root);

            // Stale head refs: doctor counts every head ref with no ws
            // dir; Oracle B's B1 additionally exempts live-merge sources
            // (so the verdict-layer count is doctor's minus the
            // live-protected ones). Re-derive doctor's expected count
            // here without going through the maw-cli crate to keep the
            // test self-contained, then add back the live-protected refs
            // to compare apples-to-apples with doctor.
            let live = live_merge_source_names(root);
            let live_protected_dangling_heads = list_all_refs(root)
                .unwrap()
                .iter()
                .filter(|(n, _)| {
                    n.strip_prefix(refs::HEAD_PREFIX).is_some_and(|ws| {
                        !ws.is_empty() && !root.join("ws").join(ws).exists() && live.contains(ws)
                    })
                })
                .count();
            assert_eq!(
                oracle_v.stale_head_refs + live_protected_dangling_heads,
                doctor_v.stale_head_refs,
                "[{}] doctor stale-head-ref count must equal Oracle-B \
                 B1-fired-count + live-protected-but-still-dangling count",
                case.name
            );
            assert_eq!(
                oracle_v.merge_state_incoherent, doctor_v.merge_state_incoherent,
                "[{}] oracle and doctor must agree on merge-state coherence",
                case.name
            );
        }
    }

    // ----- Clean lifecycle false-positive-free -----------------------------

    /// Run Oracle B at every step of a normal create/edit/commit lifecycle
    /// (sans real `maw ws merge`, which would need the maw binary on PATH —
    /// instead we hand-build the equivalent ref shapes plus a workspace
    /// directory and assert no false positives).
    #[test]
    fn clean_lifecycle_zero_violations() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Step 1: empty repo
        assert!(check(root).is_empty(), "step 1: empty repo");

        // Step 2: workspace + matching head ref (the normal case)
        make_ws_dir(root, "alice");
        write_ref(root, &refs::workspace_head_ref("alice"), &oid);
        write_ref(root, &refs::workspace_epoch_ref("alice"), &oid);
        write_ref(root, &refs::workspace_state_ref("alice"), &oid);
        assert!(check(root).is_empty(), "step 2: ws + refs");

        // Step 3: add a second workspace
        make_ws_dir(root, "bob");
        write_ref(root, &refs::workspace_head_ref("bob"), &oid);
        write_ref(root, &refs::workspace_epoch_ref("bob"), &oid);
        write_ref(root, &refs::workspace_state_ref("bob"), &oid);
        assert!(check(root).is_empty(), "step 3: two ws + refs");

        // Step 4: a normal live merge in flight (Build phase, both sources
        // present on disk) — B3 epoch_before resolves; no violations.
        write_live_merge_state(root, &["alice", "bob"]);
        let manifold = root.join(".manifold");
        let state_path = MergeStateFile::default_path(&manifold);
        let mut state = MergeStateFile::read(&state_path).unwrap();
        state.epoch_before = EpochId::new(&oid).unwrap();
        state.write_atomic(&state_path).unwrap();
        assert!(
            check(root).is_empty(),
            "step 4: in-flight merge must not fire"
        );

        // Step 5: post-merge — destroy state file, both workspaces still
        // exist (the merge result landed in default elsewhere). All refs
        // still resolve. Clean.
        std::fs::remove_file(&state_path).unwrap();
        assert!(check(root).is_empty(), "step 5: post-merge");

        // Step 6: destroy `alice` cleanly (rm dir + delete all owned refs +
        // pin recovery). This is the well-behaved destroy path.
        fs::remove_dir_all(root.join("ws/alice")).unwrap();
        for owned in refs::workspace_owned_refs("alice") {
            // Delete via update-ref -d (bypassing maw-git to keep test simple).
            let _ = Command::new("git")
                .args(["update-ref", "-d", &owned])
                .current_dir(root)
                .output();
        }
        write_ref(root, "refs/manifold/recovery/alice/20260301-000000", &oid);
        assert!(
            check(root).is_empty(),
            "step 6: clean destroy with recovery"
        );
    }

    // ----- Performance budget ---------------------------------------------

    /// Per the spec (§3.2) Oracle B should cost ≤ 5 ms on a typical
    /// ~50-workspace repo. We assert a loose 50 ms ceiling here to avoid
    /// CI flake on slow runners; the typical observed time is ~5 ms (the
    /// dominant cost is the single batched cat-file). Prints the measured
    /// time so the task report can quote it.
    #[test]
    fn per_call_cost_under_50ms_on_50_workspaces() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        // 50 workspaces, each with all three owned refs + a recovery ref.
        for i in 0..50 {
            let name = format!("ws-{i:02}");
            make_ws_dir(root, &name);
            write_ref(root, &refs::workspace_head_ref(&name), &oid);
            write_ref(root, &refs::workspace_epoch_ref(&name), &oid);
            write_ref(root, &refs::workspace_state_ref(&name), &oid);
            write_ref(
                root,
                &format!("refs/manifold/recovery/{name}/20260301-000000"),
                &oid,
            );
        }
        // Warm cache
        let _ = check(root);

        let t0 = Instant::now();
        let n_iter = 10;
        for _ in 0..n_iter {
            let vs = check(root);
            assert!(vs.is_empty(), "perf fixture must be coherent: {vs:?}");
        }
        let avg_ms = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(n_iter);
        eprintln!("oracle_b avg per call on 50-workspace repo: {avg_ms:.3} ms");
        assert!(
            avg_ms < 50.0,
            "per-call cost {avg_ms:.3} ms exceeds 50 ms ceiling (~5 ms expected)"
        );
    }

    // ----- check_first helper ----------------------------------------------

    #[test]
    fn check_first_returns_b1_before_b4() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        // Plant a B1 (dangling head) AND a B4 (tree-typed recovery ref).
        write_ref(root, &refs::workspace_head_ref("ghost"), &oid);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD^{tree}"])
            .current_dir(root)
            .output()
            .unwrap();
        let tree_oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        write_ref(root, "refs/manifold/recovery/ws/20260301-000000", &tree_oid);

        let first = check_first(root).unwrap_err();
        assert!(
            matches!(first, OracleBViolation::DanglingHeadRef { .. }),
            "deterministic order: B1 must come before B4; got {first:?}"
        );
    }
}
