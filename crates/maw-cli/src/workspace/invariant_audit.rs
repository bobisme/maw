//! Prime-Invariant runtime auditor (bn-2rnq).
//!
//! Turns the Prime Invariant — *no sibling workspace ever loses committed work
//! across an epoch mutation* — from a design goal into a runtime-enforced
//! contract. Every epoch-mutating command (`ws merge` incl. FF-absorb and
//! sibling auto-rebase, `ws sync --rebase`, `ws advance`, `epoch sync`) is
//! bracketed by two calls:
//!
//! 1. [`capture`] at the start of the critical section (INSIDE the repo-level
//!    epoch lock — see [`crate::epoch_lock`] — so no sibling HEAD can move under
//!    us): snapshot every workspace's `{name → HEAD OID, base-epoch OID}`. This
//!    is pure ref reading, no tree work.
//! 2. [`audit`] after the mutation completes but BEFORE the command declares
//!    success: for every captured *sibling* (a workspace that is neither the
//!    mutation's direct operand nor destroyed by it), prove its pre-op committed
//!    work is still preserved.
//!
//! # What "preserved" means
//!
//! A sibling that had committed work (its HEAD was ahead of its base epoch)
//! passes the audit when ANY of these hold post-mutation:
//!
//! * **Untouched / fast-forwarded** — its pre-op HEAD is still reachable from an
//!   anchor tip: the new epoch, the branch tip, the sibling's *current* HEAD, or
//!   any recovery ref. (A clean FF-only sibling keeps its old HEAD as an ancestor
//!   of the new epoch; an untouched sibling is trivially reachable from itself.)
//! * **Replayed** — the sibling was rebased onto the new epoch, so its pre-op
//!   HEAD OID is no longer reachable (rebase mints new OIDs) but its *work* is:
//!   the new HEAD is a strict descendant of the new epoch and sits at least as
//!   many commits ahead of it as the sibling was ahead of its old base. The
//!   rebase engine guarantees commit-count parity (see `sync::rebase`), so a
//!   faithful replay always satisfies this.
//!
//! Anything else is an **orphan**: the sibling had committed work, that work is
//! neither reachable nor replayed. This is exactly the bn-rah2 class (FF-absorb
//! raw-wrote a committed-ahead sibling's HEAD to the absorbed tip, discarding its
//! commit). On detection the auditor pins a durable recovery ref at the orphaned
//! OID, prints a loud violation block with a copy-pasteable `maw ws recover`
//! command, and the caller exits nonzero. No automatic rollback (v1): pin +
//! report is the safe primitive.
//!
//! # Cost
//!
//! All reachability decisions use merge-base / ahead-count checks (bounded to the
//! divergence between two tips), never a full history walk. For the common case —
//! siblings whose HEAD did not move — the first `merge_base(H, C)` returns `H`
//! immediately.
//!
//! # Relationship to the offline catalog
//!
//! This is the INLINE complement to the offline `fsck` invariant catalog
//! (bn-1uot). They deliberately share vocabulary (the `INVARIANT` prefix) but not
//! code: `fsck` audits a repo at rest; this audits a single mutation as it
//! happens.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::layout::LayoutFlavor;
use maw_git::{GitOid, GitRepo as _, GixRepo};

use super::capture::RECOVERY_PREFIX;
use super::{MawConfig, now_timestamp_iso8601_precise};

/// One workspace's committed state at the start of an epoch mutation.
#[derive(Clone, Debug)]
struct CapturedWorkspace {
    /// Workspace name.
    name: String,
    /// Worktree HEAD OID (hex) at capture time.
    head: String,
    /// The workspace's recorded base epoch OID (hex) at capture time. Its
    /// committed work is the range `base_epoch..head`.
    base_epoch: String,
}

/// Pre-mutation snapshot of every workspace's committed state.
///
/// Produced by [`capture`], consumed by [`audit`]. When
/// `invariant.audit = false` (config), `enabled` is `false` and the snapshot is
/// empty — [`audit`] then short-circuits to a disabled report.
pub struct PreCapture {
    enabled: bool,
    entries: Vec<CapturedWorkspace>,
}

impl PreCapture {
    /// A disabled snapshot — audit is off, nothing captured.
    #[must_use]
    const fn disabled() -> Self {
        Self {
            enabled: false,
            entries: Vec::new(),
        }
    }
}

/// A sibling whose committed work was orphaned by the mutation.
#[derive(Clone, Debug, Serialize)]
pub struct Orphan {
    /// Name of the workspace that lost committed work.
    pub workspace: String,
    /// The orphaned commit OID (the sibling's pre-mutation HEAD).
    pub oid: String,
    /// The recovery ref pinned at `oid`, if pinning succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<String>,
}

/// Result of a post-mutation audit.
///
/// Serializes to the structured `invariant` field embedded in `--format json`
/// command outputs: `{ "siblings_checked": N, "orphaned": [ … ] }`.
#[derive(Clone, Debug, Serialize)]
pub struct AuditReport {
    /// Number of sibling workspaces whose committed work was verified.
    pub siblings_checked: usize,
    /// Siblings whose committed work was orphaned (empty on success).
    pub orphaned: Vec<Orphan>,
    /// Whether the audit actually ran (`invariant.audit`). A disabled audit
    /// carries `siblings_checked = 0` and prints nothing.
    #[serde(skip)]
    enabled: bool,
    /// Short label for the mutation, used in the violation block (e.g.
    /// `"ws merge"`).
    #[serde(skip)]
    op_label: String,
}

impl AuditReport {
    const fn disabled() -> Self {
        Self {
            siblings_checked: 0,
            orphaned: Vec::new(),
            enabled: false,
            op_label: String::new(),
        }
    }

    /// True when the mutation orphaned committed work — the caller MUST exit
    /// nonzero.
    #[must_use]
    pub const fn is_violation(&self) -> bool {
        !self.orphaned.is_empty()
    }

    /// Whether the audit actually ran.
    ///
    /// Structured command outputs use this to omit their optional
    /// `invariant` field when `[invariant] audit = false`, matching the JSON
    /// contract instead of serializing a disabled placeholder report.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Emit the single-line success proof to stderr. No-op when the audit was
    /// disabled or a violation was found (the violation block is printed
    /// instead). Kept to ONE line — agents are token-conscious.
    pub fn emit_proof_line(&self) {
        if !self.enabled || self.is_violation() {
            return;
        }
        eprintln!(
            "INVARIANT: verified {} sibling workspace(s) — no committed work orphaned",
            self.siblings_checked
        );
    }

    /// Print the loud, actionable violation block to stderr. Names every
    /// orphaned workspace, its orphaned OID, and the exact recovery command.
    pub fn emit_violation_block(&self) {
        if self.orphaned.is_empty() {
            return;
        }
        eprintln!();
        eprintln!(
            "INVARIANT VIOLATION: {} orphaned committed work in {} sibling workspace(s)",
            self.op_label,
            self.orphaned.len()
        );
        eprintln!();
        for o in &self.orphaned {
            eprintln!(
                "  workspace '{}': commit {} is no longer reachable from the new epoch,",
                o.workspace, o.oid
            );
            eprintln!("    any workspace HEAD, or any recovery ref — it would be orphaned.");
            match &o.recovery_ref {
                Some(r) => {
                    eprintln!("    pinned recovery ref: {r}");
                    eprintln!(
                        "    recover with: maw ws recover --ref {r} --to {}",
                        o.workspace
                    );
                }
                None => {
                    eprintln!(
                        "    WARNING: failed to pin a recovery ref; the commit survives only in \
                         the reflog — recover it now: git -C <workspace> branch rescue {}",
                        o.oid
                    );
                }
            }
            eprintln!();
        }
        eprintln!(
            "  The mutation completed but was NOT rolled back (v1). The committed work above is",
        );
        eprintln!(
            "  preserved at the recovery refs; restore it, then re-run the mutation. This guard",
        );
        eprintln!("  is the Prime Invariant runtime auditor (bn-2rnq).");
    }
}

/// Snapshot every workspace's committed state at the start of an epoch mutation.
///
/// Cheap: one `backend.list()` plus a `rev_parse("HEAD")` and a ref read per
/// workspace. When `invariant.audit = false` (config) this returns a disabled
/// snapshot without touching any workspace.
///
/// Call this INSIDE the epoch lock, before the mutation begins.
#[must_use]
pub fn capture(root: &Path) -> PreCapture {
    let audit_on = MawConfig::load(root).map_or(true, |c| c.invariant_audit());
    if !audit_on {
        return PreCapture::disabled();
    }

    let backend = match super::get_backend() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "bn-2rnq: capture get_backend() failed; audit degraded");
            return PreCapture::disabled();
        }
    };

    let workspaces = match backend.list() {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!(error = %e, "bn-2rnq: capture backend.list() failed; audit degraded");
            return PreCapture::disabled();
        }
    };

    let flavor = LayoutFlavor::detect_with_env(root);
    let mut entries = Vec::with_capacity(workspaces.len());
    for info in &workspaces {
        let name = info.id.as_str();
        let ws_path = flavor.workspace_path(root, name);
        let Some(head) = resolve_head(&ws_path) else {
            // Unborn / unreadable HEAD — nothing committed to lose.
            continue;
        };
        let base_epoch = maw_core::refs::read_ref(root, &maw_core::refs::workspace_epoch_ref(name))
            .ok()
            .flatten()
            .map_or_else(|| head.clone(), |o| o.as_str().to_owned());
        entries.push(CapturedWorkspace {
            name: name.to_owned(),
            head,
            base_epoch,
        });
    }

    PreCapture {
        enabled: true,
        entries,
    }
}

/// Audit a completed mutation against its pre-capture snapshot.
///
/// `subjects` are the mutation's direct operands — the merge target and its
/// sources, or the advanced / synced workspace. They are excluded from the
/// sibling set (their HEAD movement is the mutation's whole point and is
/// governed by the mutation's own guarantees and by destroy records). Every
/// OTHER captured workspace is verified.
///
/// On an orphan, a durable recovery ref is pinned at the orphaned OID as a side
/// effect (the safe primitive). The returned report carries the orphan list; the
/// caller decides how to surface it (proof line vs. violation block) and MUST
/// exit nonzero when [`AuditReport::is_violation`] is true.
///
/// Call this AFTER the mutation, still inside the epoch lock.
#[must_use]
pub fn audit(root: &Path, pre: &PreCapture, subjects: &[&str], op_label: &str) -> AuditReport {
    if !pre.enabled {
        return AuditReport::disabled();
    }

    let repo = match GixRepo::open(root) {
        Ok(r) => r,
        Err(e) => {
            // Cannot open the repo to verify — fail safe by reporting a
            // degraded (but non-violating) audit rather than blocking the
            // mutation that already completed.
            tracing::warn!(error = %e, "bn-2rnq: audit could not open repo; skipping verification");
            return AuditReport::disabled();
        }
    };

    // Anchor tips shared by every sibling: the new epoch, the branch tip, and
    // every recovery ref (destroy-record pins + any pinned during this op).
    let mut shared_anchors: Vec<GitOid> = Vec::new();
    if let Ok(Some(epoch)) = maw_core::refs::read_epoch_current(root) {
        push_oid(&mut shared_anchors, epoch.as_str());
    }
    let new_epoch = shared_anchors.first().copied();
    let config = MawConfig::load(root).unwrap_or_default();
    let branch_ref = format!("refs/heads/{}", config.branch());
    if let Ok(Some(tip)) = maw_core::refs::read_ref(root, &branch_ref) {
        push_oid(&mut shared_anchors, tip.as_str());
    }
    if let Ok(refs) = repo.list_refs(RECOVERY_PREFIX) {
        for (_name, oid) in refs {
            shared_anchors.push(oid);
        }
    }

    let flavor = LayoutFlavor::detect_with_env(root);
    let mut siblings_checked = 0usize;
    let mut orphaned: Vec<Orphan> = Vec::new();

    for entry in &pre.entries {
        if subjects.contains(&entry.name.as_str()) {
            continue;
        }
        siblings_checked += 1;

        let Some(head) = parse_oid(&entry.head) else {
            continue;
        };
        let base = parse_oid(&entry.base_epoch);

        // No committed-ahead work → nothing to orphan.
        if base == Some(head) {
            continue;
        }

        // Per-sibling anchors: the shared set plus the sibling's CURRENT HEAD
        // (an untouched sibling still holds its own work).
        let ws_path = flavor.workspace_path(root, &entry.name);
        let current = resolve_head(&ws_path).and_then(|h| parse_oid(&h));

        if is_preserved(&repo, head, base, current, new_epoch, &shared_anchors) {
            continue;
        }

        // Orphaned: pin a durable recovery ref at the lost HEAD and record it.
        let recovery_ref = pin_invariant_recovery_ref(root, &entry.name, &entry.head);
        tracing::error!(
            workspace = %entry.name,
            oid = %entry.head,
            recovery_ref = ?recovery_ref,
            op = %op_label,
            "bn-2rnq: INVARIANT VIOLATION — committed work orphaned by epoch mutation"
        );
        orphaned.push(Orphan {
            workspace: entry.name.clone(),
            oid: entry.head.clone(),
            recovery_ref,
        });
    }

    AuditReport {
        siblings_checked,
        orphaned,
        enabled: true,
        op_label: op_label.to_owned(),
    }
}

/// Run the audit, emit its output, and fail on a violation — the one-call path
/// for commands that don't need to interleave the result into a larger output
/// struct.
///
/// On success, prints the one-line proof to stderr and returns the report (so
/// the caller may still embed the structured field in a JSON body). On a
/// violation, prints the loud block to stderr and returns an `Err` so the
/// command exits nonzero WITHOUT declaring success.
///
/// # Errors
///
/// Returns an error when the mutation orphaned committed work.
pub fn finish(
    root: &Path,
    pre: &PreCapture,
    subjects: &[&str],
    op_label: &str,
) -> Result<AuditReport> {
    let report = audit(root, pre, subjects, op_label);
    if report.is_violation() {
        report.emit_violation_block();
        anyhow::bail!(
            "Prime-Invariant violation: {} sibling workspace(s) would lose committed work (see recovery refs above).",
            report.orphaned.len()
        );
    }
    report.emit_proof_line();
    Ok(report)
}

/// Decide whether a sibling's committed work survived the mutation.
///
/// `head`/`base` are the sibling's pre-op HEAD and base epoch (known `head !=
/// base`, i.e. it had committed-ahead work). `current` is its post-op HEAD (None
/// if the worktree vanished). `new_epoch` is the post-op global epoch. `anchors`
/// are the shared reachability tips.
fn is_preserved(
    repo: &GixRepo,
    head: GitOid,
    base: Option<GitOid>,
    current: Option<GitOid>,
    new_epoch: Option<GitOid>,
    anchors: &[GitOid],
) -> bool {
    // (b) Reachable from any anchor tip, or from the sibling's own current HEAD.
    if let Some(c) = current
        && is_ancestor(repo, head, c)
    {
        return true;
    }
    for tip in anchors {
        if is_ancestor(repo, head, *tip) {
            return true;
        }
    }

    // (c) Replayed onto the new epoch: the current HEAD is a strict descendant
    // of the new epoch and sits at least as many commits ahead of it as the
    // sibling was ahead of its old base (commit-count parity of a faithful
    // rebase). This is what keeps a legitimate auto-rebase from false-flagging —
    // its pre-op OID is gone but its work rode forward as new commits.
    if let (Some(c), Some(e), Some(b)) = (current, new_epoch, base)
        && c != e
        && is_ancestor(repo, e, c)
    {
        let ahead_after = count_between(repo, e, c);
        let ahead_before = count_between(repo, b, head);
        if let (Some(after), Some(before)) = (ahead_after, ahead_before)
            && after >= before
        {
            return true;
        }
    }

    false
}

/// `ancestor` reachable from `descendant`? Uses a bounded merge-base check
/// (`merge_base(a, d) == a`), never a full history walk.
fn is_ancestor(repo: &GixRepo, ancestor: GitOid, descendant: GitOid) -> bool {
    if ancestor == descendant {
        return true;
    }
    matches!(repo.merge_base(ancestor, descendant), Ok(Some(base)) if base == ancestor)
}

/// Number of commits in `from..to` (how many `to` is ahead of `from`).
fn count_between(repo: &GixRepo, from: GitOid, to: GitOid) -> Option<u32> {
    repo.count_commits_between(from, to).ok()
}

/// Resolve a worktree's HEAD to a hex OID string, or `None` if unborn/unreadable.
fn resolve_head(ws_path: &Path) -> Option<String> {
    let repo = GixRepo::open(ws_path).ok()?;
    repo.rev_parse_opt("HEAD")
        .ok()
        .flatten()
        .map(|o| o.to_string())
}

/// Pin `refs/manifold/recovery/<ws>/invariant-<ts>` at `oid`. Best-effort;
/// returns the ref name on success. This namespace survives `ws destroy` and is
/// what `maw ws recover` reads.
fn pin_invariant_recovery_ref(root: &Path, ws_name: &str, oid: &str) -> Option<String> {
    // `maw_core::refs::write_ref` takes the domain `GitOid` (validated string),
    // distinct from the `maw_git::GitOid` byte-array used for graph ops above.
    let git_oid = match maw_core::model::types::GitOid::new(oid) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("bn-2rnq: invalid orphaned OID '{oid}' for recovery pin: {e}");
            return None;
        }
    };
    let ts = now_timestamp_iso8601_precise().replace(':', "-");
    let ref_name = format!("{RECOVERY_PREFIX}{ws_name}/invariant-{ts}");
    match maw_core::refs::write_ref(root, &ref_name, &git_oid) {
        Ok(()) => {
            tracing::info!(ref_name = %ref_name, oid = %oid, "bn-2rnq: pinned invariant recovery ref");
            Some(ref_name)
        }
        Err(e) => {
            tracing::warn!("bn-2rnq: failed to pin invariant recovery ref '{ref_name}': {e}");
            None
        }
    }
}

/// Parse a hex OID string into a `GitOid`, logging on malformed input.
fn parse_oid(hex: &str) -> Option<GitOid> {
    hex.parse::<GitOid>().ok()
}

/// Push a parsed OID onto `anchors`, silently skipping malformed hex.
fn push_oid(anchors: &mut Vec<GitOid>, hex: &str) {
    if let Some(oid) = parse_oid(hex) {
        anchors.push(oid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(orphaned: Vec<Orphan>) -> AuditReport {
        AuditReport {
            siblings_checked: 3,
            orphaned,
            enabled: true,
            op_label: "ws merge".to_owned(),
        }
    }

    #[test]
    fn clean_report_serializes_to_the_invariant_shape() {
        let r = report(Vec::new());
        assert!(!r.is_violation());
        let v: serde_json::Value = serde_json::to_value(&r).expect("serialize");
        assert_eq!(v["siblings_checked"], 3);
        assert!(
            v["orphaned"].as_array().expect("array").is_empty(),
            "clean audit has an empty orphaned list"
        );
    }

    #[test]
    fn violation_report_is_flagged_and_carries_the_orphan() {
        let r = report(vec![Orphan {
            workspace: "bob".to_owned(),
            oid: "a".repeat(40),
            recovery_ref: Some("refs/manifold/recovery/bob/invariant-x".to_owned()),
        }]);
        assert!(r.is_violation());
        let v: serde_json::Value = serde_json::to_value(&r).expect("serialize");
        assert_eq!(v["orphaned"][0]["workspace"], "bob");
        assert_eq!(v["orphaned"][0]["oid"], "a".repeat(40));
        assert_eq!(
            v["orphaned"][0]["recovery_ref"],
            "refs/manifold/recovery/bob/invariant-x"
        );
    }

    #[test]
    fn disabled_report_never_violates() {
        let r = AuditReport::disabled();
        assert!(!r.is_violation());
        assert_eq!(r.siblings_checked, 0);
    }

    #[test]
    fn config_defaults_audit_on() {
        // A fresh config (no `[invariant]` table) must default the auditor ON.
        assert!(MawConfig::default().invariant_audit());
    }
}
