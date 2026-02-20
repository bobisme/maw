//! Core workspace types for Manifold.
//!
//! Foundation types used throughout Manifold: workspace identifiers, epoch
//! identifiers, git object IDs, workspace state, and workspace info.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GitOid
// ---------------------------------------------------------------------------

/// A validated 40-character lowercase hex Git object ID (SHA-1).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitOid(String);

impl GitOid {
    /// Create a new `GitOid` from a string, validating format.
    ///
    /// # Errors
    /// Returns an error if the string is not exactly 40 lowercase hex characters.
    pub fn new(s: &str) -> Result<Self, ValidationError> {
        Self::validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// Return the inner hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(s: &str) -> Result<(), ValidationError> {
        if s.len() != 40 {
            return Err(ValidationError {
                kind: ErrorKind::GitOid,
                value: s.to_owned(),
                reason: format!("expected 40 hex characters, got {}", s.len()),
            });
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(ValidationError {
                kind: ErrorKind::GitOid,
                value: s.to_owned(),
                reason: "must contain only lowercase hex characters (0-9, a-f)".to_owned(),
            });
        }
        Ok(())
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GitOid {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for GitOid {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::validate(&s)?;
        Ok(Self(s))
    }
}

impl From<GitOid> for String {
    fn from(oid: GitOid) -> Self {
        oid.0
    }
}

// ---------------------------------------------------------------------------
// EpochId
// ---------------------------------------------------------------------------

/// An epoch identifier — a newtype over [`GitOid`] representing a specific
/// immutable snapshot (epoch) of the repository mainline.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EpochId(GitOid);

impl EpochId {
    /// Create a new `EpochId` from a hex string.
    ///
    /// # Errors
    /// Returns an error if the string is not a valid git OID.
    pub fn new(s: &str) -> Result<Self, ValidationError> {
        let oid = GitOid::new(s).map_err(|mut e| {
            e.kind = ErrorKind::EpochId;
            e
        })?;
        Ok(Self(oid))
    }

    /// Return the inner [`GitOid`].
    #[must_use]
    pub const fn oid(&self) -> &GitOid {
        &self.0
    }

    /// Return the hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for EpochId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for EpochId {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for EpochId {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        GitOid::validate(&s).map_err(|mut e| {
            e.kind = ErrorKind::EpochId;
            e
        })?;
        Ok(Self(GitOid(s)))
    }
}

impl From<EpochId> for String {
    fn from(epoch: EpochId) -> Self {
        epoch.0.into()
    }
}

// ---------------------------------------------------------------------------
// WorkspaceId
// ---------------------------------------------------------------------------

/// A validated workspace identifier.
///
/// Workspace names must be lowercase alphanumeric with hyphens, 1–64 characters.
/// Examples: `agent-1`, `feature-auth`, `bugfix-123`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// The maximum length of a workspace name.
    pub const MAX_LEN: usize = 64;

    /// Create a new `WorkspaceId` from a string, validating format.
    ///
    /// # Errors
    /// Returns an error if the name is empty, too long, or contains invalid characters.
    pub fn new(s: &str) -> Result<Self, ValidationError> {
        Self::validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// Return the workspace name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(s: &str) -> Result<(), ValidationError> {
        if s.is_empty() {
            return Err(ValidationError {
                kind: ErrorKind::WorkspaceId,
                value: s.to_owned(),
                reason: "workspace name must not be empty".to_owned(),
            });
        }
        if s.len() > Self::MAX_LEN {
            return Err(ValidationError {
                kind: ErrorKind::WorkspaceId,
                value: s.to_owned(),
                reason: format!(
                    "workspace name must be at most {} characters, got {}",
                    Self::MAX_LEN,
                    s.len()
                ),
            });
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(ValidationError {
                kind: ErrorKind::WorkspaceId,
                value: s.to_owned(),
                reason: "workspace name must not start or end with a hyphen".to_owned(),
            });
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ValidationError {
                kind: ErrorKind::WorkspaceId,
                value: s.to_owned(),
                reason: "workspace name must contain only lowercase letters (a-z), digits (0-9), and hyphens (-)".to_owned(),
            });
        }
        if s.contains("--") {
            return Err(ValidationError {
                kind: ErrorKind::WorkspaceId,
                value: s.to_owned(),
                reason: "workspace name must not contain consecutive hyphens".to_owned(),
            });
        }
        Ok(())
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for WorkspaceId {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for WorkspaceId {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::validate(&s)?;
        Ok(Self(s))
    }
}

impl From<WorkspaceId> for String {
    fn from(id: WorkspaceId) -> Self {
        id.0
    }
}

// ---------------------------------------------------------------------------
// WorkspaceState
// ---------------------------------------------------------------------------

/// The state of a workspace relative to the current epoch.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Workspace is up-to-date with the current epoch.
    Active,
    /// Workspace is behind the current epoch by some number of epochs.
    Stale {
        /// Number of epoch advancements since this workspace was last synced.
        behind_epochs: u32,
    },
    /// Workspace has been destroyed (metadata retained for history).
    Destroyed,
}

impl WorkspaceState {
    /// Returns `true` if the workspace is active.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns `true` if the workspace is stale.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }

    /// Returns `true` if the workspace is destroyed.
    #[must_use]
    pub const fn is_destroyed(&self) -> bool {
        matches!(self, Self::Destroyed)
    }
}

impl fmt::Display for WorkspaceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Stale { behind_epochs } => {
                write!(f, "stale (behind by {behind_epochs} epoch(s))")
            }
            Self::Destroyed => write!(f, "destroyed"),
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceMode
// ---------------------------------------------------------------------------

/// The lifetime mode of a workspace.
///
/// - **Ephemeral** (default): Created from the current epoch, must be merged
///   or destroyed before the next epoch advance. Warns if it survives epochs.
/// - **Persistent** (opt-in): Can survive across epochs. Supports explicit
///   `maw ws advance <name>` to rebase onto the latest epoch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Default: workspace should be merged or destroyed before epoch advances.
    #[default]
    Ephemeral,
    /// Opt-in: workspace can survive across epochs; advance explicitly.
    Persistent,
}

impl WorkspaceMode {
    /// Returns `true` if this is a persistent workspace.
    #[must_use]
    pub const fn is_persistent(&self) -> bool {
        matches!(self, Self::Persistent)
    }

    /// Returns `true` if this is an ephemeral workspace.
    #[must_use]
    pub const fn is_ephemeral(&self) -> bool {
        matches!(self, Self::Ephemeral)
    }
}

impl fmt::Display for WorkspaceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ephemeral => write!(f, "ephemeral"),
            Self::Persistent => write!(f, "persistent"),
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceInfo
// ---------------------------------------------------------------------------

/// Complete information about a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Unique workspace identifier.
    pub id: WorkspaceId,
    /// Absolute path to the workspace root directory.
    pub path: PathBuf,
    /// The epoch this workspace is based on.
    pub epoch: EpochId,
    /// Current state of the workspace.
    pub state: WorkspaceState,
    /// Lifetime mode: ephemeral (default) or persistent.
    #[serde(default)]
    pub mode: WorkspaceMode,
}

// ---------------------------------------------------------------------------
// Validation errors
// ---------------------------------------------------------------------------

/// The kind of value that failed validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A [`GitOid`] validation error.
    GitOid,
    /// An [`EpochId`] validation error.
    EpochId,
    /// A [`WorkspaceId`] validation error.
    WorkspaceId,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitOid => write!(f, "GitOid"),
            Self::EpochId => write!(f, "EpochId"),
            Self::WorkspaceId => write!(f, "WorkspaceId"),
        }
    }
}

/// A validation error for Manifold core types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// What kind of value was being validated.
    pub kind: ErrorKind,
    /// The invalid value.
    pub value: String,
    /// Human-readable explanation.
    pub reason: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {}: {:?} — {}",
            self.kind, self.value, self.reason
        )
    }
}

impl std::error::Error for ValidationError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- GitOid --

    #[test]
    fn git_oid_valid() {
        let hex = "a".repeat(40);
        let oid = GitOid::new(&hex).unwrap();
        assert_eq!(oid.as_str(), hex);
    }

    #[test]
    fn git_oid_mixed_hex() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        assert!(GitOid::new(hex).is_ok());
    }

    #[test]
    fn git_oid_rejects_short() {
        assert!(GitOid::new("abc123").is_err());
    }

    #[test]
    fn git_oid_rejects_long() {
        let hex = "a".repeat(41);
        assert!(GitOid::new(&hex).is_err());
    }

    #[test]
    fn git_oid_rejects_uppercase() {
        let hex = "A".repeat(40);
        assert!(GitOid::new(&hex).is_err());
    }

    #[test]
    fn git_oid_rejects_non_hex() {
        let bad = "g".repeat(40);
        assert!(GitOid::new(&bad).is_err());
    }

    #[test]
    fn git_oid_display() {
        let hex = "b".repeat(40);
        let oid = GitOid::new(&hex).unwrap();
        assert_eq!(format!("{oid}"), hex);
    }

    #[test]
    fn git_oid_from_str() {
        let hex = "c".repeat(40);
        let oid: GitOid = hex.parse().unwrap();
        assert_eq!(oid.as_str(), hex);
    }

    #[test]
    fn git_oid_serde_roundtrip() {
        let hex = "d".repeat(40);
        let oid = GitOid::new(&hex).unwrap();
        let json = serde_json::to_string(&oid).unwrap();
        assert_eq!(json, format!("\"{hex}\""));
        let decoded: GitOid = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, oid);
    }

    #[test]
    fn git_oid_serde_rejects_invalid() {
        let json = "\"not-a-valid-oid\"";
        assert!(serde_json::from_str::<GitOid>(json).is_err());
    }

    // -- EpochId --

    #[test]
    fn epoch_id_valid() {
        let hex = "1".repeat(40);
        let epoch = EpochId::new(&hex).unwrap();
        assert_eq!(epoch.as_str(), hex);
        assert_eq!(epoch.oid().as_str(), hex);
    }

    #[test]
    fn epoch_id_rejects_invalid() {
        assert!(EpochId::new("short").is_err());
    }

    #[test]
    fn epoch_id_error_kind() {
        let err = EpochId::new("bad").unwrap_err();
        assert_eq!(err.kind, ErrorKind::EpochId);
    }

    #[test]
    fn epoch_id_display() {
        let hex = "2".repeat(40);
        let epoch = EpochId::new(&hex).unwrap();
        assert_eq!(format!("{epoch}"), hex);
    }

    #[test]
    fn epoch_id_serde_roundtrip() {
        let hex = "3".repeat(40);
        let epoch = EpochId::new(&hex).unwrap();
        let json = serde_json::to_string(&epoch).unwrap();
        let decoded: EpochId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, epoch);
    }

    // -- WorkspaceId --

    #[test]
    fn workspace_id_valid_simple() {
        let id = WorkspaceId::new("agent-1").unwrap();
        assert_eq!(id.as_str(), "agent-1");
    }

    #[test]
    fn workspace_id_valid_letters() {
        assert!(WorkspaceId::new("default").is_ok());
    }

    #[test]
    fn workspace_id_valid_digits() {
        assert!(WorkspaceId::new("123").is_ok());
    }

    #[test]
    fn workspace_id_valid_mixed() {
        assert!(WorkspaceId::new("feature-auth-2").is_ok());
    }

    #[test]
    fn workspace_id_rejects_empty() {
        let err = WorkspaceId::new("").unwrap_err();
        assert_eq!(err.kind, ErrorKind::WorkspaceId);
    }

    #[test]
    fn workspace_id_rejects_uppercase() {
        assert!(WorkspaceId::new("Agent-1").is_err());
    }

    #[test]
    fn workspace_id_rejects_underscore() {
        assert!(WorkspaceId::new("agent_1").is_err());
    }

    #[test]
    fn workspace_id_rejects_leading_hyphen() {
        assert!(WorkspaceId::new("-agent").is_err());
    }

    #[test]
    fn workspace_id_rejects_trailing_hyphen() {
        assert!(WorkspaceId::new("agent-").is_err());
    }

    #[test]
    fn workspace_id_rejects_consecutive_hyphens() {
        assert!(WorkspaceId::new("agent--1").is_err());
    }

    #[test]
    fn workspace_id_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(WorkspaceId::new(&long).is_err());
    }

    #[test]
    fn workspace_id_max_length_ok() {
        let max = "a".repeat(64);
        assert!(WorkspaceId::new(&max).is_ok());
    }

    #[test]
    fn workspace_id_display() {
        let id = WorkspaceId::new("test-ws").unwrap();
        assert_eq!(format!("{id}"), "test-ws");
    }

    #[test]
    fn workspace_id_serde_roundtrip() {
        let id = WorkspaceId::new("my-workspace").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"my-workspace\"");
        let decoded: WorkspaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn workspace_id_serde_rejects_invalid() {
        let json = "\"INVALID\"";
        assert!(serde_json::from_str::<WorkspaceId>(json).is_err());
    }

    // -- WorkspaceState --

    #[test]
    fn workspace_state_active() {
        let state = WorkspaceState::Active;
        assert!(state.is_active());
        assert!(!state.is_stale());
        assert!(!state.is_destroyed());
    }

    #[test]
    fn workspace_state_stale() {
        let state = WorkspaceState::Stale { behind_epochs: 3 };
        assert!(!state.is_active());
        assert!(state.is_stale());
        assert!(!state.is_destroyed());
    }

    #[test]
    fn workspace_state_destroyed() {
        let state = WorkspaceState::Destroyed;
        assert!(!state.is_active());
        assert!(!state.is_stale());
        assert!(state.is_destroyed());
    }

    #[test]
    fn workspace_state_display() {
        assert_eq!(format!("{}", WorkspaceState::Active), "active");
        assert_eq!(
            format!("{}", WorkspaceState::Stale { behind_epochs: 2 }),
            "stale (behind by 2 epoch(s))"
        );
        assert_eq!(format!("{}", WorkspaceState::Destroyed), "destroyed");
    }

    #[test]
    fn workspace_state_serde_roundtrip() {
        let states = vec![
            WorkspaceState::Active,
            WorkspaceState::Stale { behind_epochs: 5 },
            WorkspaceState::Destroyed,
        ];
        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: WorkspaceState = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, state);
        }
    }

    #[test]
    fn workspace_state_serde_tagged() {
        let json = serde_json::to_string(&WorkspaceState::Active).unwrap();
        assert!(json.contains("\"state\":\"active\""));

        let json = serde_json::to_string(&WorkspaceState::Stale { behind_epochs: 1 }).unwrap();
        assert!(json.contains("\"state\":\"stale\""));
        assert!(json.contains("\"behind_epochs\":1"));
    }

    // -- WorkspaceMode --

    #[test]
    fn workspace_mode_ephemeral() {
        let mode = WorkspaceMode::Ephemeral;
        assert!(mode.is_ephemeral());
        assert!(!mode.is_persistent());
        assert_eq!(format!("{mode}"), "ephemeral");
    }

    #[test]
    fn workspace_mode_persistent() {
        let mode = WorkspaceMode::Persistent;
        assert!(mode.is_persistent());
        assert!(!mode.is_ephemeral());
        assert_eq!(format!("{mode}"), "persistent");
    }

    #[test]
    fn workspace_mode_default_is_ephemeral() {
        let mode = WorkspaceMode::default();
        assert!(mode.is_ephemeral());
    }

    #[test]
    fn workspace_mode_serde_roundtrip() {
        for mode in [WorkspaceMode::Ephemeral, WorkspaceMode::Persistent] {
            let json = serde_json::to_string(&mode).unwrap();
            let decoded: WorkspaceMode = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, mode);
        }
    }

    // -- WorkspaceInfo --

    #[test]
    fn workspace_info_construction() {
        let info = WorkspaceInfo {
            id: WorkspaceId::new("test").unwrap(),
            path: PathBuf::from("/tmp/ws/test"),
            epoch: EpochId::new(&"a".repeat(40)).unwrap(),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::Ephemeral,
        };
        assert_eq!(info.id.as_str(), "test");
        assert_eq!(info.path, PathBuf::from("/tmp/ws/test"));
        assert!(info.state.is_active());
        assert!(info.mode.is_ephemeral());
    }

    #[test]
    fn workspace_info_persistent_mode() {
        let info = WorkspaceInfo {
            id: WorkspaceId::new("agent-1").unwrap(),
            path: PathBuf::from("/repo/ws/agent-1"),
            epoch: EpochId::new(&"f".repeat(40)).unwrap(),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::Persistent,
        };
        assert!(info.mode.is_persistent());
    }

    #[test]
    fn workspace_info_serde_roundtrip() {
        let info = WorkspaceInfo {
            id: WorkspaceId::new("agent-1").unwrap(),
            path: PathBuf::from("/repo/ws/agent-1"),
            epoch: EpochId::new(&"f".repeat(40)).unwrap(),
            state: WorkspaceState::Stale { behind_epochs: 2 },
            mode: WorkspaceMode::Persistent,
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: WorkspaceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, info);
    }

    #[test]
    fn workspace_info_serde_default_mode() {
        // mode field has default, so old JSON without it deserializes to Ephemeral
        let json = r#"{"id":"test","path":"/tmp/ws/test","epoch":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":{"state":"active"}}"#;
        let info: WorkspaceInfo = serde_json::from_str(json).unwrap();
        assert!(info.mode.is_ephemeral());
    }

    // -- ValidationError --

    #[test]
    fn validation_error_display() {
        let err = ValidationError {
            kind: ErrorKind::WorkspaceId,
            value: "BAD".to_owned(),
            reason: "must be lowercase".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("WorkspaceId"));
        assert!(msg.contains("BAD"));
        assert!(msg.contains("must be lowercase"));
    }
}
