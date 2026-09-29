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
// BaseEpoch / CurrentEpoch
// ---------------------------------------------------------------------------
//
// These newtypes distinguish the two distinct "epoch" concepts that otherwise
// get passed around as bare `&str` or generic `EpochId`:
//
// - `BaseEpoch` — the workspace's ORIGINAL base epoch (what it branched from
//   at creation time). Stable for the lifetime of the workspace.
// - `CurrentEpoch` — the live/advancing epoch ref
//   (`refs/manifold/epoch/current`). Advances on every merge.
//
// Historically both flowed through the same type, which led to bn-18dj:
// `committed_ahead_of_epoch` was called with the current epoch instead of the
// base epoch, silently dropping local commits on stale workspaces. These
// newtypes intentionally have NO `From`/`Into` conversions between each other,
// so swapping them at a call site is a compile error.
//
// The newtypes do NOT appear in serialized formats (TOML/JSON) — I/O boundaries
// continue to use `String`/`EpochId` and construct the newtype on read.

/// The workspace's original base epoch — the commit it branched from at
/// creation time.
///
/// Stable for the lifetime of the workspace unless explicitly advanced.
/// Used by `committed_ahead_of_epoch` and related sync logic to detect
/// local commits that need rebasing onto a newer current epoch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BaseEpoch(GitOid);

impl BaseEpoch {
    /// Create a new `BaseEpoch` from a hex string, validating format.
    ///
    /// # Errors
    /// Returns an error if the string is not a valid git OID.
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        let s = s.into();
        let oid = GitOid::new(&s).map_err(|mut e| {
            e.kind = ErrorKind::EpochId;
            e
        })?;
        Ok(Self(oid))
    }

    /// Create a `BaseEpoch` directly from a validated [`GitOid`].
    #[must_use]
    pub const fn from_oid(oid: GitOid) -> Self {
        Self(oid)
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

    /// Convert to a generic [`EpochId`] (by clone).
    ///
    /// Useful when passing to functions that accept `&EpochId` and don't
    /// care whether it's a base or current epoch.
    #[must_use]
    pub fn to_epoch_id(&self) -> EpochId {
        EpochId(self.0.clone())
    }
}

impl fmt::Display for BaseEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl AsRef<str> for BaseEpoch {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl From<EpochId> for BaseEpoch {
    fn from(e: EpochId) -> Self {
        Self(e.0)
    }
}

/// The current / live epoch ref — `refs/manifold/epoch/current`.
///
/// Advances on every merge. Distinct from [`BaseEpoch`] by design: a
/// workspace may be behind the current epoch, and operations that want to
/// know "what did this workspace branch from" must use `BaseEpoch`, not
/// `CurrentEpoch`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CurrentEpoch(GitOid);

impl CurrentEpoch {
    /// Create a new `CurrentEpoch` from a hex string, validating format.
    ///
    /// # Errors
    /// Returns an error if the string is not a valid git OID.
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        let s = s.into();
        let oid = GitOid::new(&s).map_err(|mut e| {
            e.kind = ErrorKind::EpochId;
            e
        })?;
        Ok(Self(oid))
    }

    /// Create a `CurrentEpoch` directly from a validated [`GitOid`].
    #[must_use]
    pub const fn from_oid(oid: GitOid) -> Self {
        Self(oid)
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

    /// Convert to a generic [`EpochId`] (by clone).
    #[must_use]
    pub fn to_epoch_id(&self) -> EpochId {
        EpochId(self.0.clone())
    }
}

impl fmt::Display for CurrentEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl AsRef<str> for CurrentEpoch {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl From<EpochId> for CurrentEpoch {
    fn from(e: EpochId) -> Self {
        Self(e.0)
    }
}

// NOTE: Intentionally NO `From<BaseEpoch> for CurrentEpoch` or vice versa.
// The whole point of these newtypes is that swapping them is a compile error.

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

    /// Reserved workspace ID for the synthetic epoch-delta `PatchSet`.
    ///
    /// Used during merge to represent files changed in the epoch delta
    /// (between a stale workspace's base epoch and the current epoch).
    /// This allows the partition step to detect conflicts between
    /// workspace edits and previously-merged epoch changes.
    pub const EPOCH_DELTA: &'static str = "epoch-delta";

    /// Reserved workspace name: the recovery-ref namespace of `maw undo`'s
    /// pins (`refs/manifold/recovery/undo/<ts>`, bn-43x5k).
    ///
    /// A workspace's destroy pins land in `refs/manifold/recovery/<ws>/`, so
    /// a workspace named `undo` would share that namespace with `maw undo`
    /// (bn-asqh7). Refused at creation; an existing legacy `undo` workspace
    /// still parses (and `maw doctor` suggests renaming it).
    pub const UNDO_PIN_NAMESPACE: &'static str = "undo";

    /// Create a new `WorkspaceId` from a string, validating format.
    ///
    /// # Errors
    /// Returns an error if the name is empty, too long, or contains invalid characters.
    pub fn new(s: &str) -> Result<Self, ValidationError> {
        Self::validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// Validate a name for a workspace that is about to be **created** (or
    /// attached as a new tracked workspace).
    ///
    /// This is [`Self::new`] plus a reservation check: names the merge engine
    /// uses for synthetic sides ([`Self::EPOCH_DELTA`]), the `maw undo` pin
    /// namespace ([`Self::UNDO_PIN_NAMESPACE`], bn-asqh7) and names starting
    /// with the merge-quarantine prefix (`merge-quarantine-`, bn-ggo5) are
    /// refused. A real workspace named `epoch-delta` would be
    /// indistinguishable from the synthetic epoch-delta `PatchSet` injected
    /// for stale workspaces (bn-7phd): both sides of a conflict would carry
    /// the same id, `--resolve <path>=epoch-delta` would be ambiguous, and
    /// the real workspace would be displayed as "epoch (previous merge)"
    /// (bn-2l63).
    ///
    /// [`Self::new`] / `FromStr` / serde deliberately still accept the
    /// reserved name: they parse ids that already exist, including the
    /// synthetic id itself (conflict sides, AST edit attribution, resolution
    /// targets) and any pre-existing legacy workspace, which must stay
    /// listable and destroyable.
    ///
    /// # Errors
    /// Returns an error if the name fails [`Self::new`] validation or is a
    /// reserved synthetic workspace id.
    pub fn new_for_create(s: &str) -> Result<Self, ValidationError> {
        Self::check_create_name_bytes(s.as_bytes()).map_err(|rule| rule.to_error(s))?;
        Ok(Self(s.to_owned()))
    }

    /// Create the reserved epoch-delta workspace ID.
    ///
    /// This bypasses the normal validation since the constant is known-valid.
    #[must_use]
    pub fn epoch_delta() -> Self {
        Self(Self::EPOCH_DELTA.to_owned())
    }

    /// Returns `true` if this is the synthetic epoch-delta workspace ID.
    #[must_use]
    pub fn is_epoch_delta(&self) -> bool {
        self.0 == Self::EPOCH_DELTA
    }

    /// Return the workspace name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(s: &str) -> Result<(), ValidationError> {
        Self::check_name_bytes(s.as_bytes()).map_err(|rule| rule.to_error(s))
    }

    /// Byte-level core of [`Self::new`] validation (kept allocation- and
    /// UTF-8-free so bounded model checking can cover it exhaustively).
    ///
    /// Accepted names are 1..=[`Self::MAX_LEN`] bytes of `[a-z0-9-]`, not
    /// starting or ending with `-`, without `--`.
    ///
    /// # Errors
    /// Returns the first violated [`WorkspaceNameRule`].
    pub fn check_name_bytes(b: &[u8]) -> Result<(), WorkspaceNameRule> {
        if b.is_empty() {
            return Err(WorkspaceNameRule::Empty);
        }
        if b.len() > Self::MAX_LEN {
            return Err(WorkspaceNameRule::TooLong);
        }
        if b[0] == b'-' || b[b.len() - 1] == b'-' {
            return Err(WorkspaceNameRule::EdgeHyphen);
        }
        if !b
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(WorkspaceNameRule::InvalidChar);
        }
        if b.windows(2).any(|w| w == b"--") {
            return Err(WorkspaceNameRule::ConsecutiveHyphens);
        }
        Ok(())
    }

    /// Byte-level core of [`Self::new_for_create`]: [`Self::check_name_bytes`]
    /// plus refusal of reserved synthetic ids ([`Self::EPOCH_DELTA`]), of the
    /// `maw undo` pin namespace ([`Self::UNDO_PIN_NAMESPACE`]) and of the
    /// `merge-quarantine-` prefix.
    ///
    /// # Errors
    /// Returns the first violated [`WorkspaceNameRule`].
    pub fn check_create_name_bytes(b: &[u8]) -> Result<(), WorkspaceNameRule> {
        Self::check_name_bytes(b)?;
        if b == Self::EPOCH_DELTA.as_bytes() {
            return Err(WorkspaceNameRule::Reserved);
        }
        if b == Self::UNDO_PIN_NAMESPACE.as_bytes() {
            return Err(WorkspaceNameRule::ReservedUndo);
        }
        // bn-ggo5: `merge-quarantine-<id>` names are what `maw ws merge`
        // gives validation-failure quarantines; `ws sync` / `ws merge` refuse
        // them and `maw merge abandon <id>` targets them.
        if b.starts_with(crate::merge::quarantine_id::QUARANTINE_NAME_PREFIX.as_bytes()) {
            return Err(WorkspaceNameRule::ReservedQuarantinePrefix);
        }
        Ok(())
    }
}

/// The workspace-name rule a candidate name violated (see
/// [`WorkspaceId::check_name_bytes`] / [`WorkspaceId::check_create_name_bytes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceNameRule {
    /// Empty name.
    Empty,
    /// Longer than [`WorkspaceId::MAX_LEN`] bytes.
    TooLong,
    /// Starts or ends with `-`.
    EdgeHyphen,
    /// Contains a byte outside `[a-z0-9-]`.
    InvalidChar,
    /// Contains `--`.
    ConsecutiveHyphens,
    /// Reserved for a synthetic merge-engine workspace id (creation only).
    Reserved,
    /// Starts with the merge-quarantine prefix (creation only).
    ReservedQuarantinePrefix,
    /// The `maw undo` pin namespace (creation only, bn-asqh7).
    ReservedUndo,
}

impl WorkspaceNameRule {
    fn to_error(self, s: &str) -> ValidationError {
        let reason = match self {
            Self::Empty => "workspace name must not be empty".to_owned(),
            Self::TooLong => format!(
                "workspace name must be at most {} characters, got {}",
                WorkspaceId::MAX_LEN,
                s.len()
            ),
            Self::EdgeHyphen => "workspace name must not start or end with a hyphen".to_owned(),
            Self::InvalidChar => "workspace name must contain only lowercase letters (a-z), digits (0-9), and hyphens (-)".to_owned(),
            Self::ConsecutiveHyphens => {
                "workspace name must not contain consecutive hyphens".to_owned()
            }
            Self::Reserved => format!(
                "'{s}' is reserved: the merge engine uses it for the synthetic \
                 epoch-delta side of stale-workspace conflicts; choose another name"
            ),
            Self::ReservedUndo => format!(
                "'{s}' is reserved: `maw undo` keeps its pins under \
                 refs/manifold/recovery/{s}/, where this workspace's recovery \
                 snapshots would also go; choose another name (e.g. '{s}-work')"
            ),
            Self::ReservedQuarantinePrefix => format!(
                "'{s}' is reserved: names starting with '{}' belong to merge \
                 quarantines (see `maw merge list`); choose another name",
                crate::merge::quarantine_id::QUARANTINE_NAME_PREFIX
            ),
        };
        ValidationError {
            kind: ErrorKind::WorkspaceId,
            value: s.to_owned(),
            reason,
        }
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
    /// The epoch this workspace is based on (or the workspace HEAD if ahead of epoch).
    pub epoch: EpochId,
    /// Current state of the workspace.
    pub state: WorkspaceState,
    /// Lifetime mode: ephemeral (default) or persistent.
    #[serde(default)]
    pub mode: WorkspaceMode,
    /// Number of commits in the workspace that are ahead of the current epoch.
    /// Non-zero means the workspace has committed work that hasn't been merged yet.
    #[serde(default)]
    pub commits_ahead: u32,
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
        let oid = GitOid::new(&hex).expect("operation should succeed");
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
        let oid = GitOid::new(&hex).expect("operation should succeed");
        assert_eq!(format!("{oid}"), hex);
    }

    #[test]
    fn git_oid_from_str() {
        let hex = "c".repeat(40);
        let oid: GitOid = hex.parse().expect("operation should succeed");
        assert_eq!(oid.as_str(), hex);
    }

    #[test]
    fn git_oid_serde_roundtrip() {
        let hex = "d".repeat(40);
        let oid = GitOid::new(&hex).expect("operation should succeed");
        let json = serde_json::to_string(&oid).expect("operation should succeed");
        assert_eq!(json, format!("\"{hex}\""));
        let decoded: GitOid = serde_json::from_str(&json).expect("operation should succeed");
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
        let epoch = EpochId::new(&hex).expect("operation should succeed");
        assert_eq!(epoch.as_str(), hex);
        assert_eq!(epoch.oid().as_str(), hex);
    }

    #[test]
    fn epoch_id_rejects_invalid() {
        assert!(EpochId::new("short").is_err());
    }

    #[test]
    fn epoch_id_error_kind() {
        let err = EpochId::new("bad").expect_err("operation should fail");
        assert_eq!(err.kind, ErrorKind::EpochId);
    }

    #[test]
    fn epoch_id_display() {
        let hex = "2".repeat(40);
        let epoch = EpochId::new(&hex).expect("operation should succeed");
        assert_eq!(format!("{epoch}"), hex);
    }

    #[test]
    fn epoch_id_serde_roundtrip() {
        let hex = "3".repeat(40);
        let epoch = EpochId::new(&hex).expect("operation should succeed");
        let json = serde_json::to_string(&epoch).expect("operation should succeed");
        let decoded: EpochId = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, epoch);
    }

    // -- BaseEpoch / CurrentEpoch --

    #[test]
    fn base_epoch_valid() {
        let hex = "a".repeat(40);
        let base = BaseEpoch::new(hex.clone()).expect("operation should succeed");
        assert_eq!(base.as_str(), hex);
        assert_eq!(base.oid().as_str(), hex);
    }

    #[test]
    fn base_epoch_rejects_invalid() {
        assert!(BaseEpoch::new("nope").is_err());
        assert!(BaseEpoch::new("A".repeat(40)).is_err());
        assert!(BaseEpoch::new("a".repeat(41)).is_err());
    }

    #[test]
    fn base_epoch_display() {
        let hex = "b".repeat(40);
        let base = BaseEpoch::new(hex.clone()).expect("operation should succeed");
        assert_eq!(format!("{base}"), hex);
    }

    #[test]
    fn base_epoch_as_ref() {
        let hex = "c".repeat(40);
        let base = BaseEpoch::new(hex.clone()).expect("operation should succeed");
        let s: &str = base.as_ref();
        assert_eq!(s, hex);
    }

    #[test]
    fn base_epoch_from_epoch_id() {
        let hex = "d".repeat(40);
        let epoch = EpochId::new(&hex).expect("operation should succeed");
        let base: BaseEpoch = epoch.into();
        assert_eq!(base.as_str(), hex);
    }

    #[test]
    fn current_epoch_valid() {
        let hex = "e".repeat(40);
        let cur = CurrentEpoch::new(hex.clone()).expect("operation should succeed");
        assert_eq!(cur.as_str(), hex);
    }

    #[test]
    fn current_epoch_rejects_invalid() {
        assert!(CurrentEpoch::new("").is_err());
        assert!(CurrentEpoch::new("g".repeat(40)).is_err());
    }

    #[test]
    fn current_epoch_display() {
        let hex = "1".repeat(40);
        let cur = CurrentEpoch::new(hex.clone()).expect("operation should succeed");
        assert_eq!(format!("{cur}"), hex);
    }

    #[test]
    fn base_and_current_are_distinct_types() {
        // This is a compile-time property, not a runtime one. We assert it by
        // constructing both and observing that there is no `From` between them.
        let hex = "2".repeat(40);
        let base = BaseEpoch::new(hex.clone()).expect("operation should succeed");
        let cur = CurrentEpoch::new(hex).expect("operation should succeed");
        // They share the same underlying string but are distinct types.
        assert_eq!(base.as_str(), cur.as_str());
        // If you try to do `let _: BaseEpoch = cur;` — compile error. Good.
    }

    // -- WorkspaceId --

    #[test]
    fn workspace_id_valid_simple() {
        let id = WorkspaceId::new("agent-1").expect("operation should succeed");
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
        let err = WorkspaceId::new("").expect_err("operation should fail");
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
        let id = WorkspaceId::new("test-ws").expect("operation should succeed");
        assert_eq!(format!("{id}"), "test-ws");
    }

    #[test]
    fn workspace_id_serde_roundtrip() {
        let id = WorkspaceId::new("my-workspace").expect("operation should succeed");
        let json = serde_json::to_string(&id).expect("operation should succeed");
        assert_eq!(json, "\"my-workspace\"");
        let decoded: WorkspaceId = serde_json::from_str(&json).expect("operation should succeed");
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
            let json = serde_json::to_string(&state).expect("operation should succeed");
            let decoded: WorkspaceState =
                serde_json::from_str(&json).expect("operation should succeed");
            assert_eq!(decoded, state);
        }
    }

    #[test]
    fn workspace_state_serde_tagged() {
        let json =
            serde_json::to_string(&WorkspaceState::Active).expect("operation should succeed");
        assert!(json.contains("\"state\":\"active\""));

        let json = serde_json::to_string(&WorkspaceState::Stale { behind_epochs: 1 })
            .expect("operation should succeed");
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
            let json = serde_json::to_string(&mode).expect("operation should succeed");
            let decoded: WorkspaceMode =
                serde_json::from_str(&json).expect("operation should succeed");
            assert_eq!(decoded, mode);
        }
    }

    // -- WorkspaceInfo --

    #[test]
    fn workspace_info_construction() {
        let info = WorkspaceInfo {
            id: WorkspaceId::new("test").expect("operation should succeed"),
            path: PathBuf::from("/tmp/ws/test"),
            epoch: EpochId::new(&"a".repeat(40)).expect("operation should succeed"),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::Ephemeral,
            commits_ahead: 0,
        };
        assert_eq!(info.id.as_str(), "test");
        assert_eq!(info.path, PathBuf::from("/tmp/ws/test"));
        assert!(info.state.is_active());
        assert!(info.mode.is_ephemeral());
    }

    #[test]
    fn workspace_info_persistent_mode() {
        let info = WorkspaceInfo {
            id: WorkspaceId::new("agent-1").expect("operation should succeed"),
            path: PathBuf::from("/repo/ws/agent-1"),
            epoch: EpochId::new(&"f".repeat(40)).expect("operation should succeed"),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::Persistent,
            commits_ahead: 0,
        };
        assert!(info.mode.is_persistent());
    }

    #[test]
    fn workspace_info_serde_roundtrip() {
        let info = WorkspaceInfo {
            id: WorkspaceId::new("agent-1").expect("operation should succeed"),
            path: PathBuf::from("/repo/ws/agent-1"),
            epoch: EpochId::new(&"f".repeat(40)).expect("operation should succeed"),
            state: WorkspaceState::Stale { behind_epochs: 2 },
            mode: WorkspaceMode::Persistent,
            commits_ahead: 0,
        };
        let json = serde_json::to_string(&info).expect("operation should succeed");
        let decoded: WorkspaceInfo = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, info);
    }

    #[test]
    fn workspace_info_serde_default_mode() {
        // mode field has default, so old JSON without it deserializes to Ephemeral
        let json = r#"{"id":"test","path":"/tmp/ws/test","epoch":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":{"state":"active"}}"#;
        let info: WorkspaceInfo = serde_json::from_str(json).expect("operation should succeed");
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

    // bn-2l63: the synthetic epoch-delta id is reserved at creation time
    // but still parses everywhere else.
    #[test]
    fn epoch_delta_reserved_for_create_only() {
        let err = WorkspaceId::new_for_create(WorkspaceId::EPOCH_DELTA)
            .expect_err("epoch-delta must be refused at creation");
        assert!(err.to_string().contains("reserved"), "{err}");
        assert!(WorkspaceId::new_for_create("epoch-delta-2").is_ok());
        assert!(WorkspaceId::new_for_create("agent-1").is_ok());
        assert!(WorkspaceId::new_for_create("Bad").is_err());
        // bn-ggo5: the quarantine prefix is reserved at creation too, but
        // existing quarantine names still parse.
        let err = WorkspaceId::new_for_create("merge-quarantine-foo")
            .expect_err("quarantine prefix must be refused at creation");
        assert!(err.to_string().contains("reserved"), "{err}");
        assert!(WorkspaceId::new_for_create("merge-quarantine").is_ok());
        assert!(WorkspaceId::new_for_create("my-merge-quarantine-x").is_ok());
        assert!(WorkspaceId::new("merge-quarantine-abc123def456").is_ok());

        // bn-asqh7: `undo` is reserved at creation (its destroy pins would
        // share refs/manifold/recovery/undo/ with `maw undo`'s redo pins),
        // but an existing legacy `undo` workspace still parses.
        let err =
            WorkspaceId::new_for_create("undo").expect_err("undo must be refused at creation");
        let msg = err.to_string();
        assert!(msg.contains("reserved"), "{msg}");
        assert!(msg.contains("maw undo"), "names the owner: {msg}");
        assert!(msg.contains("undo-work"), "suggests another name: {msg}");
        assert!(WorkspaceId::new("undo").is_ok());
        assert!(WorkspaceId::new_for_create("undo-work").is_ok());
        assert!(WorkspaceId::new_for_create("redo").is_ok());
        assert!(WorkspaceId::new_for_create("undoer").is_ok());

        let parsed = WorkspaceId::new(WorkspaceId::EPOCH_DELTA).expect("parses");
        assert!(parsed.is_epoch_delta());
        assert_eq!(parsed, WorkspaceId::epoch_delta());
        let via_str: WorkspaceId = "epoch-delta".parse().expect("FromStr parses");
        assert!(via_str.is_epoch_delta());
        let via_serde: WorkspaceId = serde_json::from_str("\"epoch-delta\"").expect("serde parses");
        assert!(via_serde.is_epoch_delta());
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Exactly the distinct bytes of `epoch-delta`, so symbolic names cover
    /// the reserved id, its prefixes, extensions and one-byte near misses.
    const ALPHABET: [u8; 10] = *b"epochdlta-";

    fn any_name_bytes<const N: usize>(buf: &mut [u8; N]) -> usize {
        let len: usize = kani::any();
        kani::assume(len <= N);
        let mut i = 0;
        while i < N {
            let k: usize = kani::any();
            kani::assume(k < ALPHABET.len());
            buf[i] = ALPHABET[k];
            i += 1;
        }
        len
    }

    /// The synthetic epoch-delta id is outside the set of names accepted for
    /// workspace creation, and the reservation removes nothing else: over
    /// every name of <= 12 bytes drawn from the letters of `epoch-delta`
    /// (a domain that cannot reach the 17-byte `merge-quarantine-` prefix
    /// reservation, which is covered by unit tests instead),
    /// the creation check accepts exactly what the parse check accepts
    /// minus `epoch-delta` itself, which the parse check does accept (so
    /// conflict sides / `--resolve <path>=epoch-delta` keep parsing).
    #[kani::proof]
    #[kani::unwind(14)]
    fn create_rejects_exactly_epoch_delta_names_le_12_bytes() {
        let mut buf = [0u8; 12];
        let len = any_name_bytes(&mut buf);
        let b = &buf[..len];
        let is_synthetic = b == WorkspaceId::EPOCH_DELTA.as_bytes();
        let parse_ok = WorkspaceId::check_name_bytes(b).is_ok();
        let create_ok = WorkspaceId::check_create_name_bytes(b).is_ok();
        if is_synthetic {
            assert!(parse_ok);
            assert!(!create_ok);
        }
        assert_eq!(create_ok, parse_ok && !is_synthetic);
    }
}
