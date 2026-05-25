//! Append-only merge event log (bn-yyx — SG4 merge-engine-resilience).
//!
//! # Why this exists
//!
//! The `ws_merge_structured_conflict` friction cluster (T2.8) fires when an
//! agent re-issues `maw ws merge` after a prior merge surfaced a structured
//! conflict. The retry burns a turn because the agent has no out-of-band way to
//! recall **what the prior merge already told them** (which paths, which
//! workspaces, which conflict IDs). They re-run the merge to re-discover it.
//!
//! This module gives the agent an **oracle-quality, persistent record of "what
//! just happened"** so they can answer "what was the prior conflict?" with a
//! file read (or a `maw merge events` call) instead of a `maw ws merge`
//! retry. The retry is the wasted turn; the event log replaces it.
//!
//! # Storage format
//!
//! Append-only JSON Lines at `.manifold/events/merge.jsonl`. Each line is a
//! self-contained [`MergeEvent`] record. The log is **monotonically growing**
//! within a repo lifetime; readers should filter by `ts_unix_ms` if they want
//! a recent window. No rotation / no compaction in this slice — the log is
//! cheap (one short JSON line per merge attempt) and the cluster the file
//! addresses is far more expensive than its disk cost.
//!
//! # Layout discipline
//!
//! Path is computed from `<manifold_dir>/events/merge.jsonl` where
//! `manifold_dir = root.join(".manifold")` — the same convention every other
//! artifact in this crate uses. T3.2 (`bn-2sw3`) is centralising
//! `manifold_dir()` as a helper; when that lands, this module becomes a
//! single-line conversion (no logic change). Until then, the convention is
//! the contract.
//!
//! # Concurrency
//!
//! Writes use `OpenOptions::append(true)` so a single line is atomic on POSIX
//! up to `PIPE_BUF` (4 KiB). Event records are designed to fit (the largest
//! field is a small conflict-ID array). The merge-state lock in
//! [`crate::merge_state`] further serialises concurrent merges on the same
//! repo, so contention on this file is bounded to the rare "two agents racing
//! a merge" path the lock already guards.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version for [`MergeEvent`]. Bumped on breaking changes to consumed
/// fields; additive fields do NOT bump.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Relative path under the manifold directory where events are appended.
pub const EVENTS_RELPATH: &str = "events/merge.jsonl";

/// One row in the append-only merge event log.
///
/// Each event carries a `schema_version`, a wall-clock `ts_unix_ms`, and a
/// payload-bearing `kind`. Consumers should treat unknown kinds as forward-
/// compatible (skip + warn) rather than failing — the log is meant to survive
/// CLI version drift.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeEvent {
    /// Schema version — pinned to [`EVENT_SCHEMA_VERSION`] at write time.
    pub schema_version: u32,
    /// Wall-clock timestamp at append time (UTC milliseconds since UNIX epoch).
    pub ts_unix_ms: i64,
    /// Event payload.
    pub kind: MergeEventKind,
}

/// The payload variants of a [`MergeEvent`].
///
/// Lifecycle: `IntegrationStarted` → (`ConflictDetected` and/or `MergeAttempted`)
/// → (`IntegrationCompleted` | `IntegrationAborted`). The log is per-attempt;
/// a single merge invocation produces one `IntegrationStarted` and one of the
/// two terminal events. `ConflictDetected` may fire between (on the BUILD
/// step) **and is the load-bearing one for the friction cluster**: the agent
/// reads it to recall what happened instead of re-running the merge.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MergeEventKind {
    /// A merge attempt began. Recorded at the start of `maw ws merge` (and
    /// `--check`).
    IntegrationStarted {
        /// Source workspace names, in argv order.
        sources: Vec<String>,
        /// The destination (default workspace name, branch workspace name, or
        /// `change:<id>`).
        into: String,
        /// True if the call was a `--check` dry-run, false if it was a real
        /// merge.
        check_only: bool,
    },
    /// The BUILD phase produced structured conflict(s). This is the
    /// friction-cluster's load-bearing event: an agent that reads this
    /// instead of re-running the merge avoids a wasted turn.
    ConflictDetected {
        /// Source workspace names (same order as `IntegrationStarted.sources`).
        sources: Vec<String>,
        /// Destination as in `IntegrationStarted.into`.
        into: String,
        /// Number of conflicting paths.
        conflict_count: usize,
        /// Deterministic `cf-xxxx` conflict IDs for the report.
        conflict_ids: Vec<String>,
        /// Conflicting paths (relative to repo root), parallel to `conflict_ids`.
        paths: Vec<String>,
    },
    /// The merge completed and advanced state. Carries the resulting merge
    /// commit OID so the agent can verify out-of-band.
    IntegrationCompleted {
        /// Source workspace names.
        sources: Vec<String>,
        /// Destination as in `IntegrationStarted.into`.
        into: String,
        /// Resulting merge commit OID (full hex).
        merge_commit: String,
    },
    /// The merge was aborted (user `--abort`, crash recovery, or an explicit
    /// abort during conflict reporting).
    IntegrationAborted {
        /// Source workspace names.
        sources: Vec<String>,
        /// Destination as in `IntegrationStarted.into`.
        into: String,
        /// Short human-readable reason ("unresolved conflicts", "validate
        /// failed", "user abort", etc.).
        reason: String,
    },
}

/// Compute the event log path for a given manifold directory.
#[must_use]
pub fn events_log_path(manifold_dir: &Path) -> PathBuf {
    manifold_dir.join(EVENTS_RELPATH)
}

/// Wall-clock now in UNIX milliseconds. Falls back to `0` if the system clock
/// is set before the UNIX epoch (a state the rest of maw also treats as
/// pathological).
#[must_use]
pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Append one event to the manifold's merge event log.
///
/// Creates parent directories on demand. Errors are non-fatal at call sites —
/// the merge engine's correctness must not depend on this side channel. Call
/// sites should `let _ =` the result (logging at debug level if they want
/// visibility) so a full-disk or permission error does not regress the
/// Prime Invariant.
///
/// # Errors
///
/// Returns I/O errors propagated from path creation or write.
pub fn append_event(manifold_dir: &Path, kind: MergeEventKind) -> std::io::Result<()> {
    let path = events_log_path(manifold_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let event = MergeEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        ts_unix_ms: now_unix_ms(),
        kind,
    };
    // serde_json::to_string can only fail on a Serializer that itself errors;
    // our struct has no such field, so this is effectively infallible.
    let line = serde_json::to_string(&event).map_err(std::io::Error::other)?;
    let mut f = OpenOptions::new().append(true).create(true).open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Read all events from the log. Missing log returns an empty vector (not an
/// error) — a fresh repo legitimately has no events.
///
/// Unknown / forward-incompatible lines are SKIPPED rather than aborting the
/// read so a newer schema's log remains partially readable by an older CLI.
///
/// # Errors
///
/// Returns I/O errors from opening the log (other than `NotFound`).
pub fn read_events(manifold_dir: &Path) -> std::io::Result<Vec<MergeEvent>> {
    let path = events_log_path(manifold_dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        // Forward-compat: skip unknown/corrupt lines, do not abort the read.
        // The CLI surfaces a skipped-line count via the reader call site
        // (`maw merge events`), not as a hard error here.
        if let Ok(ev) = serde_json::from_slice::<MergeEvent>(line) {
            out.push(ev);
        }
    }
    Ok(out)
}

/// Read events at or after `since_ms` (inclusive). Convenience over
/// [`read_events`] for `maw merge events --since`.
///
/// # Errors
///
/// See [`read_events`].
pub fn read_events_since(manifold_dir: &Path, since_ms: i64) -> std::io::Result<Vec<MergeEvent>> {
    Ok(read_events(manifold_dir)?
        .into_iter()
        .filter(|e| e.ts_unix_ms >= since_ms)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn empty_log_returns_no_events() {
        let dir = td();
        let evs = read_events(dir.path()).expect("read");
        assert!(evs.is_empty());
    }

    #[test]
    fn append_then_read_roundtrips() {
        let dir = td();
        let kind = MergeEventKind::IntegrationStarted {
            sources: vec!["alice".to_string()],
            into: "default".to_string(),
            check_only: false,
        };
        append_event(dir.path(), kind.clone()).expect("append");
        let evs = read_events(dir.path()).expect("read");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, kind);
        assert_eq!(evs[0].schema_version, EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn append_creates_parent_dir() {
        let dir = td();
        // manifold_dir doesn't exist as a directory yet
        let manifold = dir.path().join("nested/.manifold");
        append_event(
            &manifold,
            MergeEventKind::IntegrationAborted {
                sources: vec!["a".into()],
                into: "default".into(),
                reason: "test".into(),
            },
        )
        .expect("append");
        assert!(events_log_path(&manifold).exists());
    }

    #[test]
    fn multiple_appends_preserve_order() {
        let dir = td();
        let kinds = vec![
            MergeEventKind::IntegrationStarted {
                sources: vec!["a".into()],
                into: "default".into(),
                check_only: true,
            },
            MergeEventKind::ConflictDetected {
                sources: vec!["a".into()],
                into: "default".into(),
                conflict_count: 1,
                conflict_ids: vec!["cf-abcd".into()],
                paths: vec!["src/lib.rs".into()],
            },
            MergeEventKind::IntegrationAborted {
                sources: vec!["a".into()],
                into: "default".into(),
                reason: "unresolved conflicts".into(),
            },
        ];
        for k in &kinds {
            append_event(dir.path(), k.clone()).expect("append");
        }
        let evs = read_events(dir.path()).expect("read");
        assert_eq!(evs.len(), kinds.len());
        for (i, ev) in evs.iter().enumerate() {
            assert_eq!(ev.kind, kinds[i]);
        }
    }

    #[test]
    fn read_skips_corrupt_lines() {
        let dir = td();
        let path = events_log_path(dir.path());
        fs::create_dir_all(path.parent().expect("path has parent")).expect("mkdir");
        // Append one valid line, one garbage line, one valid line.
        append_event(
            dir.path(),
            MergeEventKind::IntegrationStarted {
                sources: vec!["a".into()],
                into: "default".into(),
                check_only: false,
            },
        )
        .expect("append1");
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open log");
            writeln!(f, "this is not json").expect("write");
        }
        append_event(
            dir.path(),
            MergeEventKind::IntegrationCompleted {
                sources: vec!["a".into()],
                into: "default".into(),
                merge_commit: "deadbeef".repeat(5),
            },
        )
        .expect("append2");
        let evs = read_events(dir.path()).expect("read");
        assert_eq!(evs.len(), 2, "corrupt line skipped, valid lines kept");
    }

    #[test]
    fn read_since_filters_by_timestamp() {
        let dir = td();
        append_event(
            dir.path(),
            MergeEventKind::IntegrationStarted {
                sources: vec!["a".into()],
                into: "default".into(),
                check_only: false,
            },
        )
        .expect("append");
        // Sleep a touch so timestamps actually differ.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let cutoff = now_unix_ms();
        std::thread::sleep(std::time::Duration::from_millis(2));
        append_event(
            dir.path(),
            MergeEventKind::IntegrationCompleted {
                sources: vec!["a".into()],
                into: "default".into(),
                merge_commit: "cafef00d".repeat(5),
            },
        )
        .expect("append");
        let since = read_events_since(dir.path(), cutoff).expect("read");
        assert_eq!(
            since.len(),
            1,
            "only the post-cutoff event should be returned"
        );
        assert!(matches!(
            since[0].kind,
            MergeEventKind::IntegrationCompleted { .. }
        ));
    }

    #[test]
    fn conflict_detected_carries_ids_and_paths_in_parallel() {
        // Load-bearing for the friction-cluster fix: the agent must be able
        // to map each cf-id back to its path WITHOUT re-running the merge.
        let dir = td();
        append_event(
            dir.path(),
            MergeEventKind::ConflictDetected {
                sources: vec!["alice".into(), "bob".into()],
                into: "default".into(),
                conflict_count: 2,
                conflict_ids: vec!["cf-aaaa".into(), "cf-bbbb".into()],
                paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            },
        )
        .expect("append");
        let evs = read_events(dir.path()).expect("read");
        match &evs[0].kind {
            MergeEventKind::ConflictDetected {
                conflict_ids, paths, ..
            } => {
                assert_eq!(conflict_ids.len(), paths.len());
                assert_eq!(conflict_ids[0], "cf-aaaa");
                assert_eq!(paths[1], "src/b.rs");
            }
            other => panic!("expected ConflictDetected, got {other:?}"),
        }
    }
}
