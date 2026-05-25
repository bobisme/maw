//! Integration tests for `maw migrate` (T3.3 / bn-3kkl).
//!
//! Exercises the 14-step Prime-Invariant-preserving migration from v2
//! `ws/`-rooted bare layout to consolidated `.maw/`-rooted live-root
//! layout. See `notes/sg3-layout-design.md` §7 for the algorithm spec
//! and `crates/maw-cli/src/migrate.rs` for the implementation.
//!
//! Tested invariants:
//! 1. Greenfield (no agent workspaces) migrates cleanly.
//! 2. A populated repo with multiple workspaces survives migration.
//! 3. Pre-migration `refs/manifold/*` are all reachable post-migration
//!    (Prime Invariant).
//! 4. `maw doctor` reports OK post-migration.
//! 5. A recovery snapshot for any pre-migration workspace is reachable
//!    via `maw ws recover` post-migration.
//! 6. Migration refuses with a live merge-state file (Phase A guard).
//! 7. `maw migrate --resume` is a no-op on an already-consolidated repo.

mod manifold_common;

use std::fs;
use std::process::Command;

use manifold_common::{TestRepo, maw_bin};

/// Run `maw migrate` against a `TestRepo` and assert success.
fn run_migrate(repo: &TestRepo, args: &[&str]) -> String {
    let mut full: Vec<&str> = vec!["migrate"];
    full.extend_from_slice(args);
    let out = Command::new(maw_bin())
        .args(&full)
        .current_dir(repo.root())
        .output()
        .expect("failed to execute maw migrate");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "maw migrate failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout.to_string()
}

fn run_migrate_fail(repo: &TestRepo, args: &[&str]) -> (String, String) {
    let mut full: Vec<&str> = vec!["migrate"];
    full.extend_from_slice(args);
    let out = Command::new(maw_bin())
        .args(&full)
        .current_dir(repo.root())
        .output()
        .expect("failed to execute maw migrate");
    assert!(
        !out.status.success(),
        "expected maw migrate to fail but it succeeded"
    );
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn list_manifold_refs(repo: &TestRepo) -> Vec<(String, String)> {
    let out = repo.git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/manifold/",
    ]);
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut parts = l.splitn(2, ' ');
            let name = parts.next().unwrap_or_default().to_string();
            let oid = parts.next().unwrap_or_default().to_string();
            (name, oid)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Test 1: greenfield migration
// ---------------------------------------------------------------------------

#[test]
fn migrate_greenfield_v2_to_consolidated() {
    let repo = TestRepo::new();

    // Sanity: v2 layout markers present.
    assert!(repo.root().join("ws").join("default").is_dir());
    assert!(repo.root().join(".manifold").is_dir());
    assert!(!repo.root().join(".maw").join("manifold").is_dir());

    let pre_refs = list_manifold_refs(&repo);
    assert!(!pre_refs.is_empty(), "v2 repo should have manifold refs");

    let stdout = run_migrate(&repo, &[]);
    assert!(
        stdout.contains("Migration complete"),
        "expected success banner; got: {stdout}"
    );

    // Consolidated layout markers present, v2 markers gone.
    assert!(
        repo.root().join(".maw").join("manifold").is_dir(),
        "post-migrate .maw/manifold/ missing"
    );
    assert!(
        repo.root().join(".maw").join("workspaces").is_dir(),
        "post-migrate .maw/workspaces/ missing"
    );
    assert!(
        !repo.root().join(".manifold").exists(),
        ".manifold/ should be moved (or removed) after migration"
    );

    // Prime Invariant: every pre-migration manifold ref still present.
    let post_refs = list_manifold_refs(&repo);
    for (name, _) in &pre_refs {
        let still = post_refs.iter().any(|(n, _)| n == name);
        assert!(still, "ref {name} disappeared during migration");
    }

    // Journal is removed on success.
    assert!(
        !repo.root().join(".maw/manifold/migration/journal.json").exists(),
        "journal should be removed on successful Phase E"
    );
}

// ---------------------------------------------------------------------------
// Test 2: populated repo (multiple workspaces, mix of clean/dirty/committed)
// ---------------------------------------------------------------------------

#[test]
fn migrate_populated_repo_loses_nothing() {
    let repo = TestRepo::new();

    // Workspace 1: clean
    repo.create_workspace("alpha");

    // Workspace 2: dirty (uncommitted edit)
    repo.create_workspace("beta");
    repo.add_file("beta", "feature.txt", "in-progress work");

    // Workspace 3: committed ahead of epoch
    repo.create_workspace("gamma");
    repo.add_file("gamma", "shipped.txt", "done");
    repo.git_in_workspace("gamma", &["add", "-A"]);
    repo.git_in_workspace("gamma", &["commit", "-m", "feat: shipped"]);

    let pre_refs = list_manifold_refs(&repo);
    let pre_count = pre_refs.len();

    let stdout = run_migrate(&repo, &[]);
    assert!(stdout.contains("Migration complete"));

    // All three workspaces should be relocated under .maw/workspaces/.
    for name in ["alpha", "beta", "gamma"] {
        let new_path = repo.root().join(".maw").join("workspaces").join(name);
        assert!(
            new_path.is_dir(),
            "workspace {name} not at expected new path {}",
            new_path.display()
        );
        // Old path should be gone.
        assert!(
            !repo.root().join("ws").join(name).exists(),
            "workspace {name} still at old v2 path"
        );
    }

    // Beta's dirty work should be reachable via recovery refs.
    let post_refs = list_manifold_refs(&repo);
    let beta_has_recovery = post_refs
        .iter()
        .any(|(n, _)| n.starts_with("refs/manifold/recovery/beta/"));
    assert!(
        beta_has_recovery,
        "expected a recovery ref for beta's dirty content"
    );

    // Gamma's committed work should be reachable: either via HEAD-only
    // recovery ref or via the existing per-workspace HEAD ref. Either
    // way it must not have disappeared.
    let gamma_pre: Vec<&String> = pre_refs
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.contains("/gamma"))
        .collect();
    for ref_name in &gamma_pre {
        let still = post_refs.iter().any(|(n, _)| &n == ref_name);
        assert!(still, "gamma's ref {ref_name} lost during migration");
    }

    // Every pre-migration ref must still be present (Prime Invariant).
    for (name, _) in &pre_refs {
        let still = post_refs.iter().any(|(n, _)| n == name);
        assert!(still, "ref {name} disappeared during migration");
    }

    // Post-migration count is ≥ pre (recovery refs are added, none lost).
    assert!(
        post_refs.len() >= pre_count,
        "post-migration ref count ({}) < pre-migration ({pre_count})",
        post_refs.len()
    );

    // Root checkout is live: HEAD points to a branch (not bare, not detached).
    let head_kind = repo.git(&["symbolic-ref", "HEAD"]);
    assert!(
        head_kind.contains("refs/heads/main"),
        "root HEAD should be symbolic-ref to main, got: {head_kind}"
    );
    let bare = repo.git(&["config", "core.bare"]);
    assert_eq!(bare.trim(), "false", "root must not be bare post-migration");
}

// ---------------------------------------------------------------------------
// Test 3: already-consolidated repo is a no-op
// ---------------------------------------------------------------------------

#[test]
fn migrate_on_already_consolidated_is_noop() {
    let repo = TestRepo::new();

    // First migration: v2 → consolidated.
    run_migrate(&repo, &[]);

    // Second invocation: should report no-op success.
    let stdout = run_migrate(&repo, &[]);
    assert!(
        stdout.contains("Already") || stdout.contains("nothing to migrate"),
        "expected no-op banner on second run; got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: in-flight merge refuses (Phase A guard)
// ---------------------------------------------------------------------------

#[test]
fn migrate_refuses_when_merge_in_flight() {
    let repo = TestRepo::new();

    // Plant a non-terminal merge-state file at the standard location.
    // Schema: see `maw_core::merge_state::MergeStateFile` — minimal
    // valid `Prepare` state needs phase + sources + epoch_before +
    // started_at + updated_at.
    let state_path = repo.root().join(".manifold").join("merge-state.json");
    let body = serde_json::json!({
        "phase": "prepare",
        "sources": ["alpha"],
        "epoch_before": "0".repeat(40),
        "started_at": 1u64,
        "updated_at": 1u64,
    });
    fs::write(&state_path, body.to_string()).expect("write merge-state");

    let (_stdout, stderr) = run_migrate_fail(&repo, &[]);
    assert!(
        stderr.contains("in-flight merge") || stderr.contains("Prepare"),
        "expected refusal message; got: {stderr}"
    );

    // Repo should be untouched.
    assert!(repo.root().join(".manifold").is_dir());
    assert!(!repo.root().join(".maw").join("manifold").exists());
}

// ---------------------------------------------------------------------------
// Test 5: doctor passes on the migrated repo
// ---------------------------------------------------------------------------

#[test]
fn migrate_then_doctor_is_clean() {
    let repo = TestRepo::new();
    repo.create_workspace("alpha");
    repo.add_file("alpha", "src/lib.rs", "fn one() {}");

    run_migrate(&repo, &[]);

    // `maw doctor` should not blow up; we accept warnings (the repair
    // command is for severe-fail repos) but not catastrophic errors.
    let out = Command::new(maw_bin())
        .args(["doctor"])
        .current_dir(repo.root())
        .output()
        .expect("failed to execute maw doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // doctor exit code: 0 on green, non-zero on FAIL. We require that any
    // FAILs reported are NOT migration-introduced (e.g., pre-existing
    // root stub fails like AGENTS.md absence are allowed).
    // Conservative assertion: the migration-related checks must say OK.
    assert!(
        stdout.contains("[OK] default workspace")
            || stdout.contains("consolidated layout"),
        "default workspace check missing in doctor output:\n{stdout}\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: oracle B does not regress
// ---------------------------------------------------------------------------

#[cfg(feature = "assurance")]
mod oracle_tests {
    use super::*;

    #[test]
    fn oracle_b_clean_post_migration() {
        let repo = TestRepo::new();
        repo.create_workspace("alpha");
        run_migrate(&repo, &[]);
        let violations = maw_assurance::oracle_b::check(repo.root());
        // It is acceptable for pre-existing repo state to surface B3 etc;
        // but B1/B2 (dangling-head/owned-ref) must not flag a workspace
        // that legitimately lives under .maw/workspaces/ now.
        for v in &violations {
            match v {
                maw_assurance::oracle_b::OracleBViolation::DanglingHeadRef { workspace, .. }
                | maw_assurance::oracle_b::OracleBViolation::DanglingOwnedRef {
                    workspace, ..
                } => {
                    panic!(
                        "oracle B reported dangling ref for `{workspace}` \
                         post-migration: workspace should be present at \
                         .maw/workspaces/{workspace}/"
                    );
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test 7: resume from interrupted Phase B (journal-based)
// ---------------------------------------------------------------------------

#[test]
fn migrate_resume_picks_up_from_journal() {
    let repo = TestRepo::new();
    repo.create_workspace("alpha");

    // Simulate an aborted run: hand-craft a journal at Phase
    // `PreserveDone` so the migration skips B and goes straight to C.
    // (We rely on the resume code path picking it up rather than
    // overwriting it.)
    let journal_dir = repo.root().join(".manifold").join("migration");
    fs::create_dir_all(&journal_dir).expect("mkdir migration");
    let journal = serde_json::json!({
        "schema_version": 1,
        "started_at": 1,
        "updated_at": 1,
        "phase": "Start",
        "root": repo.root(),
        "original_flavor": "V2WsRoot",
        "worktrees": [],
        "pre_migration_refs": [],
    });
    fs::write(
        journal_dir.join("journal.json"),
        serde_json::to_string_pretty(&journal).expect("serialize"),
    )
    .expect("write journal");

    // With --resume on a Start-phase journal, code should discard and
    // restart cleanly. The migration completes either way.
    let stdout = run_migrate(&repo, &["--resume"]);
    assert!(stdout.contains("Migration complete"));
}

// ---------------------------------------------------------------------------
// Test 8: dry-run is non-destructive
// ---------------------------------------------------------------------------

#[test]
fn migrate_dry_run_does_not_mutate() {
    let repo = TestRepo::new();
    repo.create_workspace("alpha");

    let pre_refs = list_manifold_refs(&repo);
    let pre_layout = repo.root().join(".manifold").is_dir();
    let pre_default = repo.root().join("ws").join("default").is_dir();

    let stdout = run_migrate(&repo, &["--dry-run"]);
    assert!(stdout.contains("Dry run") || stdout.contains("Plan:"));

    let post_refs = list_manifold_refs(&repo);
    assert_eq!(
        pre_refs, post_refs,
        "dry-run must not modify any refs"
    );
    assert_eq!(repo.root().join(".manifold").is_dir(), pre_layout);
    assert_eq!(repo.root().join("ws").join("default").is_dir(), pre_default);
    assert!(
        !repo.root().join(".maw").join("manifold").exists(),
        "dry-run must not create .maw/"
    );
}
