//! TUI -- re-exported from maw-tui crate.

use std::path::PathBuf;

use anyhow::Result;
use maw_core::backend::WorkspaceBackend;
use maw_tui::{RepoDataSource, WorkspaceEntry};

/// Bridge from maw-cli workspace subsystem to maw-tui's `RepoDataSource` trait.
struct CliDataSource;

impl RepoDataSource for CliDataSource {
    fn repo_root(&self) -> Result<PathBuf> {
        crate::workspace::repo_root()
    }

    fn branch_name(&self) -> Result<String> {
        let root = crate::workspace::repo_root()?;
        let config = crate::workspace::MawConfig::load(&root)?;
        Ok(config.branch().to_string())
    }

    fn list_workspaces(&self) -> Result<Vec<WorkspaceEntry>> {
        let backend = crate::workspace::get_backend()?;
        let infos = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut entries = Vec::new();
        for info in &infos {
            let name = info.id.to_string();
            if name == "default" {
                continue;
            }
            let path = backend.workspace_path(&info.id);
            let is_stale = info.state.is_stale();
            entries.push(WorkspaceEntry {
                name,
                path,
                is_stale,
            });
        }
        Ok(entries)
    }
}

/// Run the TUI application
pub fn run() -> Result<()> {
    maw_tui::run(Box::new(CliDataSource))
}
