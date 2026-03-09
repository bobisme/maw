//! Seeded deterministic workflow simulation for maw command contracts.
//!
//! This harness picks a deterministic set of higher-level workflows from a
//! single seed, runs them against a fresh temp repo, and checks invariants after
//! each workflow. Failures print the seed and a replay command so newly found
//! bugs can graduate into stable regressions.
//!
//! Replay a single failing seed:
//! `WORKFLOW_DST_SEED=<seed> cargo test --test workflow_dst dst_seeded_workflows_preserve_contracts -- --exact --nocapture`

mod dst_support;
mod manifold_common;

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::process::Command;

use manifold_common::TestRepo;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde_json::Value;

const BASE_SEED: u64 = 0x5EED_CAFE_7000_0001;

fn trace_count(default: u64) -> u64 {
    std::env::var("WORKFLOW_DST_TRACES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn single_seed() -> Option<u64> {
    std::env::var("WORKFLOW_DST_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
}

fn replay_command(seed: u64) -> String {
    format!(
        "WORKFLOW_DST_SEED={seed} cargo test --test workflow_dst dst_seeded_workflows_preserve_contracts -- --exact --nocapture"
    )
}

#[derive(Debug, Clone, Copy)]
enum Workflow {
    DescribeAnnotate,
    MergeHappyPath,
    ConflictJsonResolution,
    StaleSyncSkip,
    RecoverFidelity,
    ChangeIsolation,
    PushRemote,
}

impl Workflow {
    const ALL: [Self; 7] = [
        Self::DescribeAnnotate,
        Self::MergeHappyPath,
        Self::ConflictJsonResolution,
        Self::StaleSyncSkip,
        Self::RecoverFidelity,
        Self::ChangeIsolation,
        Self::PushRemote,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::DescribeAnnotate => "describe_annotate",
            Self::MergeHappyPath => "merge_happy_path",
            Self::ConflictJsonResolution => "conflict_json_resolution",
            Self::StaleSyncSkip => "stale_sync_skip",
            Self::RecoverFidelity => "recover_fidelity",
            Self::ChangeIsolation => "change_isolation",
            Self::PushRemote => "push_remote",
        }
    }
}

impl fmt::Display for Workflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[derive(Debug)]
struct TraceEntry {
    step: usize,
    workflow: Workflow,
    outcome: String,
}

impl fmt::Display for TraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[step {}] {} => {}",
            self.step, self.workflow, self.outcome
        )
    }
}

#[derive(Debug)]
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

    fn push(&mut self, step: usize, workflow: Workflow, outcome: impl Into<String>) {
        self.entries.push(TraceEntry {
            step,
            workflow,
            outcome: outcome.into(),
        });
    }

    fn dump(&self) {
        eprintln!("=== Workflow DST Trace (seed={}) ===", self.seed);
        for entry in &self.entries {
            eprintln!("  {entry}");
        }
        eprintln!("Replay: {}", replay_command(self.seed));
        eprintln!("=== end trace ===");
    }

    fn lines(&self) -> Vec<String> {
        self.entries.iter().map(ToString::to_string).collect()
    }
}

#[derive(Default)]
struct ScenarioState {
    next_ws: usize,
    next_change: usize,
    next_file: usize,
    next_recovered: usize,
    tracked_commit_oids: HashSet<String>,
    change_only_paths: Vec<String>,
    remote_configured: bool,
}

impl ScenarioState {
    fn ws_name(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}-{}", self.next_ws);
        self.next_ws += 1;
        name
    }

    fn change_id(&mut self) -> String {
        let id = format!("ch-sim-{}", self.next_change);
        self.next_change += 1;
        id
    }

    fn file_path(&mut self, prefix: &str) -> String {
        let path = format!("sim/{prefix}-{}.txt", self.next_file);
        self.next_file += 1;
        path
    }

    fn recovered_name(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}-restored-{}", self.next_recovered);
        self.next_recovered += 1;
        name
    }
}

fn parse_json(text: &str, context: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{context}: invalid JSON: {e}\n{text}"))
}

fn git_output(repo: &TestRepo, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo.root())
        .output()
        .map_err(|e| format!("git {} failed to spawn: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "git {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn git_output_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {} failed to spawn: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "git {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn commit_exists(repo: &TestRepo, oid: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-e", &format!("{oid}^{{commit}}")])
        .current_dir(repo.root())
        .output()
        .is_ok_and(|out| out.status.success())
}

fn git_integrity_ok(repo: &TestRepo) -> Result<(), String> {
    let out = Command::new("git")
        .args(["fsck", "--no-progress", "--connectivity-only"])
        .current_dir(repo.root())
        .output()
        .map_err(|e| format!("git fsck spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git fsck failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn commit_workspace_file(
    repo: &TestRepo,
    state: &mut ScenarioState,
    workspace: &str,
    path: &str,
    content: &str,
    message: &str,
) {
    repo.add_file(workspace, path, content);
    repo.git_in_workspace(workspace, &["add", "-A"]);
    repo.git_in_workspace(workspace, &["commit", "-m", message]);
    state
        .tracked_commit_oids
        .insert(repo.workspace_head(workspace));
}

fn common_invariants(repo: &TestRepo, state: &ScenarioState, context: &str) -> Result<(), String> {
    let list = parse_json(&repo.maw_ok(&["ws", "list", "--format", "json"]), "ws list")?;
    let status = parse_json(
        &repo.maw_ok(&["ws", "status", "--format", "json"]),
        "ws status",
    )?;
    let _history = parse_json(
        &repo.maw_ok(&["ws", "history", "default", "--format", "json"]),
        "ws history default",
    )?;

    let workspaces = list["workspaces"]
        .as_array()
        .ok_or_else(|| format!("{context}: ws list JSON missing workspaces array: {list}"))?;
    if !workspaces
        .iter()
        .any(|w| w["name"].as_str() == Some("default"))
    {
        return Err(format!(
            "{context}: default workspace missing from ws list JSON"
        ));
    }

    let status_workspaces = status["workspaces"]
        .as_array()
        .ok_or_else(|| format!("{context}: ws status JSON missing workspaces array: {status}"))?;
    if !status_workspaces
        .iter()
        .any(|w| w["name"].as_str() == Some("default"))
    {
        return Err(format!(
            "{context}: default workspace missing from ws status JSON"
        ));
    }

    for oid in &state.tracked_commit_oids {
        if !commit_exists(repo, oid) {
            return Err(format!(
                "{context}: tracked commit {oid} is no longer readable"
            ));
        }
    }

    for path in &state.change_only_paths {
        if repo.read_file("default", path).is_some() {
            return Err(format!(
                "{context}: change-only path leaked into default: {path}"
            ));
        }
    }

    Ok(())
}

fn workflow_describe_annotate(
    repo: &TestRepo,
    state: &mut ScenarioState,
) -> Result<String, String> {
    let ws = state.ws_name("note");
    repo.create_workspace(&ws);
    repo.maw_ok(&["ws", "describe", &ws, "wip: seeded workflow"]);
    repo.maw_ok(&["ws", "annotate", &ws, "qa", r#"{"passed":1,"failed":0}"#]);

    let history = parse_json(
        &repo.maw_ok(&["ws", "history", &ws, "--format", "json"]),
        "history after annotate",
    )?;
    let ops = history["operations"]
        .as_array()
        .ok_or_else(|| format!("history operations missing: {history}"))?;
    if !ops
        .iter()
        .any(|op| op["op_type"].as_str() == Some("describe"))
    {
        return Err(format!("history missing describe op: {history}"));
    }
    let annotate = ops
        .iter()
        .find(|op| op["op_type"].as_str() == Some("annotate"))
        .ok_or_else(|| format!("history missing annotate op: {history}"))?;
    if annotate["annotation_data"]["passed"].as_u64() != Some(1) {
        return Err(format!("annotation payload missing in history: {annotate}"));
    }

    Ok(format!("annotated {ws}"))
}

fn workflow_merge_happy_path(repo: &TestRepo, state: &mut ScenarioState) -> Result<String, String> {
    let ws = state.ws_name("merge");
    let path = state.file_path("merge");
    let content = format!("content for {ws}\n");
    repo.create_workspace(&ws);
    commit_workspace_file(repo, state, &ws, &path, &content, &format!("feat: {ws}"));

    let check = parse_json(
        &repo.maw_ok(&["ws", "merge", &ws, "--check", "--format", "json"]),
        "merge check json",
    )?;
    if check["ready"].as_bool() != Some(true) {
        return Err(format!("merge check not ready: {check}"));
    }

    let _plan = parse_json(
        &repo.maw_ok(&["ws", "merge", &ws, "--plan", "--format", "json"]),
        "merge plan json",
    )?;
    let _dry = parse_json(
        &repo.maw_ok(&["ws", "merge", &ws, "--dry-run", "--format", "json"]),
        "merge dry-run json",
    )?;

    repo.maw_ok(&[
        "ws",
        "merge",
        &ws,
        "--destroy",
        "--message",
        &format!("feat: merge {ws}"),
    ]);

    if repo.read_file("default", &path).as_deref() != Some(content.as_str()) {
        return Err(format!("merged file missing from default: {path}"));
    }

    Ok(format!("merged {ws} -> default"))
}

fn workflow_conflict_json_resolution(
    repo: &TestRepo,
    state: &mut ScenarioState,
) -> Result<String, String> {
    let shared = state.file_path("conflict-shared");
    repo.seed_files(&[(shared.as_str(), "base\n")]);

    let left = state.ws_name("left");
    let right = state.ws_name("right");
    repo.create_workspace(&left);
    repo.create_workspace(&right);

    commit_workspace_file(
        repo,
        state,
        &left,
        &shared,
        "left\n",
        &format!("feat: {left}"),
    );
    commit_workspace_file(
        repo,
        state,
        &right,
        &shared,
        "right\n",
        &format!("feat: {right}"),
    );

    let conflicts = parse_json(
        &repo.maw_ok(&["ws", "conflicts", &left, &right, "--format", "json"]),
        "ws conflicts json",
    )?;
    let to_fix = conflicts["to_fix"]
        .as_str()
        .ok_or_else(|| format!("conflicts JSON missing to_fix: {conflicts}"))?;
    if !(to_fix.contains("--into default") && to_fix.contains("--message ")) {
        return Err(format!("conflicts to_fix malformed: {to_fix}"));
    }

    let mut args: Vec<String> = to_fix.split_whitespace().map(str::to_owned).collect();
    if args.first().is_some_and(|s| s == "maw") {
        args.remove(0);
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.maw_ok(&arg_refs);

    if repo.read_file("default", &shared).as_deref() != Some("left\n") {
        return Err(format!(
            "default should contain chosen resolution from first workspace for {shared}"
        ));
    }

    Ok(format!("resolved conflict for {left}/{right}"))
}

fn workflow_stale_sync_skip(repo: &TestRepo, state: &mut ScenarioState) -> Result<String, String> {
    let ahead = state.ws_name("ahead");
    let advancer = state.ws_name("advancer");
    let ahead_path = state.file_path("ahead");
    let adv_path = state.file_path("adv");
    repo.create_workspace(&ahead);
    repo.create_workspace(&advancer);

    commit_workspace_file(
        repo,
        state,
        &ahead,
        &ahead_path,
        "ahead\n",
        &format!("feat: {ahead}"),
    );
    commit_workspace_file(
        repo,
        state,
        &advancer,
        &adv_path,
        "advance\n",
        &format!("feat: {advancer}"),
    );

    repo.maw_ok(&[
        "ws",
        "merge",
        &advancer,
        "--destroy",
        "--message",
        &format!("merge {advancer}"),
    ]);

    let out = repo.maw_raw(&["ws", "sync", "--all"]);
    if out.status.success() {
        return Err(format!(
            "sync --all should fail when {ahead} is stale and ahead\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !(stdout.contains("Results:")
        && stdout.contains("skipped")
        && stdout.contains("Result: INCOMPLETE")
        && stderr.contains("sync --all incomplete"))
    {
        return Err(format!(
            "sync --all contract mismatch\nstdout: {stdout}\nstderr: {stderr}"
        ));
    }

    let status = parse_json(
        &repo.maw_ok(&["ws", "status", "--format", "json"]),
        "status after sync all",
    )?;
    let workspaces = status["workspaces"]
        .as_array()
        .ok_or_else(|| format!("status JSON missing workspaces: {status}"))?;
    let ahead_state = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some(ahead.as_str()))
        .and_then(|w| w["state"].as_str())
        .unwrap_or_default();
    if !ahead_state.contains("stale") {
        return Err(format!(
            "ahead workspace should remain stale, got: {ahead_state}"
        ));
    }

    Ok(format!("sync skipped stale-ahead workspace {ahead}"))
}

fn workflow_recover_fidelity(repo: &TestRepo, state: &mut ScenarioState) -> Result<String, String> {
    let ws = state.ws_name("recover");
    let committed_path = state.file_path("recover-committed");
    let snapshot_path = state.file_path("recover-snapshot");
    let recovered = state.recovered_name(&ws);
    repo.create_workspace(&ws);
    commit_workspace_file(
        repo,
        state,
        &ws,
        &committed_path,
        "committed baseline\n",
        &format!("feat: {ws}"),
    );
    repo.add_file(&ws, &snapshot_path, "recover me\n");

    repo.maw_ok(&["ws", "destroy", &ws, "--force"]);
    let show = repo.maw_ok(&["ws", "recover", &ws, "--show", &snapshot_path]);
    if show != "recover me\n" {
        return Err(format!(
            "recovery show returned unexpected content: {show:?}"
        ));
    }
    repo.maw_ok(&["ws", "recover", &ws, "--to", &recovered]);

    let restored_path = repo.workspace_path(&recovered);
    let tracked = git_output_in(&restored_path, &["ls-files", &snapshot_path])?;
    if tracked.trim() != snapshot_path {
        return Err(format!(
            "recovered path should be tracked, got: {tracked:?}"
        ));
    }
    let status = git_output_in(
        &restored_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !status.trim().is_empty() {
        return Err(format!(
            "recovered workspace should be clean, got: {status}"
        ));
    }
    if repo.read_file(&recovered, &committed_path).as_deref() != Some("committed baseline\n") {
        return Err(format!(
            "recovered workspace missing committed file {committed_path}"
        ));
    }

    Ok(format!("recovered {ws} to {recovered}"))
}

fn workflow_change_isolation(repo: &TestRepo, state: &mut ScenarioState) -> Result<String, String> {
    let change_id = state.change_id();
    let worker = state.ws_name("change-worker");
    let path = state.file_path("change-only");
    repo.maw_ok(&[
        "changes",
        "create",
        "Seeded Flow",
        "--from",
        "main",
        "--id",
        &change_id,
        "--workspace",
        &change_id,
    ]);
    repo.maw_ok(&["ws", "create", "--change", &change_id, &worker]);
    commit_workspace_file(
        repo,
        state,
        &worker,
        &path,
        "change branch\n",
        "feat: change worker",
    );

    repo.maw_ok(&[
        "ws",
        "merge",
        &worker,
        "--into",
        &change_id,
        "--destroy",
        "--message",
        "feat: merge into change",
    ]);

    if repo.read_file("default", &path).is_some() {
        return Err(format!("change-only file leaked into default: {path}"));
    }
    state.change_only_paths.push(path.clone());

    Ok(format!("merged worker into change {change_id}"))
}

fn workflow_push_remote(repo: &TestRepo, state: &mut ScenarioState) -> Result<String, String> {
    if !state.remote_configured {
        let out = repo.maw_raw(&["push"]);
        if out.status.success() {
            return Err("push should fail before origin is configured".to_string());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("No 'origin' remote is configured") {
            return Err(format!("missing actionable push guidance: {stderr}"));
        }

        let remote_dir = repo.root().join("origin.git");
        git_output(repo, &["init", "--bare", remote_dir.to_str().unwrap()])?;
        git_output(
            repo,
            &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        )?;
        state.remote_configured = true;
    }

    repo.maw_ok(&["push"]);
    Ok("push succeeded after remote setup".to_string())
}

fn run_workflow(
    repo: &TestRepo,
    state: &mut ScenarioState,
    workflow: Workflow,
) -> Result<String, String> {
    match workflow {
        Workflow::DescribeAnnotate => workflow_describe_annotate(repo, state),
        Workflow::MergeHappyPath => workflow_merge_happy_path(repo, state),
        Workflow::ConflictJsonResolution => workflow_conflict_json_resolution(repo, state),
        Workflow::StaleSyncSkip => workflow_stale_sync_skip(repo, state),
        Workflow::RecoverFidelity => workflow_recover_fidelity(repo, state),
        Workflow::ChangeIsolation => workflow_change_isolation(repo, state),
        Workflow::PushRemote => workflow_push_remote(repo, state),
    }
}

struct SeedResult {
    trace: TraceLog,
    violations: Vec<String>,
    artifact_bundle: Option<std::path::PathBuf>,
}

fn run_seed(seed: u64) -> SeedResult {
    let repo = TestRepo::new();
    let mut state = ScenarioState::default();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut trace = TraceLog::new(seed);
    let mut violations = Vec::new();

    repo.seed_files(&[("README.md", "# seeded workflow dst\n")]);

    let mut workflows = Workflow::ALL.to_vec();
    workflows.shuffle(&mut rng);
    let steps = rng.random_range(4..=Workflow::ALL.len());

    for (step, workflow) in workflows.into_iter().take(steps).enumerate() {
        match run_workflow(&repo, &mut state, workflow) {
            Ok(outcome) => {
                trace.push(step, workflow, outcome);
                if let Err(err) = common_invariants(&repo, &state, workflow.name()) {
                    violations.push(err);
                }
            }
            Err(err) => {
                trace.push(step, workflow, format!("failed: {err}"));
                violations.push(format!("{}: {err}", workflow.name()));
                break;
            }
        }
    }

    if let Err(err) = git_integrity_ok(&repo) {
        violations.push(err);
    }

    let artifact_bundle = if violations.is_empty() {
        None
    } else {
        Some(dst_support::write_failure_bundle(
            "workflow-dst",
            seed,
            replay_command(seed),
            None,
            trace.lines(),
            &violations,
            &repo,
        ))
    };

    SeedResult {
        trace,
        violations,
        artifact_bundle,
    }
}

#[test]
fn dst_seeded_workflows_preserve_contracts() {
    let seeds: Vec<u64> = if let Some(seed) = single_seed() {
        vec![seed]
    } else {
        (0..trace_count(8))
            .map(|i| BASE_SEED.wrapping_add(i))
            .collect()
    };
    let total = seeds.len();

    let mut failures = Vec::new();

    for seed in seeds {
        let result = run_seed(seed);
        if !result.violations.is_empty() {
            result.trace.dump();
            for violation in &result.violations {
                eprintln!("  VIOLATION: {violation}");
            }
            if let Some(bundle) = &result.artifact_bundle {
                eprintln!("  ARTIFACT: {}", bundle.display());
            }
            failures.push((seed, result.violations));
        }
    }

    assert!(
        failures.is_empty(),
        "Workflow DST found {}/{} failing seed(s). Failing seeds: {:?}. First replay: {}",
        failures.len(),
        total,
        failures.iter().map(|(seed, _)| *seed).collect::<Vec<_>>(),
        failures.first().map_or_else(
            || replay_command(BASE_SEED),
            |(seed, _)| replay_command(*seed)
        )
    );
}

#[test]
#[ignore = "Slow seeded sweep. Run with WORKFLOW_DST_TRACES=64 cargo test --test workflow_dst -- --ignored --nocapture"]
fn dst_seeded_workflows_preserve_contracts_long_run() {
    let seeds: Vec<u64> = if let Some(seed) = single_seed() {
        vec![seed]
    } else {
        (0..trace_count(64))
            .map(|i| BASE_SEED.wrapping_add(i))
            .collect()
    };

    let mut failures = Vec::new();

    for seed in seeds {
        let result = run_seed(seed);
        if !result.violations.is_empty() {
            result.trace.dump();
            for violation in &result.violations {
                eprintln!("  VIOLATION: {violation}");
            }
            if let Some(bundle) = &result.artifact_bundle {
                eprintln!("  ARTIFACT: {}", bundle.display());
            }
            failures.push((seed, result.violations));
        }
    }

    assert!(
        failures.is_empty(),
        "Workflow DST long run found failing seeds: {:?}. First replay: {}",
        failures.iter().map(|(seed, _)| *seed).collect::<Vec<_>>(),
        failures.first().map_or_else(
            || replay_command(BASE_SEED),
            |(seed, _)| replay_command(*seed)
        )
    );
}
