//! Action-level deterministic workflow simulation for maw.
//!
//! This complements `workflow_dst.rs` by selecting lower-level stateful actions
//! from a seed, rather than only pre-baked workflow blocks. Failures print a
//! replay command and the smallest failing action prefix discovered by a simple
//! prefix minimizer.

mod dst_support;
mod manifold_common;

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::process::Command;

use manifold_common::TestRepo;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use serde_json::json;

const BASE_SEED: u64 = 0xAC71_0A5E_7000_0001;

fn trace_count(default: u64) -> u64 {
    std::env::var("ACTION_DST_TRACES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn single_seed() -> Option<u64> {
    std::env::var("ACTION_DST_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
}

fn step_limit() -> Option<usize> {
    std::env::var("ACTION_DST_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
}

fn replay_command(seed: u64, steps: usize) -> String {
    format!(
        "ACTION_DST_SEED={seed} ACTION_DST_STEPS={steps} cargo test --test action_workflow_dst dst_action_sequences_preserve_contracts -- --exact --nocapture"
    )
}

#[derive(Debug, Clone, Copy)]
enum ActionKind {
    CreateWorkspace,
    CommitWorkspace,
    AnnotateWorkspace,
    UndoWorkspace,
    MergeWorkspace,
    CreateConflictPair,
    ResolveConflictPair,
    CreateRecoverable,
    RecoverWorkspace,
    RestoreWorkspace,
    AttachOrphanDir,
    CleanTargets,
    PruneEmpty,
    CreateChangeFlow,
    CreateAheadScenario,
    DirtyDefault,
    ResolveDefault,
    SyncAll,
    PushRemote,
}

impl ActionKind {
    const fn name(self) -> &'static str {
        match self {
            Self::CreateWorkspace => "create_workspace",
            Self::CommitWorkspace => "commit_workspace",
            Self::AnnotateWorkspace => "annotate_workspace",
            Self::UndoWorkspace => "undo_workspace",
            Self::MergeWorkspace => "merge_workspace",
            Self::CreateConflictPair => "create_conflict_pair",
            Self::ResolveConflictPair => "resolve_conflict_pair",
            Self::CreateRecoverable => "create_recoverable",
            Self::RecoverWorkspace => "recover_workspace",
            Self::RestoreWorkspace => "restore_workspace",
            Self::AttachOrphanDir => "attach_orphan_dir",
            Self::CleanTargets => "clean_targets",
            Self::PruneEmpty => "prune_empty",
            Self::CreateChangeFlow => "create_change_flow",
            Self::CreateAheadScenario => "create_ahead_scenario",
            Self::DirtyDefault => "dirty_default",
            Self::ResolveDefault => "resolve_default",
            Self::SyncAll => "sync_all",
            Self::PushRemote => "push_remote",
        }
    }
}

impl fmt::Display for ActionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[derive(Debug)]
struct TraceEntry {
    step: usize,
    action: ActionKind,
    outcome: String,
}

impl fmt::Display for TraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[step {}] {} => {}",
            self.step, self.action, self.outcome
        )
    }
}

#[derive(Debug)]
struct TraceLog {
    seed: u64,
    max_steps: usize,
    entries: Vec<TraceEntry>,
}

impl TraceLog {
    fn new(seed: u64, max_steps: usize) -> Self {
        Self {
            seed,
            max_steps,
            entries: Vec::new(),
        }
    }

    fn push(&mut self, step: usize, action: ActionKind, outcome: impl Into<String>) {
        self.entries.push(TraceEntry {
            step,
            action,
            outcome: outcome.into(),
        });
    }

    fn dump(&self) {
        eprintln!(
            "=== Action Workflow DST Trace (seed={}, steps={}) ===",
            self.seed, self.max_steps
        );
        for entry in &self.entries {
            eprintln!("  {entry}");
        }
        eprintln!("Replay: {}", replay_command(self.seed, self.max_steps));
        eprintln!("=== end trace ===");
    }

    fn lines(&self) -> Vec<String> {
        self.entries.iter().map(ToString::to_string).collect()
    }
}

#[derive(Default)]
struct NameGen {
    ws: usize,
    file: usize,
    change: usize,
    restore: usize,
}

impl NameGen {
    fn ws(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}-{}", self.ws);
        self.ws += 1;
        name
    }

    fn path(&mut self, prefix: &str) -> String {
        let path = format!("sim/{prefix}-{}.txt", self.file);
        self.file += 1;
        path
    }

    fn change_id(&mut self) -> String {
        let id = format!("ch-act-{}", self.change);
        self.change += 1;
        id
    }

    fn restored(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}-restored-{}", self.restore);
        self.restore += 1;
        name
    }
}

struct WorkspaceActor {
    name: String,
    tracked_path: Option<String>,
    tracked_content: Option<String>,
    committed: bool,
    annotated: bool,
    merged: bool,
}

struct ConflictPair {
    left: String,
    right: String,
    shared_path: String,
    resolved: bool,
}

struct RecoverCase {
    source: String,
    committed_path: String,
    snapshot_path: String,
    restored_name: Option<String>,
}

struct SyncCase {
    ahead: String,
    validated: bool,
}

/// Tracks a file dirtied in default before a merge (bn-3oui).
struct DirtyDefaultCase {
    /// Path of the dirtied file in default.
    path: String,
    /// Whether the merge has happened (producing conflict markers).
    merged: bool,
    /// Whether `maw ws resolve` has been run.
    resolved: bool,
}

struct ActionState {
    names: NameGen,
    actors: Vec<WorkspaceActor>,
    conflict_pairs: Vec<ConflictPair>,
    recover_cases: Vec<RecoverCase>,
    sync_cases: Vec<SyncCase>,
    dirty_default_cases: Vec<DirtyDefaultCase>,
    tracked_commit_oids: HashSet<String>,
    change_only_paths: Vec<String>,
    remote_configured: bool,
    remote_pushed: bool,
    change_flow_done: bool,
    undo_done: bool,
    restore_done: bool,
    attach_done: bool,
    clean_done: bool,
    prune_done: bool,
    warnings: Vec<String>,
}

impl Default for ActionState {
    fn default() -> Self {
        Self {
            names: NameGen::default(),
            actors: Vec::new(),
            conflict_pairs: Vec::new(),
            recover_cases: Vec::new(),
            sync_cases: Vec::new(),
            dirty_default_cases: Vec::new(),
            tracked_commit_oids: HashSet::new(),
            change_only_paths: Vec::new(),
            remote_configured: false,
            remote_pushed: false,
            change_flow_done: false,
            undo_done: false,
            restore_done: false,
            attach_done: false,
            clean_done: false,
            prune_done: false,
            warnings: Vec::new(),
        }
    }
}

fn parse_json(text: &str, context: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{context}: invalid JSON: {e}\n{text}"))
}

fn warning_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("WARNING:"))
        .map(str::to_owned)
        .collect()
}

fn record_actionable_warnings(
    warnings: &mut Vec<String>,
    text: &str,
    context: &str,
) -> Result<(), String> {
    let lines = warning_lines(text);
    if lines.is_empty() {
        return Ok(());
    }
    if !text.contains("To fix:") {
        return Err(format!(
            "{context}: warning missing actionable To fix guidance\n{text}"
        ));
    }
    warnings.extend(lines);
    Ok(())
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

fn common_invariants(repo: &TestRepo, state: &ActionState, context: &str) -> Result<(), String> {
    let list = parse_json(&repo.maw_ok(&["ws", "list", "--format", "json"]), "ws list")?;
    let status = parse_json(
        &repo.maw_ok(&["ws", "status", "--format", "json"]),
        "ws status",
    )?;
    let _history = parse_json(
        &repo.maw_ok(&["ws", "history", "default", "--format", "json"]),
        "ws history default",
    )?;

    if !list["workspaces"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|w| w["name"].as_str() == Some("default")))
    {
        return Err(format!("{context}: default missing from ws list JSON"));
    }
    if !status["workspaces"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|w| w["name"].as_str() == Some("default")))
    {
        return Err(format!("{context}: default missing from ws status JSON"));
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

fn applicable_actions(state: &ActionState) -> Vec<ActionKind> {
    // Resolve dirty-default conflicts before anything else.
    if state
        .dirty_default_cases
        .iter()
        .any(|c| c.merged && !c.resolved)
    {
        return vec![ActionKind::ResolveDefault];
    }
    if state.conflict_pairs.iter().any(|pair| !pair.resolved) {
        return vec![ActionKind::ResolveConflictPair];
    }
    if state.sync_cases.iter().any(|case| !case.validated) {
        return vec![ActionKind::SyncAll];
    }
    if state.actors.iter().any(|ws| !ws.merged && ws.committed) {
        // If there's a pending dirty-default that hasn't been merged yet,
        // force the merge so we test the overlap path.
        if state.dirty_default_cases.iter().any(|c| !c.merged) {
            return vec![ActionKind::MergeWorkspace];
        }
        // Allow DirtyDefault alongside MergeWorkspace when there's a
        // committed workspace with a tracked path and no pending dirty case.
        let can_dirty = state.dirty_default_cases.iter().all(|c| c.resolved)
            && state
                .actors
                .iter()
                .any(|ws| !ws.merged && ws.committed && ws.tracked_path.is_some());
        if can_dirty {
            return vec![ActionKind::MergeWorkspace, ActionKind::DirtyDefault];
        }
        return vec![ActionKind::MergeWorkspace];
    }
    if state.actors.iter().any(|ws| !ws.merged && !ws.committed) {
        let mut early = Vec::new();
        if state.actors.iter().any(|ws| !ws.merged && !ws.annotated) {
            early.push(ActionKind::AnnotateWorkspace);
        }
        early.push(ActionKind::CommitWorkspace);
        return early;
    }

    let mut actions = Vec::new();

    if state.actors.iter().filter(|ws| !ws.merged).count() < 3 {
        actions.push(ActionKind::CreateWorkspace);
    }
    if state.actors.iter().any(|ws| !ws.merged && !ws.committed) {
        actions.push(ActionKind::CommitWorkspace);
    }
    if state.actors.iter().any(|ws| !ws.merged && !ws.annotated) {
        actions.push(ActionKind::AnnotateWorkspace);
    }
    if state.actors.iter().any(|ws| !ws.merged && ws.committed) {
        actions.push(ActionKind::MergeWorkspace);
    }
    if state.conflict_pairs.iter().all(|pair| pair.resolved) {
        actions.push(ActionKind::CreateConflictPair);
    }
    if state.conflict_pairs.iter().any(|pair| !pair.resolved) {
        actions.push(ActionKind::ResolveConflictPair);
    }
    if state
        .recover_cases
        .iter()
        .filter(|case| case.restored_name.is_none())
        .count()
        < 2
    {
        actions.push(ActionKind::CreateRecoverable);
    }
    if state
        .recover_cases
        .iter()
        .any(|case| case.restored_name.is_none())
    {
        actions.push(ActionKind::RecoverWorkspace);
    }
    if !state.change_flow_done {
        actions.push(ActionKind::CreateChangeFlow);
    }
    if state.sync_cases.is_empty() {
        actions.push(ActionKind::CreateAheadScenario);
    }
    if state.sync_cases.iter().any(|case| !case.validated) {
        actions.push(ActionKind::SyncAll);
    }
    if !state.undo_done {
        actions.push(ActionKind::UndoWorkspace);
    }
    if !state.restore_done {
        actions.push(ActionKind::RestoreWorkspace);
    }
    if !state.attach_done {
        actions.push(ActionKind::AttachOrphanDir);
    }
    if !state.clean_done {
        actions.push(ActionKind::CleanTargets);
    }
    if !state.prune_done {
        actions.push(ActionKind::PruneEmpty);
    }
    // DirtyDefault: available when there's a committed, unmerged workspace
    // with a tracked path and no existing unresolved dirty-default case.
    if state.dirty_default_cases.iter().all(|c| c.resolved)
        && state
            .actors
            .iter()
            .any(|ws| !ws.merged && ws.committed && ws.tracked_path.is_some())
    {
        actions.push(ActionKind::DirtyDefault);
    }
    if !state.remote_pushed {
        actions.push(ActionKind::PushRemote);
    }

    actions
}

fn choose_index<T>(rng: &mut StdRng, items: &[T]) -> usize {
    rng.random_range(0..items.len())
}

fn run_action(
    repo: &TestRepo,
    state: &mut ActionState,
    rng: &mut StdRng,
    action: ActionKind,
) -> Result<String, String> {
    match action {
        ActionKind::CreateWorkspace => {
            let name = state.names.ws("actor");
            repo.create_workspace(&name);
            state.actors.push(WorkspaceActor {
                name: name.clone(),
                tracked_path: None,
                tracked_content: None,
                committed: false,
                annotated: false,
                merged: false,
            });
            Ok(format!("created {name}"))
        }
        ActionKind::CommitWorkspace => {
            let candidates: Vec<usize> = state
                .actors
                .iter()
                .enumerate()
                .filter(|(_, ws)| !ws.merged && !ws.committed)
                .map(|(idx, _)| idx)
                .collect();
            let idx = candidates[choose_index(rng, &candidates)];
            let path = state.names.path("commit");
            let content = format!("content for {}\n", state.actors[idx].name);
            let name = state.actors[idx].name.clone();
            repo.add_file(&name, &path, &content);
            repo.git_in_workspace(&name, &["add", "-A"]);
            repo.git_in_workspace(&name, &["commit", "-m", &format!("feat: {name}")]);
            state.tracked_commit_oids.insert(repo.workspace_head(&name));
            state.actors[idx].tracked_path = Some(path.clone());
            state.actors[idx].tracked_content = Some(content.clone());
            state.actors[idx].committed = true;
            Ok(format!("committed {path} in {name}"))
        }
        ActionKind::AnnotateWorkspace => {
            let candidates: Vec<usize> = state
                .actors
                .iter()
                .enumerate()
                .filter(|(_, ws)| !ws.merged && !ws.annotated)
                .map(|(idx, _)| idx)
                .collect();
            let idx = candidates[choose_index(rng, &candidates)];
            let name = state.actors[idx].name.clone();
            repo.maw_ok(&["ws", "describe", &name, "wip: action dst"]);
            repo.maw_ok(&["ws", "annotate", &name, "qa", r#"{"passed":2,"failed":0}"#]);
            let history = parse_json(
                &repo.maw_ok(&["ws", "history", &name, "--format", "json"]),
                "history annotate",
            )?;
            let ops = history["operations"]
                .as_array()
                .ok_or_else(|| format!("history missing operations: {history}"))?;
            if !ops
                .iter()
                .any(|op| op["op_type"].as_str() == Some("annotate"))
            {
                return Err(format!("annotate op missing from history: {history}"));
            }
            state.actors[idx].annotated = true;
            Ok(format!("annotated {name}"))
        }
        ActionKind::MergeWorkspace => {
            let candidates: Vec<usize> = state
                .actors
                .iter()
                .enumerate()
                .filter(|(_, ws)| !ws.merged && ws.committed)
                .map(|(idx, _)| idx)
                .collect();
            let idx = candidates[choose_index(rng, &candidates)];
            let ws = &state.actors[idx];
            // Skip --check when default is dirty — the check runs a trial
            // merge that may interact with dirty state unpredictably.
            let has_dirty_overlap = ws.tracked_path.as_ref().is_some_and(|path| {
                state.dirty_default_cases.iter().any(|c| c.path == *path && !c.merged)
            });
            if !has_dirty_overlap {
                let check = parse_json(
                    &repo.maw_ok(&["ws", "merge", &ws.name, "--check", "--format", "json"]),
                    "merge check",
                )?;
                if check["ready"].as_bool() != Some(true) {
                    return Err(format!("merge check not ready: {check}"));
                }
            }
            let merge_out = repo.maw_ok(&[
                "ws",
                "merge",
                &ws.name,
                "--destroy",
                "--message",
                &format!("feat: merge {}", ws.name),
            ]);
            record_actionable_warnings(&mut state.warnings, &merge_out, "merge workspace output")?;
            if let (Some(path), Some(content)) = (&ws.tracked_path, &ws.tracked_content) {
                let actual = repo.read_file("default", path);
                // Check if this path was dirtied in default before the merge.
                let is_dirty_overlap = state
                    .dirty_default_cases
                    .iter()
                    .any(|c| c.path == *path && !c.merged);

                if is_dirty_overlap {
                    // Conflict markers are expected. Verify they exist.
                    match &actual {
                        Some(file_content) if file_content.contains("<<<<<<<") => {
                            // Good — conflict markers present. Mark the dirty case as merged.
                            if let Some(case) = state
                                .dirty_default_cases
                                .iter_mut()
                                .find(|c| c.path == *path && !c.merged)
                            {
                                case.merged = true;
                            }
                        }
                        Some(file_content) if file_content == content => {
                            // Also acceptable — stash replay didn't interfere.
                            if let Some(case) = state
                                .dirty_default_cases
                                .iter_mut()
                                .find(|c| c.path == *path && !c.merged)
                            {
                                case.merged = true;
                                case.resolved = true; // no conflict to resolve
                            }
                        }
                        _ => {
                            return Err(format!(
                                "dirty-default overlap for {path}: expected conflict markers or merged content, got: {:?}",
                                actual.as_deref().map(|s| &s[..s.len().min(100)])
                            ));
                        }
                    }
                } else if actual.as_deref() != Some(content.as_str()) {
                    return Err(format!("default missing merged content for {path}"));
                }
            }
            state.actors[idx].merged = true;
            Ok(format!("merged {}", state.actors[idx].name))
        }
        ActionKind::CreateConflictPair => {
            let shared = state.names.path("shared");
            repo.seed_files(&[(shared.as_str(), "base\n")]);
            let left = state.names.ws("left");
            let right = state.names.ws("right");
            repo.create_workspace(&left);
            repo.create_workspace(&right);
            for (name, content) in [(&left, "left\n"), (&right, "right\n")] {
                repo.add_file(name, &shared, content);
                repo.git_in_workspace(name, &["add", &shared]);
                repo.git_in_workspace(name, &["commit", "-m", &format!("feat: {name}")]);
                state.tracked_commit_oids.insert(repo.workspace_head(name));
            }
            state.conflict_pairs.push(ConflictPair {
                left: left.clone(),
                right: right.clone(),
                shared_path: shared.clone(),
                resolved: false,
            });
            Ok(format!("created conflicting pair {left}/{right}"))
        }
        ActionKind::ResolveConflictPair => {
            let candidates: Vec<usize> = state
                .conflict_pairs
                .iter()
                .enumerate()
                .filter(|(_, pair)| !pair.resolved)
                .map(|(idx, _)| idx)
                .collect();
            let idx = candidates[choose_index(rng, &candidates)];
            let left = state.conflict_pairs[idx].left.clone();
            let right = state.conflict_pairs[idx].right.clone();
            let shared_path = state.conflict_pairs[idx].shared_path.clone();
            let conflicts = parse_json(
                &repo.maw_ok(&["ws", "conflicts", &left, &right, "--format", "json"]),
                "conflicts json",
            )?;
            let to_fix = conflicts["to_fix"]
                .as_str()
                .ok_or_else(|| format!("missing to_fix in conflicts JSON: {conflicts}"))?;
            let mut args: Vec<&str> = to_fix.split_whitespace().collect();
            if args.first().copied() == Some("maw") {
                args.remove(0);
            }
            let merge_out = repo.maw_ok(&args);
            record_actionable_warnings(&mut state.warnings, &merge_out, "resolve conflict output")?;
            if repo.read_file("default", &shared_path).as_deref() != Some("left\n") {
                return Err(format!(
                    "default missing resolved conflict content: {to_fix}"
                ));
            }
            state.conflict_pairs[idx].resolved = true;
            Ok(format!("resolved {left} vs {right}"))
        }
        ActionKind::CreateRecoverable => {
            let source = state.names.ws("recover");
            let committed_path = state.names.path("recover-committed");
            let snapshot_path = state.names.path("recover-snapshot");
            repo.create_workspace(&source);
            repo.add_file(&source, &committed_path, "baseline\n");
            repo.git_in_workspace(&source, &["add", "-A"]);
            repo.git_in_workspace(&source, &["commit", "-m", &format!("feat: {source}")]);
            state
                .tracked_commit_oids
                .insert(repo.workspace_head(&source));
            repo.add_file(&source, &snapshot_path, "snapshot\n");
            let destroy = repo.maw_ok(&["ws", "destroy", &source, "--force"]);
            record_actionable_warnings(&mut state.warnings, &destroy, "destroy output")?;
            state.recover_cases.push(RecoverCase {
                source: source.clone(),
                committed_path,
                snapshot_path,
                restored_name: None,
            });
            Ok(format!("captured recovery snapshot for {source}"))
        }
        ActionKind::RecoverWorkspace => {
            let candidates: Vec<usize> = state
                .recover_cases
                .iter()
                .enumerate()
                .filter(|(_, case)| case.restored_name.is_none())
                .map(|(idx, _)| idx)
                .collect();
            let idx = candidates[choose_index(rng, &candidates)];
            let source = state.recover_cases[idx].source.clone();
            let committed_path = state.recover_cases[idx].committed_path.clone();
            let snapshot_path = state.recover_cases[idx].snapshot_path.clone();
            let restored = state.names.restored(&source);
            let show = repo.maw_ok(&["ws", "recover", &source, "--show", &snapshot_path]);
            if show != "snapshot\n" {
                return Err(format!("unexpected recover --show output: {show:?}"));
            }
            let recover = repo.maw_ok(&["ws", "recover", &source, "--to", &restored]);
            record_actionable_warnings(&mut state.warnings, &recover, "recover output")?;
            let restored_path = repo.workspace_path(&restored);
            let tracked = git_output_in(&restored_path, &["ls-files", &snapshot_path])?;
            if tracked.trim() != snapshot_path {
                return Err(format!("restored snapshot path not tracked: {tracked:?}"));
            }
            let status = git_output_in(
                &restored_path,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            )?;
            if !status.trim().is_empty() {
                return Err(format!("restored workspace should be clean, got: {status}"));
            }
            if repo.read_file(&restored, &committed_path).as_deref() != Some("baseline\n") {
                return Err(format!(
                    "restored workspace missing committed file {}",
                    committed_path
                ));
            }
            state.recover_cases[idx].restored_name = Some(restored.clone());
            Ok(format!("restored {source} to {restored}"))
        }
        ActionKind::RestoreWorkspace => {
            let name = state.names.ws("restore");
            let path = state.names.path("restore");
            repo.create_workspace(&name);
            repo.add_file(&name, &path, "throwaway\n");
            let destroy = repo.maw_ok(&["ws", "destroy", &name, "--force"]);
            record_actionable_warnings(&mut state.warnings, &destroy, "destroy before restore")?;
            let restore = repo.maw_ok(&["ws", "restore", &name]);
            record_actionable_warnings(&mut state.warnings, &restore, "restore output")?;
            if !repo.workspace_exists(&name) {
                return Err(format!("restore should recreate workspace {name}"));
            }
            if repo.read_file(&name, &path).is_some() {
                return Err(format!(
                    "restore should not replay destroyed local file into fresh workspace: {path}"
                ));
            }
            state.restore_done = true;
            Ok(format!("restored workspace {name} at current epoch"))
        }
        ActionKind::UndoWorkspace => {
            let name = state.names.ws("undo");
            let path = state.names.path("undo");
            repo.create_workspace(&name);
            repo.add_file(&name, &path, "undo me\n");
            repo.maw_ok(&["ws", "undo", &name]);
            if repo.read_file(&name, &path).is_some() {
                return Err(format!("undo should remove local file {path} from {name}"));
            }
            let history = parse_json(
                &repo.maw_ok(&["ws", "history", &name, "--format", "json"]),
                "undo history",
            )?;
            if !history["operations"].as_array().is_some_and(|ops| {
                ops.iter()
                    .any(|op| op["op_type"].as_str() == Some("compensate"))
            }) {
                return Err(format!("undo should record compensate op: {history}"));
            }
            state.undo_done = true;
            Ok(format!("undid local changes in {name}"))
        }
        ActionKind::AttachOrphanDir => {
            let name = state.names.ws("attach");
            let path = state.names.path("attach");
            let ws_path = repo.workspace_path(&name);
            std::fs::create_dir_all(ws_path.join("sim"))
                .map_err(|e| format!("create orphan dir structure failed: {e}"))?;
            std::fs::write(ws_path.join(&path), "orphaned content\n")
                .map_err(|e| format!("write orphan file failed: {e}"))?;
            let attach = repo.maw_ok(&["ws", "attach", &name, "-r", "main"]);
            record_actionable_warnings(&mut state.warnings, &attach, "attach output")?;
            if !repo.workspace_exists(&name) {
                return Err(format!("attach should track orphaned directory {name}"));
            }
            if repo.read_file(&name, &path).as_deref() != Some("orphaned content\n") {
                return Err(format!(
                    "attach should preserve orphaned file contents: {path}"
                ));
            }
            let status = git_output_in(
                &repo.workspace_path(&name),
                &["status", "--porcelain=v1", "--untracked-files=all"],
            )?;
            if status.trim().is_empty() {
                return Err(format!(
                    "attached workspace should expose preserved local differences, got clean status"
                ));
            }
            state.attach_done = true;
            Ok(format!("attached orphaned directory {name}"))
        }
        ActionKind::CleanTargets => {
            let clean_ws = state.names.ws("clean");
            repo.create_workspace(&clean_ws);
            let default_target = repo.default_workspace().join("target").join("dummy.txt");
            let ws_target = repo
                .workspace_path(&clean_ws)
                .join("target")
                .join("dummy.txt");
            std::fs::create_dir_all(default_target.parent().expect("default target parent"))
                .map_err(|e| format!("create default target failed: {e}"))?;
            std::fs::create_dir_all(ws_target.parent().expect("workspace target parent"))
                .map_err(|e| format!("create workspace target failed: {e}"))?;
            std::fs::write(&default_target, "x\n")
                .map_err(|e| format!("write default target file failed: {e}"))?;
            std::fs::write(&ws_target, "x\n")
                .map_err(|e| format!("write workspace target file failed: {e}"))?;
            let clean = repo.maw_ok(&["ws", "clean", "--all"]);
            record_actionable_warnings(&mut state.warnings, &clean, "clean output")?;
            if default_target
                .parent()
                .expect("default target dir")
                .exists()
                || ws_target.parent().expect("workspace target dir").exists()
            {
                return Err("ws clean --all should remove target directories".to_string());
            }
            state.clean_done = true;
            Ok(format!("cleaned target directories including {clean_ws}"))
        }
        ActionKind::PruneEmpty => {
            let first = state.names.ws("empty");
            let second = state.names.ws("empty");
            repo.create_workspace(&first);
            repo.create_workspace(&second);
            let preview = repo.maw_ok(&["ws", "prune", "--empty"]);
            record_actionable_warnings(&mut state.warnings, &preview, "prune preview output")?;
            if !(preview.contains(&first) && preview.contains(&second)) {
                return Err(format!(
                    "prune preview should include both empty workspaces: {preview}"
                ));
            }
            let prune = repo.maw_ok(&["ws", "prune", "--empty", "--force"]);
            record_actionable_warnings(&mut state.warnings, &prune, "prune output")?;
            if repo.workspace_exists(&first) || repo.workspace_exists(&second) {
                return Err(format!(
                    "prune should delete empty workspaces {first} and {second}"
                ));
            }
            if !repo.workspace_exists("default") {
                return Err("prune should never remove default workspace".to_string());
            }
            state.prune_done = true;
            Ok(format!("pruned empty workspaces {first} and {second}"))
        }
        ActionKind::CreateChangeFlow => {
            let change_id = state.names.change_id();
            let worker = state.names.ws("change-worker");
            let path = state.names.path("change-only");
            repo.maw_ok(&[
                "changes",
                "create",
                "Action Flow",
                "--from",
                "main",
                "--id",
                &change_id,
                "--workspace",
                &change_id,
            ]);
            repo.maw_ok(&["ws", "create", "--change", &change_id, &worker]);
            repo.add_file(&worker, &path, "change branch\n");
            repo.git_in_workspace(&worker, &["add", &path]);
            repo.git_in_workspace(&worker, &["commit", "-m", "feat: change worker"]);
            state
                .tracked_commit_oids
                .insert(repo.workspace_head(&worker));
            let merge_out = repo.maw_ok(&[
                "ws",
                "merge",
                &worker,
                "--into",
                &change_id,
                "--destroy",
                "--message",
                "feat: merge into change",
            ]);
            record_actionable_warnings(&mut state.warnings, &merge_out, "change merge output")?;
            if repo.read_file("default", &path).is_some() {
                return Err(format!("change-only file leaked into default: {path}"));
            }
            state.change_only_paths.push(path);
            state.change_flow_done = true;
            Ok(format!("merged worker into change {change_id}"))
        }
        ActionKind::CreateAheadScenario => {
            let ahead = state.names.ws("ahead");
            let advancer = state.names.ws("adv");
            let ahead_path = state.names.path("ahead");
            let adv_path = state.names.path("adv");
            repo.create_workspace(&ahead);
            repo.create_workspace(&advancer);
            for (name, path, content) in [
                (&ahead, &ahead_path, "ahead\n"),
                (&advancer, &adv_path, "advance\n"),
            ] {
                repo.add_file(name, path, content);
                repo.git_in_workspace(name, &["add", path]);
                repo.git_in_workspace(name, &["commit", "-m", &format!("feat: {name}")]);
                state.tracked_commit_oids.insert(repo.workspace_head(name));
            }
            let merge_out = repo.maw_ok(&[
                "ws",
                "merge",
                &advancer,
                "--destroy",
                "--message",
                "feat: advance epoch",
            ]);
            record_actionable_warnings(&mut state.warnings, &merge_out, "advance merge output")?;
            state.sync_cases.push(SyncCase {
                ahead: ahead.clone(),
                validated: false,
            });
            Ok(format!("created stale-ahead sync case for {ahead}"))
        }
        ActionKind::SyncAll => {
            let out = repo.maw_raw(&["ws", "sync", "--all"]);
            if out.status.success() {
                return Err(format!(
                    "sync --all should fail when stale ahead cases exist\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !(stdout.contains("Results:")
                && stdout.contains("skipped")
                && (stdout.contains("Result: INCOMPLETE") || stdout.contains("Errors:"))
                && (stderr.contains("sync --all incomplete")
                    || stderr.contains("sync --all failed")))
            {
                return Err(format!(
                    "sync --all contract mismatch\nstdout: {stdout}\nstderr: {stderr}"
                ));
            }
            let status = parse_json(
                &repo.maw_ok(&["ws", "status", "--format", "json"]),
                "status after sync",
            )?;
            for case in &mut state.sync_cases {
                let ws_state = status["workspaces"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|w| w["name"].as_str() == Some(case.ahead.as_str()))
                    })
                    .and_then(|w| w["state"].as_str())
                    .unwrap_or_default();
                if !ws_state.contains("stale") {
                    return Err(format!(
                        "{} should remain stale after skipped sync",
                        case.ahead
                    ));
                }
                case.validated = true;
            }
            Ok("validated sync --all incomplete path".to_string())
        }
        ActionKind::DirtyDefault => {
            // Find a committed, unmerged workspace with a tracked path.
            let candidates: Vec<usize> = state
                .actors
                .iter()
                .enumerate()
                .filter(|(_, ws)| !ws.merged && ws.committed && ws.tracked_path.is_some())
                .map(|(idx, _)| idx)
                .collect();
            let idx = candidates[choose_index(rng, &candidates)];
            let ws = &state.actors[idx];
            let path = ws.tracked_path.as_ref().unwrap().clone();

            // Write a different version of the file to default (uncommitted).
            let dirty_content = format!("dirty-default-edit for {}\n", ws.name);
            repo.add_file("default", &path, &dirty_content);
            // Do NOT git add/commit — leave it uncommitted in default.

            state.dirty_default_cases.push(DirtyDefaultCase {
                path: path.clone(),
                merged: false,
                resolved: false,
            });

            Ok(format!("dirtied default/{path} (overlaps with {})", ws.name))
        }
        ActionKind::ResolveDefault => {
            // Find an unresolved dirty-default case that has been merged.
            let case_idx = state
                .dirty_default_cases
                .iter()
                .position(|c| c.merged && !c.resolved)
                .ok_or_else(|| "no dirty-default case to resolve".to_string())?;

            let path = state.dirty_default_cases[case_idx].path.clone();

            // Verify conflict markers exist before resolving.
            let content = repo
                .read_file("default", &path)
                .ok_or_else(|| format!("dirty-default file missing: {path}"))?;
            if !content.contains("<<<<<<<") {
                return Err(format!(
                    "expected conflict markers in default/{path} but none found"
                ));
            }

            // Resolve using "both" — always valid regardless of side labels.
            let resolve_output = repo.maw_ok(&[
                "ws", "resolve", "default", "--keep",
                &format!("{path}=both"),
            ]);

            // Verify markers are gone.
            if let Some(after) = repo.read_file("default", &path) {
                if after.contains("<<<<<<<") {
                    return Err(format!(
                        "conflict markers still present after resolve for {path}"
                    ));
                }
            }

            state.dirty_default_cases[case_idx].resolved = true;

            Ok(format!("resolved default/{path}: {resolve_output}"))
        }
        ActionKind::PushRemote => {
            unreachable!("push is handled by run_action_dispatch")
        }
    }
}

fn git_output_in_root(repo: &TestRepo, args: &[&str]) -> Result<String, String> {
    git_output_in(repo.root(), args)
}

fn run_action_seed(
    seed: u64,
    max_steps: usize,
    minimized_replay: Option<String>,
    capture_bundle: bool,
) -> ActionSeedResult {
    let repo = TestRepo::new();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut state = ActionState::default();
    let mut trace = TraceLog::new(seed, max_steps);
    let mut violations = Vec::new();

    repo.seed_files(&[("README.md", "# action workflow dst\n")]);

    for step in 0..max_steps {
        let choices = applicable_actions(&state);
        if choices.is_empty() {
            break;
        }
        let action = choices[choose_index(&mut rng, &choices)];
        match run_action_dispatch(&repo, &mut state, &mut rng, action) {
            Ok(outcome) => {
                trace.push(step, action, outcome);
                if let Err(err) = common_invariants(&repo, &state, action.name()) {
                    violations.push(err);
                    break;
                }
            }
            Err(err) => {
                trace.push(step, action, format!("failed: {err}"));
                violations.push(format!("{}: {err}", action.name()));
                break;
            }
        }
    }

    if let Err(err) = git_integrity_ok(&repo) {
        violations.push(err);
    }

    let artifact_bundle = if capture_bundle && !violations.is_empty() {
        Some(dst_support::write_failure_bundle(
            "action-workflow-dst",
            seed,
            replay_command(seed, max_steps),
            minimized_replay,
            trace.lines(),
            &violations,
            &state.warnings,
            &repo,
        ))
    } else {
        None
    };

    ActionSeedResult {
        trace,
        violations,
        artifact_bundle,
        warnings: state.warnings,
    }
}

fn run_action_dispatch(
    repo: &TestRepo,
    state: &mut ActionState,
    rng: &mut StdRng,
    action: ActionKind,
) -> Result<String, String> {
    if matches!(action, ActionKind::PushRemote) && !state.remote_configured {
        let out = repo.maw_raw(&["push"]);
        if out.status.success() {
            return Err("push should fail before origin exists".to_string());
        }
        if !String::from_utf8_lossy(&out.stderr).contains("No 'origin' remote is configured") {
            return Err(format!(
                "push stderr missing actionable remote guidance: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let remote_dir = repo.root().join("origin.git");
        git_output_in_root(repo, &["init", "--bare", remote_dir.to_str().unwrap()])?;
        git_output_in_root(
            repo,
            &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        )?;
        state.remote_configured = true;
    }

    if matches!(action, ActionKind::PushRemote) {
        let push = repo.maw_ok(&["push"]);
        record_actionable_warnings(&mut state.warnings, &push, "push output")?;
        state.remote_pushed = true;
        return Ok("push succeeded".to_string());
    }

    run_action(repo, state, rng, action)
}

struct ActionSeedResult {
    trace: TraceLog,
    violations: Vec<String>,
    artifact_bundle: Option<std::path::PathBuf>,
    warnings: Vec<String>,
}

fn minimize_prefix(seed: u64, executed_steps: usize) -> usize {
    (1..=executed_steps)
        .find(|steps| {
            !run_action_seed(seed, *steps, None, false)
                .violations
                .is_empty()
        })
        .unwrap_or(executed_steps)
}

#[test]
fn dst_action_sequences_preserve_contracts() {
    let steps = step_limit().unwrap_or(10);
    let seeds: Vec<u64> = if let Some(seed) = single_seed() {
        vec![seed]
    } else {
        (0..trace_count(8))
            .map(|i| BASE_SEED.wrapping_add(i))
            .collect()
    };

    let mut failures = Vec::new();
    let mut summaries = Vec::new();

    for seed in seeds {
        let result = run_action_seed(seed, steps, None, false);
        if !result.violations.is_empty() {
            result.trace.dump();
            for violation in &result.violations {
                eprintln!("  VIOLATION: {violation}");
            }
            let min_prefix = minimize_prefix(seed, result.trace.entries.len().max(1));
            let artifact =
                run_action_seed(seed, steps, Some(replay_command(seed, min_prefix)), true);
            if let Some(bundle) = &artifact.artifact_bundle {
                eprintln!("  ARTIFACT: {}", bundle.display());
            }
            failures.push((seed, min_prefix, result.violations));
        } else {
            summaries.push(dst_support::SuccessSeedSummary {
                seed,
                steps_executed: result.trace.entries.len(),
                warnings: result.warnings,
            });
        }
    }

    assert!(
        failures.is_empty(),
        "Action DST found failing seeds: {:?}. First replay: {}",
        failures
            .iter()
            .map(|(seed, prefix, _)| format!("seed={seed}, prefix={prefix}"))
            .collect::<Vec<_>>(),
        failures.first().map_or_else(
            || replay_command(BASE_SEED, steps),
            |(seed, prefix, _)| replay_command(*seed, *prefix)
        )
    );
}

#[test]
#[ignore = "Slow action-sequence sweep. Run with ACTION_DST_TRACES=32 cargo test --test action_workflow_dst -- --ignored --nocapture"]
fn dst_action_sequences_preserve_contracts_long_run() {
    let steps = step_limit().unwrap_or(14);
    let seeds: Vec<u64> = if let Some(seed) = single_seed() {
        vec![seed]
    } else {
        (0..trace_count(32))
            .map(|i| BASE_SEED.wrapping_add(i))
            .collect()
    };

    let mut failures = Vec::new();
    let mut summaries = Vec::new();

    for seed in seeds {
        let result = run_action_seed(seed, steps, None, false);
        if !result.violations.is_empty() {
            result.trace.dump();
            for violation in &result.violations {
                eprintln!("  VIOLATION: {violation}");
            }
            let min_prefix = minimize_prefix(seed, result.trace.entries.len().max(1));
            let artifact =
                run_action_seed(seed, steps, Some(replay_command(seed, min_prefix)), true);
            if let Some(bundle) = &artifact.artifact_bundle {
                eprintln!("  ARTIFACT: {}", bundle.display());
            }
            failures.push((seed, min_prefix, result.violations));
        } else {
            summaries.push(dst_support::SuccessSeedSummary {
                seed,
                steps_executed: result.trace.entries.len(),
                warnings: result.warnings,
            });
        }
    }

    if failures.is_empty() {
        let summary = dst_support::write_success_bundle(
            "action-workflow-dst",
            json!({
                "base_seed": BASE_SEED,
                "trace_count": summaries.len(),
                "mode": "long_run",
                "step_limit": steps,
            }),
            summaries,
        );
        eprintln!("Action DST success artifact: {}", summary.display());
    }

    assert!(
        failures.is_empty(),
        "Action DST long run found failing seeds: {:?}. First replay: {}",
        failures
            .iter()
            .map(|(seed, prefix, _)| format!("seed={seed}, prefix={prefix}"))
            .collect::<Vec<_>>(),
        failures.first().map_or_else(
            || replay_command(BASE_SEED, steps),
            |(seed, prefix, _)| replay_command(*seed, *prefix)
        )
    );
}
