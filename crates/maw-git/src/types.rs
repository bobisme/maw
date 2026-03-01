//! Core types for the maw git abstraction layer.
//!
//! These types form the vocabulary shared between the [`GitRepo`](crate::GitRepo) trait and
//! all maw crates. They intentionally contain no gix (or libgit2, or CLI)
//! types — the backend is an implementation detail.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// GitOid
// ---------------------------------------------------------------------------

/// A git object identifier (SHA-1, 20 bytes).
///
/// Stored as raw bytes for efficient comparison, hashing, and Copy semantics.
/// Displays as 40 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitOid([u8; 20]);

impl GitOid {
    /// The zero OID (`0000...0000`), used as a sentinel for "ref does not exist."
    pub const ZERO: Self = Self([0; 20]);

    /// Create a `GitOid` from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Return the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Return `true` if this is the zero OID.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        *self == Self::ZERO
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitOid({self})")
    }
}

impl FromStr for GitOid {
    type Err = OidParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 40 {
            return Err(OidParseError {
                value: s.to_owned(),
                reason: format!("expected 40 hex characters, got {}", s.len()),
            });
        }
        let mut bytes = [0u8; 20];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_digit(chunk[0]).ok_or_else(|| OidParseError {
                value: s.to_owned(),
                reason: format!("invalid hex digit '{}'", chunk[0] as char),
            })?;
            let lo = hex_digit(chunk[1]).ok_or_else(|| OidParseError {
                value: s.to_owned(),
                reason: format!("invalid hex digit '{}'", chunk[1] as char),
            })?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

/// Error from parsing a hex string into a [`GitOid`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidParseError {
    /// The raw value that failed.
    pub value: String,
    /// Why it failed.
    pub reason: String,
}

impl fmt::Display for OidParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid OID {:?}: {}", self.value, self.reason)
    }
}

impl std::error::Error for OidParseError {}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        // Accept uppercase for leniency during parsing
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// RefName
// ---------------------------------------------------------------------------

/// A validated git ref name.
///
/// Must start with `refs/` or be one of the well-known bare names (`HEAD`,
/// `FETCH_HEAD`, `MERGE_HEAD`, etc.).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RefName(String);

/// Well-known bare ref names that don't start with `refs/`.
const BARE_REFS: &[&str] = &["HEAD", "FETCH_HEAD", "MERGE_HEAD", "ORIG_HEAD", "CHERRY_PICK_HEAD"];

impl RefName {
    /// Create a new `RefName`, validating that it looks like a git ref.
    ///
    /// # Errors
    /// Returns an error if the name is empty, doesn't start with `refs/`,
    /// and isn't a well-known bare ref.
    pub fn new(name: &str) -> Result<Self, RefNameError> {
        Self::validate(name)?;
        Ok(Self(name.to_owned()))
    }

    /// Return the ref name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(name: &str) -> Result<(), RefNameError> {
        if name.is_empty() {
            return Err(RefNameError {
                value: name.to_owned(),
                reason: "ref name must not be empty".to_owned(),
            });
        }
        if name.starts_with("refs/") || BARE_REFS.contains(&name) {
            Ok(())
        } else {
            Err(RefNameError {
                value: name.to_owned(),
                reason: "ref name must start with 'refs/' or be a well-known ref (HEAD, etc.)"
                    .to_owned(),
            })
        }
    }
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RefName {
    type Err = RefNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Error from validating a [`RefName`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefNameError {
    /// The invalid value.
    pub value: String,
    /// Why it was rejected.
    pub reason: String,
}

impl fmt::Display for RefNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ref name {:?}: {}", self.value, self.reason)
    }
}

impl std::error::Error for RefNameError {}

// ---------------------------------------------------------------------------
// RefEdit
// ---------------------------------------------------------------------------

/// A single ref update for use in atomic ref transactions.
///
/// Encodes the ref name, the expected new OID, and the expected old OID for
/// compare-and-swap semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefEdit {
    /// The ref to update.
    pub name: RefName,
    /// The new OID to set the ref to.
    pub new_oid: GitOid,
    /// The expected current OID (for CAS). Use [`GitOid::ZERO`] to assert
    /// that the ref must not already exist.
    pub expected_old_oid: GitOid,
}

// ---------------------------------------------------------------------------
// Tree types
// ---------------------------------------------------------------------------

/// The file mode of a tree entry (analogous to `git ls-tree` mode column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryMode {
    /// Regular file (`100644`).
    Blob,
    /// Executable file (`100755`).
    BlobExecutable,
    /// Subdirectory (`040000`).
    Tree,
    /// Symbolic link (`120000`).
    Link,
    /// Gitlink / submodule (`160000`).
    Commit,
}

/// A single entry in a git tree object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// File or directory name (just the basename, not a full path).
    pub name: String,
    /// The entry mode.
    pub mode: EntryMode,
    /// The OID of the blob, tree, or commit this entry points to.
    pub oid: GitOid,
}

/// An edit operation on a tree.
///
/// Used with [`GitRepo::edit_tree`](crate::GitRepo::edit_tree) to build a new
/// tree from an existing one by inserting, updating, or removing entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeEdit {
    /// Insert or update an entry. If a path component is missing, intermediate
    /// trees are created automatically.
    Upsert {
        /// Slash-separated path relative to tree root (e.g., `"src/main.rs"`).
        path: String,
        /// File mode for the entry.
        mode: EntryMode,
        /// OID of the object to store at this path.
        oid: GitOid,
    },
    /// Remove an entry. No-op if the path does not exist.
    Remove {
        /// Slash-separated path relative to tree root.
        path: String,
    },
}

// ---------------------------------------------------------------------------
// Diff types
// ---------------------------------------------------------------------------

/// The kind of change detected between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeType {
    /// File was added.
    Added,
    /// File content or mode was modified.
    Modified,
    /// File was deleted.
    Deleted,
    /// File was renamed (may also be modified).
    Renamed {
        /// The original path before the rename.
        from: String,
    },
}

/// A single file-level change between two trees.
///
/// Produced by [`GitRepo::diff_trees`](crate::GitRepo::diff_trees).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffEntry {
    /// Path of the file (in the new tree, or the old tree for deletions).
    pub path: String,
    /// What kind of change occurred.
    pub change_type: ChangeType,
    /// OID of the old blob (zero OID for additions).
    pub old_oid: GitOid,
    /// OID of the new blob (zero OID for deletions).
    pub new_oid: GitOid,
    /// File mode in the old tree.
    pub old_mode: Option<EntryMode>,
    /// File mode in the new tree.
    pub new_mode: Option<EntryMode>,
}

// ---------------------------------------------------------------------------
// Status types
// ---------------------------------------------------------------------------

/// The status of a single file in the working tree relative to HEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    /// File content differs from HEAD.
    Modified,
    /// File is tracked in the index but not in HEAD.
    Added,
    /// File is in HEAD but missing from the working tree.
    Deleted,
    /// File exists in the working tree but is not tracked.
    Untracked,
    /// File was renamed.
    Renamed,
}

/// A single entry from `git status`, pairing a file path with its status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// Path relative to the repository root.
    pub path: String,
    /// The status of the file.
    pub status: FileStatus,
}

// ---------------------------------------------------------------------------
// Index types
// ---------------------------------------------------------------------------

/// A single entry in the git index (staging area).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Path relative to the repository root.
    pub path: String,
    /// The file mode.
    pub mode: EntryMode,
    /// OID of the blob in the index.
    pub oid: GitOid,
}

// ---------------------------------------------------------------------------
// Worktree types
// ---------------------------------------------------------------------------

/// Information about a git worktree.
///
/// Produced by [`GitRepo::worktree_list`](crate::GitRepo::worktree_list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// The worktree name (for linked worktrees) or `"main"` for the main worktree.
    pub name: String,
    /// Absolute path to the worktree root directory.
    pub path: PathBuf,
    /// The OID that HEAD points to in this worktree.
    pub head_oid: Option<GitOid>,
    /// `true` if HEAD is detached (not on a branch).
    pub is_detached: bool,
}

// ---------------------------------------------------------------------------
// Commit types
// ---------------------------------------------------------------------------

/// Information about a commit object.
///
/// Returned by [`GitRepo::read_commit`](crate::GitRepo::read_commit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    /// OID of the tree this commit points to.
    pub tree_oid: GitOid,
    /// OIDs of parent commits (empty for root commits).
    pub parents: Vec<GitOid>,
    /// The commit message.
    pub message: String,
    /// Author identity string (e.g., `"Alice <alice@example.com>"`).
    pub author: String,
    /// Committer identity string.
    pub committer: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- GitOid --

    #[test]
    fn oid_roundtrip_hex() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let oid: GitOid = hex.parse().unwrap();
        assert_eq!(oid.to_string(), hex);
    }

    #[test]
    fn oid_zero() {
        assert!(GitOid::ZERO.is_zero());
        assert_eq!(
            GitOid::ZERO.to_string(),
            "0000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn oid_rejects_short() {
        assert!("abc".parse::<GitOid>().is_err());
    }

    #[test]
    fn oid_rejects_non_hex() {
        let bad = "g".repeat(40);
        assert!(bad.parse::<GitOid>().is_err());
    }

    #[test]
    fn oid_copy_semantics() {
        let hex = "a".repeat(40);
        let oid: GitOid = hex.parse().unwrap();
        let copy = oid; // Copy
        assert_eq!(oid, copy);
    }

    #[test]
    fn oid_from_bytes() {
        let bytes = [0xab; 20];
        let oid = GitOid::from_bytes(bytes);
        assert_eq!(oid.as_bytes(), &bytes);
        assert_eq!(oid.to_string(), "ab".repeat(20));
    }

    // -- RefName --

    #[test]
    fn refname_valid_refs_prefix() {
        assert!(RefName::new("refs/heads/main").is_ok());
        assert!(RefName::new("refs/manifold/epoch/current").is_ok());
    }

    #[test]
    fn refname_valid_head() {
        assert!(RefName::new("HEAD").is_ok());
    }

    #[test]
    fn refname_rejects_bare() {
        assert!(RefName::new("main").is_err());
    }

    #[test]
    fn refname_rejects_empty() {
        assert!(RefName::new("").is_err());
    }

    #[test]
    fn refname_display() {
        let r = RefName::new("refs/heads/main").unwrap();
        assert_eq!(r.to_string(), "refs/heads/main");
    }
}
