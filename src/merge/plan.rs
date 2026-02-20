//! Merge preview plan types and artifact serialization.
//!
//! Design doc §5.12.1 specifies a deterministic merge plan that describes
//! what a merge *would* do without actually committing. The plan is produced
//! by running PREPARE → BUILD → VALIDATE and stopping before COMMIT.
//!
//! # Merge ID
//!
//! The `merge_id` is a stable identifier: `sha256(epoch_before || sorted(sources) || heads || config)`.
//! Same inputs always produce the same ID, enabling caching and debugging.
//!
//! # Artifacts
//!
//! All artifacts are written via atomic rename (write-to-temp + fsync + rename) and are:
//! - Disposable and regenerable (running `--plan` again produces the same output).
//! - Written to `.manifold/artifacts/merge/<merge_id>/plan.json`.
//! - Per-workspace reports written to `.manifold/artifacts/ws/<workspace_id>/report.json`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::types::{EpochId, WorkspaceId};

// ---------------------------------------------------------------------------
// PredictedConflict
// ---------------------------------------------------------------------------

/// A predicted merge conflict for a specific path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictedConflict {
    /// Path relative to the repo root.
    pub path: PathBuf,
    /// Conflict kind (e.g., `"Diff3Conflict"`, `"AddAddDifferent"`, `"ModifyDelete"`).
    pub kind: String,
    /// The workspace IDs involved in this conflict.
    pub sides: Vec<String>,
}

// ---------------------------------------------------------------------------
// DriverInfo
// ---------------------------------------------------------------------------

/// Information about a merge driver that applies to a specific path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverInfo {
    /// Path relative to the repo root.
    pub path: PathBuf,
    /// Driver kind: "ours", "theirs", or "regenerate".
    pub kind: String,
    /// Command string (only present for "regenerate" drivers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

// ---------------------------------------------------------------------------
// ValidationInfo
// ---------------------------------------------------------------------------

/// Validation configuration that would be applied during the merge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationInfo {
    /// Validation commands in execution order.
    pub commands: Vec<String>,
    /// Timeout in seconds per command.
    pub timeout_seconds: u32,
    /// On-failure policy: "warn", "block", "quarantine", "block+quarantine".
    pub policy: String,
}

// ---------------------------------------------------------------------------
// WorkspaceChange
// ---------------------------------------------------------------------------

/// A single file change from a workspace (for per-workspace reports).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceChange {
    /// Path relative to the repo root.
    pub path: PathBuf,
    /// Change kind: "added", "modified", or "deleted".
    pub kind: String,
}

// ---------------------------------------------------------------------------
// WorkspaceReport
// ---------------------------------------------------------------------------

/// Per-workspace change report.
///
/// Written to `.manifold/artifacts/ws/<workspace_id>/report.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReport {
    /// The workspace this report covers.
    pub workspace_id: String,
    /// The frozen HEAD commit OID for this workspace.
    pub head: String,
    /// All file changes in this workspace.
    pub changes: Vec<WorkspaceChange>,
}

// ---------------------------------------------------------------------------
// MergePlan
// ---------------------------------------------------------------------------

/// A deterministic, machine-parseable merge plan produced by `maw ws merge --plan`.
///
/// The plan describes exactly what a merge *would* do without performing any
/// commit or ref update. Artifacts are written to `.manifold/artifacts/`.
///
/// Schema matches design doc §5.12.1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlan {
    /// Stable identifier: `sha256(epoch_before || sorted(sources) || sorted(heads) || config_hash)`.
    pub merge_id: String,

    /// The epoch commit OID before this merge.
    pub epoch_before: String,

    /// Source workspace IDs (sorted for determinism).
    pub sources: Vec<String>,

    /// All paths touched by at least one workspace (sorted).
    pub touched_paths: Vec<PathBuf>,

    /// Paths touched by two or more workspaces (potential conflicts).
    pub overlaps: Vec<PathBuf>,

    /// Conflicts predicted by the merge engine.
    pub predicted_conflicts: Vec<PredictedConflict>,

    /// Merge drivers that apply to touched paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drivers: Vec<DriverInfo>,

    /// Validation configuration (absent if no validation is configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationInfo>,
}

// ---------------------------------------------------------------------------
// merge_id computation
// ---------------------------------------------------------------------------

/// Compute a deterministic merge ID from inputs.
///
/// Algorithm: SHA-256 of `epoch_oid || '\n' || sorted_sources || '\n' || sorted_heads || '\n'`.
/// Each source is `"<workspace_id>:<head_oid>\n"`. This ensures the same inputs
/// always produce the same ID regardless of map iteration order.
#[must_use]
pub fn compute_merge_id(
    epoch: &EpochId,
    sources: &[WorkspaceId],
    heads: &BTreeMap<WorkspaceId, crate::model::types::GitOid>,
) -> String {
    let mut hasher = Sha256::new();

    // Epoch OID
    hasher.update(epoch.as_str().as_bytes());
    hasher.update(b"\n");

    // Sources (sort for determinism — BTreeMap already sorted, but sources slice may not be)
    let mut sorted_sources: Vec<&WorkspaceId> = sources.iter().collect();
    sorted_sources.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for ws in &sorted_sources {
        hasher.update(ws.as_str().as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"---\n");

    // Heads (BTreeMap is already sorted by workspace ID)
    for (ws, head) in heads {
        hasher.update(ws.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(head.as_str().as_bytes());
        hasher.update(b"\n");
    }

    let result = hasher.finalize();
    // Return lowercase hex string (64 chars)
    let mut hex = String::with_capacity(64);
    for b in result.iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

// ---------------------------------------------------------------------------
// Artifact writing
// ---------------------------------------------------------------------------

/// Error type for plan artifact operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanArtifactError {
    /// I/O error.
    Io(String),
    /// Serialization error.
    Serialize(String),
}

impl std::fmt::Display for PlanArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "plan artifact I/O error: {msg}"),
            Self::Serialize(msg) => write!(f, "plan artifact serialize error: {msg}"),
        }
    }
}

impl std::error::Error for PlanArtifactError {}

/// Write the merge plan to `.manifold/artifacts/merge/<merge_id>/plan.json`.
///
/// The write is atomic (write-to-temp + fsync + rename). Returns the path
/// to the written artifact.
///
/// # Errors
///
/// Returns [`PlanArtifactError`] on I/O or serialization failure.
pub fn write_plan_artifact(
    manifold_dir: &Path,
    plan: &MergePlan,
) -> Result<PathBuf, PlanArtifactError> {
    let artifact_dir = manifold_dir
        .join("artifacts")
        .join("merge")
        .join(&plan.merge_id);
    write_json_artifact(&artifact_dir, "plan.json", plan)
}

/// Write a per-workspace report to `.manifold/artifacts/ws/<workspace_id>/report.json`.
///
/// # Errors
///
/// Returns [`PlanArtifactError`] on I/O or serialization failure.
pub fn write_workspace_report_artifact(
    manifold_dir: &Path,
    report: &WorkspaceReport,
) -> Result<PathBuf, PlanArtifactError> {
    let artifact_dir = manifold_dir
        .join("artifacts")
        .join("ws")
        .join(&report.workspace_id);
    write_json_artifact(&artifact_dir, "report.json", report)
}

/// Write a JSON value atomically to `<artifact_dir>/<filename>`.
///
/// 1. Create `<artifact_dir>` recursively.
/// 2. Serialize `value` to pretty JSON.
/// 3. Write to a temp file in the same directory.
/// 4. fsync + atomic rename.
fn write_json_artifact<T: Serialize>(
    artifact_dir: &Path,
    filename: &str,
    value: &T,
) -> Result<PathBuf, PlanArtifactError> {
    fs::create_dir_all(artifact_dir).map_err(|e| {
        PlanArtifactError::Io(format!("create dir {}: {e}", artifact_dir.display()))
    })?;

    let final_path = artifact_dir.join(filename);
    let tmp_path = artifact_dir.join(format!(".{filename}.tmp"));

    let json = serde_json::to_string_pretty(value)
        .map_err(|e| PlanArtifactError::Serialize(format!("{e}")))?;

    let mut file = fs::File::create(&tmp_path)
        .map_err(|e| PlanArtifactError::Io(format!("create {}: {e}", tmp_path.display())))?;
    file.write_all(json.as_bytes())
        .map_err(|e| PlanArtifactError::Io(format!("write {}: {e}", tmp_path.display())))?;
    file.sync_all()
        .map_err(|e| PlanArtifactError::Io(format!("fsync {}: {e}", tmp_path.display())))?;
    drop(file);

    fs::rename(&tmp_path, &final_path).map_err(|e| {
        PlanArtifactError::Io(format!(
            "rename {} → {}: {e}",
            tmp_path.display(),
            final_path.display()
        ))
    })?;

    Ok(final_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{EpochId, GitOid, WorkspaceId};
    use std::collections::BTreeMap;

    fn test_epoch() -> EpochId {
        EpochId::new(&"a".repeat(40)).unwrap()
    }

    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    // -- merge_id --

    #[test]
    fn merge_id_is_64_hex_chars() {
        let epoch = test_epoch();
        let sources = vec![test_ws("ws-a"), test_ws("ws-b")];
        let mut heads = BTreeMap::new();
        heads.insert(test_ws("ws-a"), test_oid('b'));
        heads.insert(test_ws("ws-b"), test_oid('c'));
        let id = compute_merge_id(&epoch, &sources, &heads);
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn merge_id_is_deterministic() {
        let epoch = test_epoch();
        let sources = vec![test_ws("ws-a"), test_ws("ws-b")];
        let mut heads = BTreeMap::new();
        heads.insert(test_ws("ws-a"), test_oid('b'));
        heads.insert(test_ws("ws-b"), test_oid('c'));

        let id1 = compute_merge_id(&epoch, &sources, &heads);
        let id2 = compute_merge_id(&epoch, &sources, &heads);
        assert_eq!(id1, id2);
    }

    #[test]
    fn merge_id_stable_regardless_of_source_order() {
        let epoch = test_epoch();
        let sources_ab = vec![test_ws("ws-a"), test_ws("ws-b")];
        let sources_ba = vec![test_ws("ws-b"), test_ws("ws-a")];
        let mut heads = BTreeMap::new();
        heads.insert(test_ws("ws-a"), test_oid('b'));
        heads.insert(test_ws("ws-b"), test_oid('c'));

        // Sources are sorted internally, so order doesn't matter
        let id_ab = compute_merge_id(&epoch, &sources_ab, &heads);
        let id_ba = compute_merge_id(&epoch, &sources_ba, &heads);
        assert_eq!(id_ab, id_ba, "merge_id must be stable regardless of source order");
    }

    #[test]
    fn merge_id_changes_with_different_epoch() {
        let epoch1 = EpochId::new(&"a".repeat(40)).unwrap();
        let epoch2 = EpochId::new(&"b".repeat(40)).unwrap();
        let sources = vec![test_ws("ws-a")];
        let mut heads = BTreeMap::new();
        heads.insert(test_ws("ws-a"), test_oid('c'));

        let id1 = compute_merge_id(&epoch1, &sources, &heads);
        let id2 = compute_merge_id(&epoch2, &sources, &heads);
        assert_ne!(id1, id2, "different epochs must produce different merge_ids");
    }

    #[test]
    fn merge_id_changes_with_different_heads() {
        let epoch = test_epoch();
        let sources = vec![test_ws("ws-a")];
        let mut heads1 = BTreeMap::new();
        heads1.insert(test_ws("ws-a"), test_oid('b'));
        let mut heads2 = BTreeMap::new();
        heads2.insert(test_ws("ws-a"), test_oid('c'));

        let id1 = compute_merge_id(&epoch, &sources, &heads1);
        let id2 = compute_merge_id(&epoch, &sources, &heads2);
        assert_ne!(id1, id2, "different heads must produce different merge_ids");
    }

    // -- MergePlan serde --

    fn make_plan() -> MergePlan {
        MergePlan {
            merge_id: "a".repeat(64),
            epoch_before: "b".repeat(40),
            sources: vec!["ws-a".to_owned(), "ws-b".to_owned()],
            touched_paths: vec![PathBuf::from("src/main.rs"), PathBuf::from("README.md")],
            overlaps: vec![PathBuf::from("README.md")],
            predicted_conflicts: vec![PredictedConflict {
                path: PathBuf::from("README.md"),
                kind: "Diff3Conflict".to_owned(),
                sides: vec!["ws-a".to_owned(), "ws-b".to_owned()],
            }],
            drivers: vec![DriverInfo {
                path: PathBuf::from("Cargo.lock"),
                kind: "regenerate".to_owned(),
                command: Some("cargo generate-lockfile".to_owned()),
            }],
            validation: Some(ValidationInfo {
                commands: vec!["cargo check".to_owned(), "cargo test".to_owned()],
                timeout_seconds: 60,
                policy: "block".to_owned(),
            }),
        }
    }

    #[test]
    fn merge_plan_serde_roundtrip() {
        let plan = make_plan();
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let decoded: MergePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, plan);
    }

    #[test]
    fn merge_plan_is_pretty_printed() {
        let plan = make_plan();
        let json = serde_json::to_string_pretty(&plan).unwrap();
        assert!(json.contains('\n'));
        assert!(json.contains("  "));
    }

    #[test]
    fn merge_plan_omits_empty_optional_fields() {
        let plan = MergePlan {
            merge_id: "a".repeat(64),
            epoch_before: "b".repeat(40),
            sources: vec!["ws-a".to_owned()],
            touched_paths: Vec::new(),
            overlaps: Vec::new(),
            predicted_conflicts: Vec::new(),
            drivers: Vec::new(),
            validation: None,
        };
        let json = serde_json::to_string_pretty(&plan).unwrap();
        // drivers is skip_serializing_if = "Vec::is_empty"
        assert!(!json.contains("\"drivers\""));
        // validation is skip_serializing_if = "Option::is_none"
        assert!(!json.contains("\"validation\""));
    }

    #[test]
    fn validation_info_serde_roundtrip() {
        let info = ValidationInfo {
            commands: vec!["cargo check".to_owned()],
            timeout_seconds: 30,
            policy: "warn".to_owned(),
        };
        let json = serde_json::to_string_pretty(&info).unwrap();
        let decoded: ValidationInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, info);
    }

    #[test]
    fn driver_info_no_command_omitted() {
        let info = DriverInfo {
            path: PathBuf::from("file.txt"),
            kind: "ours".to_owned(),
            command: None,
        };
        let json = serde_json::to_string_pretty(&info).unwrap();
        assert!(!json.contains("command"));
    }

    // -- Artifact writing --

    #[test]
    fn write_plan_artifact_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        let plan = make_plan();

        let path = write_plan_artifact(&manifold_dir, &plan).unwrap();
        assert!(path.exists());
        assert_eq!(
            path,
            manifold_dir
                .join("artifacts/merge")
                .join(&plan.merge_id)
                .join("plan.json")
        );

        // Verify contents round-trip
        let contents = std::fs::read_to_string(&path).unwrap();
        let decoded: MergePlan = serde_json::from_str(&contents).unwrap();
        assert_eq!(decoded, plan);
    }

    #[test]
    fn write_plan_artifact_is_atomic_no_tmp_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        let plan = make_plan();

        write_plan_artifact(&manifold_dir, &plan).unwrap();

        let artifact_dir = manifold_dir.join("artifacts/merge").join(&plan.merge_id);
        assert!(!artifact_dir.join(".plan.json.tmp").exists());
    }

    #[test]
    fn write_plan_artifact_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        let mut plan = make_plan();

        write_plan_artifact(&manifold_dir, &plan).unwrap();

        // Modify and re-write
        plan.overlaps = vec![PathBuf::from("new.rs")];
        let path = write_plan_artifact(&manifold_dir, &plan).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let decoded: MergePlan = serde_json::from_str(&contents).unwrap();
        assert_eq!(decoded.overlaps, vec![PathBuf::from("new.rs")]);
    }

    #[test]
    fn write_workspace_report_artifact_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        let report = WorkspaceReport {
            workspace_id: "agent-1".to_owned(),
            head: "c".repeat(40),
            changes: vec![
                WorkspaceChange {
                    path: PathBuf::from("src/new.rs"),
                    kind: "added".to_owned(),
                },
                WorkspaceChange {
                    path: PathBuf::from("README.md"),
                    kind: "modified".to_owned(),
                },
            ],
        };

        let path = write_workspace_report_artifact(&manifold_dir, &report).unwrap();
        assert!(path.exists());
        assert_eq!(
            path,
            manifold_dir.join("artifacts/ws/agent-1/report.json")
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        let decoded: WorkspaceReport = serde_json::from_str(&contents).unwrap();
        assert_eq!(decoded, report);
    }

    #[test]
    fn error_display() {
        let e = PlanArtifactError::Io("disk full".into());
        assert!(format!("{e}").contains("disk full"));

        let e = PlanArtifactError::Serialize("bad type".into());
        assert!(format!("{e}").contains("bad type"));
    }
}
