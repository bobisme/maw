//! Persistent "last conflict" surface (bn-yyx — SG4 merge-engine-resilience).
//!
//! # Why this exists
//!
//! The `ws_merge_structured_conflict` friction cluster's load-bearing wasted
//! turn is an agent re-issuing `maw ws merge` after a prior merge surfaced
//! conflicts — because the prior conflict report was scrollback-bound and the
//! agent has no out-of-band recall of it.
//!
//! This module persists the latest conflict surface to disk as a structured
//! JSON object the agent can re-read via `maw merge last-conflict`. The
//! file's existence + content is the **first-class conflict object** the
//! mitigation class names. It survives across CLI invocations until either
//! (a) the merge is retried successfully (cleared by [`clear`]), (b) the
//! merge is aborted (cleared by [`clear`]), or (c) the user / agent runs
//! `maw merge resume` (which consumes it).
//!
//! # Storage path
//!
//! `<manifold_dir>/artifacts/merge/last-conflict.json`
//!
//! Same `manifold_dir = root.join(".manifold")` convention as the rest of the
//! crate; T3.2's `manifold_dir()` helper, when it lands, replaces the
//! `root.join(".manifold")` call sites without affecting this module's
//! contract.
//!
//! # Schema stability
//!
//! Like [`crate::merge::events`], this struct is versioned. The CLI consumer
//! (`maw merge last-conflict`) pins the schema-version field so a backward-
//! incompatible bump is caught at the producer side, not at the agent's first
//! confused re-read.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version pin for [`LastConflict`].
pub const LAST_CONFLICT_SCHEMA_VERSION: u32 = 1;

/// Relative path under the manifold directory.
pub const LAST_CONFLICT_RELPATH: &str = "artifacts/merge/last-conflict.json";

/// One conflicting path's recorded surface — what the agent needs to resolve
/// it without re-running the merge.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastConflictEntry {
    /// Deterministic terseid (`cf-xxxx`) for the conflict, matching the
    /// `--resolve` accepted form.
    pub id: String,
    /// Repo-relative path of the conflicting file.
    pub path: String,
    /// Source workspace IDs that contributed conflicting edits to this path.
    pub sides: Vec<String>,
    /// Short human-readable reason ("overlapping edits (diff3 conflict)" etc.).
    pub reason: String,
}

/// The most recent unresolved merge conflict, persisted so the agent can
/// recall it without re-running `maw ws merge`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastConflict {
    /// Schema-version pin.
    pub schema_version: u32,
    /// UNIX milliseconds at which this conflict was recorded.
    pub ts_unix_ms: i64,
    /// Source workspace names (argv order).
    pub sources: Vec<String>,
    /// Merge destination (workspace name, `ws:<name>`, or `change:<id>`).
    pub into: String,
    /// Per-conflict records.
    pub conflicts: Vec<LastConflictEntry>,
    /// Pre-built `--resolve` commands the agent can copy. Each is fully
    /// qualified (`maw ws merge <sources> --into <into> --resolve cf-...=<ws>`).
    /// Carries the same recovery cues `print_conflict_report` shows but in a
    /// machine-stable form the agent can read off disk.
    pub recovery_commands: Vec<String>,
}

/// Path on disk for the persisted last-conflict snapshot.
#[must_use]
pub fn last_conflict_path(manifold_dir: &Path) -> PathBuf {
    manifold_dir.join(LAST_CONFLICT_RELPATH)
}

/// Persist `snapshot` to disk, replacing any previous record. Atomic via
/// write-then-rename so a concurrent reader never sees a half-written file.
///
/// # Errors
///
/// Returns I/O errors from path creation, write, or rename.
pub fn write(manifold_dir: &Path, snapshot: &LastConflict) -> std::io::Result<()> {
    let path = last_conflict_path(manifold_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.clone();
    tmp.set_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(snapshot).map_err(std::io::Error::other)?;
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the persisted snapshot, if any. Returns `Ok(None)` if no snapshot is
/// recorded (a fresh repo or a successfully cleared merge state).
///
/// # Errors
///
/// Returns I/O errors from open / read (other than `NotFound`), or JSON parse
/// errors when the on-disk file is corrupt.
pub fn read(manifold_dir: &Path) -> std::io::Result<Option<LastConflict>> {
    let path = last_conflict_path(manifold_dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let snapshot: LastConflict = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    Ok(Some(snapshot))
}

/// Remove the persisted snapshot. Idempotent (no error if the file is
/// already absent).
///
/// # Errors
///
/// Returns I/O errors from remove (other than `NotFound`, which is treated as
/// success).
pub fn clear(manifold_dir: &Path) -> std::io::Result<()> {
    let path = last_conflict_path(manifold_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Build the recovery-command list for a conflict surface.
///
/// Helper for call sites: takes a flat
/// (`conflict_id`, `default_workspace`) pairing and produces the
/// copy-pasteable recovery commands. Exposed so the CLI's
/// `print_conflict_report` path can call this and pass the result through
/// to [`write`], guaranteeing the persisted commands match what was
/// printed.
#[must_use]
pub fn build_recovery_commands(
    sources: &[String],
    into: &str,
    conflict_ids: &[String],
    default_resolve_ws: &str,
) -> Vec<String> {
    let mut out = Vec::with_capacity(conflict_ids.len() + 2);
    let ws_args = sources.join(" ");
    // Individual `--resolve` form — one per conflict, most copy-pasteable.
    for id in conflict_ids {
        out.push(format!(
            "maw ws merge {ws_args} --into {into} --resolve {id}={default_resolve_ws}"
        ));
    }
    // Bulk `--resolve-all` form — fewer chars, same effect.
    out.push(format!(
        "maw ws merge {ws_args} --into {into} --resolve-all={default_resolve_ws}"
    ));
    // `maw merge resume` form — runs the queued mergeback using the persisted
    // last-conflict as source-of-truth. Intentionally NOT `maw ws merge` so
    // attribution heuristics don't double-count this as a retry of the
    // failing class.
    out.push(format!(
        "maw merge resume --resolve-all={default_resolve_ws}"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn sample() -> LastConflict {
        LastConflict {
            schema_version: LAST_CONFLICT_SCHEMA_VERSION,
            ts_unix_ms: 1_700_000_000_000,
            sources: vec!["alice".into(), "bob".into()],
            into: "default".into(),
            conflicts: vec![
                LastConflictEntry {
                    id: "cf-aaaa".into(),
                    path: "src/lib.rs".into(),
                    sides: vec!["alice".into(), "bob".into()],
                    reason: "overlapping edits".into(),
                },
                LastConflictEntry {
                    id: "cf-bbbb".into(),
                    path: "Cargo.toml".into(),
                    sides: vec!["alice".into(), "bob".into()],
                    reason: "overlapping edits".into(),
                },
            ],
            recovery_commands: build_recovery_commands(
                &["alice".into(), "bob".into()],
                "default",
                &["cf-aaaa".into(), "cf-bbbb".into()],
                "alice",
            ),
        }
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = td();
        let got = read(dir.path()).expect("read");
        assert!(got.is_none());
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = td();
        let s = sample();
        write(dir.path(), &s).expect("write");
        let got = read(dir.path()).expect("read").expect("present");
        assert_eq!(got, s);
    }

    #[test]
    fn clear_removes_file() {
        let dir = td();
        write(dir.path(), &sample()).expect("write");
        assert!(last_conflict_path(dir.path()).exists());
        clear(dir.path()).expect("clear");
        assert!(!last_conflict_path(dir.path()).exists());
        // Idempotent: second clear is a no-op.
        clear(dir.path()).expect("clear again");
    }

    #[test]
    fn write_replaces_previous_snapshot() {
        let dir = td();
        write(dir.path(), &sample()).expect("write1");
        let mut next = sample();
        next.into = "crib2".into();
        next.ts_unix_ms = 1_800_000_000_000;
        write(dir.path(), &next).expect("write2");
        let got = read(dir.path()).expect("read").expect("present");
        assert_eq!(got.into, "crib2");
        assert_eq!(got.ts_unix_ms, 1_800_000_000_000);
    }

    #[test]
    fn build_recovery_commands_includes_resume_verb() {
        // Load-bearing for the friction-cluster fix: the recovery surface
        // must point at `maw merge resume` (not `maw ws merge`) so the
        // agent's next call lands on a non-friction verb.
        let cmds =
            build_recovery_commands(&["alice".into()], "default", &["cf-abcd".into()], "alice");
        assert!(
            cmds.iter().any(|c| c.starts_with("maw merge resume")),
            "missing `maw merge resume` recovery cue in: {cmds:#?}"
        );
        // And the individual --resolve form is present too, for agents that
        // prefer the explicit per-conflict path.
        assert!(
            cmds.iter().any(|c| c.contains("--resolve cf-abcd=alice")),
            "missing per-conflict --resolve cue in: {cmds:#?}"
        );
    }

    #[test]
    fn schema_version_is_pinned() {
        let dir = td();
        write(dir.path(), &sample()).expect("write");
        let bytes = fs::read(last_conflict_path(dir.path())).expect("read raw");
        let txt = String::from_utf8(bytes).expect("utf8");
        assert!(
            txt.contains("\"schema_version\""),
            "field missing from on-disk form"
        );
        assert!(txt.contains(&LAST_CONFLICT_SCHEMA_VERSION.to_string()));
    }
}
