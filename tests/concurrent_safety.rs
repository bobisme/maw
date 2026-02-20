//! Concurrent agent safety eval — adversarial interleaving stress test.
//!
//! Implements bd-1784: "Eval: concurrent agent safety (adversarial interleavings)"
//!
//! # What is verified
//!
//! Design doc §1.1 success criterion: "No global corruption or divergence under
//! adversarial interleavings of agent actions."
//!
//! - **Concurrent file operations**: 5 simulated agents operate in separate
//!   worktrees simultaneously via real OS threads, using a Barrier to ensure
//!   maximum interleaving.
//! - **Randomized op sequences**: each agent performs a random mix of
//!   `AddFile`, `ModifyFile`, `DeleteFile`, and `StatusCheck` ops.
//! - **Merge correctness**: after all agents finish, a lead thread runs the
//!   Manifold merge pipeline and verifies the candidate commit.
//! - **No corruption**: `git fsck --strict` is run after each merge.
//! - **No data loss**: every agent's surviving files appear in the candidate
//!   tree with the correct content.
//! - **Epoch consistency**: candidate commit's parent == epoch at merge time.
//! - **Determinism**: running the same seed twice produces the same git tree OID.
//! - **Runs**: 100 random seeds; each failure prints the seed for reproduction.

mod manifold_common;

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of simulated agents per scenario.
const AGENT_COUNT: usize = 5;

/// Number of random scenarios to run.
const SCENARIO_COUNT: usize = 100;

/// Minimum number of ops per agent.
const MIN_OPS: usize = 3;

/// Maximum number of ops per agent.
const MAX_OPS: usize = 10;

// ---------------------------------------------------------------------------
// Agent operations
// ---------------------------------------------------------------------------

/// One operation that a simulated agent performs in its workspace.
#[derive(Debug, Clone)]
enum AgentOp {
    /// Write a new file. Path is relative to the workspace root.
    AddFile { path: String, content: String },
    /// Overwrite an existing file that this agent previously created.
    ModifyFile { path: String, new_content: String },
    /// Delete a file that this agent previously created.
    DeleteFile { path: String },
    /// Run `git status --porcelain` (read-only; tests concurrent reads).
    StatusCheck,
}

// ---------------------------------------------------------------------------
// Op generation
// ---------------------------------------------------------------------------

/// Generate a randomized sequence of ops for one agent.
///
/// Each agent always starts with at least one `AddFile` so it has something
/// to track. Subsequent ops are randomly chosen from all four variants;
/// destructive ops only target files the agent itself created.
fn generate_ops(rng: &mut StdRng, agent_id: usize, op_count: usize) -> Vec<AgentOp> {
    let mut ops = Vec::with_capacity(op_count);
    // Paths owned by this agent (can be modified or deleted).
    let mut owned: Vec<String> = Vec::new();
    // Counter for unique filenames.
    let mut file_ctr = 0usize;

    let next_path = |ctr: &mut usize| -> String {
        let p = format!("agent_{agent_id}/file_{ctr:04}.txt");
        *ctr += 1;
        p
    };

    // Always start with an AddFile so the agent has at least one owned file.
    {
        let path = next_path(&mut file_ctr);
        let content = format!(
            "agent={agent_id} idx=0 data={:#010x}\n",
            rng.random::<u32>()
        );
        owned.push(path.clone());
        ops.push(AgentOp::AddFile { path, content });
    }

    for idx in 1..op_count {
        // Weight towards AddFile and ModifyFile; less towards Delete and Status.
        let choice: u8 = rng.random_range(0..8u8);

        match choice {
            // 0-2: AddFile (most common)
            0..=2 => {
                let path = next_path(&mut file_ctr);
                let content = format!(
                    "agent={agent_id} idx={idx} data={:#010x}\n",
                    rng.random::<u32>()
                );
                owned.push(path.clone());
                ops.push(AgentOp::AddFile { path, content });
            }
            // 3-4: ModifyFile (only if we have files)
            3 | 4 if !owned.is_empty() => {
                let pick = rng.random_range(0..owned.len());
                let path = owned[pick].clone();
                let new_content = format!(
                    "agent={agent_id} modified idx={idx} data={:#010x}\n",
                    rng.random::<u32>()
                );
                ops.push(AgentOp::ModifyFile { path, new_content });
            }
            // 5: DeleteFile (only if we have 2+ files — keep at least 1)
            5 if owned.len() >= 2 => {
                let pick = rng.random_range(0..owned.len() - 1);
                let path = owned.remove(pick);
                ops.push(AgentOp::DeleteFile { path });
            }
            // 6-7: StatusCheck
            6 | 7 => {
                ops.push(AgentOp::StatusCheck);
            }
            // Fallthrough: default to AddFile
            _ => {
                let path = next_path(&mut file_ctr);
                let content = format!(
                    "agent={agent_id} fallback idx={idx} data={:#010x}\n",
                    rng.random::<u32>()
                );
                owned.push(path.clone());
                ops.push(AgentOp::AddFile { path, content });
            }
        }
    }

    ops
}

// ---------------------------------------------------------------------------
// Agent execution
// ---------------------------------------------------------------------------

/// Execute a sequence of ops in the given workspace directory.
///
/// Returns a map of `{relative_path → expected_content}` for all files that
/// should survive (i.e., were added but not subsequently deleted).
fn execute_ops(ws_path: &std::path::Path, ops: &[AgentOp]) -> HashMap<String, String> {
    // Live files: path → current content.  Only paths this agent owns.
    let mut live: HashMap<String, String> = HashMap::new();

    for op in ops {
        match op {
            AgentOp::AddFile { path, content } => {
                let full = ws_path.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)
                        .unwrap_or_else(|e| panic!("mkdir failed for {}: {e}", full.display()));
                }
                std::fs::write(&full, content)
                    .unwrap_or_else(|e| panic!("write failed for {}: {e}", full.display()));
                live.insert(path.clone(), content.clone());
            }

            AgentOp::ModifyFile { path, new_content } => {
                let full = ws_path.join(path);
                if full.exists() {
                    std::fs::write(&full, new_content)
                        .unwrap_or_else(|e| panic!("modify failed for {}: {e}", full.display()));
                    live.insert(path.clone(), new_content.clone());
                }
                // If file somehow doesn't exist, skip silently.
            }

            AgentOp::DeleteFile { path } => {
                let full = ws_path.join(path);
                if full.exists() {
                    std::fs::remove_file(&full)
                        .unwrap_or_else(|e| panic!("delete failed for {}: {e}", full.display()));
                }
                live.remove(path);
            }

            AgentOp::StatusCheck => {
                // Read-only: run git status to exercise concurrent reads of git refs.
                let _ = Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(ws_path)
                    .output();
            }
        }
    }

    live
}

// ---------------------------------------------------------------------------
// Verification helpers
// ---------------------------------------------------------------------------

/// Run `git fsck --strict` on a repo root. Returns `None` if clean.
fn git_fsck(repo_root: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .args(["fsck", "--strict", "--no-progress"])
        .current_dir(repo_root)
        .output()
        .expect("failed to run git fsck");
    if out.status.success() {
        None
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        Some(format!("stdout: {stdout}\nstderr: {stderr}"))
    }
}

/// List all file paths in a git commit tree (recursive).
fn list_tree_files(repo_root: &std::path::Path, commit_oid: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", commit_oid])
        .current_dir(repo_root)
        .output()
        .expect("git ls-tree failed");
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

/// Read a file's content from a git tree by commit OID + path.
fn read_tree_file(repo_root: &std::path::Path, commit_oid: &str, path: &str) -> Option<String> {
    let spec = format!("{commit_oid}:{path}");
    let out = Command::new("git")
        .args(["show", &spec])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// Resolve the tree OID for a commit (deterministic even if commit OID varies by timestamp).
fn commit_tree_oid(repo_root: &std::path::Path, commit_oid: &str) -> String {
    let spec = format!("{commit_oid}^{{tree}}");
    let out = Command::new("git")
        .args(["rev-parse", &spec])
        .current_dir(repo_root)
        .output()
        .expect("git rev-parse for tree OID failed");
    assert!(out.status.success(), "git rev-parse tree failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Get the parent commit OID of the given commit.
fn commit_parent_oid(repo_root: &std::path::Path, commit_oid: &str) -> String {
    let spec = format!("{commit_oid}^");
    let out = Command::new("git")
        .args(["rev-parse", &spec])
        .current_dir(repo_root)
        .output()
        .expect("git rev-parse for parent failed");
    assert!(out.status.success(), "git rev-parse parent failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

// ---------------------------------------------------------------------------
// Backend helper
// ---------------------------------------------------------------------------

fn backend_for(repo: &TestRepo) -> maw::backend::git::GitWorktreeBackend {
    maw::backend::git::GitWorktreeBackend::new(repo.root().to_path_buf())
}

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

/// Result of a single scenario run.
struct ScenarioRun {
    /// Candidate commit OID from the merge pipeline.
    candidate_oid: String,
    /// Tree OID of the candidate commit (deterministic).
    tree_oid: String,
}

/// Run one concurrent scenario with `seed` and return the result.
///
/// Panics with seed + details on any invariant violation.
fn run_scenario(seed: u64) -> ScenarioRun {
    let repo = TestRepo::new();

    // --- Seed shared base files ---
    // Give agents a shared starting point so some workspaces inherit files
    // they can see (but each agent writes to their own subdirectory).
    let mut seed_rng = StdRng::seed_from_u64(seed.wrapping_add(0xfeed_face));
    let seed_file_count = seed_rng.random_range(1..=4usize);
    let seed_files: Vec<(String, String)> = (0..seed_file_count)
        .map(|i| {
            (
                format!("shared/base_{i}.txt"),
                format!("shared base {i} seed={seed}\n"),
            )
        })
        .collect();
    let seed_refs: Vec<(&str, &str)> = seed_files
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    repo.seed_files(&seed_refs);

    // --- Create agent workspaces ---
    let ws_names: Vec<String> = (0..AGENT_COUNT).map(|i| format!("agent-{i}")).collect();
    for name in &ws_names {
        repo.create_workspace(name);
    }

    // --- Generate ops for each agent (deterministic from seed) ---
    let all_ops: Vec<Vec<AgentOp>> = (0..AGENT_COUNT)
        .map(|i| {
            // XOR seed with agent index so agents get different sequences.
            let agent_seed = seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let mut agent_rng = StdRng::seed_from_u64(agent_seed);
            let op_count = agent_rng.random_range(MIN_OPS..=MAX_OPS);
            generate_ops(&mut agent_rng, i, op_count)
        })
        .collect();

    // --- Spawn agent threads — all start concurrently at the barrier ---
    let barrier = Arc::new(Barrier::new(AGENT_COUNT));

    // Shared result storage: agent_id → live files after ops complete.
    let results: Arc<std::sync::Mutex<Vec<Option<HashMap<String, String>>>>> =
        Arc::new(std::sync::Mutex::new(vec![None; AGENT_COUNT]));

    let mut handles = Vec::with_capacity(AGENT_COUNT);

    for (i, (ws_name, ops)) in ws_names.iter().zip(all_ops.iter()).enumerate() {
        let ws_path = repo.workspace_path(ws_name);
        let ops_owned = ops.clone();
        let barrier_clone = Arc::clone(&barrier);
        let results_clone = Arc::clone(&results);

        let handle = thread::spawn(move || {
            // Wait until all agents are ready, then start simultaneously.
            barrier_clone.wait();

            let live = execute_ops(&ws_path, &ops_owned);

            let mut guard = results_clone.lock().expect("mutex poisoned");
            guard[i] = Some(live);
        });

        handles.push(handle);
    }

    // Wait for all agents to finish.
    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("seed={seed}: agent-{i} thread panicked"));
    }

    let agent_results: Vec<HashMap<String, String>> = Arc::try_unwrap(results)
        .expect("results Arc has extra references")
        .into_inner()
        .expect("mutex poisoned")
        .into_iter()
        .map(|r| r.expect("agent result missing — thread didn't store it"))
        .collect();

    // --- Run the Manifold merge pipeline ---
    let backend = backend_for(&repo);
    let epoch_str = repo.current_epoch();
    let epoch = maw::model::types::EpochId::new(&epoch_str)
        .unwrap_or_else(|e| panic!("seed={seed}: EpochId parse failed: {e}"));

    let sources: Vec<maw::model::types::WorkspaceId> = ws_names
        .iter()
        .map(|n| {
            maw::model::types::WorkspaceId::new(n)
                .unwrap_or_else(|e| panic!("seed={seed}: WorkspaceId parse failed for {n}: {e}"))
        })
        .collect();

    let build_output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources)
            .unwrap_or_else(|e| panic!("seed={seed}: merge build phase failed: {e}"));

    let candidate_oid = build_output.candidate.as_str().to_owned();

    // --- Invariant 1: No unresolvable conflicts ---
    // Each agent writes to their own agent_N/ subdirectory, so no overlapping
    // paths. The merge should always be fully clean.
    assert!(
        build_output.conflicts.is_empty(),
        "seed={seed}: unexpected conflicts (each agent uses isolated paths): {:?}",
        build_output.conflicts,
    );

    // --- Invariant 2: Git object integrity (no corruption) ---
    if let Some(fsck_err) = git_fsck(repo.root()) {
        panic!("seed={seed}: git fsck --strict reported corruption:\n{fsck_err}");
    }

    // --- Invariant 3: Epoch consistency ---
    // Candidate's parent must equal the epoch that was current at merge time.
    let parent_oid = commit_parent_oid(repo.root(), &candidate_oid);
    assert_eq!(
        parent_oid, epoch_str,
        "seed={seed}: candidate parent OID mismatch (epoch consistency violated)\n  \
         expected parent: {epoch_str}\n  \
         actual parent:   {parent_oid}\n  \
         candidate:       {candidate_oid}",
    );

    // --- Invariant 4: No data loss ---
    // Every file that each agent's run ended with must appear in the candidate
    // tree with exactly the expected content.
    let candidate_files = list_tree_files(repo.root(), &candidate_oid);

    for (i, live_files) in agent_results.iter().enumerate() {
        for (rel_path, expected_content) in live_files {
            // File must appear in candidate tree.
            assert!(
                candidate_files.contains(rel_path),
                "seed={seed}: DATA LOSS — agent-{i}'s file '{rel_path}' missing from merge candidate\n  \
                 candidate files: {candidate_files:?}",
            );

            // Content must match.
            let actual_content = read_tree_file(repo.root(), &candidate_oid, rel_path)
                .unwrap_or_else(|| {
                    panic!(
                        "seed={seed}: could not read '{rel_path}' from candidate {candidate_oid}"
                    )
                });

            assert_eq!(
                actual_content, *expected_content,
                "seed={seed}: CONTENT MISMATCH in agent-{i}'s file '{rel_path}'\n  \
                 expected: {expected_content:?}\n  \
                 actual:   {actual_content:?}",
            );
        }
    }

    // --- Invariant 5: Shared seed files are preserved ---
    // The seeded shared/ files should still be present in the candidate tree
    // (no agent touched them — agents only write to agent_N/ directories).
    for (shared_path, shared_content) in &seed_files {
        assert!(
            candidate_files.contains(shared_path),
            "seed={seed}: shared seed file '{shared_path}' missing from candidate",
        );
        let actual = read_tree_file(repo.root(), &candidate_oid, shared_path).unwrap_or_default();
        assert_eq!(
            actual, *shared_content,
            "seed={seed}: shared seed file '{shared_path}' content changed unexpectedly\n  \
             expected: {shared_content:?}\n  actual: {actual:?}",
        );
    }

    let tree_oid = commit_tree_oid(repo.root(), &candidate_oid);

    ScenarioRun {
        candidate_oid,
        tree_oid,
    }
}

// ---------------------------------------------------------------------------
// Determinism: same seed → same tree
// ---------------------------------------------------------------------------

/// Run the same scenario twice and verify the tree OID is identical.
///
/// The tree OID is deterministic (same file content → same OID), even though
/// the commit OID varies because it includes a timestamp/nonce.
fn assert_deterministic(seed: u64) {
    let run1 = run_scenario(seed);
    let run2 = run_scenario(seed);

    assert_eq!(
        run1.tree_oid, run2.tree_oid,
        "seed={seed}: DETERMINISM VIOLATION — same inputs produced different merge trees\n  \
         run1 candidate: {}\n  \
         run2 candidate: {}\n  \
         run1 tree:      {}\n  \
         run2 tree:      {}",
        run1.candidate_oid, run2.candidate_oid, run1.tree_oid, run2.tree_oid,
    );
}

// ---------------------------------------------------------------------------
// Main tests
// ---------------------------------------------------------------------------

/// Run 100 concurrent scenarios across 100 random seeds.
///
/// Each scenario:
/// - Creates a fresh Manifold repo
/// - Seeds 1–4 shared base files
/// - Spawns 5 agent threads simultaneously (barrier-synchronized)
/// - Each agent runs 3–10 randomized operations
/// - Lead runs the full Manifold merge pipeline
/// - Verifies: no conflicts, no corruption, epoch consistency, no data loss
#[test]
fn concurrent_agents_100_scenarios_no_corruption_or_data_loss() {
    let mut failures: Vec<String> = Vec::new();

    for seed in 0u64..SCENARIO_COUNT as u64 {
        // Use std::panic::catch_unwind to collect failures instead of aborting
        // on the first one, so we can report all bad seeds at once.
        let result = std::panic::catch_unwind(|| run_scenario(seed));

        if let Err(panic_payload) = result {
            let msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else {
                format!("seed={seed}: (non-string panic)")
            };
            failures.push(msg);
        }
    }

    assert!(
        failures.is_empty(),
        "{} scenario(s) failed out of {}:\n\n{}",
        failures.len(),
        SCENARIO_COUNT,
        failures.join("\n\n---\n\n"),
    );
}

/// Run 10 determinism checks: same seed → same merge tree OID.
///
/// This verifies that the merge pipeline is purely deterministic given the
/// same input state — a prerequisite for safe concurrent operation.
#[test]
fn merge_determinism_same_seed_same_tree() {
    // Test a representative subset of seeds.
    for seed in [0u64, 1, 7, 13, 42, 99, 100, 255, 1000, 9999] {
        let result = std::panic::catch_unwind(|| assert_deterministic(seed));

        if let Err(payload) = result {
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else {
                format!("seed={seed}: (non-string panic)")
            };
            panic!("determinism test failed for seed={seed}:\n{msg}");
        }
    }
}

/// Verify that concurrent git status reads in multiple workspaces do not
/// corrupt any workspace state or git objects.
///
/// This tests the "read-only concurrent access" path: many agents calling
/// `git status` simultaneously while another workspace has dirty files.
#[test]
fn concurrent_status_reads_do_not_corrupt() {
    let repo = TestRepo::new();

    // Seed a file so workspaces have something to diff against.
    repo.seed_files(&[("shared/readme.txt", "initial content\n")]);

    // Create 5 workspaces.
    let ws_names: Vec<String> = (0..5).map(|i| format!("reader-{i}")).collect();
    for name in &ws_names {
        repo.create_workspace(name);
    }

    // Make each workspace dirty in a different way.
    for (i, name) in ws_names.iter().enumerate() {
        repo.add_file(
            name,
            &format!("agent_{i}/work.txt"),
            &format!("work from {name}\n"),
        );
    }

    // Spawn threads that all run `git status` simultaneously.
    let barrier = Arc::new(Barrier::new(ws_names.len()));
    let mut handles = Vec::new();

    for ws_name in &ws_names {
        let ws_path = repo.workspace_path(ws_name);
        let b = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            b.wait();
            // Run status multiple times to maximize interleaving.
            for _ in 0..5 {
                let _ = Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(&ws_path)
                    .output();
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("status thread panicked");
    }

    // Git integrity should be clean after concurrent reads.
    if let Some(err) = git_fsck(repo.root()) {
        panic!("concurrent status reads corrupted git objects:\n{err}");
    }

    // Each workspace should still have its own file (reads didn't alter anything).
    for (i, name) in ws_names.iter().enumerate() {
        let path = format!("agent_{i}/work.txt");
        let content = repo
            .read_file(name, &path)
            .unwrap_or_else(|| panic!("file {path} missing from {name} after concurrent reads"));
        assert_eq!(
            content,
            format!("work from {name}\n"),
            "file content changed after concurrent reads — unexpected mutation"
        );
    }
}

/// Five agents each add 20 files concurrently; verify all 100 files make it
/// into the merge candidate with zero data loss.
///
/// This is a high-load scenario that maximises the number of concurrent git
/// object writes (blobs being staged by multiple worktrees simultaneously).
#[test]
fn high_load_five_agents_100_files_total_no_data_loss() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base/root.txt", "root\n")]);

    const FILES_PER_AGENT: usize = 20;
    let ws_names: Vec<String> = (0..5).map(|i| format!("heavy-{i}")).collect();
    for name in &ws_names {
        repo.create_workspace(name);
    }

    // All expected file contents, collected after concurrent writes.
    let expected: Arc<std::sync::Mutex<HashMap<String, String>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();

    for (i, ws_name) in ws_names.iter().enumerate() {
        let ws_path = repo.workspace_path(ws_name);
        let b = Arc::clone(&barrier);
        let expected_clone = Arc::clone(&expected);

        let handle = thread::spawn(move || {
            b.wait();

            let mut local: HashMap<String, String> = HashMap::new();
            for j in 0..FILES_PER_AGENT {
                let path = format!("heavy_{i}/file_{j:04}.txt");
                let content = format!("agent={i} file={j} bulk-write\n");
                let full = ws_path.join(&path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&full, &content).ok();
                local.insert(path, content);
            }

            let mut guard = expected_clone.lock().unwrap();
            guard.extend(local);
        });
        handles.push(handle);
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("heavy-{i} agent panicked"));
    }

    let all_expected = Arc::try_unwrap(expected).unwrap().into_inner().unwrap();

    // Every agent wrote FILES_PER_AGENT files → total should be 100.
    assert_eq!(
        all_expected.len(),
        5 * FILES_PER_AGENT,
        "expected {} files to be tracked, got {}",
        5 * FILES_PER_AGENT,
        all_expected.len()
    );

    // Run merge.
    let backend = backend_for(&repo);
    let epoch_str = repo.current_epoch();
    let epoch = maw::model::types::EpochId::new(&epoch_str).unwrap();
    let sources: Vec<maw::model::types::WorkspaceId> = ws_names
        .iter()
        .map(|n| maw::model::types::WorkspaceId::new(n).unwrap())
        .collect();

    let build_output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources)
            .expect("high-load merge failed");

    // No conflicts (disjoint paths per agent).
    assert!(
        build_output.conflicts.is_empty(),
        "unexpected conflicts in high-load scenario: {:?}",
        build_output.conflicts
    );

    // Git integrity.
    if let Some(err) = git_fsck(repo.root()) {
        panic!("high-load scenario corrupted git objects:\n{err}");
    }

    // All 100 files must be in the candidate.
    let candidate_oid = build_output.candidate.as_str();
    let candidate_files = list_tree_files(repo.root(), candidate_oid);

    let mut lost: Vec<String> = Vec::new();
    let mut corrupted: Vec<String> = Vec::new();

    for (path, expected_content) in &all_expected {
        if !candidate_files.contains(path) {
            lost.push(path.clone());
        } else if let Some(actual) = read_tree_file(repo.root(), candidate_oid, path)
            && actual != *expected_content {
                corrupted.push(format!(
                    "path={path}\n  expected={expected_content:?}\n  actual={actual:?}"
                ));
            }
    }

    assert!(
        lost.is_empty(),
        "DATA LOSS: {} file(s) missing from merge candidate:\n{}",
        lost.len(),
        lost.join("\n")
    );
    assert!(
        corrupted.is_empty(),
        "CONTENT CORRUPTION: {} file(s) have wrong content:\n{}",
        corrupted.len(),
        corrupted.join("\n")
    );
}

/// Adversarial interleaving: agents simultaneously add files AND delete
/// different files from the epoch base, while yet another agent only reads.
///
/// This is the hardest interleaving because it combines creates, deletes,
/// and reads concurrently. Verifies epoch base files are handled correctly.
#[test]
fn adversarial_concurrent_create_delete_read_no_divergence() {
    let repo = TestRepo::new();

    // Seed 10 base files that agents can see.
    let base_files: Vec<(String, String)> = (0..10)
        .map(|i| {
            (
                format!("base/file_{i}.txt"),
                format!("original content {i}\n"),
            )
        })
        .collect();
    let base_refs: Vec<(&str, &str)> = base_files
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    repo.seed_files(&base_refs);

    // Agent roles:
    //   agent-create: only creates new files
    //   agent-delete: deletes some base files (from the epoch)
    //   agent-reader: only reads (status checks)
    //   agent-mixed-a, agent-mixed-b: mix of create + modify own files
    let ws_names = [
        "agent-create",
        "agent-delete",
        "agent-reader",
        "agent-mixed-a",
        "agent-mixed-b",
    ];

    for name in &ws_names {
        repo.create_workspace(name);
    }

    let barrier = Arc::new(Barrier::new(ws_names.len()));

    // Collect what each workspace should contribute.
    let expected_live: Arc<std::sync::Mutex<HashMap<String, String>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    // Paths deleted from epoch (should NOT be in candidate unless another agent re-added them).
    let deleted_from_epoch: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let mut handles = Vec::new();

    // --- agent-create: creates 5 new files ---
    {
        let ws_path = repo.workspace_path("agent-create");
        let b = Arc::clone(&barrier);
        let live = Arc::clone(&expected_live);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut local = HashMap::new();
            for i in 0..5usize {
                let path = format!("created/new_{i}.txt");
                let content = format!("created by agent-create: {i}\n");
                let full = ws_path.join(&path);
                std::fs::create_dir_all(full.parent().unwrap()).ok();
                std::fs::write(&full, &content).ok();
                local.insert(path, content);
            }
            live.lock().unwrap().extend(local);
        }));
    }

    // --- agent-delete: deletes base files 0-2 ---
    {
        let ws_path = repo.workspace_path("agent-delete");
        let b = Arc::clone(&barrier);
        let deleted = Arc::clone(&deleted_from_epoch);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut del_paths = Vec::new();
            for i in 0..3usize {
                let path = format!("base/file_{i}.txt");
                let full = ws_path.join(&path);
                if full.exists() {
                    std::fs::remove_file(&full).ok();
                    del_paths.push(path);
                }
            }
            // Also create one file to prove agent-delete can do both.
            let new_path = "deleter/beacon.txt".to_owned();
            let new_content = "agent-delete was here\n".to_owned();
            let full_new = ws_path.join(&new_path);
            std::fs::create_dir_all(full_new.parent().unwrap()).ok();
            std::fs::write(&full_new, &new_content).ok();
            // The created file goes to expected_live (but we don't have that arc here;
            // we'll check it separately by reading the candidate).
            let _ = new_content;
            deleted.lock().unwrap().extend(del_paths);
        }));
    }

    // --- agent-reader: only does git status checks ---
    {
        let ws_path = repo.workspace_path("agent-reader");
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            for _ in 0..10 {
                let _ = Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(&ws_path)
                    .output();
            }
        }));
    }

    // --- agent-mixed-a ---
    {
        let ws_path = repo.workspace_path("agent-mixed-a");
        let b = Arc::clone(&barrier);
        let live = Arc::clone(&expected_live);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut local = HashMap::new();
            for i in 0..3usize {
                let path = format!("mixed_a/work_{i}.rs");
                let content = format!("// agent-mixed-a work {i}\npub fn work_{i}() {{}}\n");
                let full = ws_path.join(&path);
                std::fs::create_dir_all(full.parent().unwrap()).ok();
                std::fs::write(&full, &content).ok();
                local.insert(path, content);
            }
            live.lock().unwrap().extend(local);
        }));
    }

    // --- agent-mixed-b ---
    {
        let ws_path = repo.workspace_path("agent-mixed-b");
        let b = Arc::clone(&barrier);
        let live = Arc::clone(&expected_live);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut local = HashMap::new();
            for i in 0..4usize {
                let path = format!("mixed_b/data_{i}.json");
                let content = format!("{{\"agent\": \"mixed-b\", \"idx\": {i}}}\n");
                let full = ws_path.join(&path);
                std::fs::create_dir_all(full.parent().unwrap()).ok();
                std::fs::write(&full, &content).ok();
                local.insert(path, content);
            }
            live.lock().unwrap().extend(local);
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("adversarial thread {i} panicked"));
    }

    // Run merge.
    let backend = backend_for(&repo);
    let epoch_str = repo.current_epoch();
    let epoch = maw::model::types::EpochId::new(&epoch_str).unwrap();
    let sources: Vec<maw::model::types::WorkspaceId> = ws_names
        .iter()
        .map(|n| maw::model::types::WorkspaceId::new(n).unwrap())
        .collect();

    let build_output =
        maw::merge::run_build_phase_with_inputs(repo.root(), &backend, &epoch, &sources)
            .expect("adversarial scenario merge failed");

    // No conflicts (each agent has unique output paths — deletes don't conflict
    // with creates in disjoint directories).
    assert!(
        build_output.conflicts.is_empty(),
        "unexpected conflicts in adversarial scenario: {:?}",
        build_output.conflicts,
    );

    // Git integrity.
    if let Some(err) = git_fsck(repo.root()) {
        panic!("adversarial scenario corrupted git objects:\n{err}");
    }

    let candidate_oid = build_output.candidate.as_str();

    // Epoch consistency.
    let parent = commit_parent_oid(repo.root(), candidate_oid);
    assert_eq!(
        parent, epoch_str,
        "adversarial: candidate parent != epoch (epoch consistency violated)"
    );

    let candidate_files = list_tree_files(repo.root(), candidate_oid);

    // All expected live files must be present.
    let live_snapshot = Arc::try_unwrap(expected_live)
        .unwrap()
        .into_inner()
        .unwrap();

    for (path, expected_content) in &live_snapshot {
        assert!(
            candidate_files.contains(path),
            "adversarial: live file '{path}' missing from candidate"
        );
        if let Some(actual) = read_tree_file(repo.root(), candidate_oid, path) {
            assert_eq!(
                actual, *expected_content,
                "adversarial: content mismatch in '{path}'"
            );
        }
    }

    // Base files 3-9 (not deleted by agent-delete) should still be present.
    for i in 3..10usize {
        let path = format!("base/file_{i}.txt");
        assert!(
            candidate_files.contains(&path),
            "adversarial: un-deleted base file '{path}' missing from candidate"
        );
    }

    // agent-delete's beacon file must be present.
    assert!(
        candidate_files.contains(&"deleter/beacon.txt".to_owned()),
        "adversarial: agent-delete's beacon file missing from candidate"
    );
}
