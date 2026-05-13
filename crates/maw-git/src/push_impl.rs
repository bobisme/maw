//! Push operations: permanent push/fetch protocol carveouts.
//!
//! Push is **kept permanently** as a `git` CLI subprocess: gix-protocol is
//! too low-level to host a maintained high-level push API today. These
//! functions are the single chokepoint for outgoing protocol traffic from
//! `maw-git` and are referenced as "carveout" calls in
//! `docs/git-subprocess-inventory.md` (bn-28ky).
//!
//! Any new push/fetch protocol shellout must live here (or in
//! `maw-cli`'s `transport::carveout` wrapper), be annotated with
//! `// CARVEOUT(transport): <reason>`, and be enumerated in the inventory
//! doc.

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
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| GitError::BackendError {
            message: "cannot determine repository working directory".into(),
        })
}

/// CARVEOUT(transport): `git push <remote> <refspec>` — gix-protocol push API
/// is too low-level to host here. Kept permanently. See module docs and
/// `docs/git-subprocess-inventory.md`.
pub fn push_branch(
    repo: &GixRepo,
    remote: &str,
    local_ref: &str,
    remote_ref: &str,
    force: bool,
) -> Result<(), GitError> {
    let dir = repo_dir(repo)?;
    let refspec = format!("{local_ref}:{remote_ref}");

    // CARVEOUT(transport): outbound git push protocol; gix-protocol is too
    // low-level for a maintained high-level push API. Kept permanently.
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

/// CARVEOUT(transport): `git push <remote> <tag>` — gix-protocol push API
/// is too low-level to host here. Kept permanently. See module docs and
/// `docs/git-subprocess-inventory.md`.
pub fn push_tag(repo: &GixRepo, remote: &str, tag: &str) -> Result<(), GitError> {
    let dir = repo_dir(repo)?;

    // CARVEOUT(transport): outbound git push protocol for a tag ref. Kept
    // permanently for the same reason as `push_branch`.
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
