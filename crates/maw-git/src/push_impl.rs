//! Push operations via git CLI fallback.
//!
//! Push is the one operation kept as a CLI subprocess because gix
//! does not yet provide a high-level push API.

use std::process::Command;

use crate::error::GitError;
use crate::gix_repo::GixRepo;

/// Resolve the working directory for running git commands.
///
/// Prefers the stored workdir, falls back to the git dir's parent.
fn repo_dir(repo: &GixRepo) -> Result<std::path::PathBuf, GitError> {
    if let Some(ref wd) = repo.workdir {
        return Ok(wd.clone());
    }
    repo.repo
        .git_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| GitError::BackendError {
            message: "cannot determine repository working directory".into(),
        })
}

pub fn push_branch(
    repo: &GixRepo,
    remote: &str,
    local_ref: &str,
    remote_ref: &str,
    force: bool,
) -> Result<(), GitError> {
    let dir = repo_dir(repo)?;
    let refspec = format!("{local_ref}:{remote_ref}");

    let mut cmd = Command::new("git");
    cmd.arg("push");
    if force {
        cmd.arg("--force-with-lease");
    }
    cmd.arg(remote).arg(&refspec).current_dir(&dir);

    let output = cmd.output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::PushFailed {
            remote: remote.to_string(),
            message: stderr.trim().to_string(),
        });
    }

    Ok(())
}

pub fn push_tag(repo: &GixRepo, remote: &str, tag: &str) -> Result<(), GitError> {
    let dir = repo_dir(repo)?;

    let output = Command::new("git")
        .args(["push", remote, tag])
        .current_dir(&dir)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::PushFailed {
            remote: remote.to_string(),
            message: stderr.trim().to_string(),
        });
    }

    Ok(())
}
