//! **Oracle A — no committed work lost** (SG1 DST, bn-1z8q / T1.3).
//!
//! Implements the SP2 §2 predicate
//!
//! > **`W ⊆ U(F(state))`** — every blob ever authored by any workspace, at
//! > any point in the run, is still reachable from the frontier
//! > `F = {refs/heads/main} ∪ {refs/manifold/recovery/*} ∪
//! >       {refs/manifold/epoch/current} ∪ {refs/manifold/epoch/ws/*} ∪
//! >       {refs/manifold/ws/*} ∪ {extant ws/<x>/ HEAD}`.
//!
//! ## Why not commit-ancestry?
//!
//! The proven-wrong [`crate::oracle::check_g1_reachability`] uses
//! `git merge-base --is-ancestor` over commit OIDs. SP2's spike showed this
//! false-positives on **every** maw merge because maw's merge engine rebuilds
//! the merged tree and emits a fresh epoch commit, and `maw ws destroy`
//! writes a fresh recovery snapshot commit. Neither is a descendant of the
//! workspace's literal HEAD commit — yet the authored content (blob OIDs)
//! is preserved. The Prime Invariant protects committed **content**, not
//! commit identity. Oracle A therefore checks **blob/content reachability**.
//!
//! ## Why incremental?
//!
//! SP2 §2.1 proved the naive design — `git rev-list --objects <F>` recomputed
//! per step — scales **O(history depth)** per step (~2–3 s at 1e6 commits),
//! and the harness needs ~1e6 steps, so naive is **days** (O(N²)). The
//! mandated design (this module) maintains:
//!
//! - **`W` (witness blob set)** — every blob OID ever authored by any
//!   workspace, accreted across steps; per-workspace tip OID memoized so an
//!   unchanged tip contributes nothing (O(1)). When a tip changes the
//!   *workspace delta* vs the base-epoch ref is enumerated (NOT the full
//!   tip tree), bounding `|W|` to authored content.
//! - **`U` (live reachable-blob set)** — every blob currently reachable
//!   from `F`, accreted incrementally via `git rev-list --objects <new>
//!   ^<old>` per advanced root. Removed roots are NOT decremented on the
//!   fast path; on a witness-miss we do **one** authoritative full
//!   `git rev-list` to confirm true loss before reporting (lazy-confirm).
//!
//! Amortized budget: **≤ 1 ms/step** at 1e6 steps. The full rev-list is
//! paid at most once (the run stops on first real violation and shrinks).
//!
//! ## Independent verifier carveout
//!
//! Oracle A shells out to `git` (run with cwd = repo root, so it resolves the
//! git dir for either layout: a normal `.git/` in the consolidated layout, or
//! the bare `repo.git` in legacy v2), **deliberately not gix**. The oracle
//! must not share gix code paths with the system
//! under test, or a gix bug could mask a genuine invariant violation. See
//! the `TODO(gix): assurance carveout` markers throughout.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::oracle::{AssuranceState, AssuranceViolation};

// ---------------------------------------------------------------------------
// WitnessOrigin — shrinker-friendly provenance
// ---------------------------------------------------------------------------

/// Provenance tag for a blob OID in [`Witnesses`].
///
/// Tagging witnesses with `(workspace, step, tip)` lets the shrinker (bn-32k3,
/// T1.6) print a minimal-seed-friendly violation message ("blob X authored
/// by workspace Y at step Z when tip was T no longer reachable") instead of
/// a bare OID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WitnessOrigin {
    /// Workspace name that first carried the blob.
    pub ws: String,
    /// Plan step index at which the blob was witnessed.
    pub step: usize,
    /// Workspace tip OID at the time of witnessing.
    pub tip: String,
}

// ---------------------------------------------------------------------------
// OracleA — the live oracle state
// ---------------------------------------------------------------------------

/// Live, mutable Oracle A state — carries `W` and `U` across steps.
///
/// Construction is cheap (empty sets). The first call to
/// [`OracleA::check_step`] seeds `U` from the post-step frontier; thereafter
/// every step pays only `O(ΔF)` (added/advanced roots → `git rev-list
/// --objects <new> ^<all-previous>`; an unchanged frontier contributes
/// nothing).
///
/// Drop the oracle when the run ends; it owns no external resources.
#[derive(Debug)]
pub struct OracleA {
    /// Absolute path to the bare repo root (where `git for-each-ref`,
    /// `git rev-list`, etc. are invoked).
    repo_root: PathBuf,
    /// `W` — every blob OID ever authored by any workspace, with origin.
    witnesses: BTreeMap<String, WitnessOrigin>,
    /// Per-workspace memo: last `(tip OID, base epoch OID)` we already
    /// harvested blobs for. Unchanged tip ⇒ skip.
    ws_last_tip: HashMap<String, (String, Option<String>)>,
    /// `U` — every blob OID currently believed reachable from the frontier
    /// (membership-test only).
    reachable_blobs: HashSet<String>,
    /// Last-seen frontier `(ref_name → OID)`, used to compute `ΔF`.
    /// Includes the synthetic root `extant-ws:<name>` (which captures the
    /// HEAD OID of `ws/<name>/` for workspaces that are extant on disk).
    last_frontier: BTreeMap<String, String>,
    /// Have we seeded `U` from the initial frontier yet?
    seeded: bool,
    /// Total per-step cost across `check_step` invocations (for ≤1 ms/step
    /// budget tracking).
    total_check_time: Duration,
    /// Number of `check_step` invocations.
    steps_checked: u64,
}

/// Outcome of a single [`OracleA::check_step`] call.
#[derive(Debug)]
pub struct StepReport {
    /// `None` ⇒ Oracle A holds at this state.
    /// `Some(violation)` ⇒ a blob in `W` is unreachable from `F` (Prime
    /// Invariant breach: irreversibly lost committed content).
    pub violation: Option<AssuranceViolation>,
    /// How long this step took.
    pub duration: Duration,
    /// Whether the lazy-confirm full rev-list was triggered (rare — only
    /// on witness miss).
    pub did_full_rescan: bool,
    /// Current `|W|` after this step.
    pub witness_count: usize,
    /// Current `|U|` after this step.
    pub reachable_count: usize,
}

impl OracleA {
    /// Construct a fresh Oracle A bound to `repo_root` (the bare-repo root
    /// where `.git/` lives — the same path passed to
    /// [`crate::oracle::capture_state`]).
    #[must_use]
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            witnesses: BTreeMap::new(),
            ws_last_tip: HashMap::new(),
            reachable_blobs: HashSet::new(),
            last_frontier: BTreeMap::new(),
            seeded: false,
            total_check_time: Duration::ZERO,
            steps_checked: 0,
        }
    }

    /// Mean per-step cost across all [`Self::check_step`] calls so far.
    /// The SP2 §2.1 budget is **≤ 1 ms/step** amortized at 1e6 steps.
    #[must_use]
    pub fn mean_step_cost(&self) -> Duration {
        if self.steps_checked == 0 {
            Duration::ZERO
        } else {
            self.total_check_time / u32::try_from(self.steps_checked).unwrap_or(u32::MAX)
        }
    }

    /// Total checks run.
    #[must_use]
    pub const fn steps_checked(&self) -> u64 {
        self.steps_checked
    }

    /// Current `|W|` (witness count).
    #[must_use]
    pub fn witness_count(&self) -> usize {
        self.witnesses.len()
    }

    /// Current `|U|` (live reachable-blob count).
    #[must_use]
    pub fn reachable_count(&self) -> usize {
        self.reachable_blobs.len()
    }

    /// Check Oracle A against the post-step state.
    ///
    /// Implements SP2 §2.1's incremental design end-to-end:
    /// 1. Harvest witness contributions from per-workspace **delta**
    ///    (`git diff --raw <base-epoch> <tip>` → added/modified blob OIDs).
    ///    Unchanged tip ⇒ O(1) (memoized).
    /// 2. Compute `ΔF` vs the last frontier. For each added/advanced root,
    ///    do `git rev-list --objects --no-object-names <new> ^<all-prev>`
    ///    and union the result into `U`. For retreated/removed roots, do
    ///    nothing on the fast path.
    /// 3. Membership-test every witness against `U`. On the first miss, do
    ///    ONE authoritative full `git rev-list --objects <F>` and re-check.
    ///    If still missing, that is a Prime-Invariant breach.
    ///
    /// `step_index` is the [`crate::scenario::PlannedStep::index`] (used to
    /// tag witness origin for shrinker diagnostics).
    ///
    /// # Errors
    ///
    /// Returns the violation wrapped in `Ok(StepReport { violation: Some,
    /// .. })`; a true `Err` is reserved for `git`-CLI plumbing failures
    /// (the bare repo went missing, permissions, etc.).
    pub fn check_step(
        &mut self,
        state: &AssuranceState,
        step_index: usize,
    ) -> Result<StepReport, AssuranceViolation> {
        let t0 = Instant::now();

        // (1) harvest witness blobs from each extant workspace's delta.
        self.harvest_witnesses(state, step_index)?;

        // (2) update U from ΔF. The bool flags "at least one root was
        // removed or retreated" — content can only leave U via those, so
        // we MUST force the lazy-confirm path on a retreat even if every
        // witness still tests present against the stale (superset) U.
        let frontier = compute_frontier(state);
        let retreated = self.update_reachable_set(&frontier)?;

        // (3) membership-test witnesses; lazy-confirm on miss OR retreat.
        // We iterate once over a snapshot of witness keys to keep the
        // borrow on `self.witnesses` immutable while we may need to mutate
        // `self.reachable_blobs` via `full_rescan_reachable`.
        let mut did_full_rescan = false;
        let mut violation: Option<AssuranceViolation> = None;
        let fast_miss = self
            .witnesses
            .keys()
            .any(|b| !self.reachable_blobs.contains(b));
        if fast_miss || retreated {
            // Fast-path miss OR a root was removed/retreated → ONE
            // authoritative full rev-list to establish the true U. This
            // is the SP2 §2.1 lazy-confirm: amortised cost stays low
            // because retreats happen only on destroy/merge/abort/branch-
            // delete ops (a small minority of steps) and a true miss
            // halts the run anyway.
            did_full_rescan = true;
            self.full_rescan_reachable(&frontier)?;
            for (blob, origin) in &self.witnesses {
                if !self.reachable_blobs.contains(blob) {
                    violation = Some(AssuranceViolation::ReachabilityLost {
                        oid: blob.clone(),
                        previous_ref: format!(
                            "oracle-a/W: blob authored by ws:{} at step {} (tip {})",
                            origin.ws, origin.step, &origin.tip
                        ),
                    });
                    break;
                }
            }
        }

        self.last_frontier = frontier;
        let dt = t0.elapsed();
        self.total_check_time += dt;
        self.steps_checked += 1;

        Ok(StepReport {
            violation,
            duration: dt,
            did_full_rescan,
            witness_count: self.witnesses.len(),
            reachable_count: self.reachable_blobs.len(),
        })
    }

    /// Harvest workspace-delta blob OIDs into `W`.
    ///
    /// Per SP2 §1.2 we use the **workspace delta** vs the base epoch
    /// (`refs/manifold/epoch/ws/<ws>`) rather than the full tip tree, so
    /// `|W|` is bounded by authored content rather than repo size.
    fn harvest_witnesses(
        &mut self,
        state: &AssuranceState,
        step_index: usize,
    ) -> Result<(), AssuranceViolation> {
        for (ws_name, ws_status) in &state.workspaces {
            // Only extant workspaces contribute witnesses (a workspace
            // without a directory is either pre-creation or already
            // destroyed; its content is now Oracle A's job to keep
            // reachable via frontier roots, not to re-witness).
            if !ws_status.exists || ws_status.head_oid.is_empty() {
                continue;
            }
            let tip = &ws_status.head_oid;
            // Per-workspace base epoch ref, if maw has pinned one.
            let base_epoch = state
                .durable_refs
                .get(&format!("refs/manifold/epoch/ws/{ws_name}"))
                .cloned();
            // Memoization key: (tip, base_epoch). If unchanged, skip — O(1).
            if let Some(prev) = self.ws_last_tip.get(ws_name)
                && prev.0 == *tip
                && prev.1 == base_epoch
            {
                continue;
            }
            self.ws_last_tip
                .insert(ws_name.clone(), (tip.clone(), base_epoch.clone()));

            // Enumerate the workspace delta blobs.
            let blobs = match &base_epoch {
                Some(base) => diff_blobs(&self.repo_root, base, tip)?,
                // No base epoch ref: workspace pre-dates the epoch model
                // (e.g. brand-new ws/<x>/ with no manifold/ refs yet) →
                // fall back to the full tip tree. This is safe; the
                // |W|-bound argument is best-effort.
                None => ls_tree_blobs(&self.repo_root, tip)?,
            };
            for blob in blobs {
                self.witnesses.entry(blob).or_insert_with(|| WitnessOrigin {
                    ws: ws_name.clone(),
                    step: step_index,
                    tip: tip.clone(),
                });
            }
        }
        Ok(())
    }

    /// Update `U` incrementally from the new frontier vs `last_frontier`.
    ///
    /// Three cases per SP2 §2.1:
    /// 1. **Added or advanced root** (`name` is new, or `oid` differs) →
    ///    enumerate ONLY the newly reachable objects via
    ///    `git rev-list --objects <new> ^<all previous frontier OIDs>` and
    ///    union into `U`. This is the amortised-`O(1)` fast path that
    ///    keeps soak runs within budget.
    /// 2. **No change** → nothing to do.
    /// 3. **Removed or retreated root** (`name` no longer present, or
    ///    `oid` changed and we don't know whether the new OID covers the
    ///    old reach) → mark a *deferred* full rescan. We do not rescan
    ///    eagerly: the membership test in [`Self::check_step`] may pass
    ///    against the stale-but-superset `U` (no false-positive risk), and
    ///    a miss triggers the same authoritative rescan via lazy-confirm.
    ///    The retreat flag is the *trigger* — content can only leave `U`
    ///    via removed/retreated roots (SP2 §2.1 alt design), so when we
    ///    miss after a retreat the lazy-confirm correctly establishes
    ///    "really lost" vs "U was over-counted".
    ///
    /// Returns `true` iff at least one root was removed or retreated since
    /// the last frontier — used by [`Self::check_step`] to force the
    /// lazy-confirm path even if every witness was present in the stale
    /// `U` (the case where a retreat **uniquely** removed a witness blob
    /// that the stale `U` still nominally covers).
    fn update_reachable_set(
        &mut self,
        frontier: &BTreeMap<String, String>,
    ) -> Result<bool, AssuranceViolation> {
        if !self.seeded {
            // First call: enumerate everything reachable from F once.
            self.full_rescan_reachable(frontier)?;
            self.seeded = true;
            return Ok(false);
        }
        // Compute ΔF: added or changed roots only, plus a retreat flag.
        //
        // **Retreat definition (load-bearing):** a root has *retreated*
        // iff content reachable from the previous OID may no longer be
        // covered by the new frontier — i.e. the previous OID is NOT a
        // commit ancestor of the new OID. This is asymmetric on purpose:
        // for an advanced root (new descends from old) the new OID covers
        // everything the old one did, so nothing was lost; no full rescan
        // needed. For a true retreat (branch reset, non-FF swap, refdel)
        // we MUST authoritatively rescan, because the previous root may
        // have uniquely held witness blobs.
        //
        // The ancestry probe is itself the *proven-wrong* commit-ancestry
        // model (SP2 §0) when used as the *oracle*. Here it is only a
        // **safe heuristic for when to skip the full rescan**: if it says
        // "fast-forward", we trust it (correct); if it says "retreat", we
        // fall through to the authoritative blob rescan (also correct,
        // just slightly more expensive). False-positives on the ancestry
        // probe cost a rescan, NOT a false oracle violation — the rescan
        // is the source of truth.
        let mut new_roots: Vec<String> = Vec::new();
        let mut retreated = false;
        for (name, oid) in frontier {
            match self.last_frontier.get(name) {
                Some(prev) if prev == oid => {}
                Some(prev) => {
                    new_roots.push(oid.clone());
                    if !is_ancestor(&self.repo_root, prev, oid) {
                        retreated = true;
                    }
                }
                None => new_roots.push(oid.clone()),
            }
        }
        for name in self.last_frontier.keys() {
            if !frontier.contains_key(name) {
                retreated = true;
                break;
            }
        }
        if !new_roots.is_empty() {
            // `^<prev>` exclusions: every previously-seen frontier OID, so
            // we enumerate ONLY objects newly reachable via the
            // added/advanced roots. Deduplicate so we don't pass the same
            // OID twice.
            let prev_oids: BTreeSet<String> = self.last_frontier.values().cloned().collect();
            let novel = rev_list_objects(&self.repo_root, &new_roots, &prev_oids)?;
            self.reachable_blobs.extend(novel);
        }
        Ok(retreated)
    }

    /// Authoritative full `git rev-list` over the entire current frontier;
    /// rebuilds `U` from scratch. Called at most once per `check_step`
    /// (lazy-confirm) and once at seeding.
    fn full_rescan_reachable(
        &mut self,
        frontier: &BTreeMap<String, String>,
    ) -> Result<(), AssuranceViolation> {
        let roots: Vec<String> = frontier.values().cloned().collect();
        if roots.is_empty() {
            self.reachable_blobs.clear();
            return Ok(());
        }
        let all = rev_list_objects(&self.repo_root, &roots, &BTreeSet::new())?;
        self.reachable_blobs = all;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Frontier computation (SP2 §1.1)
// ---------------------------------------------------------------------------

/// Synthetic frontier-root key for the on-disk HEAD of an extant `ws/<x>/`.
///
/// We can't use a real ref name for this (the on-disk HEAD may diverge from
/// any manifold ref mid-operation), so the key is namespaced to keep the
/// `(name → OID)` invariant of [`OracleA::last_frontier`] sound.
fn extant_ws_root_key(ws: &str) -> String {
    format!("oracle-a/extant-ws-head/{ws}")
}

/// Compute `F(state)` per SP2 §1.1.
///
/// Includes: `refs/heads/main`, every `refs/manifold/recovery/*`,
/// `refs/manifold/epoch/current`, every `refs/manifold/epoch/ws/*`, every
/// `refs/manifold/ws/*`, and the on-disk HEAD of every extant `ws/<x>/`.
///
/// Explicitly **excludes** `refs/manifold/head/<ws>` — that is the
/// oplog-head **blob** ref (not a commit), and is Oracle B's subject (SP2
/// §1.1, §3).
fn compute_frontier(state: &AssuranceState) -> BTreeMap<String, String> {
    let mut frontier = BTreeMap::new();
    for (name, oid) in &state.durable_refs {
        if name == "refs/heads/main"
            || name == "refs/manifold/epoch/current"
            || name.starts_with("refs/manifold/recovery/")
            || name.starts_with("refs/manifold/epoch/ws/")
            || name.starts_with("refs/manifold/ws/")
        {
            frontier.insert(name.clone(), oid.clone());
        }
    }
    // Plus the on-disk HEAD of every extant workspace.
    for (ws, status) in &state.workspaces {
        if status.exists && !status.head_oid.is_empty() {
            frontier.insert(extant_ws_root_key(ws), status.head_oid.clone());
        }
    }
    frontier
}

// ---------------------------------------------------------------------------
// git CLI primitives — INDEPENDENT VERIFIER CARVEOUT
// ---------------------------------------------------------------------------

/// Enumerate every object reachable from `roots`, excluding everything
/// reachable from `exclude`.
///
/// Wraps `git rev-list --objects --no-object-names <roots> ^<exclude>`.
/// `--no-object-names` prints one OID per line (commits, trees, blobs); we
/// take the union without type-filtering because we only membership-test
/// known blob OIDs against this set (SP2 §1.3).
///
/// TODO(gix): assurance carveout — Oracle A is an *independent* verifier;
/// using git CLI keeps its code path distinct from the production gix code
/// it verifies, so a gix bug cannot mask a genuine invariant violation.
fn rev_list_objects(
    repo_root: &Path,
    roots: &[String],
    exclude: &BTreeSet<String>,
) -> Result<HashSet<String>, AssuranceViolation> {
    if roots.is_empty() {
        return Ok(HashSet::new());
    }
    let mut cmd = Command::new("git");
    cmd.arg("rev-list")
        .arg("--objects")
        .arg("--no-object-names")
        .current_dir(repo_root);
    for r in roots {
        cmd.arg(r);
    }
    for e in exclude {
        cmd.arg(format!("^{e}"));
    }
    let output = cmd.output().map_err(|e| AssuranceViolation::GitError {
        check: "oracle_a::rev_list_objects".to_owned(),
        command: format!(
            "git rev-list --objects --no-object-names <{} roots>",
            roots.len()
        ),
        stderr: e.to_string(),
    })?;
    if !output.status.success() {
        // Special-case: an exclude OID that has no objects in common with
        // the include set is not an error; but a missing exclude OID is.
        // We let git's own non-zero exit propagate so plumbing bugs are
        // visible.
        return Err(AssuranceViolation::GitError {
            check: "oracle_a::rev_list_objects".to_owned(),
            command: format!(
                "git rev-list --objects --no-object-names <{} roots>",
                roots.len()
            ),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|s| s.trim().to_owned()).collect())
}

/// Workspace delta blobs: every blob OID present in `tip`'s tree that is
/// added or modified vs `base`'s tree.
///
/// Wraps `git diff --raw <base> <tip>` (raw output is `:<srcmode> <dstmode>
/// <srcoid> <dstoid> <status>\t<path>`). The `<dstoid>` is the post-image
/// blob; we collect those for `A`/`M`/`T`/`C`/`R` statuses (deletions have
/// `dstoid=0`).
///
/// TODO(gix): assurance carveout — see [`rev_list_objects`].
fn diff_blobs(repo_root: &Path, base: &str, tip: &str) -> Result<Vec<String>, AssuranceViolation> {
    if base == tip {
        return Ok(Vec::new());
    }
    // --no-abbrev: by default `diff --raw` outputs abbreviated OIDs; we
    // need full 40-hex blob OIDs to match against `git rev-list --objects`
    // (which always prints full OIDs).
    let output = Command::new("git")
        .args([
            "diff",
            "--raw",
            "--no-renames",
            "--no-abbrev",
            "-z",
            base,
            tip,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| AssuranceViolation::GitError {
            check: "oracle_a::diff_blobs".to_owned(),
            command: format!("git diff --raw {base} {tip}"),
            stderr: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(AssuranceViolation::GitError {
            check: "oracle_a::diff_blobs".to_owned(),
            command: format!("git diff --raw {base} {tip}"),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    // -z output: records terminated by NUL; format
    //   `:<srcmode> <dstmode> <srcoid> <dstoid> <status>\0<path>\0`
    // We split on NUL, then for each record-header (lines starting with
    // `:`) parse the 5 colon-separated fields and take dstoid when nonzero.
    let mut blobs = Vec::new();
    let bytes = output.stdout;
    let mut i = 0;
    while i < bytes.len() {
        let end = bytes[i..]
            .iter()
            .position(|&b| b == 0)
            .map_or(bytes.len(), |p| i + p);
        let record = std::str::from_utf8(&bytes[i..end]).unwrap_or("");
        i = end + 1;
        if !record.starts_with(':') {
            // path field — skip; we already consumed dstoid from the
            // header on the previous iteration.
            continue;
        }
        // header: `:100644 100644 <src> <dst> M`
        let fields: Vec<&str> = record.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        let dstoid = fields[3];
        if dstoid.chars().all(|c| c == '0') {
            // pure deletion — nothing to witness
            continue;
        }
        blobs.push(dstoid.to_owned());
    }
    Ok(blobs)
}

/// Is `maybe_ancestor` a commit-ancestor of (or equal to) `maybe_descendant`?
///
/// Wraps `git merge-base --is-ancestor`. Used ONLY as a heuristic to decide
/// whether a frontier root's OID change is a fast-forward (no rescan) or a
/// retreat (rescan needed). Both args must be commit OIDs in normal maw
/// operation — all frontier roots are commits. If either is not a commit
/// (e.g. a synthetic blob OID slipped through), git returns non-zero and
/// we conservatively treat as retreat.
///
/// TODO(gix): assurance carveout — see [`rev_list_objects`].
fn is_ancestor(repo_root: &Path, maybe_ancestor: &str, maybe_descendant: &str) -> bool {
    if maybe_ancestor == maybe_descendant {
        return true;
    }
    let output = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            maybe_ancestor,
            maybe_descendant,
        ])
        .current_dir(repo_root)
        .output();
    matches!(output, Ok(o) if o.status.success())
}

/// Every blob OID in `tip`'s tree (recursive). Fallback when no base-epoch
/// ref exists; in normal maw operation [`diff_blobs`] is preferred.
///
/// TODO(gix): assurance carveout — see [`rev_list_objects`].
fn ls_tree_blobs(repo_root: &Path, tip: &str) -> Result<Vec<String>, AssuranceViolation> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", tip])
        .current_dir(repo_root)
        .output()
        .map_err(|e| AssuranceViolation::GitError {
            check: "oracle_a::ls_tree_blobs".to_owned(),
            command: format!("git ls-tree -r {tip}"),
            stderr: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(AssuranceViolation::GitError {
            check: "oracle_a::ls_tree_blobs".to_owned(),
            command: format!("git ls-tree -r {tip}"),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    // ls-tree format: `<mode> <type> <oid>\t<path>`
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let _mode = parts.next();
        let kind = parts.next().unwrap_or("");
        let oid = parts.next().unwrap_or("");
        if kind == "blob" && !oid.is_empty() {
            out.push(oid.to_owned());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{WorkspaceStatus, capture_state};
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // git fixture helpers — real repo, real refs, real blobs
    // -----------------------------------------------------------------------

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git start");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    fn git_capture(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git start");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn setup_repo() -> TempDir {
        let dir = TempDir::new().expect("tmpdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "T"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("README.md"), "x").unwrap();
        git(root, &["add", "README.md"]);
        git(root, &["commit", "-q", "--no-gpg-sign", "-m", "init"]);
        // initial epoch ref
        let head = git_capture(root, &["rev-parse", "HEAD"]);
        git(root, &["update-ref", "refs/manifold/epoch/current", &head]);
        dir
    }

    /// Build an AssuranceState by reading the repo + the given ws-dir map
    /// `(ws_name, head_oid, dirty, exists)`.
    fn make_state(root: &Path, workspaces: &[(&str, &str, bool, bool)]) -> AssuranceState {
        // Re-read refs via capture_state (uses git for-each-ref).
        let mut state = capture_state(root).expect("capture");
        state.workspaces.clear();
        for (name, head, dirty, exists) in workspaces {
            state.workspaces.insert(
                (*name).to_owned(),
                WorkspaceStatus {
                    head_oid: (*head).to_owned(),
                    is_dirty: *dirty,
                    exists: *exists,
                },
            );
        }
        state
    }

    fn commit_file(root: &Path, ws_branch_ref: &str, path: &str, content: &str) -> String {
        // Build a tree containing only `path → blob(content)` and a commit
        // on top of `ws_branch_ref`'s current commit. This keeps the
        // fixture small without needing a worktree.
        let blob = {
            let mut c = Command::new("git");
            c.args(["hash-object", "-w", "--stdin"])
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = c.spawn().expect("spawn hash-object");
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(content.as_bytes())
                .unwrap();
            let out = child.wait_with_output().expect("hash-object");
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let mktree_input = format!("100644 blob {blob}\t{path}\n");
        let tree = {
            let mut c = Command::new("git");
            c.args(["mktree"])
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = c.spawn().expect("spawn mktree");
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(mktree_input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().expect("mktree");
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        // Parent: if the ref already exists, build on it; otherwise root
        // the ws branch at the current HEAD (refs/heads/main).
        let parent = {
            let out = Command::new("git")
                .args([
                    "rev-parse",
                    "--verify",
                    &format!("{ws_branch_ref}^{{commit}}"),
                ])
                .current_dir(root)
                .output()
                .expect("git rev-parse");
            if out.status.success() {
                String::from_utf8_lossy(&out.stdout).trim().to_owned()
            } else {
                git_capture(root, &["rev-parse", "HEAD"])
            }
        };
        let commit = git_capture(
            root,
            &[
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                &format!("ws commit on {ws_branch_ref}"),
            ],
        );
        git(root, &["update-ref", ws_branch_ref, &commit]);
        // Return the blob OID — that's what's witnessed.
        blob
    }

    // -----------------------------------------------------------------------
    // (1) NEGATIVE: bn-cm63 is a B-class bug → Oracle A must NOT fire
    // -----------------------------------------------------------------------

    #[test]
    fn bn_cm63_class_does_not_trip_oracle_a() {
        // Setup: a workspace 'alice' is created, commits, then is destroyed
        // WITH a recovery ref (the bn-cm63 fix). We then *manually plant* a
        // dangling refs/manifold/head/alice → some unrelated oplog-head
        // blob, simulating what would happen if the bn-cm63 fix were
        // reverted. Oracle A must stay green: a dangling head ref is a
        // coherence defect (Oracle B's job), NOT work loss.
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);

        // Alice authors a blob and gets a recovery ref (post-destroy).
        git(root, &["update-ref", "refs/manifold/epoch/ws/alice", &head]);
        let alice_blob = commit_file(root, "refs/manifold/ws/alice", "a.txt", "alice-content");
        let alice_tip = git_capture(root, &["rev-parse", "refs/manifold/ws/alice"]);
        // Pin a recovery ref pointing at the workspace tip (mimics
        // post-destroy state) and drop refs/manifold/ws/alice +
        // epoch/ws/alice (workspace gone).
        git(
            root,
            &[
                "update-ref",
                "refs/manifold/recovery/alice/2026-05-25T00-00-00Z",
                &alice_tip,
            ],
        );
        git(root, &["update-ref", "-d", "refs/manifold/ws/alice"]);
        git(root, &["update-ref", "-d", "refs/manifold/epoch/ws/alice"]);

        // Plant the bn-cm63 leak: a dangling oplog-head blob ref for the
        // (now-gone) workspace. The ref points at a blob, not a commit.
        let oplog_blob = {
            let mut c = Command::new("git");
            c.args(["hash-object", "-w", "--stdin"])
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = c.spawn().unwrap();
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(br#"{"workspace_id":"alice","payload":{"type":"create"}}"#)
                .unwrap();
            let out = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(
            root,
            &["update-ref", "refs/manifold/head/alice", &oplog_blob],
        );

        let mut oracle = OracleA::new(root);

        // Step 0: alice existed, witnesses harvested. The bn-cm63 scenario
        // already dropped alice's epoch ref to mimic post-destroy state,
        // so harvest_witnesses will use the ls-tree fallback (no base
        // epoch) and still collect alice's authored blob.
        let state0 = make_state(root, &[("alice", &alice_tip, false, true)]);
        let r0 = oracle
            .check_step(&state0, 0)
            .expect("step 0 should not error");
        assert!(r0.violation.is_none(), "step 0 should be clean");
        assert!(
            oracle.witnesses.contains_key(&alice_blob),
            "alice's blob should be witnessed"
        );

        // Step 1: alice destroyed, recovery ref present, AND dangling
        // refs/manifold/head/alice planted. Oracle A must stay green.
        git(root, &["update-ref", "-d", "refs/manifold/epoch/ws/alice"]);
        let state1 = make_state(root, &[]);
        let r1 = oracle
            .check_step(&state1, 1)
            .expect("step 1 should not error");
        assert!(
            r1.violation.is_none(),
            "bn-cm63 (dangling head ref, no work loss) must NOT trip Oracle A — got: {:?}",
            r1.violation
        );
    }

    // -----------------------------------------------------------------------
    // (2) POSITIVE: planted work-loss MUST trip Oracle A
    // -----------------------------------------------------------------------

    #[test]
    fn planted_work_loss_trips_oracle_a() {
        // Setup: workspace 'dave' commits a uniquely-identifiable blob.
        // Then we delete the workspace ref AND any recovery ref pointing
        // at the commit (simulating a destroy that failed to pin recovery
        // — the literal Prime-Invariant breach).
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);

        git(root, &["update-ref", "refs/manifold/epoch/ws/dave", &head]);
        let dave_blob = commit_file(
            root,
            "refs/manifold/ws/dave",
            "d.txt",
            "dave-unique-content",
        );
        let dave_tip = git_capture(root, &["rev-parse", "refs/manifold/ws/dave"]);

        let mut oracle = OracleA::new(root);

        // Step 0: dave exists & is witnessed.
        let state0 = make_state(root, &[("dave", &dave_tip, false, true)]);
        let r0 = oracle.check_step(&state0, 0).unwrap();
        assert!(r0.violation.is_none(), "step 0 should be clean");
        assert!(
            oracle.witnesses.contains_key(&dave_blob),
            "dave's blob {} should be witnessed",
            dave_blob
        );

        // Step 1: WORK LOSS — delete dave's workspace refs + epoch ref
        // WITHOUT pinning recovery. Hard-prune so blob is genuinely
        // unreachable.
        git(root, &["update-ref", "-d", "refs/manifold/ws/dave"]);
        git(root, &["update-ref", "-d", "refs/manifold/epoch/ws/dave"]);
        // (no refs/manifold/recovery/dave/* created → blob is now
        // unreachable from F)
        let state1 = make_state(root, &[]);
        let r1 = oracle.check_step(&state1, 1).unwrap();
        let v = r1
            .violation
            .as_ref()
            .expect("planted work-loss MUST trip Oracle A");
        let msg = format!("{v}");
        assert!(
            msg.contains(&dave_blob[..12]),
            "violation should name dave's blob: {msg}"
        );
        assert!(
            msg.contains("dave"),
            "violation should name dave (shrinker-friendly): {msg}"
        );
        assert!(
            r1.did_full_rescan,
            "lazy-confirm full rescan should have fired"
        );
    }

    // -----------------------------------------------------------------------
    // (3) POSITIVE: 2026-02-05 incident — conflict resolution drops a side
    // -----------------------------------------------------------------------

    #[test]
    fn lost_commits_2026_02_05_incident_trips_oracle_a() {
        // Reproduce the incident class: workspace A authors blob alpha;
        // workspace B authors blob beta on the same file. Merge resolves
        // by dropping A's side. A's recovery ref is absent (the incident
        // scenario: the conflict was resolved silently into a botbox
        // upgrade commit, NOT via maw destroy's recovery snapshot, so
        // refs/manifold/recovery/A/* doesn't exist).
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);

        git(root, &["update-ref", "refs/manifold/epoch/ws/wsA", &head]);
        git(root, &["update-ref", "refs/manifold/epoch/ws/wsB", &head]);
        let alpha = commit_file(root, "refs/manifold/ws/wsA", "shared.txt", "alpha-side");
        let _beta = commit_file(root, "refs/manifold/ws/wsB", "shared.txt", "beta-side");
        let tip_a = git_capture(root, &["rev-parse", "refs/manifold/ws/wsA"]);
        let tip_b = git_capture(root, &["rev-parse", "refs/manifold/ws/wsB"]);

        let mut oracle = OracleA::new(root);

        // Step 0: both workspaces exist; both blobs witnessed.
        let state0 = make_state(
            root,
            &[("wsA", &tip_a, false, true), ("wsB", &tip_b, false, true)],
        );
        let r0 = oracle.check_step(&state0, 0).unwrap();
        assert!(r0.violation.is_none());
        assert!(
            oracle.witnesses.contains_key(&alpha),
            "alpha blob must be witnessed"
        );

        // Step 1: merge picks B's side, drops A. The "merged main" advances
        // to a commit containing only beta. A's refs are dropped (the
        // incident: conflict resolved silently → wsA never got a recovery
        // ref). The on-disk wsA also vanishes.
        // Build a merged-main commit whose tree contains only beta.
        let merged_commit = {
            // Compute beta's tree: a single file `shared.txt` → beta-blob.
            let beta_blob = git_capture(root, &["rev-parse", &format!("{tip_b}:shared.txt")]);
            let merged_tree = {
                let mut c = Command::new("git");
                c.args(["mktree"])
                    .current_dir(root)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped());
                let mut child = c.spawn().unwrap();
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(format!("100644 blob {beta_blob}\tshared.txt\n").as_bytes())
                    .unwrap();
                let out = child.wait_with_output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_owned()
            };
            git_capture(
                root,
                &[
                    "commit-tree",
                    &merged_tree,
                    "-p",
                    &head,
                    "-m",
                    "merged (dropped wsA)",
                ],
            )
        };
        git(root, &["update-ref", "refs/heads/main", &merged_commit]);
        git(
            root,
            &["update-ref", "refs/manifold/epoch/current", &merged_commit],
        );
        // Drop A's refs entirely — the incident.
        git(root, &["update-ref", "-d", "refs/manifold/ws/wsA"]);
        git(root, &["update-ref", "-d", "refs/manifold/epoch/ws/wsA"]);
        // (deliberately NO refs/manifold/recovery/wsA/*)
        let state1 = make_state(root, &[("wsB", &tip_b, false, true)]);
        let r1 = oracle.check_step(&state1, 1).unwrap();
        let v = r1
            .violation
            .as_ref()
            .expect("dropped-side incident MUST trip Oracle A");
        let msg = format!("{v}");
        assert!(
            msg.contains(&alpha[..12]),
            "violation should name the lost alpha blob: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // (4) FALSE-POSITIVE FREEDOM: tight smoke loop
    // -----------------------------------------------------------------------

    /// Smoke run: K steps of "advance an extant workspace and (occasionally)
    /// move the epoch forward". No work is ever lost, so Oracle A must stay
    /// green for the whole run. We use K=200 in unit tests (the spec calls
    /// for 1e5; that's run separately when the full DST harness lands and
    /// would dominate per-PR CI time. The per-step cost test below covers
    /// the budget separately).
    #[test]
    fn false_positive_freedom_smoke_200() {
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);

        git(root, &["update-ref", "refs/manifold/epoch/ws/ws1", &head]);

        let mut oracle = OracleA::new(root);
        let _ = head;
        for i in 0_usize..200 {
            // Advance ws1 by one commit.
            let _blob = commit_file(
                root,
                "refs/manifold/ws/ws1",
                &format!("f{i}.txt"),
                &format!("c{i}"),
            );
            let tip = git_capture(root, &["rev-parse", "refs/manifold/ws/ws1"]);

            // Occasionally advance main forward (merge into main) — clean
            // path, never drops content because we move main to ws1's
            // current tip.
            if i.is_multiple_of(20) {
                git(root, &["update-ref", "refs/heads/main", &tip]);
                git(root, &["update-ref", "refs/manifold/epoch/current", &tip]);
            }

            let state = make_state(root, &[("ws1", &tip, false, true)]);
            let r = oracle.check_step(&state, i).unwrap();
            assert!(
                r.violation.is_none(),
                "false positive at step {i}: {:?}",
                r.violation
            );
        }
        assert_eq!(oracle.steps_checked(), 200);
    }

    // -----------------------------------------------------------------------
    // (5) PER-STEP COST: incremental design must stay within SP2 budget
    // -----------------------------------------------------------------------

    /// Cost-shape test: 1000 active steps (workspace advances each step)
    /// + 1000 no-op steps (state unchanged). Asserts:
    /// 1. **Zero full rescans on clean runs** (the load-bearing design
    ///    property — naive O(N²) is what we MUST avoid).
    /// 2. **No-op steps are ~O(1)**: each ~free in subprocess time,
    ///    because harvest memoization + frontier-equality short-circuit
    ///    both kick in.
    /// 3. **Active steps stay flat** with history depth (the SP2 §2.1
    ///    property — naive scaled O(history); incremental does not).
    ///
    /// We deliberately do NOT assert an absolute `mean ≤ 1 ms` here: the
    /// SP2 budget of ≤ 1 ms/step amortized at 1e6 steps assumes a soak
    /// mix where most steps don't change a workspace (sync, recover,
    /// no-op faults). The unit-test workload is *worst case* — every
    /// step advances ws1 by a commit and therefore pays 3 git
    /// subprocesses (~3 ms each on this host). In production the no-op
    /// majority brings the mean down to the budget. The amortized
    /// comparison below verifies the no-op fast path.
    #[test]
    fn per_step_cost_shape_active_then_noop_2000() {
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);
        git(root, &["update-ref", "refs/manifold/epoch/ws/ws1", &head]);

        let mut oracle = OracleA::new(root);
        let mut active_total = Duration::ZERO;
        let mut active_full_rescans = 0u64;
        for i in 0..1000 {
            commit_file(
                root,
                "refs/manifold/ws/ws1",
                &format!("p{i}.txt"),
                &format!("v{i}"),
            );
            let tip = git_capture(root, &["rev-parse", "refs/manifold/ws/ws1"]);
            let state = make_state(root, &[("ws1", &tip, false, true)]);
            let r = oracle.check_step(&state, i).unwrap();
            active_total += r.duration;
            if r.did_full_rescan {
                active_full_rescans += 1;
            }
        }
        let active_mean_ms = (active_total.as_secs_f64() / 1000.0) * 1e3;
        assert_eq!(
            active_full_rescans, 0,
            "lazy-confirm MUST NOT fire on a clean fast-forward run"
        );

        // Now 1000 no-op steps — same state. Memoization + frontier-
        // equality must make these near-zero cost.
        let last_tip = git_capture(root, &["rev-parse", "refs/manifold/ws/ws1"]);
        let mut noop_total = Duration::ZERO;
        let mut noop_full_rescans = 0u64;
        for i in 1000..2000 {
            let state = make_state(root, &[("ws1", &last_tip, false, true)]);
            let r = oracle.check_step(&state, i).unwrap();
            noop_total += r.duration;
            if r.did_full_rescan {
                noop_full_rescans += 1;
            }
        }
        let noop_mean_ms = (noop_total.as_secs_f64() / 1000.0) * 1e3;
        assert_eq!(
            noop_full_rescans, 0,
            "no-op steps must never trigger lazy-confirm"
        );
        // The no-op steps MUST be substantially cheaper than active steps —
        // proof that the incremental ΔF design (not the naive recompute)
        // is in force. Ratio is loose (3x) to absorb host noise and
        // parallel-test contention; the real signal is the absolute
        // no-op cost (next assertion).
        assert!(
            noop_mean_ms * 3.0 < active_mean_ms.max(1.0),
            "no-op steps ({noop_mean_ms:.3} ms) should be much cheaper than \
             active steps ({active_mean_ms:.3} ms) — incremental design check"
        );
        // No-op mean ≤ 2 ms is a smoke proxy for the SP2 ≤ 1 ms budget.
        // The threshold is loose (~2x budget) because unit-test
        // subprocess overhead on tiny temp repos dominates; the 100k
        // soak (#[ignore]'d) is the closer-to-production measurement.
        assert!(
            noop_mean_ms < 2.0,
            "no-op mean {noop_mean_ms:.3} ms exceeds smoke ≤ 2 ms budget"
        );

        eprintln!(
            "oracle_a per-step cost: active {active_mean_ms:.3} ms × 1000, \
             no-op {noop_mean_ms:.3} ms × 1000 (W={}, U={})",
            oracle.witness_count(),
            oracle.reachable_count()
        );
    }

    // -----------------------------------------------------------------------
    // 1e5-step soak: false-positive freedom at the scale the bone calls
    // for. Marked #[ignore] because at ~8 ms/active-step this takes ~13
    // minutes; CI runs it under a dedicated soak job. Direct invoke via
    // `cargo test ... -- --ignored fpf_smoke_100k`.
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "slow: 1e5-step soak (~13 min); run via `-- --ignored`"]
    fn false_positive_freedom_smoke_100k() {
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);
        git(root, &["update-ref", "refs/manifold/epoch/ws/ws1", &head]);
        let mut oracle = OracleA::new(root);
        for i in 0_usize..100_000 {
            commit_file(
                root,
                "refs/manifold/ws/ws1",
                &format!("p{i}.txt"),
                &format!("v{i}"),
            );
            let tip = git_capture(root, &["rev-parse", "refs/manifold/ws/ws1"]);
            // Mix in a no-op every 5 steps + an occasional main fast-forward.
            let state = make_state(root, &[("ws1", &tip, false, true)]);
            let r = oracle.check_step(&state, i).unwrap();
            assert!(
                r.violation.is_none(),
                "false positive at step {i}: {:?}",
                r.violation
            );
            if i.is_multiple_of(5) {
                let r2 = oracle.check_step(&state, i).unwrap();
                assert!(r2.violation.is_none(), "no-op false-positive");
            }
            if i.is_multiple_of(100) {
                git(root, &["update-ref", "refs/heads/main", &tip]);
            }
        }
        eprintln!(
            "100k soak: 0 false positives; mean per-step {:.3} ms",
            oracle.mean_step_cost().as_secs_f64() * 1e3
        );
    }

    // -----------------------------------------------------------------------
    // (6) Frontier excludes refs/manifold/head/<ws> (SP2 §1.1)
    // -----------------------------------------------------------------------

    #[test]
    fn frontier_excludes_oplog_head_refs() {
        let dir = setup_repo();
        let root = dir.path();
        let head = git_capture(root, &["rev-parse", "HEAD"]);
        // A "head" ref that points at a blob, like the real oplog-head.
        // Make a blob and pin a head ref to it; compute_frontier must
        // ignore it.
        let blob = {
            let mut c = Command::new("git");
            c.args(["hash-object", "-w", "--stdin"])
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = c.spawn().unwrap();
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"{\"workspace_id\":\"x\"}")
                .unwrap();
            let out = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(root, &["update-ref", "refs/manifold/head/x", &blob]);
        let state = capture_state(root).unwrap();
        // Sanity: capture_state DID see the head ref.
        assert!(state.durable_refs.contains_key("refs/manifold/head/x"));
        // But compute_frontier must NOT include it.
        let f = compute_frontier(&state);
        assert!(
            !f.contains_key("refs/manifold/head/x"),
            "refs/manifold/head/<ws> is an oplog-blob ref and MUST be excluded \
             from F (SP2 §1.1). frontier keys: {:?}",
            f.keys().collect::<Vec<_>>()
        );
        // And the included keys are the expected categories.
        assert!(f.values().any(|v| v == &head));
    }
}
