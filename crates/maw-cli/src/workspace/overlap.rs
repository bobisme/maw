use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::format::OutputFormat;
use maw_core::model::types::WorkspaceId;

use super::get_backend;
use super::touched::collect_touched_workspace;

#[derive(Debug, Serialize)]
struct OverlapOutput {
    workspace_a: String,
    workspace_b: String,
    touched_a_count: usize,
    touched_b_count: usize,
    overlap_count: usize,
    overlap_paths: Vec<String>,
    risk: &'static str,
}

pub fn overlap(ws1: &str, ws2: &str, format: OutputFormat) -> Result<()> {
    if ws1 == ws2 {
        bail!(
            "Workspace names must be different\n  Got: {ws1}\n  Fix: maw ws overlap <workspace-a> <workspace-b> --format json"
        );
    }

    let ws1_id = WorkspaceId::new(ws1)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{ws1}': {e}"))?;
    let ws2_id = WorkspaceId::new(ws2)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{ws2}': {e}"))?;

    let backend = get_backend()?;
    let touched_a = collect_touched_workspace(&backend, &ws1_id)?;
    let touched_b = collect_touched_workspace(&backend, &ws2_id)?;

    let set_a: BTreeSet<_> = touched_a.touched_paths.iter().collect();
    let set_b: BTreeSet<_> = touched_b.touched_paths.iter().collect();

    let overlap_paths: Vec<String> = set_a
        .intersection(&set_b)
        .map(|p| p.display().to_string())
        .collect();

    let risk = if overlap_paths.is_empty() { "low" } else { "high" };

    let output = OverlapOutput {
        workspace_a: touched_a.workspace.clone(),
        workspace_b: touched_b.workspace.clone(),
        touched_a_count: touched_a.touched_paths.len(),
        touched_b_count: touched_b.touched_paths.len(),
        overlap_count: overlap_paths.len(),
        overlap_paths,
        risk,
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Text | OutputFormat::Pretty => {
            print_overlap_text(&output);
        }
    }

    Ok(())
}

fn print_overlap_text(output: &OverlapOutput) {
    println!(
        "Overlap prediction: '{}' vs '{}'",
        output.workspace_a, output.workspace_b
    );
    println!(
        "Touched: {}={} path(s), {}={} path(s)",
        output.workspace_a, output.touched_a_count, output.workspace_b, output.touched_b_count
    );
    println!("Overlap: {} path(s)", output.overlap_count);

    if output.overlap_paths.is_empty() {
        println!("  (no overlapping paths detected)");
    } else {
        for path in &output.overlap_paths {
            println!("  {path}");
        }
    }

    println!("Risk: {}", output.risk);
    println!();
    println!(
        "Next: maw ws merge {} {} --check --format json",
        output.workspace_a, output.workspace_b
    );
}
