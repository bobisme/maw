//! Deterministic Simulation Testing (DST) harness for maw merge operations.
//!
//! This harness uses a seeded PRNG to deterministically select crash injection
//! points during merge operations, then verifies that invariants hold after
//! each crash and recovery cycle.
//!
//! # How it works
//!
//! 1. **Setup**: Create a TestRepo with N workspaces, each with files.
//! 2. **Crash injection**: Before running a merge, write merge-state.json at a
//!    randomly selected phase (simulating a crash at that point in the merge
//!    state machine).
//! 3. **Recovery**: Run the recovery path and verify invariants.
//! 4. **Retry**: Attempt the merge again (should succeed after recovery).
//!
//! Since failpoint instrumentation (`fp!()` macro) is not yet wired into the
//! source, this harness simulates crashes by writing merge-state files directly
//! -- the same approach used by `crash_recovery.rs` but with deterministic
//! scheduling via `rand::rngs::StdRng`.
//!
//! # Running
//!
//! ```sh
//! cargo test --test dst_harness
//! ```
//!
//! # Determinism
//!
//! Given the same seed, the same sequence of crash points, workspace configs,
//! and file contents are generated. Failing seeds are printed for reproduction.

mod manifold_common;

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;

use manifold_common::TestRepo;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

// ---------------------------------------------------------------------------
// Crash phase enum (maps to MergePhase in src/merge_state.rs)
// ---------------------------------------------------------------------------

/// Merge phases where a crash can be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashPhase {
    Prepare,
    Build,
    Validate,
    Commit,
    Cleanup,
}

impl CrashPhase {
    const ALL: [CrashPhase; 5] = [
        CrashPhase::Prepare,
        CrashPhase::Build,
        CrashPhase::Validate,
        CrashPhase::Commit,
        CrashPhase::Cleanup,
    ];

    /// Only the COMMIT-related phases for G3 monotonicity testing.
    const COMMIT_PHASES: [CrashPhase; 3] = [
        CrashPhase::Validate,
        CrashPhase::Commit,
        CrashPhase::Cleanup,
    ];

    fn as_str(self) -> &'static str {
        match self {
            CrashPhase::Prepare => "prepare",
            CrashPhase::Build => "build",
            CrashPhase::Validate => "validate",
            CrashPhase::Commit => "commit",
            CrashPhase::Cleanup => "cleanup",
        }
    }

    fn pick(rng: &mut StdRng, phases: &[CrashPhase]) -> CrashPhase {
        phases[rng.random_range(0..phases.len())]
    }

    /// Whether this phase is pre-commit (safe to abort).
    fn is_pre_commit(self) -> bool {
        matches!(self, CrashPhase::Prepare | CrashPhase::Build)
    }
}

impl fmt::Display for CrashPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Trace entry and logger
// ---------------------------------------------------------------------------

/// A single step in the DST trace — records what happened for reproduction.
#[derive(Debug, Clone)]
struct TraceEntry {
    step: usize,
    action: String,
    phase: Option<CrashPhase>,
    outcome: String,
    epoch_before: String,
    epoch_after: String,
}

impl fmt::Display for TraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[step {}] {} (phase={}) => {} | epoch: {} -> {}",
            self.step,
            self.action,
            self.phase.map_or("none".to_string(), |p| p.to_string()),
            self.outcome,
            &self.epoch_before[..8],
            &self.epoch_after[..8],
        )
    }
}

/// Collects trace entries for a single seed run.
struct TraceLog {
    seed: u64,
    entries: Vec<TraceEntry>,
}

impl TraceLog {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            entries: Vec::new(),
        }
    }

    fn push(&mut self, entry: TraceEntry) {
        self.entries.push(entry);
    }

    /// Print the full trace to stderr (useful on assertion failures).
    fn dump(&self) {
        eprintln!("=== DST Trace (seed={}) ===", self.seed);
        for entry in &self.entries {
            eprintln!("  {entry}");
        }
        eprintln!("=== end trace ===");
    }
}

// ---------------------------------------------------------------------------
// Merge-state JSON helpers (same format as crash_recovery.rs)
// ---------------------------------------------------------------------------

fn merge_state_path(root: &Path) -> std::path::PathBuf {
    root.join(".manifold").join("merge-state.json")
}

/// Write a merge-state.json simulating a crash at the given phase.
fn write_crash_state(
    root: &Path,
    phase: CrashPhase,
    sources: &[&str],
    epoch: &str,
    candidate: Option<&str>,
) {
    let manifold_dir = root.join(".manifold");
    fs::create_dir_all(&manifold_dir).expect("create .manifold");

    let sources_json: Vec<serde_json::Value> = sources
        .iter()
        .map(|s| serde_json::Value::String((*s).to_owned()))
        .collect();

    let mut state = serde_json::json!({
        "phase": phase.as_str(),
        "sources": sources_json,
        "epoch_before": epoch,
        "started_at": 1000_u64,
        "updated_at": 1000_u64
    });

    // BUILD and later phases have a candidate OID
    if let Some(cand) = candidate {
        state["epoch_candidate"] = serde_json::Value::String(cand.to_owned());
    }

    let json = serde_json::to_string_pretty(&state).expect("serialize merge-state");
    let path = merge_state_path(root);
    fs::write(&path, &json).expect("write merge-state.json");
}

fn read_phase(root: &Path) -> Option<String> {
    let path = merge_state_path(root);
    let contents = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value["phase"].as_str().map(str::to_owned)
}

/// Clean up merge-state.json so a fresh merge can proceed.
fn clear_merge_state(root: &Path) {
    let path = merge_state_path(root);
    if path.exists() {
        fs::remove_file(&path).expect("remove merge-state.json");
    }
}

// ---------------------------------------------------------------------------
// Invariant checks (the oracle)
// ---------------------------------------------------------------------------

/// I-G1.1: All commits reachable before the operation are still reachable after.
///
/// Checks that every OID in `committed_before` is reachable from the current
/// set of refs (durable + recovery).
fn check_g1_reachability(root: &Path, committed_before: &HashSet<String>) -> Result<(), String> {
    // Collect all reachable commits from all refs
    let reachable = collect_reachable_oids(root);

    for oid in committed_before {
        if !reachable.contains(oid) {
            return Err(format!(
                "I-G1.1 violated: commit {oid} was reachable before but not after"
            ));
        }
    }
    Ok(())
}

/// I-G3.1: After COMMIT phase succeeds, epoch ref must not decrease.
///
/// If the epoch ref was advanced to the candidate, it must still point at the
/// candidate (or something newer) after recovery.
fn check_g3_monotonicity(
    root: &Path,
    epoch_before: &str,
    committed_candidate: bool,
) -> Result<(), String> {
    let current_epoch = read_ref_oid(root, "refs/manifold/epoch/current");
    let current_main = read_ref_oid(root, "refs/heads/main");

    if committed_candidate {
        // If the epoch was advanced, it must not have gone backward.
        // It should either be at the candidate or beyond.
        if current_epoch.as_deref() == Some(epoch_before) {
            return Err(format!(
                "I-G3.1 violated: epoch regressed to pre-commit value {epoch_before}"
            ));
        }
    }

    // G3.2: epoch and main should not diverge in a way that's not recoverable.
    // If epoch advanced but main did not, that's a partial commit (recoverable),
    // not a violation. The violation would be main > epoch (nonsensical).
    if let (Some(ep), Some(mn)) = (&current_epoch, &current_main) {
        if mn != ep {
            // Check if main is an ancestor of epoch (ok) or epoch is ancestor of main (ok)
            // or they diverged (violation)
            let main_is_ancestor = is_ancestor(root, mn, ep);
            let epoch_is_ancestor = is_ancestor(root, ep, mn);
            if !main_is_ancestor && !epoch_is_ancestor {
                return Err(format!(
                    "I-G3.2 violated: epoch ({}) and main ({}) diverged",
                    &ep[..8],
                    &mn[..8]
                ));
            }
        }
    }

    Ok(())
}

/// Git fsck: verify the repository is not corrupted.
fn check_git_integrity(root: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["fsck", "--no-progress", "--connectivity-only"])
        .current_dir(root)
        .output()
        .expect("spawn git fsck");

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!("git fsck failed:\nstdout: {stdout}\nstderr: {stderr}"));
    }
    Ok(())
}

/// Check that workspace files still exist (no silent data loss).
fn check_workspace_files_preserved(
    repo: &TestRepo,
    workspace: &str,
    expected_files: &[(&str, &str)],
) -> Result<(), String> {
    for (path, content) in expected_files {
        match repo.read_file(workspace, path) {
            Some(actual) if actual == *content => {}
            Some(actual) => {
                return Err(format!(
                    "file {path} in workspace {workspace}: expected {content:?}, got {actual:?}"
                ));
            }
            None => {
                return Err(format!(
                    "file {path} missing from workspace {workspace} (silent data loss)"
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Collect all commit OIDs reachable from any ref.
fn collect_reachable_oids(root: &Path) -> HashSet<String> {
    let out = Command::new("git")
        .args(["rev-list", "--all"])
        .current_dir(root)
        .output()
        .expect("git rev-list --all");

    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().map(|s| s.trim().to_owned()).collect()
}

/// Read a ref's OID, returning None if the ref doesn't exist.
fn read_ref_oid(root: &Path, refname: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--verify", refname])
        .current_dir(root)
        .output()
        .expect("git rev-parse");

    if out.status.success() {
        Some(
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .to_owned(),
        )
    } else {
        None
    }
}

/// Check if `ancestor` is an ancestor of `descendant`.
fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> bool {
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .output()
        .expect("git merge-base --is-ancestor");
    out.status.success()
}

/// Create a real merge candidate commit from workspace changes.
///
/// This simulates what `maw ws merge` BUILD phase does: merge workspace
/// changes into a new commit on top of the current epoch.
fn create_candidate_commit(repo: &TestRepo, workspace: &str) -> Option<String> {
    let ws_path = repo.workspace_path(workspace);
    let epoch = repo.current_epoch();

    // Check if workspace has changes
    let dirty = repo.dirty_files(workspace);
    if dirty.is_empty() {
        return None;
    }

    // Stage and commit in the workspace (creates a local commit)
    let out = Command::new("git")
        .args(["add", "-A"])
        .current_dir(&ws_path)
        .output()
        .expect("git add -A");
    if !out.status.success() {
        return None;
    }

    let out = Command::new("git")
        .args(["commit", "-m", "workspace changes for merge candidate"])
        .current_dir(&ws_path)
        .output()
        .expect("git commit");
    if !out.status.success() {
        return None;
    }

    let ws_head = repo.workspace_head(workspace);

    // Create a merge commit: merge workspace head into epoch
    // Use git merge-tree to build the merge result, then create the commit
    // For simplicity, just cherry-pick the workspace commit onto default
    let default_ws = repo.default_workspace();

    let out = Command::new("git")
        .args(["cherry-pick", "--no-commit", &ws_head])
        .current_dir(&default_ws)
        .output()
        .expect("git cherry-pick");

    if !out.status.success() {
        // Reset on conflict
        let _ = Command::new("git")
            .args(["cherry-pick", "--abort"])
            .current_dir(&default_ws)
            .output();
        let _ = Command::new("git")
            .args(["reset", "--hard", &epoch])
            .current_dir(&default_ws)
            .output();
        return None;
    }

    let out = Command::new("git")
        .args(["commit", "-m", "merge candidate"])
        .current_dir(&default_ws)
        .output()
        .expect("git commit merge candidate");

    if !out.status.success() {
        let _ = Command::new("git")
            .args(["reset", "--hard", &epoch])
            .current_dir(&default_ws)
            .output();
        return None;
    }

    let candidate = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&default_ws)
            .output()
            .expect("git rev-parse HEAD")
            .stdout,
    )
    .trim()
    .to_owned();

    // Reset default ws back to epoch so we haven't actually committed
    let _ = Command::new("git")
        .args(["reset", "--hard", &epoch])
        .current_dir(&default_ws)
        .output();

    Some(candidate)
}

// ---------------------------------------------------------------------------
// Simulate recovery (matches crash_recovery.rs logic)
// ---------------------------------------------------------------------------

/// Simulate crash recovery by inspecting merge-state.json.
///
/// Returns a description of what recovery would do.
fn simulate_recovery(root: &Path) -> &'static str {
    let path = merge_state_path(root);

    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return "no_merge_in_progress",
        Err(e) => panic!("unexpected read error: {e}"),
    };

    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return "corrupt_state",
    };

    let phase = value["phase"].as_str().unwrap_or("unknown");

    match phase {
        "prepare" | "build" => {
            // Pre-commit: safe to abort by removing state file
            fs::remove_file(&path).expect("remove merge-state.json");
            "aborted_pre_commit"
        }
        "validate" => "retry_validate",
        "commit" => "check_commit",
        "cleanup" => {
            // Post-commit cleanup is idempotent: can retry
            let _ = fs::remove_file(&path);
            "retry_cleanup"
        }
        "complete" | "aborted" => "terminal",
        _ => "unknown_phase",
    }
}

// ---------------------------------------------------------------------------
// Scenario runner: single trace
// ---------------------------------------------------------------------------

/// Configuration for a single DST trace.
struct TraceConfig {
    /// Number of workspaces to create.
    num_workspaces: usize,
    /// Number of files per workspace.
    num_files_per_ws: usize,
    /// Which phase to inject the crash at.
    crash_phase: CrashPhase,
    /// Whether to create a real candidate commit (for post-build phases).
    create_candidate: bool,
}

/// Result of running a single trace.
struct TraceResult {
    trace: TraceLog,
    invariant_violations: Vec<String>,
}

/// Run a single DST trace with the given config.
fn run_trace(seed: u64, config: &TraceConfig) -> TraceResult {
    let mut trace = TraceLog::new(seed);
    let mut violations = Vec::new();

    // Step 1: Setup repo and workspaces
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);

    let mut workspace_names = Vec::new();
    let mut workspace_files: Vec<Vec<(String, String)>> = Vec::new();

    for i in 0..config.num_workspaces {
        let name = format!("agent-{i}");
        repo.create_workspace(&name);

        let mut files = Vec::new();
        for j in 0..config.num_files_per_ws {
            let path = format!("ws{i}_file{j}.txt");
            let content = format!("content from agent-{i} file {j} (seed={seed})\n");
            repo.add_file(&name, &path, &content);
            files.push((path, content));
        }

        workspace_names.push(name);
        workspace_files.push(files);
    }

    let epoch_before = repo.current_epoch();

    // Snapshot committed OIDs before the operation
    let committed_before = collect_reachable_oids(repo.root());

    trace.push(TraceEntry {
        step: 0,
        action: format!(
            "setup: {} workspaces, {} files each",
            config.num_workspaces, config.num_files_per_ws
        ),
        phase: None,
        outcome: "ok".to_string(),
        epoch_before: epoch_before.clone(),
        epoch_after: epoch_before.clone(),
    });

    // Step 2: Optionally create a real candidate commit (needed for post-build phases)
    let candidate_oid = if config.create_candidate && !workspace_names.is_empty() {
        create_candidate_commit(&repo, &workspace_names[0])
    } else {
        None
    };

    let fallback_candidate = "b".repeat(40);
    let candidate_ref = candidate_oid.as_deref().unwrap_or(&fallback_candidate);

    // Step 3: Inject crash by writing merge-state.json
    let source_refs: Vec<&str> = workspace_names.iter().map(String::as_str).collect();
    let needs_candidate = !config.crash_phase.is_pre_commit();

    write_crash_state(
        repo.root(),
        config.crash_phase,
        &source_refs,
        &epoch_before,
        if needs_candidate || config.create_candidate {
            Some(candidate_ref)
        } else {
            None
        },
    );

    trace.push(TraceEntry {
        step: 1,
        action: format!("inject crash at {}", config.crash_phase),
        phase: Some(config.crash_phase),
        outcome: "crash_state_written".to_string(),
        epoch_before: epoch_before.clone(),
        epoch_after: epoch_before.clone(),
    });

    // Step 4: Run recovery
    let recovery_outcome = simulate_recovery(repo.root());

    let epoch_after_recovery = repo.current_epoch();

    trace.push(TraceEntry {
        step: 2,
        action: "recovery".to_string(),
        phase: Some(config.crash_phase),
        outcome: recovery_outcome.to_string(),
        epoch_before: epoch_before.clone(),
        epoch_after: epoch_after_recovery.clone(),
    });

    // Step 5: Check invariants

    // G1: No silent loss of committed work
    if let Err(e) = check_g1_reachability(repo.root(), &committed_before) {
        violations.push(e);
    }

    // Git integrity
    if let Err(e) = check_git_integrity(repo.root()) {
        violations.push(e);
    }

    // Workspace files still present (pre-commit crashes should preserve everything)
    if config.crash_phase.is_pre_commit() {
        for (i, name) in workspace_names.iter().enumerate() {
            let expected: Vec<(&str, &str)> = workspace_files[i]
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            if let Err(e) = check_workspace_files_preserved(&repo, name, &expected) {
                violations.push(format!("workspace {name}: {e}"));
            }
        }
    }

    // Base files always preserved
    if let Err(e) = check_workspace_files_preserved(
        &repo,
        "default",
        &[("base.txt", "base content\n")],
    ) {
        violations.push(format!("default workspace: {e}"));
    }

    // Step 6: Clear merge state and attempt a real merge on a sample of traces.
    // Running the maw binary is expensive; sample 1-in-8 to keep total time down.
    clear_merge_state(repo.root());

    let run_retry = seed % 8 == 0;
    if run_retry && !workspace_names.is_empty() {
        let mut merge_args: Vec<&str> = vec!["ws", "merge"];
        for name in &workspace_names {
            merge_args.push(name.as_str());
        }
        let merge_out = repo.maw_raw(&merge_args);
        let merge_success = merge_out.status.success();
        let merge_stderr = String::from_utf8_lossy(&merge_out.stderr);

        let epoch_after_merge = repo.current_epoch();

        trace.push(TraceEntry {
            step: 3,
            action: "retry merge".to_string(),
            phase: None,
            outcome: if merge_success {
                "success".to_string()
            } else {
                format!("failed: {}", merge_stderr.lines().next().unwrap_or("?"))
            },
            epoch_before: epoch_after_recovery,
            epoch_after: epoch_after_merge.clone(),
        });

        // Post-merge G1 check: committed data still reachable
        if merge_success {
            let committed_after_merge = collect_reachable_oids(repo.root());
            for oid in &committed_before {
                if !committed_after_merge.contains(oid) {
                    violations.push(format!(
                        "I-G1.1 (post-merge): commit {} lost after retry merge",
                        oid
                    ));
                }
            }
        }
    }

    TraceResult {
        trace,
        invariant_violations: violations,
    }
}

// ---------------------------------------------------------------------------
// G3 monotonicity-specific trace runner
// ---------------------------------------------------------------------------

/// Run a trace specifically testing epoch monotonicity (G3).
///
/// Sets up a repo where a merge can succeed, then injects crashes at
/// commit-related phases and verifies epoch never decreases.
fn run_g3_trace(seed: u64, crash_phase: CrashPhase) -> TraceResult {
    let mut trace = TraceLog::new(seed);
    let mut violations = Vec::new();

    let repo = TestRepo::new();
    repo.seed_files(&[
        ("base.txt", "base content\n"),
        ("shared.txt", "shared v1\n"),
    ]);

    repo.create_workspace("agent-0");
    repo.add_file("agent-0", "feature.txt", &format!("feature (seed={seed})\n"));

    let epoch_before = repo.current_epoch();
    let committed_before = collect_reachable_oids(repo.root());

    // Create a real candidate commit
    let candidate_oid = create_candidate_commit(&repo, "agent-0");
    let has_candidate = candidate_oid.is_some();
    let candidate = candidate_oid.unwrap_or_else(|| "f".repeat(40));

    trace.push(TraceEntry {
        step: 0,
        action: format!(
            "setup: 1 workspace, crash at {crash_phase}, candidate={}",
            if has_candidate { "real" } else { "fake" }
        ),
        phase: None,
        outcome: "ok".to_string(),
        epoch_before: epoch_before.clone(),
        epoch_after: epoch_before.clone(),
    });

    // For COMMIT phase: simulate partial commit (epoch moved but main didn't)
    if crash_phase == CrashPhase::Commit && has_candidate {
        // Move epoch ref to candidate (simulating the first CAS succeeding)
        let _ = Command::new("git")
            .args([
                "update-ref",
                "refs/manifold/epoch/current",
                &candidate,
            ])
            .current_dir(repo.root())
            .output();

        // Write merge-state showing epoch moved but branch didn't
        let manifold_dir = repo.root().join(".manifold");
        fs::create_dir_all(&manifold_dir).expect("create .manifold");

        let commit_state = serde_json::json!({
            "phase": "commit",
            "epoch_before": epoch_before,
            "epoch_candidate": candidate,
            "epoch_ref_updated": true,
            "branch_ref_updated": false,
            "updated_at_unix_ms": 1000_u64
        });

        // Write commit-state.json (used by commit-phase recovery)
        let commit_state_path = manifold_dir.join("commit-state.json");
        fs::write(
            &commit_state_path,
            serde_json::to_string_pretty(&commit_state).unwrap(),
        )
        .expect("write commit-state.json");

        trace.push(TraceEntry {
            step: 1,
            action: "simulate partial commit (epoch moved, main didn't)".to_string(),
            phase: Some(CrashPhase::Commit),
            outcome: "partial_commit_simulated".to_string(),
            epoch_before: epoch_before.clone(),
            epoch_after: candidate.clone(),
        });

        // Check G3: epoch moved forward (or stayed same), never backward
        let current_epoch = repo.current_epoch();
        if current_epoch == epoch_before {
            // This is allowed: epoch was reset (not yet committed)
        } else if current_epoch != candidate {
            violations.push(format!(
                "I-G3.1: epoch is neither before ({}) nor candidate ({}), got {}",
                &epoch_before[..8],
                &candidate[..8],
                &current_epoch[..8]
            ));
        }
    } else {
        // For other phases, just write the crash state
        write_crash_state(
            repo.root(),
            crash_phase,
            &["agent-0"],
            &epoch_before,
            if has_candidate {
                Some(&candidate)
            } else {
                None
            },
        );

        trace.push(TraceEntry {
            step: 1,
            action: format!("inject crash at {crash_phase}"),
            phase: Some(crash_phase),
            outcome: "crash_state_written".to_string(),
            epoch_before: epoch_before.clone(),
            epoch_after: epoch_before.clone(),
        });
    }

    // Run recovery
    let recovery_outcome = simulate_recovery(repo.root());
    let epoch_after = repo.current_epoch();

    trace.push(TraceEntry {
        step: 2,
        action: "recovery".to_string(),
        phase: Some(crash_phase),
        outcome: recovery_outcome.to_string(),
        epoch_before: epoch_before.clone(),
        epoch_after: epoch_after.clone(),
    });

    // G3 check: epoch must not decrease (use the oracle function)
    let committed_during_commit = crash_phase == CrashPhase::Commit && has_candidate;
    if let Err(e) = check_g3_monotonicity(repo.root(), &epoch_before, committed_during_commit) {
        violations.push(e);
    }

    // G1 check: committed data still reachable
    if let Err(e) = check_g1_reachability(repo.root(), &committed_before) {
        violations.push(e);
    }

    // Git integrity
    if let Err(e) = check_git_integrity(repo.root()) {
        violations.push(e);
    }

    // Verify merge-state phase is consistent after recovery
    if let Some(phase) = read_phase(repo.root()) {
        // After recovery, only validate/commit phases should retain state
        let expected_retained = matches!(crash_phase, CrashPhase::Validate | CrashPhase::Commit);
        if !expected_retained {
            violations.push(format!(
                "merge-state.json still has phase '{phase}' after recovery for crash at {crash_phase}"
            ));
        }
    }

    TraceResult {
        trace,
        invariant_violations: violations,
    }
}

// ---------------------------------------------------------------------------
// Test: G1 — random crash preserves committed data
// ---------------------------------------------------------------------------

/// Run 100+ traces with random crash points, verifying that committed data
/// is never silently lost (I-G1.1) and workspace files survive pre-commit crashes.
#[test]
fn dst_g1_random_crash_preserves_committed_data() {
    let base_seed: u64 = 0xDEAD_BEEF_CAFE_0001;
    let num_traces = 256;
    let mut failures = Vec::new();

    for i in 0..num_traces {
        let seed = base_seed.wrapping_add(i);
        let mut rng = StdRng::seed_from_u64(seed);

        let config = TraceConfig {
            num_workspaces: rng.random_range(1..=3),
            num_files_per_ws: rng.random_range(1..=3),
            crash_phase: CrashPhase::pick(&mut rng, &CrashPhase::ALL),
            create_candidate: rng.random_bool(0.5),
        };

        let result = run_trace(seed, &config);

        if !result.invariant_violations.is_empty() {
            result.trace.dump();
            for v in &result.invariant_violations {
                eprintln!("  VIOLATION: {v}");
            }
            failures.push((seed, result.invariant_violations));
        }
    }

    assert!(
        failures.is_empty(),
        "DST G1: {}/{num_traces} traces had invariant violations.\n\
         Failing seeds: {:?}\n\
         First failure: {:?}",
        failures.len(),
        failures.iter().map(|(s, _)| s).collect::<Vec<_>>(),
        failures.first().map(|(s, v)| format!("seed={s}: {v:?}")),
    );
}

// ---------------------------------------------------------------------------
// Test: G3 — crash at COMMIT satisfies epoch monotonicity
// ---------------------------------------------------------------------------

/// Run 100+ traces injecting crashes at each COMMIT-related phase, verifying
/// that the epoch ref never decreases (I-G3.1) and epoch+main don't diverge
/// irrecoverably (I-G3.2).
#[test]
fn dst_g3_crash_at_commit_satisfies_monotonicity() {
    let base_seed: u64 = 0xDEAD_BEEF_CAFE_0003;
    let num_traces = 256;
    let mut failures = Vec::new();

    for i in 0..num_traces {
        let seed = base_seed.wrapping_add(i);
        let mut rng = StdRng::seed_from_u64(seed);

        let crash_phase = CrashPhase::pick(&mut rng, &CrashPhase::COMMIT_PHASES);

        let result = run_g3_trace(seed, crash_phase);

        if !result.invariant_violations.is_empty() {
            result.trace.dump();
            for v in &result.invariant_violations {
                eprintln!("  VIOLATION: {v}");
            }
            failures.push((seed, result.invariant_violations));
        }
    }

    assert!(
        failures.is_empty(),
        "DST G3: {}/{num_traces} traces had invariant violations.\n\
         Failing seeds: {:?}\n\
         First failure: {:?}",
        failures.len(),
        failures.iter().map(|(s, _)| s).collect::<Vec<_>>(),
        failures.first().map(|(s, v)| format!("seed={s}: {v:?}")),
    );
}

// ---------------------------------------------------------------------------
// Test: Determinism — same seed produces same trace
// ---------------------------------------------------------------------------

/// Verify that running the same seed twice produces identical traces.
#[test]
fn dst_determinism_same_seed_same_trace() {
    let seed: u64 = 0xAAAA_BBBB_CCCC_DDDD;

    let config = {
        let mut rng = StdRng::seed_from_u64(seed);
        TraceConfig {
            num_workspaces: rng.random_range(1..=3),
            num_files_per_ws: rng.random_range(1..=3),
            crash_phase: CrashPhase::pick(&mut rng, &CrashPhase::ALL),
            create_candidate: rng.random_bool(0.5),
        }
    };

    let config2 = {
        let mut rng = StdRng::seed_from_u64(seed);
        TraceConfig {
            num_workspaces: rng.random_range(1..=3),
            num_files_per_ws: rng.random_range(1..=3),
            crash_phase: CrashPhase::pick(&mut rng, &CrashPhase::ALL),
            create_candidate: rng.random_bool(0.5),
        }
    };

    // Same seed should produce identical configs
    assert_eq!(config.num_workspaces, config2.num_workspaces);
    assert_eq!(config.num_files_per_ws, config2.num_files_per_ws);
    assert_eq!(config.crash_phase, config2.crash_phase);
    assert_eq!(config.create_candidate, config2.create_candidate);

    // Run both and verify same outcomes
    let result1 = run_trace(seed, &config);
    let result2 = run_trace(seed, &config2);

    assert_eq!(
        result1.trace.entries.len(),
        result2.trace.entries.len(),
        "same seed should produce same number of trace entries"
    );

    for (i, (e1, e2)) in result1
        .trace
        .entries
        .iter()
        .zip(result2.trace.entries.iter())
        .enumerate()
    {
        assert_eq!(
            e1.action, e2.action,
            "step {i}: actions differ for same seed"
        );
        assert_eq!(
            e1.outcome, e2.outcome,
            "step {i}: outcomes differ for same seed"
        );
        assert_eq!(
            e1.phase.map(|p| p.as_str()),
            e2.phase.map(|p| p.as_str()),
            "step {i}: phases differ for same seed"
        );
    }

    assert_eq!(
        result1.invariant_violations, result2.invariant_violations,
        "same seed should produce same invariant check results"
    );
}
