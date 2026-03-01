//! Push operations via git CLI fallback.
//!
//! Push is the one operation kept as a CLI subprocess because gix
//! does not yet provide a high-level push API.

use crate::error::GitError;
use crate::gix_repo::GixRepo;

pub fn push_branch(
    repo: &GixRepo,
    remote: &str,
    local_ref: &str,
    remote_ref: &str,
    force: bool,
) -> Result<(), GitError> {
    todo!("Implement with Command::new(\"git\") push")
}

pub fn push_tag(repo: &GixRepo, remote: &str, tag: &str) -> Result<(), GitError> {
    todo!("Implement with Command::new(\"git\") push --tags")
}
