//! Agent task scenarios and scoring rubric for UX friction evaluation.
//!
//! Each scenario tests a specific aspect of the agent experience with maw.
//! Scenarios are designed to be run by real Claude agents against /tmp repos
//! with no prior git knowledge — only directories, files, and JSON output.
//!
//! # Design Principles
//!
//! - **Non-leading prompts**: task descriptions tell agents *what* to do, not *how*.
//! - **Objective scoring**: rubric uses observable metrics (errors, retries, confusion).
//! - **Reproducible**: each scenario creates a deterministic initial state.
//! - **Zero VCS knowledge**: agents never need to understand git or VCS concepts.

use serde::{Deserialize, Serialize};

/// Target: average friction score across all scenarios must be ≤ this threshold.
pub const TARGET_AVERAGE_SCORE: f64 = 1.5;

/// Total number of defined scenarios.
pub const SCENARIO_COUNT: usize = 5;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Unique identifier for a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScenarioId {
    /// S1: basic single-agent lifecycle.
    BasicLifecycle,
    /// S2: multi-file edit in a single workspace.
    MultiFileEdit,
    /// S3: two-agent coordination (parallel workspaces, sequential merge).
    MultiAgent,
    /// S4: conflict detection and resolution.
    ConflictResolution,
    /// S5: read-only inspection (list, status, history).
    ReadOnlyInspection,
}

/// A complete scenario definition.
///
/// Uses `&'static` references for zero-allocation static definitions.
/// Only `Serialize` is derived — scenarios are defined in code, not loaded from files.
#[derive(Debug, Clone, Serialize)]
pub struct Scenario {
    /// Machine-readable id.
    pub id: ScenarioId,
    /// Human-readable short name.
    pub name: &'static str,
    /// What aspect of agent UX this scenario tests.
    pub tests: &'static str,
    /// Repository state that must exist before the agent starts.
    pub preconditions: Preconditions,
    /// The plain-English task prompt given to the agent.
    /// Must NOT mention git, branches, commits, or VCS concepts.
    pub task_prompt: &'static str,
    /// Observable outcomes that determine success.
    pub expected_outcomes: &'static [&'static str],
    /// Maw commands the agent is expected to use (for scoring reference,
    /// not shown to the agent).
    pub expected_commands: &'static [&'static str],
    /// Maximum number of maw commands an expert would need.
    pub optimal_command_count: u32,
}

/// Preconditions describe the repo state before the scenario starts.
#[derive(Debug, Clone, Serialize)]
pub struct Preconditions {
    /// Description of the initial repository state.
    pub repo_state: &'static str,
    /// Files that must exist in ws/default/ before the agent starts.
    pub seed_files: &'static [SeedFile],
    /// Workspaces that must exist (beyond default).
    pub existing_workspaces: &'static [WorkspaceSetup],
}

/// A file to seed into the repo before the scenario starts.
#[derive(Debug, Clone, Serialize)]
pub struct SeedFile {
    /// Path relative to workspace root (e.g., "src/main.rs").
    pub path: &'static str,
    /// File contents.
    pub content: &'static str,
}

/// A workspace to pre-create with optional file modifications.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSetup {
    /// Workspace name.
    pub name: &'static str,
    /// Files to create or modify in this workspace.
    pub files: &'static [SeedFile],
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Friction score on a 1–5 scale.
///
/// Scoring is based on *observable agent behavior*, not subjective judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum FrictionScore {
    /// Completed on first try with zero errors or retries.
    Perfect = 1,
    /// One recoverable error, self-corrected within 1 retry.
    Minor = 2,
    /// 2–3 retries or workarounds needed.
    Moderate = 3,
    /// Extensive trial-and-error (4+ retries or confusion markers).
    Difficult = 4,
    /// Could not complete the task.
    Failed = 5,
}

/// Raw metrics collected from an agent transcript.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunMetrics {
    /// Total tool invocations (Bash, Read, Write, Edit, etc.).
    pub tool_calls: u32,
    /// Maw commands specifically (subset of `tool_calls`).
    pub maw_commands: u32,
    /// Commands that returned non-zero exit codes.
    pub errors: u32,
    /// Repeated identical commands (sign of confusion).
    pub retries: u32,
    /// Confusion markers: "not sure", "let me try again", backtracking, etc.
    pub confusion_markers: u32,
    /// Whether the agent achieved all expected outcomes.
    pub goal_achieved: bool,
    /// Commands spent on recovery (after first error, not forward progress).
    pub recovery_steps: u32,
}

/// Result of scoring a single scenario run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Which scenario was run.
    pub scenario_id: ScenarioId,
    /// Raw metrics from the run.
    pub metrics: RunMetrics,
    /// Computed friction score.
    pub score: FrictionScore,
    /// Whether this scenario passed (score ≤ 2).
    pub passed: bool,
}

/// Aggregate result across all scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    /// Individual scenario results.
    pub results: Vec<ScenarioResult>,
    /// Average friction score across all scenarios.
    pub average_score: f64,
    /// Whether the overall eval passed (average ≤ `TARGET_AVERAGE_SCORE`).
    pub passed: bool,
}

impl RunMetrics {
    /// Compute the friction score from raw metrics.
    #[must_use]
    pub const fn score(&self) -> FrictionScore {
        if !self.goal_achieved {
            return FrictionScore::Failed;
        }
        if self.errors == 0 && self.retries == 0 && self.confusion_markers == 0 {
            return FrictionScore::Perfect;
        }
        if self.errors <= 1 && self.retries <= 1 && self.confusion_markers <= 1 {
            return FrictionScore::Minor;
        }
        if self.retries <= 3 && self.confusion_markers <= 3 {
            return FrictionScore::Moderate;
        }
        FrictionScore::Difficult
    }
}

impl FrictionScore {
    /// Numeric value (1–5) for averaging.
    #[must_use]
    pub const fn value(self) -> u8 {
        self as u8
    }
}

impl EvalReport {
    /// Build an eval report from individual scenario results.
    #[must_use]
    pub fn from_results(results: Vec<ScenarioResult>) -> Self {
        let sum: u32 = results.iter().map(|r| u32::from(r.score.value())).sum();
        let count = u32::try_from(results.len().max(1)).unwrap_or(u32::MAX);
        let average_score = f64::from(sum) / f64::from(count);
        let passed = average_score <= TARGET_AVERAGE_SCORE;
        Self {
            results,
            average_score,
            passed,
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario definitions
// ---------------------------------------------------------------------------

/// Return all 5 defined scenarios.
#[must_use]
pub const fn all_scenarios() -> [Scenario; SCENARIO_COUNT] {
    [
        scenario_basic_lifecycle(),
        scenario_multi_file_edit(),
        scenario_multi_agent(),
        scenario_conflict_resolution(),
        scenario_read_only_inspection(),
    ]
}

/// S1: Basic single-agent lifecycle.
///
/// Tests the minimal happy path: create workspace, add a file, merge.
/// This is the simplest possible agent interaction with maw.
#[must_use]
pub const fn scenario_basic_lifecycle() -> Scenario {
    Scenario {
        id: ScenarioId::BasicLifecycle,
        name: "basic-lifecycle",
        tests: "Minimal lifecycle: create workspace, add file, merge, verify",
        preconditions: Preconditions {
            repo_state:
                "Fresh maw repo with a seed Rust project (Cargo.toml, src/main.rs, src/lib.rs)",
            seed_files: &[
                SeedFile {
                    path: "Cargo.toml",
                    content: concat!(
                        "[package]\n",
                        "name = \"agent-eval\"\n",
                        "version = \"0.1.0\"\n",
                        "edition = \"2021\"\n",
                        "\n",
                        "[dependencies]\n",
                    ),
                },
                SeedFile {
                    path: "src/main.rs",
                    content: "fn main() {\n    println!(\"hello from eval\");\n}\n",
                },
                SeedFile {
                    path: "src/lib.rs",
                    content: "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
                },
            ],
            existing_workspaces: &[],
        },
        task_prompt: concat!(
            "You are working on a Rust project managed by maw.\n",
            "\n",
            "Task:\n",
            "1. Create a workspace named \"agent-1\".\n",
            "2. Add a new file src/hello.rs containing:\n",
            "     pub fn hello() -> &'static str { \"hello\" }\n",
            "3. Merge workspace agent-1 back (destroy the workspace after merge).\n",
            "4. Confirm that src/hello.rs exists in the main workspace (ws/default/).\n",
            "\n",
            "Use only maw commands and file operations. Do not use git directly.\n",
            "Use absolute paths for all file operations.\n",
        ),
        expected_outcomes: &[
            "src/hello.rs exists in ws/default/ with correct content",
            "workspace agent-1 no longer exists (destroyed)",
            "no git commands were used by the agent",
        ],
        expected_commands: &[
            "maw ws create agent-1",
            "write src/hello.rs (file operation)",
            "maw ws merge agent-1 --destroy",
        ],
        optimal_command_count: 3,
    }
}

/// S2: Multi-file edit in a single workspace.
///
/// Tests editing multiple existing files and adding new ones, then merging.
/// More realistic than S1 — agents commonly modify several files per task.
#[must_use]
pub const fn scenario_multi_file_edit() -> Scenario {
    Scenario {
        id: ScenarioId::MultiFileEdit,
        name: "multi-file-edit",
        tests: "Multiple file edits in one workspace: modify existing, add new, merge",
        preconditions: Preconditions {
            repo_state: "Rust project with src/main.rs, src/lib.rs, src/utils.rs",
            seed_files: &[
                SeedFile {
                    path: "Cargo.toml",
                    content: concat!(
                        "[package]\n",
                        "name = \"agent-eval\"\n",
                        "version = \"0.1.0\"\n",
                        "edition = \"2021\"\n",
                        "\n",
                        "[dependencies]\n",
                    ),
                },
                SeedFile {
                    path: "src/main.rs",
                    content: concat!(
                        "mod utils;\n",
                        "\n",
                        "fn main() {\n",
                        "    let result = utils::format_greeting(\"world\");\n",
                        "    println!(\"{result}\");\n",
                        "}\n",
                    ),
                },
                SeedFile {
                    path: "src/lib.rs",
                    content: "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
                },
                SeedFile {
                    path: "src/utils.rs",
                    content: concat!(
                        "pub fn format_greeting(name: &str) -> String {\n",
                        "    format!(\"Hello, {name}!\")\n",
                        "}\n",
                    ),
                },
            ],
            existing_workspaces: &[],
        },
        task_prompt: concat!(
            "You are working on a Rust project managed by maw.\n",
            "\n",
            "Task:\n",
            "1. Create a workspace named \"feature-work\".\n",
            "2. In the workspace, make these changes:\n",
            "   a. Modify src/lib.rs: add a new function `pub fn multiply(a: i32, b: i32) -> i32 { a * b }`\n",
            "   b. Modify src/utils.rs: add a new function `pub fn format_farewell(name: &str) -> String { format!(\"Goodbye, {name}!\") }`\n",
            "   c. Add a new file src/config.rs with: `pub const VERSION: &str = \"1.0.0\";`\n",
            "3. Merge workspace feature-work back (destroy after merge).\n",
            "4. Confirm all three changes are present in the main workspace.\n",
            "\n",
            "Use only maw commands and file operations. Do not use git directly.\n",
            "Use absolute paths for all file operations.\n",
        ),
        expected_outcomes: &[
            "src/lib.rs contains multiply function in ws/default/",
            "src/utils.rs contains format_farewell function in ws/default/",
            "src/config.rs exists with VERSION constant in ws/default/",
            "workspace feature-work no longer exists",
        ],
        expected_commands: &[
            "maw ws create feature-work",
            "edit src/lib.rs (add multiply)",
            "edit src/utils.rs (add format_farewell)",
            "write src/config.rs",
            "maw ws merge feature-work --destroy",
        ],
        optimal_command_count: 5,
    }
}

/// S3: Two-agent coordination.
///
/// Tests the multi-workspace workflow: one workspace already has changes,
/// agent creates a second workspace, edits different files, and merges both.
/// This validates that agents can reason about parallel workspaces.
#[must_use]
pub const fn scenario_multi_agent() -> Scenario {
    Scenario {
        id: ScenarioId::MultiAgent,
        name: "multi-agent",
        tests: "Two workspaces with non-overlapping edits, sequential merge",
        preconditions: Preconditions {
            repo_state: concat!(
                "Rust project with src/auth.rs, src/api.rs. ",
                "Workspace 'agent-1' already exists with modifications to src/auth.rs.",
            ),
            seed_files: &[
                SeedFile {
                    path: "Cargo.toml",
                    content: concat!(
                        "[package]\n",
                        "name = \"agent-eval\"\n",
                        "version = \"0.1.0\"\n",
                        "edition = \"2021\"\n",
                        "\n",
                        "[dependencies]\n",
                    ),
                },
                SeedFile {
                    path: "src/main.rs",
                    content: "fn main() {}\n",
                },
                SeedFile {
                    path: "src/auth.rs",
                    content: concat!(
                        "pub fn authenticate(user: &str) -> bool {\n",
                        "    user == \"admin\"\n",
                        "}\n",
                    ),
                },
                SeedFile {
                    path: "src/api.rs",
                    content: concat!(
                        "pub fn handle_request(path: &str) -> String {\n",
                        "    format!(\"OK: {path}\")\n",
                        "}\n",
                    ),
                },
            ],
            existing_workspaces: &[WorkspaceSetup {
                name: "agent-1",
                files: &[SeedFile {
                    path: "src/auth.rs",
                    content: concat!(
                        "pub fn authenticate(user: &str) -> bool {\n",
                        "    user == \"admin\" || user == \"root\"\n",
                        "}\n",
                        "\n",
                        "pub fn is_admin(user: &str) -> bool {\n",
                        "    user == \"admin\"\n",
                        "}\n",
                    ),
                }],
            }],
        },
        task_prompt: concat!(
            "You are agent-2 working on a Rust project managed by maw.\n",
            "Another agent (agent-1) has already made changes in a workspace named \"agent-1\".\n",
            "Agent-1 modified src/auth.rs (you don't need to know the details).\n",
            "\n",
            "Task:\n",
            "1. Create a workspace named \"agent-2\".\n",
            "2. In your workspace, modify src/api.rs: add a new function\n",
            "   `pub fn handle_error(code: u16) -> String { format!(\"Error: {code}\") }`\n",
            "3. Merge BOTH workspaces (agent-1 and agent-2) back, destroying them.\n",
            "4. Confirm that both sets of changes are present in the main workspace:\n",
            "   - src/auth.rs should contain an is_admin function\n",
            "   - src/api.rs should contain a handle_error function\n",
            "\n",
            "Use only maw commands and file operations. Do not use git directly.\n",
            "Use absolute paths for all file operations.\n",
        ),
        expected_outcomes: &[
            "src/auth.rs contains is_admin function in ws/default/",
            "src/api.rs contains handle_error function in ws/default/",
            "workspace agent-1 no longer exists",
            "workspace agent-2 no longer exists",
        ],
        expected_commands: &[
            "maw ws create agent-2",
            "edit src/api.rs (add handle_error)",
            "maw ws merge agent-1 --destroy",
            "maw ws merge agent-2 --destroy",
        ],
        optimal_command_count: 4,
    }
}

/// S4: Conflict detection and resolution.
///
/// Tests the conflict handling workflow: two workspaces modify the same file,
/// merge produces a conflict, agent must resolve it. This is the hardest
/// scenario and validates error message quality.
#[must_use]
pub const fn scenario_conflict_resolution() -> Scenario {
    Scenario {
        id: ScenarioId::ConflictResolution,
        name: "conflict-resolution",
        tests: "Same-file conflict: detect, inspect, resolve, merge",
        preconditions: Preconditions {
            repo_state: concat!(
                "Rust project with src/lib.rs. Two workspaces (left, right) both modify src/lib.rs. ",
                "Workspace 'left' changes the add function body. ",
                "Workspace 'right' also changes the add function body differently.",
            ),
            seed_files: &[
                SeedFile {
                    path: "Cargo.toml",
                    content: concat!(
                        "[package]\n",
                        "name = \"agent-eval\"\n",
                        "version = \"0.1.0\"\n",
                        "edition = \"2021\"\n",
                        "\n",
                        "[dependencies]\n",
                    ),
                },
                SeedFile {
                    path: "src/main.rs",
                    content: "fn main() {}\n",
                },
                SeedFile {
                    path: "src/lib.rs",
                    content: concat!(
                        "pub fn add(a: i32, b: i32) -> i32 {\n",
                        "    a + b\n",
                        "}\n",
                    ),
                },
            ],
            existing_workspaces: &[
                WorkspaceSetup {
                    name: "left",
                    files: &[SeedFile {
                        path: "src/lib.rs",
                        content: concat!(
                            "pub fn add(a: i32, b: i32) -> i32 {\n",
                            "    let result = a + b;\n",
                            "    assert!(result >= a.min(b), \"overflow\");\n",
                            "    result\n",
                            "}\n",
                        ),
                    }],
                },
                WorkspaceSetup {
                    name: "right",
                    files: &[SeedFile {
                        path: "src/lib.rs",
                        content: concat!(
                            "pub fn add(a: i32, b: i32) -> i32 {\n",
                            "    a.checked_add(b).expect(\"overflow\")\n",
                            "}\n",
                        ),
                    }],
                },
            ],
        },
        task_prompt: concat!(
            "You are working on a Rust project managed by maw.\n",
            "Two workspaces ('left' and 'right') both modified src/lib.rs.\n",
            "\n",
            "Task:\n",
            "1. Try merging workspace 'left' — this should succeed.\n",
            "2. Try merging workspace 'right' — this may produce a conflict.\n",
            "3. If there is a conflict:\n",
            "   a. Inspect the conflicted file to understand both sides.\n",
            "   b. Resolve the conflict by keeping the checked_add approach from 'right'\n",
            "      (it's the safer implementation).\n",
            "   c. Complete the merge.\n",
            "4. Confirm src/lib.rs in the main workspace uses checked_add.\n",
            "5. Destroy both workspaces if not already destroyed.\n",
            "\n",
            "Use only maw commands and file operations. Do not use git directly.\n",
            "Use absolute paths for all file operations.\n",
        ),
        expected_outcomes: &[
            "src/lib.rs in ws/default/ uses checked_add",
            "workspace left no longer exists",
            "workspace right no longer exists",
            "conflict was detected and resolved (not silently dropped)",
        ],
        expected_commands: &[
            "maw ws merge left --destroy",
            "maw ws merge right (conflict detected)",
            "read/inspect conflicted file",
            "edit src/lib.rs (resolve conflict)",
            "maw ws merge right --destroy (or complete merge)",
        ],
        optimal_command_count: 5,
    }
}

/// S5: Read-only inspection.
///
/// Tests the observation/inspection workflow: list workspaces, check status,
/// get history. This validates that agents can gather information without
/// modifying state — essential for debugging and coordination.
#[must_use]
pub const fn scenario_read_only_inspection() -> Scenario {
    Scenario {
        id: ScenarioId::ReadOnlyInspection,
        name: "read-only-inspection",
        tests: "Inspect workspace state: list, status, files — without modifying anything",
        preconditions: Preconditions {
            repo_state: concat!(
                "Rust project with two active workspaces. ",
                "Workspace 'alice' has modified src/lib.rs (dirty). ",
                "Workspace 'bob' has no modifications (clean).",
            ),
            seed_files: &[
                SeedFile {
                    path: "Cargo.toml",
                    content: concat!(
                        "[package]\n",
                        "name = \"agent-eval\"\n",
                        "version = \"0.1.0\"\n",
                        "edition = \"2021\"\n",
                        "\n",
                        "[dependencies]\n",
                    ),
                },
                SeedFile {
                    path: "src/main.rs",
                    content: "fn main() {}\n",
                },
                SeedFile {
                    path: "src/lib.rs",
                    content: "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
                },
            ],
            existing_workspaces: &[
                WorkspaceSetup {
                    name: "alice",
                    files: &[SeedFile {
                        path: "src/lib.rs",
                        content: concat!(
                            "pub fn add(a: i32, b: i32) -> i32 {\n",
                            "    a + b\n",
                            "}\n",
                            "\n",
                            "pub fn subtract(a: i32, b: i32) -> i32 {\n",
                            "    a - b\n",
                            "}\n",
                        ),
                    }],
                },
                WorkspaceSetup {
                    name: "bob",
                    files: &[],
                },
            ],
        },
        task_prompt: concat!(
            "You are a lead agent inspecting the state of a Rust project managed by maw.\n",
            "DO NOT modify any files or merge anything — this is read-only inspection.\n",
            "\n",
            "Task:\n",
            "1. List all workspaces. Report how many exist and their names.\n",
            "2. Check the status of workspace 'alice'. Report whether it has dirty files.\n",
            "3. Check the status of workspace 'bob'. Report whether it has dirty files.\n",
            "4. Read the file src/lib.rs in alice's workspace. Report what function(s) it contains.\n",
            "5. Read the file src/lib.rs in bob's workspace. Report what function(s) it contains.\n",
            "\n",
            "Output your findings as a structured summary.\n",
            "\n",
            "Use only maw commands and file read operations. Do not use git directly.\n",
            "Do NOT merge, create, or destroy any workspaces.\n",
        ),
        expected_outcomes: &[
            "agent lists 3 workspaces: default, alice, bob",
            "agent reports alice has dirty files (src/lib.rs modified)",
            "agent reports bob has no dirty files (clean)",
            "agent reports alice's lib.rs has add and subtract functions",
            "agent reports bob's lib.rs has only the add function",
            "no workspaces were created, destroyed, or merged",
        ],
        expected_commands: &[
            "maw ws list",
            "maw ws status alice",
            "maw ws status bob",
            "read ws/alice/src/lib.rs",
            "read ws/bob/src/lib.rs",
        ],
        optimal_command_count: 5,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_scenarios_returns_five() {
        assert_eq!(all_scenarios().len(), SCENARIO_COUNT);
    }

    #[test]
    fn scenario_ids_are_unique() {
        let scenarios = all_scenarios();
        let mut ids: Vec<_> = scenarios.iter().map(|s| s.id).collect();
        let original_len = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "duplicate scenario IDs detected");
    }

    #[test]
    fn scenario_names_are_unique() {
        let scenarios = all_scenarios();
        let mut names: Vec<_> = scenarios.iter().map(|s| s.name).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate scenario names detected"
        );
    }

    #[test]
    fn each_scenario_has_nonempty_fields() {
        for s in &all_scenarios() {
            assert!(!s.name.is_empty(), "{:?} has empty name", s.id);
            assert!(
                !s.tests.is_empty(),
                "{:?} has empty tests description",
                s.id
            );
            assert!(
                !s.task_prompt.is_empty(),
                "{:?} has empty task_prompt",
                s.id
            );
            assert!(
                !s.expected_outcomes.is_empty(),
                "{:?} has no expected outcomes",
                s.id
            );
            assert!(
                !s.expected_commands.is_empty(),
                "{:?} has no expected commands",
                s.id
            );
            assert!(
                s.optimal_command_count > 0,
                "{:?} has zero optimal command count",
                s.id
            );
        }
    }

    #[test]
    fn each_scenario_has_seed_files() {
        for s in &all_scenarios() {
            assert!(
                !s.preconditions.seed_files.is_empty(),
                "{:?} has no seed files",
                s.id
            );
        }
    }

    #[test]
    fn conflict_scenario_has_two_workspaces() {
        let conflict = scenario_conflict_resolution();
        assert_eq!(
            conflict.preconditions.existing_workspaces.len(),
            2,
            "conflict scenario must have exactly 2 pre-existing workspaces"
        );
    }

    #[test]
    fn read_only_scenario_has_two_workspaces() {
        let readonly = scenario_read_only_inspection();
        assert_eq!(
            readonly.preconditions.existing_workspaces.len(),
            2,
            "read-only scenario must have exactly 2 pre-existing workspaces"
        );
    }

    #[test]
    fn conflict_and_readonly_scenarios_present() {
        let scenarios = all_scenarios();
        let has_conflict = scenarios
            .iter()
            .any(|s| s.id == ScenarioId::ConflictResolution);
        let has_readonly = scenarios
            .iter()
            .any(|s| s.id == ScenarioId::ReadOnlyInspection);
        assert!(has_conflict, "must have a conflict handling scenario");
        assert!(has_readonly, "must have a read-only inspection scenario");
    }

    #[test]
    fn task_prompts_do_not_mention_vcs() {
        let forbidden = ["git ", "branch", "commit", "checkout", "rebase"];
        for s in &all_scenarios() {
            for word in &forbidden {
                // Allow "git" only in the "Do not use git" instruction
                let prompt_lower = s.task_prompt.to_lowercase();
                let occurrences: Vec<_> = prompt_lower.match_indices(word).collect();
                for (idx, _) in &occurrences {
                    // Check context: it's OK if it's in "do not use git"
                    let start = idx.saturating_sub(20);
                    let context = &prompt_lower[start..prompt_lower.len().min(idx + 30)];
                    assert!(
                        context.contains("do not use") || context.contains("don't use"),
                        "{:?} task prompt mentions '{}' outside of prohibition context: ...{}...",
                        s.id,
                        word.trim(),
                        context,
                    );
                }
            }
        }
    }

    #[test]
    fn target_threshold_is_encoded() {
        assert!(
            TARGET_AVERAGE_SCORE > 0.0 && TARGET_AVERAGE_SCORE <= 5.0,
            "target must be between 0 and 5"
        );
        // The bead specifies ≤ 1.5
        assert!(
            (TARGET_AVERAGE_SCORE - 1.5).abs() < f64::EPSILON,
            "target must be 1.5 per bead spec"
        );
    }

    // --- Scoring tests ---

    #[test]
    fn perfect_run_scores_1() {
        let metrics = RunMetrics {
            tool_calls: 5,
            maw_commands: 3,
            errors: 0,
            retries: 0,
            confusion_markers: 0,
            goal_achieved: true,
            recovery_steps: 0,
        };
        assert_eq!(metrics.score(), FrictionScore::Perfect);
        assert_eq!(metrics.score().value(), 1);
    }

    #[test]
    fn minor_error_scores_2() {
        let metrics = RunMetrics {
            tool_calls: 7,
            maw_commands: 4,
            errors: 1,
            retries: 0,
            confusion_markers: 0,
            goal_achieved: true,
            recovery_steps: 1,
        };
        assert_eq!(metrics.score(), FrictionScore::Minor);
        assert_eq!(metrics.score().value(), 2);
    }

    #[test]
    fn moderate_difficulty_scores_3() {
        let metrics = RunMetrics {
            tool_calls: 12,
            maw_commands: 6,
            errors: 2,
            retries: 3,
            confusion_markers: 2,
            goal_achieved: true,
            recovery_steps: 4,
        };
        assert_eq!(metrics.score(), FrictionScore::Moderate);
        assert_eq!(metrics.score().value(), 3);
    }

    #[test]
    fn difficult_scores_4() {
        let metrics = RunMetrics {
            tool_calls: 20,
            maw_commands: 10,
            errors: 5,
            retries: 6,
            confusion_markers: 4,
            goal_achieved: true,
            recovery_steps: 8,
        };
        assert_eq!(metrics.score(), FrictionScore::Difficult);
        assert_eq!(metrics.score().value(), 4);
    }

    #[test]
    fn failed_run_scores_5() {
        let metrics = RunMetrics {
            tool_calls: 15,
            maw_commands: 8,
            errors: 3,
            retries: 2,
            confusion_markers: 1,
            goal_achieved: false,
            recovery_steps: 5,
        };
        assert_eq!(metrics.score(), FrictionScore::Failed);
        assert_eq!(metrics.score().value(), 5);
    }

    #[test]
    fn eval_report_passes_below_threshold() {
        let results = vec![
            ScenarioResult {
                scenario_id: ScenarioId::BasicLifecycle,
                metrics: RunMetrics {
                    goal_achieved: true,
                    ..Default::default()
                },
                score: FrictionScore::Perfect,
                passed: true,
            },
            ScenarioResult {
                scenario_id: ScenarioId::MultiFileEdit,
                metrics: RunMetrics {
                    goal_achieved: true,
                    errors: 1,
                    ..Default::default()
                },
                score: FrictionScore::Minor,
                passed: true,
            },
        ];
        let report = EvalReport::from_results(results);
        assert_eq!(report.average_score, 1.5);
        assert!(report.passed);
    }

    #[test]
    fn eval_report_fails_above_threshold() {
        let results = vec![
            ScenarioResult {
                scenario_id: ScenarioId::BasicLifecycle,
                metrics: RunMetrics {
                    goal_achieved: true,
                    ..Default::default()
                },
                score: FrictionScore::Perfect,
                passed: true,
            },
            ScenarioResult {
                scenario_id: ScenarioId::ConflictResolution,
                metrics: RunMetrics {
                    goal_achieved: true,
                    errors: 3,
                    retries: 4,
                    confusion_markers: 5,
                    ..Default::default()
                },
                score: FrictionScore::Difficult,
                passed: false,
            },
        ];
        let report = EvalReport::from_results(results);
        assert_eq!(report.average_score, 2.5);
        assert!(!report.passed);
    }

    #[test]
    fn scenarios_serialize_to_json() {
        let scenarios = all_scenarios();
        for s in &scenarios {
            let json = serde_json::to_string(s).expect("scenario should serialize");
            assert!(!json.is_empty());
            // Verify it's valid JSON by parsing to Value
            let value: serde_json::Value =
                serde_json::from_str(&json).expect("should be valid JSON");
            assert!(value.is_object(), "scenario JSON should be an object");
            assert!(
                value.get("id").is_some(),
                "{:?} JSON missing 'id' field",
                s.id
            );
            assert!(
                value.get("task_prompt").is_some(),
                "{:?} JSON missing 'task_prompt' field",
                s.id
            );
        }
    }

    #[test]
    fn scoring_rubric_is_monotonic() {
        // Scores 1..5 must be strictly ordered
        assert!(FrictionScore::Perfect < FrictionScore::Minor);
        assert!(FrictionScore::Minor < FrictionScore::Moderate);
        assert!(FrictionScore::Moderate < FrictionScore::Difficult);
        assert!(FrictionScore::Difficult < FrictionScore::Failed);
    }
}
