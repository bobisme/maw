//! gix-backed config read/write.

use crate::error::GitError;
use crate::gix_repo::GixRepo;

pub fn read_config(repo: &GixRepo, key: &str) -> Result<Option<String>, GitError> {
    todo!("Implement with gix: repo.config_snapshot()")
}

pub fn write_config(repo: &GixRepo, key: &str, value: &str) -> Result<(), GitError> {
    todo!("Implement with gix config write or INI fallback")
}
