use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::format::OutputFormat;
use crate::workspace;

#[derive(Subcommand)]
pub enum DevCommands {
    /// Deterministic simulation helpers for maw repo/workspace harnesses
    #[command(subcommand)]
    Sim(SimCommands),
}

#[derive(Subcommand)]
pub enum SimCommands {
    /// Run deterministic simulation campaigns for maw repo/workspace harnesses
    Run(RunArgs),

    /// Inspect a DST success or failure bundle without replaying it
    Inspect(InspectArgs),

    /// Replay a deterministic simulation failure from a bundle or explicit seed
    Replay(ReplayArgs),

    /// Minimize a failing action-sequence seed to the smallest failing prefix
    Shrink(ShrinkArgs),
}

#[derive(Args)]
pub struct RunArgs {
    /// Which deterministic simulation harness to run
    #[arg(long, value_enum, default_value = "all")]
    harness: RunHarness,

    /// Number of seeds to execute in the long-run sweep
    #[arg(long)]
    seeds: Option<u64>,

    /// Step limit for the action harness
    #[arg(long)]
    steps: Option<usize>,

    /// Print the generated command(s) without executing them
    #[arg(long)]
    print_only: bool,

    /// Override execution directory
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Output format: text (default) or json
    #[arg(long)]
    format: Option<OutputFormat>,

    /// Shorthand for --format json
    #[arg(long, hide = true, conflicts_with = "format")]
    json: bool,
}

#[derive(Args)]
pub struct InspectArgs {
    /// DST success or failure bundle JSON
    bundle: Option<PathBuf>,

    /// Inspect the newest artifact bundle from the default DST artifact root
    #[arg(long, conflicts_with = "bundle")]
    latest: bool,

    /// Limit latest-artifact lookup to one harness
    #[arg(long, value_enum, requires = "latest")]
    harness: Option<SimHarness>,

    /// Output format: text (default) or json
    #[arg(long)]
    format: Option<OutputFormat>,

    /// Shorthand for --format json
    #[arg(long, hide = true, conflicts_with = "format")]
    json: bool,
}

#[derive(Args)]
pub struct ReplayArgs {
    /// DST failure bundle JSON produced under /tmp/maw-dst-artifacts
    #[arg(long, conflicts_with_all = ["harness", "seed", "steps"])]
    bundle: Option<PathBuf>,

    /// Replay an explicit harness seed instead of loading a bundle
    #[arg(long, value_enum, requires = "seed")]
    harness: Option<SimHarness>,

    /// Seed to replay in explicit mode
    #[arg(long, requires = "harness")]
    seed: Option<u64>,

    /// Step/prefix limit for action-harness replay
    #[arg(long)]
    steps: Option<usize>,

    /// Use the full replay command from a bundle instead of the minimized replay
    #[arg(long, requires = "bundle")]
    full: bool,

    /// Print the replay command without executing it
    #[arg(long)]
    print_only: bool,

    /// Override execution directory for replay commands
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Output format: text (default) or json
    #[arg(long)]
    format: Option<OutputFormat>,

    /// Shorthand for --format json
    #[arg(long, hide = true, conflicts_with = "format")]
    json: bool,
}

#[derive(Args)]
pub struct ShrinkArgs {
    /// DST failure bundle JSON produced under /tmp/maw-dst-artifacts
    #[arg(long, conflicts_with_all = ["seed", "max_steps"])]
    bundle: Option<PathBuf>,

    /// Action-harness seed to shrink
    #[arg(long, requires = "max_steps")]
    seed: Option<u64>,

    /// Largest action prefix to test while shrinking
    #[arg(long, requires = "seed")]
    max_steps: Option<usize>,

    /// Print the minimized replay command without executing it
    #[arg(long)]
    print_only: bool,

    /// Override execution directory
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Output format: text (default) or json
    #[arg(long)]
    format: Option<OutputFormat>,

    /// Shorthand for --format json
    #[arg(long, hide = true, conflicts_with = "format")]
    json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SimHarness {
    Workflow,
    Action,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RunHarness {
    Workflow,
    Action,
    All,
}

#[derive(Deserialize)]
struct ReplayBundle {
    harness: String,
    seed: u64,
    replay_command: String,
    minimized_replay_command: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct SuccessBundle {
    harness: String,
    settings: serde_json::Value,
    seeds: Vec<SuccessSeedSummary>,
}

#[derive(Deserialize, Serialize)]
struct SuccessSeedSummary {
    seed: u64,
    steps_executed: usize,
    warnings: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct InspectFailureBundle {
    harness: String,
    seed: u64,
    replay_command: String,
    minimized_replay_command: Option<String>,
    trace: Vec<String>,
    violations: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct RunOutput {
    commands: Vec<String>,
    cwd: Option<String>,
    print_only: bool,
    results: Vec<RunCommandResult>,
}

#[derive(Serialize)]
struct RunCommandResult {
    harness: String,
    command: String,
    success: bool,
    status_code: Option<i32>,
    artifact_path: Option<String>,
}

#[derive(Serialize)]
struct ReplayOutput {
    command: String,
    cwd: Option<String>,
    print_only: bool,
}

#[derive(Serialize)]
struct ShrinkOutput {
    seed: u64,
    max_steps: usize,
    min_prefix: usize,
    replay_command: String,
    cwd: Option<String>,
    print_only: bool,
}

#[derive(Serialize)]
#[serde(tag = "bundle_type", rename_all = "kebab-case")]
enum InspectOutput {
    Failure {
        path: String,
        harness: String,
        seed: u64,
        replay_command: String,
        minimized_replay_command: Option<String>,
        trace_steps: usize,
        violation_count: usize,
        warning_count: usize,
    },
    Success {
        path: String,
        harness: String,
        seed_count: usize,
        settings: serde_json::Value,
        total_warning_count: usize,
    },
}

pub fn run(cmd: &DevCommands) -> Result<()> {
    match cmd {
        DevCommands::Sim(cmd) => run_sim(cmd),
    }
}

fn run_sim(cmd: &SimCommands) -> Result<()> {
    match cmd {
        SimCommands::Run(args) => run_campaign(args),
        SimCommands::Inspect(args) => inspect(args),
        SimCommands::Replay(args) => replay(args),
        SimCommands::Shrink(args) => shrink(args),
    }
}

fn run_campaign(args: &RunArgs) -> Result<()> {
    let commands = commands_for_run(args)?;
    let format = OutputFormat::with_json_flag(args.format, args.json).unwrap_or(OutputFormat::Text);
    let suggested_cwd = args
        .cwd
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| default_replay_cwd().ok());
    let harnesses = harness_sequence(args.harness);

    if format == OutputFormat::Json {
        if args.print_only {
            println!(
                "{}",
                serde_json::to_string_pretty(&RunOutput {
                    commands: commands.clone(),
                    cwd: suggested_cwd.as_ref().map(|p| p.display().to_string()),
                    print_only: true,
                    results: Vec::new(),
                })?
            );
            return Ok(());
        }

        let cwd = resolve_replay_cwd(args.cwd.as_deref())?;
        let before: Vec<Option<PathBuf>> = harnesses
            .iter()
            .map(|h| latest_artifact_for_harness(*h))
            .collect::<Result<_>>()?;
        let mut results = Vec::new();

        for (idx, command) in commands.iter().enumerate() {
            let out = Command::new("sh")
                .args(["-lc", command])
                .current_dir(&cwd)
                .output()
                .context("failed to execute simulation campaign")?;
            let after = latest_artifact_for_harness(harnesses[idx])?;
            let artifact_path = after.and_then(|path| {
                if before[idx].as_ref() == Some(&path) {
                    None
                } else {
                    Some(path.display().to_string())
                }
            });
            results.push(RunCommandResult {
                harness: harness_label(harnesses[idx]).to_string(),
                command: command.clone(),
                success: out.status.success(),
                status_code: out.status.code(),
                artifact_path,
            });

            if !out.status.success() {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&RunOutput {
                        commands: commands.clone(),
                        cwd: Some(cwd.display().to_string()),
                        print_only: false,
                        results,
                    })?
                );
                bail!(
                    "Simulation campaign exited with status {}.",
                    out.status
                        .code()
                        .map_or_else(|| "signal".to_string(), |code| code.to_string())
                );
            }
        }

        println!(
            "{}",
            serde_json::to_string_pretty(&RunOutput {
                commands,
                cwd: Some(cwd.display().to_string()),
                print_only: false,
                results,
            })?
        );
        if args.print_only {
            return Ok(());
        }
        return Ok(());
    }

    if args.print_only {
        println!("Deterministic simulation campaign commands:");
        for command in &commands {
            println!("  {command}");
        }
        if let Some(cwd) = args.cwd.as_deref() {
            println!("Run from: {}", cwd.display());
        } else if let Ok(cwd) = default_replay_cwd() {
            println!("Suggested run directory: {}", cwd.display());
        }
        println!("Next: rerun without --print-only to execute this campaign.");
        return Ok(());
    }

    let cwd = resolve_replay_cwd(args.cwd.as_deref())?;
    for command in &commands {
        println!("Running deterministic simulation campaign...");
        println!("  Directory: {}", cwd.display());
        println!("  Command:   {command}");
        let status = Command::new("sh")
            .args(["-lc", command])
            .current_dir(&cwd)
            .status()
            .context("failed to execute simulation campaign")?;
        if !status.success() {
            bail!(
                "Simulation campaign exited with status {}.\n  To inspect without executing:\n    maw dev sim run{} --print-only",
                status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string()),
                run_invocation_hint(args)
            );
        }
    }

    println!("Deterministic simulation campaigns completed successfully.");
    Ok(())
}

fn inspect(args: &InspectArgs) -> Result<()> {
    let format = OutputFormat::with_json_flag(args.format, args.json).unwrap_or(OutputFormat::Text);
    let bundle_path = resolve_inspect_bundle(args)?;
    let text = std::fs::read_to_string(&bundle_path)
        .with_context(|| format!("failed to read bundle {}", bundle_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("bundle {} is not valid JSON", bundle_path.display()))?;

    let out = if value.get("settings").is_some() && value.get("seeds").is_some() {
        let bundle: SuccessBundle = serde_json::from_value(value)?;
        let warning_count: usize = bundle.seeds.iter().map(|seed| seed.warnings.len()).sum();
        InspectOutput::Success {
            path: bundle_path.display().to_string(),
            harness: bundle.harness,
            seed_count: bundle.seeds.len(),
            settings: bundle.settings,
            total_warning_count: warning_count,
        }
    } else {
        let bundle: InspectFailureBundle = serde_json::from_value(value)?;
        InspectOutput::Failure {
            path: bundle_path.display().to_string(),
            harness: bundle.harness,
            seed: bundle.seed,
            replay_command: bundle.replay_command,
            minimized_replay_command: bundle.minimized_replay_command,
            trace_steps: bundle.trace.len(),
            violation_count: bundle.violations.len(),
            warning_count: bundle.warnings.len(),
        }
    };

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match out {
        InspectOutput::Failure {
            path,
            harness,
            seed,
            replay_command,
            minimized_replay_command,
            trace_steps,
            violation_count,
            warning_count,
        } => {
            println!("Deterministic simulation failure bundle:");
            println!("  Path:        {path}");
            println!("  Harness:     {harness}");
            println!("  Seed:        {seed}");
            println!("  Trace steps: {trace_steps}");
            println!("  Violations:  {violation_count}");
            println!("  Warnings:    {warning_count}");
            println!("  Replay:      {replay_command}");
            if let Some(minimized) = minimized_replay_command {
                println!("  Min replay:  {minimized}");
            }
        }
        InspectOutput::Success {
            path,
            harness,
            seed_count,
            settings,
            total_warning_count,
        } => {
            println!("Deterministic simulation success bundle:");
            println!("  Path:         {path}");
            println!("  Harness:      {harness}");
            println!("  Seeds:        {seed_count}");
            println!("  Warnings:     {total_warning_count}");
            println!("  Settings:     {}", serde_json::to_string(&settings)?);
        }
    }

    Ok(())
}

fn replay(args: &ReplayArgs) -> Result<()> {
    let command = if let Some(bundle_path) = &args.bundle {
        command_from_bundle(bundle_path, args.full)?
    } else {
        command_from_explicit_args(args)?
    };
    let format = OutputFormat::with_json_flag(args.format, args.json).unwrap_or(OutputFormat::Text);
    let suggested_cwd = args
        .cwd
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| default_replay_cwd().ok());

    if format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ReplayOutput {
                command: command.clone(),
                cwd: suggested_cwd.as_ref().map(|p| p.display().to_string()),
                print_only: args.print_only,
            })?
        );
        if args.print_only {
            return Ok(());
        }
    }

    if args.print_only {
        println!("Deterministic simulation replay command:");
        println!("  {command}");
        if let Some(cwd) = args.cwd.as_deref() {
            println!("Run from: {}", cwd.display());
        } else if let Ok(cwd) = default_replay_cwd() {
            println!("Suggested run directory: {}", cwd.display());
        }
        println!("Next: rerun without --print-only to execute this replay.");
        return Ok(());
    }

    let cwd = resolve_replay_cwd(args.cwd.as_deref())?;
    println!("Replaying deterministic simulation...");
    println!("  Directory: {}", cwd.display());
    println!("  Command:   {command}");

    let status = Command::new("sh")
        .args(["-lc", &command])
        .current_dir(&cwd)
        .status()
        .context("failed to execute replay command")?;

    if status.success() {
        println!("Replay completed successfully.");
        return Ok(());
    }

    bail!(
        "Replay command exited with status {}. This usually means the failing seed reproduced successfully.\n  To inspect without executing:\n    maw dev sim replay {} --print-only",
        status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string()),
        replay_invocation_hint(args)
    )
}

fn shrink(args: &ShrinkArgs) -> Result<()> {
    let (seed, max_steps) = if let Some(bundle_path) = &args.bundle {
        action_seed_and_steps_from_bundle(bundle_path)?
    } else {
        let seed = args.seed.ok_or_else(|| {
            anyhow::anyhow!(
                "Shrink requires either --bundle <PATH> or --seed <SEED> --max-steps <N>."
            )
        })?;
        let max_steps = args.max_steps.expect("clap requires max_steps with seed");
        (seed, max_steps)
    };
    let format = OutputFormat::with_json_flag(args.format, args.json).unwrap_or(OutputFormat::Text);

    if args.print_only {
        let minimized = if let Some(bundle_path) = &args.bundle {
            action_seed_and_steps_from_bundle(bundle_path)?.1
        } else {
            max_steps
        };
        let command = action_replay_command(seed, minimized);
        if format == OutputFormat::Json {
            println!(
                "{}",
                serde_json::to_string_pretty(&ShrinkOutput {
                    seed,
                    max_steps,
                    min_prefix: minimized,
                    replay_command: command,
                    cwd: args.cwd.as_ref().map(|p| p.display().to_string()),
                    print_only: true,
                })?
            );
            return Ok(());
        }
        println!("Deterministic simulation shrink result:");
        println!("  Seed:         {seed}");
        println!("  Max steps:    {max_steps}");
        println!("  Min prefix:   {minimized}");
        println!("  Replay cmd:   {command}");
        println!("Next: rerun without --print-only to execute the minimized replay.");
        return Ok(());
    }

    let cwd = resolve_replay_cwd(args.cwd.as_deref())?;
    let minimized = minimize_action_prefix(&cwd, seed, max_steps)?;
    let command = action_replay_command(seed, minimized);

    if format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ShrinkOutput {
                seed,
                max_steps,
                min_prefix: minimized,
                replay_command: command.clone(),
                cwd: Some(cwd.display().to_string()),
                print_only: false,
            })?
        );
    }

    println!("Deterministic simulation shrink result:");
    println!("  Seed:         {seed}");
    println!("  Max steps:    {max_steps}");
    println!("  Min prefix:   {minimized}");
    println!("  Replay cmd:   {command}");

    let status = Command::new("sh")
        .args(["-lc", &command])
        .current_dir(&cwd)
        .status()
        .context("failed to execute minimized replay command")?;
    if status.success() {
        bail!(
            "Minimized replay unexpectedly passed.\n  To inspect without executing:\n    maw dev sim shrink {} --print-only",
            shrink_invocation_hint(args, seed, max_steps)
        );
    }

    println!("Minimized replay reproduced the failure as expected.");
    Ok(())
}

fn command_from_bundle(path: &Path, full: bool) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read bundle {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("bundle {} is not valid JSON", path.display()))?;

    if value.get("settings").is_some() && value.get("seeds").is_some() {
        bail!(
            "{} is a DST success summary, not a replayable failure bundle.\n  To replay a specific seed, use:\n    maw dev sim replay --harness <workflow|action> --seed <SEED> [--steps <PREFIX>]",
            path.display()
        );
    }

    let bundle: ReplayBundle = serde_json::from_value(value).with_context(|| {
        format!(
            "{} is not a recognized DST failure bundle (missing replay_command fields)",
            path.display()
        )
    })?;

    let command = if full {
        bundle.replay_command
    } else {
        bundle
            .minimized_replay_command
            .unwrap_or(bundle.replay_command)
    };

    if command.trim().is_empty() {
        bail!(
            "{} does not contain a replayable command.\n  To fix: regenerate the failure bundle from a failing DST seed.",
            path.display()
        );
    }

    let _ = (bundle.harness, bundle.seed);
    Ok(command)
}

fn command_from_explicit_args(args: &ReplayArgs) -> Result<String> {
    let harness = args.harness.ok_or_else(|| {
        anyhow::anyhow!(
            "Replay requires either --bundle <PATH> or --harness <workflow|action> --seed <SEED>."
        )
    })?;
    let seed = args.seed.expect("clap requires seed with harness");

    match harness {
        SimHarness::Workflow => {
            if args.steps.is_some() {
                bail!(
                    "--steps is only valid with --harness action.\n  To fix: remove --steps or choose --harness action."
                );
            }
            Ok(workflow_replay_command(seed))
        }
        SimHarness::Action => {
            let steps = args.steps.ok_or_else(|| {
                anyhow::anyhow!(
                    "Action replay requires --steps <PREFIX>.\n  To fix: pass the failing prefix from the artifact bundle or DST output."
                )
            })?;
            Ok(action_replay_command(seed, steps))
        }
    }
}

fn commands_for_run(args: &RunArgs) -> Result<Vec<String>> {
    let workflow_traces = args.seeds.unwrap_or(12);
    let action_traces = args.seeds.unwrap_or(12);
    let action_steps = args.steps.unwrap_or(14);

    let workflow = format!(
        "WORKFLOW_DST_TRACES={workflow_traces} cargo test -p maw-workspaces --test workflow_dst dst_seeded_workflows_preserve_contracts_long_run -- --ignored --nocapture"
    );
    let action = format!(
        "ACTION_DST_TRACES={action_traces} ACTION_DST_STEPS={action_steps} cargo test -p maw-workspaces --test action_workflow_dst dst_action_sequences_preserve_contracts_long_run -- --ignored --nocapture"
    );

    Ok(match args.harness {
        RunHarness::Workflow => vec![workflow],
        RunHarness::Action => vec![action],
        RunHarness::All => vec![workflow, action],
    })
}

fn harness_sequence(harness: RunHarness) -> Vec<RunHarness> {
    match harness {
        RunHarness::Workflow => vec![RunHarness::Workflow],
        RunHarness::Action => vec![RunHarness::Action],
        RunHarness::All => vec![RunHarness::Workflow, RunHarness::Action],
    }
}

fn harness_label(harness: RunHarness) -> &'static str {
    match harness {
        RunHarness::Workflow => "workflow",
        RunHarness::Action => "action",
        RunHarness::All => "all",
    }
}

fn harness_dir_name(harness: SimHarness) -> &'static str {
    match harness {
        SimHarness::Workflow => "workflow-dst",
        SimHarness::Action => "action-workflow-dst",
    }
}

fn artifact_root() -> PathBuf {
    env::var_os("DST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("maw-dst-artifacts"))
}

fn latest_artifact_for_harness(harness: RunHarness) -> Result<Option<PathBuf>> {
    let sim = match harness {
        RunHarness::Workflow => SimHarness::Workflow,
        RunHarness::Action => SimHarness::Action,
        RunHarness::All => return Ok(None),
    };
    latest_artifact_path(Some(sim))
}

fn latest_artifact_path(harness: Option<SimHarness>) -> Result<Option<PathBuf>> {
    let root = artifact_root();
    let dirs: Vec<PathBuf> = match harness {
        Some(h) => vec![root.join(harness_dir_name(h))],
        None => vec![
            root.join(harness_dir_name(SimHarness::Workflow)),
            root.join(harness_dir_name(SimHarness::Action)),
        ],
    };

    let mut latest: Option<(SystemTime, PathBuf)> = None;
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read artifact directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let candidate = if path.join("bundle.json").is_file() {
                path.join("bundle.json")
            } else if path.join("summary.json").is_file() {
                path.join("summary.json")
            } else {
                continue;
            };
            let modified = candidate
                .metadata()
                .and_then(|m| m.modified())
                .with_context(|| format!("failed to read metadata for {}", candidate.display()))?;
            let replace = latest
                .as_ref()
                .is_none_or(|(current, _)| modified > *current);
            if replace {
                latest = Some((modified, candidate));
            }
        }
    }
    Ok(latest.map(|(_, path)| path))
}

fn resolve_inspect_bundle(args: &InspectArgs) -> Result<PathBuf> {
    if let Some(bundle) = &args.bundle {
        return Ok(bundle.clone());
    }
    if args.latest {
        return latest_artifact_path(args.harness).and_then(|path| {
            path.ok_or_else(|| {
                anyhow::anyhow!(
                    "No DST artifacts found under {}.\n  To fix: run maw dev sim run first, or pass an explicit bundle path.",
                    artifact_root().display()
                )
            })
        });
    }
    bail!("Inspect requires either <bundle> or --latest [--harness <workflow|action>].")
}

fn workflow_replay_command(seed: u64) -> String {
    format!(
        "WORKFLOW_DST_SEED={seed} cargo test -p maw-workspaces --test workflow_dst dst_seeded_workflows_preserve_contracts -- --exact --nocapture"
    )
}

fn action_replay_command(seed: u64, steps: usize) -> String {
    format!(
        "ACTION_DST_SEED={seed} ACTION_DST_STEPS={steps} cargo test -p maw-workspaces --test action_workflow_dst dst_action_sequences_preserve_contracts -- --exact --nocapture"
    )
}

fn action_seed_and_steps_from_bundle(path: &Path) -> Result<(u64, usize)> {
    let command = command_from_bundle(path, false)?;
    parse_action_seed_and_steps(&command).ok_or_else(|| {
        anyhow::anyhow!(
            "{} does not contain an action replay command with ACTION_DST_SEED and ACTION_DST_STEPS.\n  To fix: use an action-workflow-dst failure bundle or pass --seed/--max-steps explicitly.",
            path.display()
        )
    })
}

fn parse_action_seed_and_steps(command: &str) -> Option<(u64, usize)> {
    let mut seed = None;
    let mut steps = None;
    for token in command.split_whitespace() {
        if let Some(value) = token.strip_prefix("ACTION_DST_SEED=") {
            seed = value.parse().ok();
        }
        if let Some(value) = token.strip_prefix("ACTION_DST_STEPS=") {
            steps = value.parse().ok();
        }
    }
    Some((seed?, steps?))
}

fn minimize_action_prefix(cwd: &Path, seed: u64, max_steps: usize) -> Result<usize> {
    for steps in 1..=max_steps {
        let command = action_replay_command(seed, steps);
        let status = Command::new("sh")
            .args(["-lc", &command])
            .current_dir(cwd)
            .status()
            .with_context(|| format!("failed to execute shrink probe for step prefix {steps}"))?;
        if !status.success() {
            return Ok(steps);
        }
    }
    bail!(
        "No failing prefix found up to {max_steps}.\n  To fix: verify the seed still reproduces, or increase --max-steps."
    )
}

fn replay_invocation_hint(args: &ReplayArgs) -> String {
    if let Some(bundle) = &args.bundle {
        let mut hint = format!("--bundle {}", bundle.display());
        if args.full {
            hint.push_str(" --full");
        }
        hint
    } else {
        let mut parts = Vec::new();
        if let Some(harness) = args.harness {
            let harness = match harness {
                SimHarness::Workflow => "workflow",
                SimHarness::Action => "action",
            };
            parts.push(format!("--harness {harness}"));
        }
        if let Some(seed) = args.seed {
            parts.push(format!("--seed {seed}"));
        }
        if let Some(steps) = args.steps {
            parts.push(format!("--steps {steps}"));
        }
        parts.join(" ")
    }
}

fn run_invocation_hint(args: &RunArgs) -> String {
    let mut parts = Vec::new();
    let harness = match args.harness {
        RunHarness::Workflow => "workflow",
        RunHarness::Action => "action",
        RunHarness::All => "all",
    };
    parts.push(format!(" --harness {harness}"));
    if let Some(seeds) = args.seeds {
        parts.push(format!(" --seeds {seeds}"));
    }
    if let Some(steps) = args.steps {
        parts.push(format!(" --steps {steps}"));
    }
    parts.concat()
}

fn shrink_invocation_hint(args: &ShrinkArgs, seed: u64, max_steps: usize) -> String {
    if let Some(bundle) = &args.bundle {
        format!("--bundle {}", bundle.display())
    } else {
        format!("--seed {seed} --max-steps {max_steps}")
    }
}

fn default_replay_cwd() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to determine current directory")?;
    if cwd.join("Cargo.toml").exists() {
        return Ok(cwd);
    }

    if let Ok(repo_root) = workspace::repo_root() {
        let default_ws = repo_root.join("ws").join("default");
        if default_ws.join("Cargo.toml").exists() {
            return Ok(default_ws);
        }
    }

    bail!(
        "Could not locate a Cargo workspace for deterministic simulation replay.\n  Run from ws/default or pass --cwd <PATH>."
    )
}

fn resolve_replay_cwd(override_path: Option<&Path>) -> Result<PathBuf> {
    let cwd = if let Some(path) = override_path {
        path.to_path_buf()
    } else {
        default_replay_cwd()?
    };

    if !cwd.join("Cargo.toml").exists() {
        bail!(
            "Replay directory {} does not contain Cargo.toml.\n  To fix: pass --cwd <repo-root-with-Cargo.toml> or run from ws/default.",
            cwd.display()
        );
    }
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{
        ReplayArgs, RunArgs, RunHarness, ShrinkArgs, action_seed_and_steps_from_bundle,
        command_from_bundle, command_from_explicit_args, commands_for_run,
        parse_action_seed_and_steps, shrink_invocation_hint,
    };

    #[test]
    fn bundle_uses_minimized_replay_by_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bundle.json");
        fs::write(
            &path,
            r#"{
              "harness": "action-workflow-dst",
              "seed": 7,
              "replay_command": "FULL",
              "minimized_replay_command": "MIN"
            }"#,
        )
        .unwrap();

        assert_eq!(command_from_bundle(&path, false).unwrap(), "MIN");
        assert_eq!(command_from_bundle(&path, true).unwrap(), "FULL");
    }

    #[test]
    fn explicit_action_replay_requires_steps() {
        let err = command_from_explicit_args(&ReplayArgs {
            bundle: None,
            harness: Some(super::SimHarness::Action),
            seed: Some(42),
            steps: None,
            full: false,
            print_only: true,
            cwd: None,
            format: None,
            json: false,
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("requires --steps"), "unexpected error: {err}");
    }

    #[test]
    fn run_all_builds_both_long_run_commands() {
        let commands = commands_for_run(&RunArgs {
            harness: RunHarness::All,
            seeds: Some(9),
            steps: Some(17),
            print_only: true,
            cwd: None,
            format: None,
            json: false,
        })
        .unwrap();

        assert_eq!(commands.len(), 2);
        assert!(commands[0].contains("WORKFLOW_DST_TRACES=9"));
        assert!(commands[1].contains("ACTION_DST_TRACES=9"));
        assert!(commands[1].contains("ACTION_DST_STEPS=17"));
    }

    #[test]
    fn parses_action_seed_and_steps_from_bundle_command() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bundle.json");
        fs::write(
            &path,
            r#"{
              "harness": "action-workflow-dst",
              "seed": 7,
              "replay_command": "ACTION_DST_SEED=7 ACTION_DST_STEPS=12 cargo test foo",
              "minimized_replay_command": "ACTION_DST_SEED=7 ACTION_DST_STEPS=5 cargo test foo"
            }"#,
        )
        .unwrap();

        assert_eq!(action_seed_and_steps_from_bundle(&path).unwrap(), (7, 5));
        assert_eq!(
            parse_action_seed_and_steps("ACTION_DST_SEED=3 ACTION_DST_STEPS=8 cargo test"),
            Some((3, 8))
        );
    }

    #[test]
    fn shrink_hint_prefers_bundle_when_present() {
        let args = ShrinkArgs {
            bundle: Some(PathBuf::from("/tmp/bundle.json")),
            seed: None,
            max_steps: None,
            print_only: true,
            cwd: None,
            format: None,
            json: false,
        };
        assert_eq!(
            shrink_invocation_hint(&args, 3, 9),
            "--bundle /tmp/bundle.json"
        );
    }
}
