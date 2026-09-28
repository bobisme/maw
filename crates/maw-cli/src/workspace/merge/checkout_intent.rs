//! Durable intent record for the target checkout inside
//! `update_default_workspace` (bn-15fzo).
//!
//! `update_default_workspace` snapshots the target's dirty state (the
//! snapshot resets the tree to the anchor), checks out the merged commit,
//! replays the snapshot, and only then writes the target's per-workspace
//! epoch ref. A crash anywhere in between left no trace of the snapshot or
//! the anchor it was taken against: the next recovery anchored at
//! `epoch_before` against the already-merged tree, snapshotted the merge's
//! own changes as "user edits", and never replayed the user's real edits
//! (they survived only in a recovery ref).
//!
//! The intent is written after the snapshot is pinned and before any
//! snapshot cleanup or checkout, and removed right after the epoch ref is written. When
//! `update_default_workspace` finds an intent for the same merged commit it
//! resumes from it: the pre-merge state is the intent's snapshot relative to
//! the intent's anchor, not whatever the interrupted run left on disk.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What an interrupted target checkout had already decided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutIntent {
    /// The merged commit being checked out.
    pub epoch_after: String,
    /// The commit the snapshot was taken against (the target's HEAD after
    /// the anchor step).
    pub anchor: String,
    /// The snapshot (stash-shaped) commit holding the target's pre-merge
    /// edits, or `None` when the target was clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// The durable recovery ref pinning `snapshot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<String>,
}

fn intent_path(repo_root: &Path, ws_name: &str) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(repo_root)
        .manifold_dir(repo_root)
        .join(format!("target-checkout-{ws_name}.json"))
}

/// Read the intent for `ws_name`, if any. Only a missing record is absent.
/// Unreadable or corrupt intent must stop recovery before it touches the tree.
pub fn read(repo_root: &Path, ws_name: &str) -> Result<Option<CheckoutIntent>> {
    let path = intent_path(repo_root, ws_name);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("cannot read checkout intent {}", path.display()));
        }
    };
    serde_json::from_str(&text)
        .with_context(|| format!("cannot parse checkout intent {}; preserve it and repair it before retrying maw ws merge --recover", path.display()))
        .map(Some)
}

/// Whether an interrupted target checkout of `epoch_after` into `ws_name`
/// is pending. Propagate intent errors so callers cannot snapshot an
/// interrupted checkout as new user work.
pub fn pending_for(repo_root: &Path, ws_name: &str, epoch_after: &str) -> Result<bool> {
    Ok(read(repo_root, ws_name)?.is_some_and(|i| i.epoch_after == epoch_after))
}

/// Durably write the intent (temp file, fsync, rename, fsync dir).
///
/// # Errors
/// Returns an error if the record cannot be written durably.
pub fn write(repo_root: &Path, ws_name: &str, intent: &CheckoutIntent) -> Result<()> {
    let path = intent_path(repo_root, ws_name);
    let dir = path
        .parent()
        .context("checkout intent path has no parent")?
        .to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(".target-checkout-{ws_name}.json.tmp"));
    let json = serde_json::to_vec_pretty(intent).context("serialize checkout intent")?;
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&json)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    std::fs::File::open(&dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync {}", dir.display()))?;
    Ok(())
}

/// Remove the intent (idempotent).
pub fn clear(repo_root: &Path, ws_name: &str) {
    let path = intent_path(repo_root, ws_name);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("cannot remove {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_pending_match_only_same_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".maw/manifold")).expect("mkdir");
        assert_eq!(read(root, "default").expect("read"), None);
        let intent = CheckoutIntent {
            epoch_after: "b".repeat(40),
            anchor: "a".repeat(40),
            snapshot: Some("c".repeat(40)),
            recovery_ref: Some("refs/manifold/recovery/default/x".into()),
        };
        write(root, "default", &intent).expect("write");
        assert_eq!(read(root, "default").expect("read"), Some(intent));
        assert!(pending_for(root, "default", &"b".repeat(40)).expect("pending"));
        assert!(!pending_for(root, "default", &"d".repeat(40)).expect("pending"));
        assert!(!pending_for(root, "other", &"b".repeat(40)).expect("pending"));
        clear(root, "default");
        clear(root, "default");
        assert_eq!(read(root, "default").expect("read"), None);
    }
}
