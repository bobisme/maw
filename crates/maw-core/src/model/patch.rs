//! Patch-set model — core data types (§5.4, §5.8).
//!
//! Instead of storing a full git tree per workspace, Manifold records only
//! the files that changed. This makes workspace state proportional to
//! *changed files*, not repo size.
//!
//! Key types:
//! - [`FileId`] — stable identity that survives renames (§5.8)
//! - [`PatchSet`] — epoch + `BTreeMap` of path → change
//! - [`PatchValue`] — the four kinds of change (Add, Delete, Modify, Rename)

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::{EpochId, GitOid};

// ---------------------------------------------------------------------------
// FileId
// ---------------------------------------------------------------------------

/// A stable file identity that persists across renames and moves (§5.8).
///
/// A `FileId` is assigned when a file is first created and never changes,
/// even if the file is renamed or moved. This makes rename-aware merge
/// possible without heuristics.
///
/// Internally stored as a `u128` and serialized as a 32-character lowercase
/// hex string for canonical JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FileId(u128);

impl FileId {
    /// Create a `FileId` from a raw `u128`.
    #[must_use]
    pub const fn new(id: u128) -> Self {
        Self(id)
    }

    /// Generate a cryptographically-random `FileId`.
    ///
    /// Uses the thread-local PRNG (rand 0.9). Each call produces a unique
    /// 128-bit random identifier suitable for stable file identity.
    #[must_use]
    pub fn random() -> Self {
        Self(rand::random::<u128>())
    }

    /// Return the inner `u128` value.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    /// Parse a `FileId` from a 32-character lowercase hex string.
    ///
    /// # Errors
    /// Returns an error if the string is not exactly 32 lowercase hex digits.
    pub fn from_hex(s: &str) -> Result<Self, FileIdError> {
        match Self::decode_hex_bytes(s.as_bytes()) {
            Ok(n) => Ok(Self(n)),
            Err(FileIdHexError::Length) => Err(FileIdError {
                value: s.to_owned(),
                reason: format!("expected 32 hex characters, got {}", s.len()),
            }),
            Err(FileIdHexError::Digit) => Err(FileIdError {
                value: s.to_owned(),
                reason: "must contain only lowercase hex characters (0-9, a-f)".to_owned(),
            }),
        }
    }

    /// Return a 32-character lowercase hex representation of this `FileId`.
    #[must_use]
    pub fn to_hex(self) -> String {
        let bytes = Self::encode_hex_bytes(self.0);
        bytes.iter().map(|&b| char::from(b)).collect()
    }

    /// Allocation-free core of [`Self::to_hex`]: the 32 lowercase hex digits
    /// of `n`, most significant first (bn-2l63: kept byte-level so Kani can
    /// prove the codec over all 2^128 ids).
    #[must_use]
    pub const fn encode_hex_bytes(n: u128) -> [u8; 32] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            let shift = 4 * (31 - i);
            // Masked to 0..=15, so the cast is lossless.
            #[allow(clippy::cast_possible_truncation)]
            let nibble = ((n >> shift) & 0xf) as usize;
            out[i] = DIGITS[nibble];
            i += 1;
        }
        out
    }

    /// Allocation-free core of [`Self::from_hex`]: accepts exactly 32
    /// lowercase hex digits.
    ///
    /// # Errors
    /// [`FileIdHexError::Length`] unless `b.len() == 32`, else
    /// [`FileIdHexError::Digit`] on any byte outside `[0-9a-f]`.
    pub fn decode_hex_bytes(b: &[u8]) -> Result<u128, FileIdHexError> {
        if b.len() != 32 {
            return Err(FileIdHexError::Length);
        }
        let mut n: u128 = 0;
        let mut i = 0;
        while i < 32 {
            let d = match b[i] {
                c @ b'0'..=b'9' => c - b'0',
                c @ b'a'..=b'f' => c - b'a' + 10,
                _ => return Err(FileIdHexError::Digit),
            };
            n = (n << 4) | u128::from(d);
            i += 1;
        }
        Ok(n)
    }
}

/// Why [`FileId::decode_hex_bytes`] rejected its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileIdHexError {
    /// Input is not exactly 32 bytes.
    Length,
    /// A byte is not a lowercase hex digit.
    Digit,
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl TryFrom<String> for FileId {
    type Error = FileIdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::from_hex(&s)
    }
}

impl From<FileId> for String {
    fn from(id: FileId) -> Self {
        id.to_hex()
    }
}

/// Error returned when a `FileId` string is malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdError {
    /// The invalid value.
    pub value: String,
    /// Human-readable explanation.
    pub reason: String,
}

impl fmt::Display for FileIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid FileId: {:?} — {}", self.value, self.reason)
    }
}

impl std::error::Error for FileIdError {}

// ---------------------------------------------------------------------------
// PatchSet
// ---------------------------------------------------------------------------

/// A workspace's changed state relative to a base epoch (§5.4).
///
/// A `PatchSet` records only the files that changed between the base epoch
/// and the current working directory. Snapshot cost is O(changed files), not
/// O(repo size).
///
/// The `patches` map uses [`BTreeMap`] to guarantee **deterministic iteration
/// order** for canonical JSON serialization and hashing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchSet {
    /// The epoch these patches are relative to.
    pub base_epoch: EpochId,
    /// Changed paths, sorted for determinism.
    pub patches: BTreeMap<PathBuf, PatchValue>,
}

impl PatchSet {
    /// Create an empty `PatchSet` relative to the given epoch.
    #[must_use]
    pub const fn empty(base_epoch: EpochId) -> Self {
        Self {
            base_epoch,
            patches: BTreeMap::new(),
        }
    }

    /// Return `true` if no paths are changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
    }

    /// Return the number of changed paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.patches.len()
    }
}

// ---------------------------------------------------------------------------
// PatchValue
// ---------------------------------------------------------------------------

/// The change applied to a single path within a [`PatchSet`] (§5.4).
///
/// Serialized with a `"op"` tag for canonical JSON:
/// `{"op":"add","blob":"…","file_id":"…"}` etc.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchValue {
    /// File was created at this path.
    Add {
        /// Blob OID for the new file content.
        blob: GitOid,
        /// Stable identity assigned at file creation.
        file_id: FileId,
    },
    /// File was removed from this path.
    Delete {
        /// Blob OID the file had before deletion (needed for undo).
        previous_blob: GitOid,
        /// Stable identity of the deleted file.
        file_id: FileId,
    },
    /// File content was changed in place.
    Modify {
        /// Blob OID the file had before the modification (needed for undo).
        base_blob: GitOid,
        /// Blob OID for the new file content.
        new_blob: GitOid,
        /// Stable file identity (unchanged by a modify).
        file_id: FileId,
    },
    /// File was moved (and optionally also modified).
    ///
    /// The path key in [`PatchSet::patches`] is the **destination** path.
    /// `from` records the **source** path.
    Rename {
        /// Source path the file was moved from.
        from: PathBuf,
        /// Stable file identity (unchanged by a rename).
        file_id: FileId,
        /// New blob OID if the content was also changed during the rename.
        /// `None` means the content is identical to the epoch's blob.
        new_blob: Option<GitOid>,
    },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    // bn-2l63: the hand-written codec matches the previous format!/radix one.
    #[test]
    fn file_id_hex_codec_matches_std_formatting() {
        for n in [
            0u128,
            1,
            0xf,
            0xabc,
            u128::MAX,
            u128::MAX - 1,
            1 << 127,
            0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
        ] {
            let id = FileId::new(n);
            assert_eq!(id.to_hex(), format!("{n:032x}"));
            assert_eq!(FileId::from_hex(&id.to_hex()), Ok(id));
        }
        assert!(FileId::from_hex(&"A".repeat(32)).is_err());
        assert!(FileId::from_hex(&"0".repeat(31)).is_err());
        assert!(FileId::from_hex(&"0".repeat(33)).is_err());
        assert!(FileId::from_hex(&format!("+{}", "0".repeat(31))).is_err());
    }

    use super::*;

    // Helper: build a valid 40-char hex OID string.
    fn oid(c: char) -> String {
        c.to_string().repeat(40)
    }

    // Helper: build a valid 40-char hex EpochId.
    fn epoch(c: char) -> EpochId {
        EpochId::new(&oid(c)).expect("operation should succeed")
    }

    // Helper: build a valid GitOid.
    fn git_oid(c: char) -> GitOid {
        GitOid::new(&oid(c)).expect("operation should succeed")
    }

    // -----------------------------------------------------------------------
    // FileId tests
    // -----------------------------------------------------------------------

    #[test]
    fn file_id_round_trip_u128() {
        let id = FileId::new(42);
        assert_eq!(id.as_u128(), 42);
    }

    #[test]
    fn file_id_display_is_32_hex_chars() {
        let id = FileId::new(0);
        let s = format!("{id}");
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn file_id_to_hex_round_trip() {
        for n in [0_u128, 1, u128::from(u64::MAX), u128::MAX] {
            let id = FileId::new(n);
            let hex = id.to_hex();
            let decoded = FileId::from_hex(&hex).expect("operation should succeed");
            assert_eq!(decoded, id);
        }
    }

    #[test]
    fn file_id_from_hex_rejects_short() {
        assert!(FileId::from_hex("abc").is_err());
    }

    #[test]
    fn file_id_from_hex_rejects_long() {
        assert!(FileId::from_hex(&"a".repeat(33)).is_err());
    }

    #[test]
    fn file_id_from_hex_rejects_uppercase() {
        let hex = "A".repeat(32);
        assert!(FileId::from_hex(&hex).is_err());
    }

    #[test]
    fn file_id_from_hex_rejects_non_hex() {
        let bad = "z".repeat(32);
        assert!(FileId::from_hex(&bad).is_err());
    }

    #[test]
    fn file_id_serde_round_trip() {
        let id = FileId::new(0xdead_beef_cafe);
        let json = serde_json::to_string(&id).expect("operation should succeed");
        // Serialized as quoted hex string.
        assert!(json.starts_with('"'));
        let decoded: FileId = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, id);
    }

    #[test]
    fn file_id_serde_rejects_invalid() {
        let json = "\"not-a-valid-id\"";
        assert!(serde_json::from_str::<FileId>(json).is_err());
    }

    #[test]
    fn file_id_zero_display() {
        assert_eq!(FileId::new(0).to_hex(), "0".repeat(32));
    }

    #[test]
    fn file_id_max_display() {
        assert_eq!(FileId::new(u128::MAX).to_hex(), "f".repeat(32));
    }

    // -----------------------------------------------------------------------
    // PatchSet tests
    // -----------------------------------------------------------------------

    #[test]
    fn patch_set_empty() {
        let ps = PatchSet::empty(epoch('1'));
        assert!(ps.is_empty());
        assert_eq!(ps.len(), 0);
    }

    #[test]
    fn patch_set_len_and_is_empty() {
        let mut ps = PatchSet::empty(epoch('2'));
        ps.patches.insert(
            PathBuf::from("src/main.rs"),
            PatchValue::Add {
                blob: git_oid('a'),
                file_id: FileId::new(1),
            },
        );
        assert!(!ps.is_empty());
        assert_eq!(ps.len(), 1);
    }

    #[test]
    fn patch_set_btreemap_is_sorted() {
        let mut ps = PatchSet::empty(epoch('3'));
        // Insert in reverse order.
        ps.patches.insert(
            PathBuf::from("z.rs"),
            PatchValue::Add {
                blob: git_oid('a'),
                file_id: FileId::new(10),
            },
        );
        ps.patches.insert(
            PathBuf::from("a.rs"),
            PatchValue::Add {
                blob: git_oid('b'),
                file_id: FileId::new(11),
            },
        );

        // BTreeMap iteration is always sorted.
        let keys: Vec<_> = ps.patches.keys().collect();
        assert_eq!(keys[0], &PathBuf::from("a.rs"));
        assert_eq!(keys[1], &PathBuf::from("z.rs"));
    }

    #[test]
    fn patch_set_serde_round_trip_empty() {
        let ps = PatchSet::empty(epoch('4'));
        let json = serde_json::to_string(&ps).expect("operation should succeed");
        let decoded: PatchSet = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, ps);
    }

    #[test]
    fn patch_set_serde_round_trip_with_entries() {
        let mut ps = PatchSet::empty(epoch('5'));
        ps.patches.insert(
            PathBuf::from("src/lib.rs"),
            PatchValue::Modify {
                base_blob: git_oid('b'),
                new_blob: git_oid('c'),
                file_id: FileId::new(99),
            },
        );
        ps.patches.insert(
            PathBuf::from("README.md"),
            PatchValue::Delete {
                previous_blob: git_oid('d'),
                file_id: FileId::new(100),
            },
        );

        let json = serde_json::to_string(&ps).expect("operation should succeed");
        let decoded: PatchSet = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, ps);
    }

    // -----------------------------------------------------------------------
    // PatchValue tests — construction + serde round-trip for each variant
    // -----------------------------------------------------------------------

    #[test]
    fn patch_value_add_round_trip() {
        let pv = PatchValue::Add {
            blob: git_oid('a'),
            file_id: FileId::new(1),
        };
        let json = serde_json::to_string(&pv).expect("operation should succeed");
        // Tagged with "op":"add"
        assert!(json.contains("\"op\":\"add\""));
        let decoded: PatchValue = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, pv);
    }

    #[test]
    fn patch_value_delete_round_trip() {
        let pv = PatchValue::Delete {
            previous_blob: git_oid('b'),
            file_id: FileId::new(2),
        };
        let json = serde_json::to_string(&pv).expect("operation should succeed");
        assert!(json.contains("\"op\":\"delete\""));
        let decoded: PatchValue = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, pv);
    }

    #[test]
    fn patch_value_modify_round_trip() {
        let pv = PatchValue::Modify {
            base_blob: git_oid('c'),
            new_blob: git_oid('d'),
            file_id: FileId::new(3),
        };
        let json = serde_json::to_string(&pv).expect("operation should succeed");
        assert!(json.contains("\"op\":\"modify\""));
        let decoded: PatchValue = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, pv);
    }

    #[test]
    fn patch_value_rename_no_content_change_round_trip() {
        let pv = PatchValue::Rename {
            from: PathBuf::from("old/path.rs"),
            file_id: FileId::new(4),
            new_blob: None,
        };
        let json = serde_json::to_string(&pv).expect("operation should succeed");
        assert!(json.contains("\"op\":\"rename\""));
        assert!(json.contains("\"new_blob\":null"));
        let decoded: PatchValue = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, pv);
    }

    #[test]
    fn patch_value_rename_with_content_change_round_trip() {
        let pv = PatchValue::Rename {
            from: PathBuf::from("old/path.rs"),
            file_id: FileId::new(5),
            new_blob: Some(git_oid('e')),
        };
        let json = serde_json::to_string(&pv).expect("operation should succeed");
        assert!(json.contains("\"op\":\"rename\""));
        let decoded: PatchValue = serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(decoded, pv);
    }

    #[test]
    fn patch_value_serde_tagged() {
        // Confirm the "op" tag is present in all variants.
        let variants: &[PatchValue] = &[
            PatchValue::Add {
                blob: git_oid('a'),
                file_id: FileId::new(10),
            },
            PatchValue::Delete {
                previous_blob: git_oid('b'),
                file_id: FileId::new(11),
            },
            PatchValue::Modify {
                base_blob: git_oid('c'),
                new_blob: git_oid('d'),
                file_id: FileId::new(12),
            },
            PatchValue::Rename {
                from: PathBuf::from("foo.rs"),
                file_id: FileId::new(13),
                new_blob: None,
            },
        ];

        for pv in variants {
            let json = serde_json::to_string(pv).expect("operation should succeed");
            assert!(json.contains("\"op\":"), "Missing 'op' tag in: {json}");
            let decoded: PatchValue =
                serde_json::from_str(&json).expect("operation should succeed");
            assert_eq!(&decoded, pv);
        }
    }

    #[test]
    fn patch_set_json_is_deterministic() {
        // Two PatchSets with the same content should serialize identically.
        let make = || {
            let mut ps = PatchSet::empty(epoch('6'));
            ps.patches.insert(
                PathBuf::from("b.rs"),
                PatchValue::Add {
                    blob: git_oid('1'),
                    file_id: FileId::new(20),
                },
            );
            ps.patches.insert(
                PathBuf::from("a.rs"),
                PatchValue::Add {
                    blob: git_oid('2'),
                    file_id: FileId::new(21),
                },
            );
            ps
        };
        let json1 = serde_json::to_string(&make()).expect("operation should succeed");
        let json2 = serde_json::to_string(&make()).expect("operation should succeed");
        assert_eq!(json1, json2);
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// The hex codec round-trips every one of the 2^128 ids: `encode` then
    /// `decode` is the identity (hence `to_hex` is injective).
    #[kani::proof]
    #[kani::unwind(33)]
    fn file_id_hex_round_trip_all_u128() {
        let n: u128 = kani::any();
        let hex = FileId::encode_hex_bytes(n);
        assert_eq!(FileId::decode_hex_bytes(&hex), Ok(n));
    }

    /// `decode` accepts only canonical encodings: for every string of
    /// <= 33 bytes over `0`, `9`, `a`, `f`, `A`, `g`, `/`, `:` and a backtick (range edges,
    /// uppercase, and the bytes just outside each digit range), an accepted string is
    /// exactly `encode` of the decoded value. Hence `from_hex` is injective:
    /// two distinct strings never parse to the same `FileId`.
    #[kani::proof]
    #[kani::unwind(34)]
    fn file_id_decode_accepts_only_canonical_hex_le_33_bytes() {
        const ALPHABET: [u8; 9] = *b"09afAg/:`";
        let mut buf = [0u8; 33];
        let len: usize = kani::any();
        kani::assume(len <= buf.len());
        let mut i = 0;
        while i < buf.len() {
            let k: usize = kani::any();
            kani::assume(k < ALPHABET.len());
            buf[i] = ALPHABET[k];
            i += 1;
        }
        let b = &buf[..len];
        if let Ok(n) = FileId::decode_hex_bytes(b) {
            assert!(FileId::encode_hex_bytes(n).as_slice() == b);
        }
    }
}
