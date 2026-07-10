//! `maw fsck` — offline deep verifier of all manifold state (bn-1uot).
//!
//! Where [`crate::doctor`] is a fast environment/health triage for interactive
//! use, `maw fsck` exhaustively checks every cross-artifact invariant of the
//! manifold: refs point at real objects of the right kind, registered
//! workspaces agree with their worktrees, destroy records stay coherent with
//! their recovery refs, the oplog parses, no stale merge-state blocks merges,
//! and the epoch is an ancestor of the configured branch.
//!
//! # The catalog is the product
//!
//! Every invariant is declared **once** in [`invariants::catalog`] with a
//! stable `id`, a [`Severity`], a human description, a `check` implementation,
//! and (for the provably-safe subset) a `repair`. `maw doctor` renders a fast
//! subset of the same catalog (the state-coherence checks) so the logic lives
//! in a single place; `maw fsck` runs everything.
//!
//! # Repair safety
//!
//! `maw fsck --repair` only applies repairs that are provably non-destructive:
//! re-pinning a recovery ref to an OID that still exists, or removing a stale
//! merge-state whose owner process is gone. It **never** deletes refs,
//! records, or objects that pin content — that destructive cleanup stays in
//! `maw gc`. A repair that would need to delete content declines and says so.

pub mod invariants;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::format::OutputFormat;
use crate::workspace;
use maw_core::model::layout::LayoutFlavor;

/// Version of the `--format json` shape. Bump on any breaking change.
pub const FSCK_SCHEMA: u32 = 1;

// Exit codes (bn-1uot). Distinct codes let CI distinguish warn-only drift
// from actual corruption.
/// Everything checked clean (or only informational notes remain).
pub const EXIT_CLEAN: i32 = 0;
/// At least one warn-severity violation remains (no corruption).
pub const EXIT_WARN: i32 = 1;
/// At least one error-severity violation remains (corruption).
pub const EXIT_CORRUPTION: i32 = 2;

// ---------------------------------------------------------------------------
// Catalog vocabulary
// ---------------------------------------------------------------------------

/// Severity of an invariant. Ordered so `max()` yields the worst seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Harmless, purely informational (e.g. a stale-but-safe lockfile).
    Info,
    /// Something is off but no content is at risk (drain a queue, run gc).
    Warn,
    /// Structural corruption — a ref/record points at a missing object.
    Error,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Map to a `maw doctor` status string (`ok`/`warn`/`fail`).
    ///
    /// Doctor has no "info" tier; harmless notes render as `warn`.
    const fn doctor_status(self) -> &'static str {
        match self {
            Self::Info | Self::Warn => "warn",
            Self::Error => "fail",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Info => "[INFO]",
            Self::Warn => "[WARN]",
            Self::Error => "[FAIL]",
        }
    }
}

/// A single concrete violation of an invariant.
#[derive(Clone, Debug)]
pub struct Violation {
    /// Human-readable description of the specific violation.
    pub detail: String,
    /// The exact command that fixes it (AGENTS.md output rules).
    pub fix: Option<String>,
    /// Whether `maw fsck --repair` can safely resolve this specific violation.
    pub repairable: bool,
}

impl Violation {
    /// Build a non-repairable violation with a fix hint.
    #[must_use]
    pub fn new(detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            detail: detail.into(),
            fix,
            repairable: false,
        }
    }

    /// Build a violation that `--repair` can safely resolve.
    #[must_use]
    pub fn repairable(detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            detail: detail.into(),
            fix,
            repairable: true,
        }
    }
}

/// Shared, read-mostly context handed to every invariant check.
pub struct Ctx {
    /// Absolute repo root.
    pub root: PathBuf,
    /// Detected layout flavor (consolidated vs v2).
    pub flavor: LayoutFlavor,
    /// Working directory to open git from for ref/object queries — the default
    /// workspace worktree when present, else the root (mirrors the convention
    /// in `workspace::recover`).
    pub git_cwd: PathBuf,
    /// Configured branch name (default `main`).
    pub branch: String,
    /// Configured default-workspace name (default `default`).
    pub default_workspace: String,
}

impl Ctx {
    fn new(root: PathBuf) -> Self {
        let flavor = LayoutFlavor::detect_with_env(&root);
        let default_ws = flavor.default_target_path(&root, "default");
        let git_cwd = if default_ws.exists() {
            default_ws
        } else {
            root.clone()
        };
        let (branch, default_workspace) = workspace::MawConfig::load(&root).map_or_else(
            |_| ("main".to_string(), "default".to_string()),
            |c| (c.branch().to_string(), c.default_workspace().to_string()),
        );
        Self {
            root,
            flavor,
            git_cwd,
            branch,
            default_workspace,
        }
    }

    /// Open the repo for ref/object queries (from `git_cwd`).
    ///
    /// # Errors
    ///
    /// Returns an error if the repository cannot be opened.
    pub fn open_repo(&self) -> Result<maw_git::GixRepo> {
        maw_git::GixRepo::open(&self.git_cwd)
            .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", self.git_cwd.display()))
    }
}

/// One catalog invariant: a stable id, severity, description, a check, and an
/// optional provably-safe repair.
pub trait Invariant: Sync {
    /// Stable identifier (kebab-case). Part of the JSON contract — never
    /// rename an existing id.
    fn id(&self) -> &'static str;
    /// Declared worst-case severity for this invariant.
    fn severity(&self) -> Severity;
    /// One-line human description of what the invariant guarantees.
    fn description(&self) -> &'static str;
    /// Run the check. An empty vec means the invariant holds.
    ///
    /// # Errors
    ///
    /// Returns an error only for an unexpected failure to evaluate the check
    /// itself (I/O, git); a detected violation is a normal `Ok` value.
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>>;
    /// Whether `--repair` can act on this invariant at all.
    fn is_repairable(&self) -> bool {
        false
    }
    /// Whether `maw doctor` includes this invariant in its fast subset.
    fn in_doctor(&self) -> bool {
        false
    }
    /// Apply the provably-safe repair. Returns one receipt line per action
    /// taken (or, under `dry_run`, per action that *would* be taken). A
    /// receipt may also explain why a repair was declined.
    ///
    /// # Errors
    ///
    /// Returns an error if a repair action fails partway.
    fn repair(&self, _ctx: &Ctx, _dry_run: bool) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Outcome of running one invariant.
struct InvariantOutcome {
    id: &'static str,
    severity: Severity,
    description: &'static str,
    violations: Vec<Violation>,
    repairs: Vec<String>,
}

impl InvariantOutcome {
    const fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Run one invariant, applying repair when requested.
fn run_invariant(inv: &dyn Invariant, ctx: &Ctx, repair: bool, dry_run: bool) -> InvariantOutcome {
    let mut violations = match inv.check(ctx) {
        Ok(v) => v,
        Err(e) => vec![Violation::new(format!("could not evaluate ({e})"), None)],
    };

    let mut repairs = Vec::new();
    if repair && inv.is_repairable() && !violations.is_empty() {
        match inv.repair(ctx, dry_run) {
            Ok(receipts) => repairs = receipts,
            Err(e) => repairs.push(format!("repair failed: {e}")),
        }
        if !dry_run {
            // Re-check so the reported state reflects the post-repair reality.
            violations = inv.check(ctx).unwrap_or(violations);
        }
    }

    InvariantOutcome {
        id: inv.id(),
        severity: inv.severity(),
        description: inv.description(),
        violations,
        repairs,
    }
}

/// Compute the process exit code from the worst remaining violation.
fn exit_code(outcomes: &[InvariantOutcome]) -> i32 {
    let mut worst: Option<Severity> = None;
    for o in outcomes {
        if o.violations.is_empty() {
            continue;
        }
        worst = Some(worst.map_or(o.severity, |w| w.max(o.severity)));
    }
    match worst {
        Some(Severity::Error) => EXIT_CORRUPTION,
        Some(Severity::Warn) => EXIT_WARN,
        // Info-only findings are harmless: report them but exit clean.
        Some(Severity::Info) | None => EXIT_CLEAN,
    }
}

fn total_violations(outcomes: &[InvariantOutcome]) -> usize {
    outcomes.iter().map(|o| o.violations.len()).sum()
}

fn repairable_violations(outcomes: &[InvariantOutcome]) -> usize {
    outcomes
        .iter()
        .flat_map(|o| &o.violations)
        .filter(|v| v.repairable)
        .count()
}

// ---------------------------------------------------------------------------
// JSON shape
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ViolationJson {
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
    repairable: bool,
}

#[derive(Serialize)]
struct InvariantJson {
    id: &'static str,
    severity: Severity,
    status: &'static str,
    description: &'static str,
    detail: String,
    violations: Vec<ViolationJson>,
    repair: Vec<String>,
}

#[derive(Serialize)]
struct SummaryJson {
    checked: usize,
    violations: usize,
    repairable: usize,
    exit_code: i32,
}

#[derive(Serialize)]
struct FsckEnvelope {
    fsck_schema: u32,
    invariants: Vec<InvariantJson>,
    summary: SummaryJson,
}

fn to_json(outcome: &InvariantOutcome) -> InvariantJson {
    let status = if outcome.is_clean() {
        "ok"
    } else {
        "violation"
    };
    let detail = if outcome.is_clean() {
        format!("{}: ok", outcome.id)
    } else {
        format!("{} violation(s)", outcome.violations.len())
    };
    InvariantJson {
        id: outcome.id,
        severity: outcome.severity,
        status,
        description: outcome.description,
        detail,
        violations: outcome
            .violations
            .iter()
            .map(|v| ViolationJson {
                detail: v.detail.clone(),
                fix: v.fix.clone(),
                repairable: v.repairable,
            })
            .collect(),
        repair: outcome.repairs.clone(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn print_text(outcomes: &[InvariantOutcome], repair: bool, dry_run: bool) {
    println!("maw fsck");
    println!("========");
    println!();

    for o in outcomes {
        if o.is_clean() {
            println!("[OK]   {} — {}", o.id, o.description);
        } else {
            println!("{} {} — {}", o.severity.label(), o.id, o.description);
            for v in &o.violations {
                println!("       - {}", v.detail);
                if let Some(fix) = &v.fix {
                    println!("         fix: {fix}");
                }
            }
        }
        for r in &o.repairs {
            let prefix = if dry_run { "would repair" } else { "repaired" };
            println!("       {prefix}: {r}");
        }
    }

    println!();
    let checked = outcomes.len();
    let violations = total_violations(outcomes);
    let repairable = repairable_violations(outcomes);
    println!(
        "fsck: {checked} invariants checked, {violations} violation(s) ({repairable} repairable)"
    );
    if violations > 0 && !repair && repairable > 0 {
        println!("Run `maw fsck --repair` to apply the {repairable} safe repair(s).");
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run `maw fsck` over the whole catalog.
///
/// When `repair` is set, provably-safe repairs are applied (or, with
/// `dry_run`, only reported). Exits the process with [`EXIT_CLEAN`],
/// [`EXIT_WARN`], or [`EXIT_CORRUPTION`] according to the worst remaining
/// violation.
///
/// # Errors
///
/// Returns an error if the repo root cannot be determined or output
/// serialization fails.
pub fn run(format: Option<OutputFormat>, repair: bool, dry_run: bool) -> Result<()> {
    let format = OutputFormat::resolve(format);
    let root = workspace::repo_root()
        .context("maw fsck must run inside a maw repository (could not locate the repo root)")?;
    let outcomes = run_catalog(&root, repair, dry_run);

    match format {
        OutputFormat::Json => {
            let envelope = FsckEnvelope {
                fsck_schema: FSCK_SCHEMA,
                invariants: outcomes.iter().map(to_json).collect(),
                summary: SummaryJson {
                    checked: outcomes.len(),
                    violations: total_violations(&outcomes),
                    repairable: repairable_violations(&outcomes),
                    exit_code: exit_code(&outcomes),
                },
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text | OutputFormat::Pretty => print_text(&outcomes, repair, dry_run),
    }

    let _ = std::io::stdout().flush();
    let code = exit_code(&outcomes);
    if code != EXIT_CLEAN {
        std::process::exit(code);
    }
    Ok(())
}

/// Run every catalog invariant against `root`. Exposed for tests.
fn run_catalog(root: &Path, repair: bool, dry_run: bool) -> Vec<InvariantOutcome> {
    let ctx = Ctx::new(root.to_path_buf());
    invariants::catalog()
        .iter()
        .map(|inv| run_invariant(inv.as_ref(), &ctx, repair, dry_run))
        .collect()
}

// ---------------------------------------------------------------------------
// Doctor delegation (bn-1uot)
// ---------------------------------------------------------------------------

/// A doctor-facing rendering of one catalog invariant.
///
/// `maw doctor` runs the fast subset of the catalog (`in_doctor()`) and folds
/// the result into its own check list, so the coherence logic lives once.
pub struct DoctorDelegation {
    /// Stable invariant id (used as the doctor check `name`).
    pub name: String,
    /// `ok`/`warn`/`fail`.
    pub status: String,
    /// Human-readable summary line.
    pub message: String,
    /// Exact fix command, if any.
    pub fix: Option<String>,
}

/// Run the doctor subset of the catalog and return one delegation per
/// invariant, in catalog order.
///
/// # Errors
///
/// Never returns `Err` today (check failures are folded into the message),
/// but the signature is fallible for forward-compatibility.
#[must_use]
pub fn doctor_delegations(root: &Path) -> Vec<DoctorDelegation> {
    let ctx = Ctx::new(root.to_path_buf());
    let mut out = Vec::new();
    for inv in &invariants::catalog() {
        if !inv.in_doctor() {
            continue;
        }
        let outcome = run_invariant(inv.as_ref(), &ctx, false, false);
        out.push(delegation_for(&outcome));
    }
    out
}

fn delegation_for(outcome: &InvariantOutcome) -> DoctorDelegation {
    if outcome.is_clean() {
        return DoctorDelegation {
            name: outcome.id.to_string(),
            status: "ok".to_string(),
            message: format!("{}: ok — {}", outcome.id, outcome.description),
            fix: None,
        };
    }
    // Summarize: first violation's detail carries the specifics; the fix is
    // the first violation's fix.
    let first = outcome.violations.first().expect("non-empty checked above");
    let extra = outcome.violations.len().saturating_sub(1);
    let suffix = if extra > 0 {
        format!(" (+{extra} more)")
    } else {
        String::new()
    };
    DoctorDelegation {
        name: outcome.id.to_string(),
        status: outcome.severity.doctor_status().to_string(),
        message: format!("{}: {}{}", outcome.id, first.detail, suffix),
        fix: first.fix.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_error_worst() {
        assert!(Severity::Error > Severity::Warn);
        assert!(Severity::Warn > Severity::Info);
    }

    #[test]
    fn exit_code_reflects_worst_severity() {
        let clean = vec![InvariantOutcome {
            id: "x",
            severity: Severity::Error,
            description: "d",
            violations: vec![],
            repairs: vec![],
        }];
        assert_eq!(exit_code(&clean), EXIT_CLEAN);

        let warn = vec![InvariantOutcome {
            id: "x",
            severity: Severity::Warn,
            description: "d",
            violations: vec![Violation::new("bad", None)],
            repairs: vec![],
        }];
        assert_eq!(exit_code(&warn), EXIT_WARN);

        let corrupt = vec![InvariantOutcome {
            id: "x",
            severity: Severity::Error,
            description: "d",
            violations: vec![Violation::new("bad", None)],
            repairs: vec![],
        }];
        assert_eq!(exit_code(&corrupt), EXIT_CORRUPTION);

        let info = vec![InvariantOutcome {
            id: "x",
            severity: Severity::Info,
            description: "d",
            violations: vec![Violation::new("note", None)],
            repairs: vec![],
        }];
        assert_eq!(exit_code(&info), EXIT_CLEAN);
    }

    #[test]
    fn doctor_subset_is_the_documented_six() {
        let ids: Vec<&'static str> = invariants::catalog()
            .iter()
            .filter(|i| i.in_doctor())
            .map(|i| i.id())
            .collect();
        for expected in [
            "dangling-snapshots",
            "abandoned-with-snapshot",
            "destroy-record-unpinned",
            "stale-head-refs",
            "merge-state",
            "ghost-working-copy",
        ] {
            assert!(
                ids.contains(&expected),
                "doctor subset must include {expected}, got {ids:?}"
            );
        }
    }

    #[test]
    fn catalog_ids_are_unique() {
        let cat = invariants::catalog();
        let mut ids: Vec<&'static str> = cat.iter().map(|i| i.id()).collect();
        ids.sort_unstable();
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids, deduped, "catalog ids must be unique");
    }
}
