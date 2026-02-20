use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use crate::model::diff::compute_patchset;
use crate::model::patch::{PatchSet, PatchValue};
use crate::model::types::WorkspaceId;

use super::get_backend;

#[derive(Debug, Clone)]
pub struct WorkspaceTouched {
    pub(crate) workspace: String,
    pub(crate) base_epoch: String,
    pub(crate) is_stale: bool,
    pub(crate) touched_paths: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
struct TouchedOutput {
    workspace: String,
    base_epoch: String,
    is_stale: bool,
    touched_count: usize,
    touched_paths: Vec<String>,
}

pub fn touched(workspace: &str, format: OutputFormat) -> Result<()> {
    let ws_id = WorkspaceId::new(workspace)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{workspace}': {e}"))?;

    let backend = get_backend()?;
    let touched = collect_touched_workspace(&backend, &ws_id)?;

    match format {
        OutputFormat::Json => {
            let output = TouchedOutput {
                workspace: touched.workspace,
                base_epoch: touched.base_epoch,
                is_stale: touched.is_stale,
                touched_count: touched.touched_paths.len(),
                touched_paths: touched
                    .touched_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Text | OutputFormat::Pretty => {
            print_touched_text(&touched);
        }
    }

    Ok(())
}

pub fn collect_touched_workspace<B: WorkspaceBackend>(
    backend: &B,
    ws_id: &WorkspaceId,
) -> Result<WorkspaceTouched>
where
    B::Error: std::fmt::Display,
{
    if !backend.exists(ws_id) {
        let ws = ws_id.as_str();
        bail!(
            "Workspace '{ws}' does not exist\n  Check: maw ws list\n  Next: maw ws touched <workspace> --format json"
        );
    }

    let status = backend.status(ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let ws_path = backend.workspace_path(ws_id);
    let patch_set = compute_patchset(&ws_path, &status.base_epoch).map_err(|e| {
        anyhow::anyhow!(
            "Failed to compute touched set for '{}': {e}",
            ws_id.as_str()
        )
    })?;

    Ok(WorkspaceTouched {
        workspace: ws_id.as_str().to_owned(),
        base_epoch: status.base_epoch.as_str().to_owned(),
        is_stale: status.is_stale,
        touched_paths: touched_paths_from_patchset(&patch_set),
    })
}

pub fn touched_paths_from_patchset(patch_set: &PatchSet) -> Vec<PathBuf> {
    let mut touched = BTreeSet::new();

    for (path, value) in &patch_set.patches {
        touched.insert(path.clone());
        if let PatchValue::Rename { from, .. } = value {
            touched.insert(from.clone());
        }
    }

    touched.into_iter().collect()
}

fn print_touched_text(touched: &WorkspaceTouched) {
    println!(
        "Workspace '{}' touched {} path(s).",
        touched.workspace,
        touched.touched_paths.len()
    );

    if touched.touched_paths.is_empty() {
        println!("  (no local changes)");
    } else {
        for path in &touched.touched_paths {
            println!("  {}", path.display());
        }
    }

    if touched.is_stale {
        println!();
        println!(
            "WARNING: Workspace '{}' is stale (base epoch: {}).",
            touched.workspace,
            &touched.base_epoch[..12]
        );
        println!("  Fix: maw ws sync");
    }

    println!();
    println!(
        "Next: maw ws overlap {} <other-workspace> --format json",
        touched.workspace
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::model::patch::FileId;
    use crate::model::types::{EpochId, GitOid};

    use super::*;

    #[test]
    fn touched_paths_include_rename_source_and_destination() {
        let mut patches = BTreeMap::new();
        patches.insert(
            PathBuf::from("new/name.rs"),
            PatchValue::Rename {
                from: PathBuf::from("old/name.rs"),
                file_id: FileId::new(1),
                new_blob: None,
            },
        );

        let patch_set = PatchSet {
            base_epoch: EpochId::new(&"a".repeat(40)).unwrap(),
            patches,
        };

        let touched = touched_paths_from_patchset(&patch_set);
        assert_eq!(
            touched,
            vec![PathBuf::from("new/name.rs"), PathBuf::from("old/name.rs")]
        );
    }

    #[test]
    fn touched_paths_are_sorted_and_deduplicated() {
        let mut patches = BTreeMap::new();
        patches.insert(
            PathBuf::from("b.rs"),
            PatchValue::Add {
                blob: GitOid::new(&"b".repeat(40)).unwrap(),
                file_id: FileId::new(2),
            },
        );
        patches.insert(
            PathBuf::from("a.rs"),
            PatchValue::Modify {
                base_blob: GitOid::new(&"c".repeat(40)).unwrap(),
                new_blob: GitOid::new(&"d".repeat(40)).unwrap(),
                file_id: FileId::new(3),
            },
        );

        let patch_set = PatchSet {
            base_epoch: EpochId::new(&"e".repeat(40)).unwrap(),
            patches,
        };

        let touched = touched_paths_from_patchset(&patch_set);
        assert_eq!(touched, vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
    }
}
