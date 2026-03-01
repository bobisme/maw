//! gix-backed config read/write.

use std::process::Command;

use gix::config::AsKey;

use crate::error::GitError;
use crate::gix_repo::GixRepo;

pub fn read_config(repo: &GixRepo, key: &str) -> Result<Option<String>, GitError> {
    // Validate key format before querying (avoid panic from as_key())
    if key.try_as_key().is_none() {
        return Err(GitError::BackendError {
            message: format!("invalid config key: {key}"),
        });
    }

    let snapshot = repo.repo.config_snapshot();
    Ok(snapshot.string(key).map(|v| v.to_string()))
}

pub fn write_config(repo: &GixRepo, key: &str, value: &str) -> Result<(), GitError> {
    // gix config write support is limited; use git CLI as a reliable fallback.
    let git_dir = repo.repo.git_dir();
    let workdir = git_dir.parent().unwrap_or(git_dir);

    let output = Command::new("git")
        .args(["config", key, value])
        .current_dir(workdir)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::BackendError {
            message: format!("git config write failed: {stderr}"),
        });
    }

    Ok(())
}
