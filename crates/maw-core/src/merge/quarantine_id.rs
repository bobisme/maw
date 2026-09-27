//! Pure identifier and path helpers for merge-quarantine workspaces.
//!
//! A quarantine is addressed by a `merge_id` that arrives from the command
//! line (`maw merge promote <id>`, `maw merge abandon <id>`) and is joined
//! into filesystem paths (`<manifold>/quarantine/<id>/`) and into a workspace
//! name (`merge-quarantine-<id>`). [`validate_merge_id`] is the single gate
//! that keeps a caller-supplied id from escaping those directories and keeps
//! the derived workspace name a valid [`WorkspaceId`].
//!
//! Ids produced by maw itself are the first 12 lowercase hex characters of
//! the candidate commit OID, which always pass.
//!
//! [`WorkspaceId`]: crate::model::types::WorkspaceId

use std::fmt;
use std::path::{Path, PathBuf};

use crate::model::types::WorkspaceId;

/// Prefix for quarantine workspace names.
pub const QUARANTINE_NAME_PREFIX: &str = "merge-quarantine-";

/// Subdirectory of the manifold dir that holds quarantine state directories.
pub const QUARANTINE_STATE_SUBDIR: &str = "quarantine";

/// Maximum merge-id length: the derived workspace name
/// (`merge-quarantine-<id>`) must fit in [`WorkspaceId::MAX_LEN`].
pub const MERGE_ID_MAX_LEN: usize = WorkspaceId::MAX_LEN - QUARANTINE_NAME_PREFIX.len();

/// A merge id was rejected by [`validate_merge_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMergeId {
    /// Human-readable reason (static so validation never allocates).
    pub reason: &'static str,
}

impl fmt::Display for InvalidMergeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for InvalidMergeId {}

/// Validate a quarantine `merge_id`.
///
/// Accepted ids are 1..=[`MERGE_ID_MAX_LEN`] bytes of `[a-z0-9-]`, not
/// starting or ending with `-`, and without `--`. This guarantees:
/// - the id is a single normal path component (no `/`, `.`, `..`, empty);
/// - `merge-quarantine-<id>` is a valid [`WorkspaceId`].
///
/// # Errors
///
/// Returns [`InvalidMergeId`] describing the first violated rule.
pub fn validate_merge_id(id: &str) -> Result<(), InvalidMergeId> {
    validate_merge_id_bytes(id.as_bytes())
}

/// Byte-level core of [`validate_merge_id`] (kept separate so bounded model
/// checking does not have to reason about UTF-8 decoding).
///
/// # Errors
///
/// Returns [`InvalidMergeId`] describing the first violated rule.
pub fn validate_merge_id_bytes(bytes: &[u8]) -> Result<(), InvalidMergeId> {
    if bytes.is_empty() {
        return Err(InvalidMergeId {
            reason: "merge id must not be empty",
        });
    }
    if bytes.len() > MERGE_ID_MAX_LEN {
        return Err(InvalidMergeId {
            reason: "merge id is too long (at most 47 characters)",
        });
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(InvalidMergeId {
            reason: "merge id may contain only lowercase letters (a-z), digits (0-9), and hyphens (-)",
        });
    }
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return Err(InvalidMergeId {
            reason: "merge id must not start or end with a hyphen",
        });
    }
    if bytes.windows(2).any(|w| w == b"--") {
        return Err(InvalidMergeId {
            reason: "merge id must not contain consecutive hyphens",
        });
    }
    Ok(())
}

/// Workspace name for a quarantine: `merge-quarantine-<merge_id>`.
///
/// Callers handling untrusted ids must call [`validate_merge_id`] first.
#[must_use]
pub fn quarantine_workspace_name(merge_id: &str) -> String {
    let mut s = String::with_capacity(QUARANTINE_NAME_PREFIX.len() + merge_id.len());
    s.push_str(QUARANTINE_NAME_PREFIX);
    s.push_str(merge_id);
    s
}

/// Extract the `merge_id` from a quarantine workspace name.
#[must_use]
pub fn merge_id_from_name(name: &str) -> Option<&str> {
    name.strip_prefix(QUARANTINE_NAME_PREFIX)
}

/// Directory holding all quarantine state dirs: `<manifold_dir>/quarantine`.
#[must_use]
pub fn quarantine_state_base(manifold_dir: &Path) -> PathBuf {
    manifold_dir.join(QUARANTINE_STATE_SUBDIR)
}

/// State directory for one quarantine: `<manifold_dir>/quarantine/<merge_id>`.
///
/// Callers handling untrusted ids must call [`validate_merge_id`] first.
#[must_use]
pub fn quarantine_state_dir(manifold_dir: &Path, merge_id: &str) -> PathBuf {
    quarantine_state_base(manifold_dir).join(merge_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Component;

    #[test]
    fn accepts_maw_generated_ids() {
        assert!(validate_merge_id("c52ffd2c3714").is_ok());
        assert!(validate_merge_id("0").is_ok());
        assert!(validate_merge_id("a-b").is_ok());
    }

    #[test]
    fn rejects_traversal_and_separators() {
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "../../victim",
            "a/b",
            "/abs",
            "a\\b",
            "A",
            "a.b",
            "-a",
            "a-",
            "a--b",
            " a",
            "a\0",
        ] {
            assert!(
                validate_merge_id(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn length_boundary_matches_workspace_id() {
        assert_eq!(MERGE_ID_MAX_LEN, 47);
        let ok = "a".repeat(MERGE_ID_MAX_LEN);
        assert!(validate_merge_id(&ok).is_ok());
        assert!(WorkspaceId::new(&quarantine_workspace_name(&ok)).is_ok());
        let too_long = "a".repeat(MERGE_ID_MAX_LEN + 1);
        assert!(validate_merge_id(&too_long).is_err());
        assert!(WorkspaceId::new(&quarantine_workspace_name(&too_long)).is_err());
    }

    /// Exhaustive over every id of at most 7 bytes drawn from the adversarial
    /// alphabet {'.', '/', '-', 'a', '0'} (97 656 ids), using the real
    /// `std::path` and `WorkspaceId` code that is too heavy for Kani: every
    /// accepted id yields a state dir exactly one `Normal` component below the
    /// quarantine base, and a workspace name that round-trips and is a valid
    /// `WorkspaceId`. Also checks the accept set is non-trivial.
    #[test]
    fn exhaustive_ids_le_7_bytes_are_contained_and_named() {
        const ALPHABET: [u8; 5] = *b"./-a0";
        let base = Path::new("/m");
        let state_base = quarantine_state_base(base);
        let mut accepted = 0usize;
        let mut buf = Vec::with_capacity(7);
        for len in 0..=7u32 {
            for mut n in 0..5usize.pow(len) {
                buf.clear();
                for _ in 0..len {
                    buf.push(ALPHABET[n % 5]);
                    n /= 5;
                }
                let id = std::str::from_utf8(&buf).expect("ascii");
                if validate_merge_id(id).is_err() {
                    continue;
                }
                accepted += 1;
                let dir = quarantine_state_dir(base, id);
                let rel = dir.strip_prefix(&state_base).expect("under base");
                let comps: Vec<_> = rel.components().collect();
                assert!(
                    comps.len() == 1 && matches!(comps[0], Component::Normal(_)),
                    "{id:?} escapes: {comps:?}"
                );
                let name = quarantine_workspace_name(id);
                assert_eq!(merge_id_from_name(&name), Some(id));
                assert!(WorkspaceId::new(&name).is_ok(), "{name:?} invalid");
            }
        }
        assert!(accepted > 1000, "accept set suspiciously small: {accepted}");
    }

    #[test]
    fn valid_id_is_one_normal_component() {
        let base = Path::new("/m");
        let dir = quarantine_state_dir(base, "abc123");
        let rel = dir
            .strip_prefix(quarantine_state_base(base))
            .expect("under base");
        let comps: Vec<_> = rel.components().collect();
        assert_eq!(comps.len(), 1);
        assert!(matches!(comps[0], Component::Normal(_)));
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Adversarial alphabet: path separator, dot (for `.`/`..`), hyphen
    /// (leading/trailing/double), and one representative of each legal class.
    const ALPHABET: [u8; 5] = *b"./-a0";

    /// Fill `buf` with symbolic bytes from [`ALPHABET`] and return a symbolic
    /// length `0..=N`.
    fn any_id_bytes<const N: usize>(buf: &mut [u8; N]) -> usize {
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

    /// Containment: every id (<= 8 bytes over the adversarial alphabet) that
    /// `validate_merge_id` accepts is a single normal path component: non-empty,
    /// no `/`, and neither `.` nor `..`. Joining it onto the quarantine base
    /// therefore stays exactly one level below that base.
    ///
    /// (`std::path::Path` itself is too heavy for CBMC; the `Path`-level
    /// statement over the same domain is checked exhaustively by the unit
    /// test `exhaustive_ids_le_7_bytes_are_contained_and_named`.)
    #[kani::proof]
    #[kani::unwind(10)]
    fn validated_merge_id_is_single_component_ids_le_8_bytes() {
        let mut buf = [0u8; 8];
        let len = any_id_bytes(&mut buf);
        let b = &buf[..len];
        if validate_merge_id_bytes(b).is_ok() {
            assert!(!b.is_empty());
            assert!(!b.contains(&b'/'));
            assert!(b != b"." && b != b"..");
        }
    }
}
