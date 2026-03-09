use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;

use crate::workspace;

#[derive(Subcommand)]
pub enum DevCommands {
    /// Deterministic simulation helpers for maw repo/workspace harnesses
    #[command(subcommand)]
    Sim(SimCommands),
}

#[derive(Subcommand)]
pub enum SimCommands {
    /// Replay a deterministic simulation failure from a bundle or explicit seed
    Replay(ReplayArgs),
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SimHarness {
    Workflow,
    Action,
}

#[derive(Deserialize)]
struct ReplayBundle {
    harness: String,
    seed: u64,
    replay_command: String,
    minimized_replay_command: Option<String>,
}

pub fn run(cmd: &DevCommands) -> Result<()> {
    match cmd {
        DevCommands::Sim(cmd) => run_sim(cmd),
    }
}

fn run_sim(cmd: &SimCommands) -> Result<()> {
    match cmd {
        SimCommands::Replay(args) => replay(args),
    }
}

fn replay(args: &ReplayArgs) -> Result<()> {
    let command = if let Some(bundle_path) = &args.bundle {
        command_from_bundle(bundle_path, args.full)?
    } else {
        command_from_explicit_args(args)?
    };

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
            Ok(format!(
                "WORKFLOW_DST_SEED={seed} cargo test -p maw-workspaces --test workflow_dst dst_seeded_workflows_preserve_contracts -- --exact --nocapture"
            ))
        }
        SimHarness::Action => {
            let steps = args.steps.ok_or_else(|| {
                anyhow::anyhow!(
                    "Action replay requires --steps <PREFIX>.\n  To fix: pass the failing prefix from the artifact bundle or DST output."
                )
            })?;
            Ok(format!(
                "ACTION_DST_SEED={seed} ACTION_DST_STEPS={steps} cargo test -p maw-workspaces --test action_workflow_dst dst_action_sequences_preserve_contracts -- --exact --nocapture"
            ))
        }
    }
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

    use tempfile::tempdir;

    use super::{ReplayArgs, command_from_bundle, command_from_explicit_args};

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
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("requires --steps"), "unexpected error: {err}");
    }
}
