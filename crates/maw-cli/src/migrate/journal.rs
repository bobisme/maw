//! Migration journal — crash-safe checkpoint for `maw migrate`.
//!
//! Written atomically (tmp-file + rename) at every phase boundary so a
//! crash always leaves a coherent `journal.json` on disk. Lives at:
//!
//! - **Phase A–D (pre-flip)**: `<root>/.manifold/migration/journal.json`
//! - **Phase D (post-flip) and Phase E**:
//!   `<root>/.maw/manifold/migration/journal.json`
//!
//! The journal is *deleted* on successful Phase E completion. Its
//! presence is the sole signal `maw migrate --resume` uses to know that
//! a prior run was interrupted.
//!
//! # Schema (versioned)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "started_at": 1730851200,
//!   "updated_at": 1730851305,
//!   "phase": "PreserveDone",
//!   "root": "/abs/path/to/repo",
//!   "original_flavor": "V2WsRoot",
//!   "worktrees": [
//!     {
//!       "name": "alice",
//!       "old_path": "/abs/path/to/repo/ws/alice",
//!       "new_path": "/abs/path/to/repo/.maw/workspaces/alice",
//!       "head_oid": "abc123...",
//!       "is_detached": false,
//!       "relocated": false,
//!       "recovery_ref": "refs/manifold/recovery/alice/2026-05-25T..."
//!     }
//!   ],
//!   "pre_migration_refs": [
//!     ["refs/manifold/epoch/current", "deadbeef..."]
//!   ]
//! }
//! ```
//!
//! The `phase` field is monotonically advanced; phases are linearly
//! ordered (`Start < PreflightDone < PreserveDone < RelocateDone <
//! UnBareDone < FinalizeDone`). A phase function returning success
//! after writing the journal at the *next* phase value means the prior
//! phase is durable.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Linear sequence of phases the migration journal records.
///
/// `u8` ordering is used to compare phases in skip-logic; do NOT reorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum JournalPhase {
    /// Initial state — no work performed yet.
    Start = 0,
    /// Phase A complete: preflight passed, refs snapshotted.
    PreflightDone = 1,
    /// Phase B complete: recovery snapshots pinned for every workspace.
    PreserveDone = 2,
    /// Phase C complete: worktrees relocated under `.maw/workspaces/`.
    RelocateDone = 3,
    /// Phase D complete: root un-bared, branch materialized, ws/default
    /// removed, `.manifold/` moved to `.maw/manifold/`.
    UnBareDone = 4,
    /// Phase E complete: gitignore updated, ws/ rmdir'd, invariants
    /// verified. Journal is deleted after this point.
    FinalizeDone = 5,
}

/// A single worktree's state across the migration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalWorktree {
    /// Workspace name (the linked-worktree admin key under
    /// `<common>/worktrees/<name>/`).
    pub name: String,
    /// Pre-migration path: `<root>/ws/<name>/`.
    pub old_path: PathBuf,
    /// Target path: `<root>/.maw/workspaces/<name>/` (or `<root>` for the
    /// privileged default target).
    pub new_path: PathBuf,
    /// Recorded HEAD OID before relocation; used by
    /// [`crate::migrate`]'s `verify_no_work_lost`.
    pub head_oid: Option<String>,
    /// Whether the worktree HEAD was detached at journal-write time.
    pub is_detached: bool,
    /// True once Phase C has moved the directory + rewritten admin gitdir.
    pub relocated: bool,
    /// Pinned recovery ref produced by Phase B, if any.
    pub recovery_ref: Option<String>,
}

/// Top-level on-disk journal record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Journal {
    /// Schema version (bump on breaking change).
    pub schema_version: u32,
    /// Unix seconds when the migration started.
    pub started_at: u64,
    /// Unix seconds of the last update.
    pub updated_at: u64,
    /// Current phase.
    pub phase: JournalPhase,
    /// Absolute path to the repo root the journal pertains to.
    pub root: PathBuf,
    /// Layout flavor at start (always `"V2WsRoot"` for now).
    pub original_flavor: String,
    /// Per-workspace records.
    pub worktrees: Vec<JournalWorktree>,
    /// `[(ref_name, oid)]` snapshot of `refs/manifold/*` at Phase A.
    pub pre_migration_refs: Vec<(String, String)>,
}

const SCHEMA_VERSION: u32 = 1;

impl Journal {
    /// Build a fresh journal anchored at `root` (phase = Start).
    #[must_use]
    pub fn new_for(root: &Path) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            schema_version: SCHEMA_VERSION,
            started_at: now,
            updated_at: now,
            phase: JournalPhase::Start,
            root: root.to_path_buf(),
            original_flavor: "V2WsRoot".to_string(),
            worktrees: Vec::new(),
            pre_migration_refs: Vec::new(),
        }
    }

    /// Write the journal atomically (tmp-file + rename).
    ///
    /// Creates parent directories as needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the file system mutations fail.
    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).context("serialize journal to JSON")?;
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(json.as_bytes())
                .with_context(|| format!("write {}", tmp.display()))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Read a journal from disk if it exists; returns `Ok(None)` if absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but is malformed.
    pub fn read(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let parsed: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} as Journal", path.display()))?;
        if parsed.schema_version != SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported migration journal schema_version={} (expected {})",
                parsed.schema_version,
                SCHEMA_VERSION
            );
        }
        Ok(Some(parsed))
    }
}

/// Pre-flip journal path (`.manifold/migration/journal.json`).
#[must_use]
pub fn path_v2(root: &Path) -> PathBuf {
    root.join(".manifold")
        .join("migration")
        .join("journal.json")
}

/// Post-flip journal path (`.maw/manifold/migration/journal.json`).
#[must_use]
pub fn path_consolidated(root: &Path) -> PathBuf {
    root.join(".maw")
        .join("manifold")
        .join("migration")
        .join("journal.json")
}

/// Whichever path currently exists, falling back to `path_consolidated`.
#[must_use]
pub fn active_path(root: &Path) -> PathBuf {
    let p2 = path_v2(root);
    if p2.exists() {
        return p2;
    }
    path_consolidated(root)
}

/// Try each candidate path; return the first journal that loads.
///
/// # Errors
///
/// Returns an error only if a file exists but parses incorrectly.
pub fn load_first(paths: &[&Path]) -> Result<Option<Journal>> {
    for p in paths {
        if let Some(j) = Journal::read(p)? {
            return Ok(Some(j));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test asserts")]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trip_atomic_write() {
        let tmp = tempfile::tempdir().expect("mktemp");
        let path = tmp.path().join(".manifold/migration/journal.json");
        let mut j = Journal::new_for(tmp.path());
        j.worktrees.push(JournalWorktree {
            name: "alice".into(),
            old_path: tmp.path().join("ws/alice"),
            new_path: tmp.path().join(".maw/workspaces/alice"),
            head_oid: Some("0".repeat(40)),
            is_detached: false,
            relocated: false,
            recovery_ref: None,
        });
        j.pre_migration_refs
            .push(("refs/manifold/epoch/current".into(), "abc".into()));
        j.phase = JournalPhase::PreflightDone;

        j.write_atomic(&path).expect("write");
        let read = Journal::read(&path)
            .expect("read result")
            .expect("journal present");
        assert_eq!(read.schema_version, SCHEMA_VERSION);
        assert_eq!(read.phase, JournalPhase::PreflightDone);
        assert_eq!(read.worktrees.len(), 1);
        assert_eq!(read.worktrees[0].name, "alice");
        assert_eq!(read.pre_migration_refs.len(), 1);
    }

    #[test]
    fn journal_phase_ordering_is_monotonic() {
        assert_eq!(JournalPhase::Start as u8, 0);
        assert!((JournalPhase::PreflightDone as u8) < (JournalPhase::PreserveDone as u8));
        assert!((JournalPhase::PreserveDone as u8) < (JournalPhase::RelocateDone as u8));
        assert!((JournalPhase::RelocateDone as u8) < (JournalPhase::UnBareDone as u8));
        assert!((JournalPhase::UnBareDone as u8) < (JournalPhase::FinalizeDone as u8));
    }

    #[test]
    fn read_returns_none_for_missing_path() {
        let tmp = tempfile::tempdir().expect("mktemp");
        let path = tmp.path().join("nope.json");
        assert!(Journal::read(&path).expect("read result").is_none());
    }

    #[test]
    fn load_first_picks_existing() {
        let tmp = tempfile::tempdir().expect("mktemp");
        let a = tmp.path().join("a.json");
        let b = tmp.path().join("b.json");
        let j = Journal::new_for(tmp.path());
        j.write_atomic(&b).expect("write");
        let got = load_first(&[&a, &b]).expect("load_first");
        assert!(got.is_some());
    }
}
