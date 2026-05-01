//! Text 3-way merge using gix-merge's built-in text driver.
//!
//! Provides [`merge_text`] — a pure-Rust 3-way merge that cleanly merges
//! non-overlapping edits and produces diff3-style conflict markers for
//! true conflicts. No git CLI subprocess required.
//!
//! Also provides [`merge_text_with_style`] for selecting the conflict
//! resolution strategy per-file, honoring `.gitattributes` merge drivers
//! (e.g., `merge=union` for append-only files like logs and CHANGELOGs).

use gix::bstr::ByteSlice;
use gix::diff::blob::intern::InternedInput;
use gix::merge::blob::Resolution;
use gix::merge::blob::builtin_driver;
use gix::merge::blob::builtin_driver::text::{ConflictStyle, Labels, Options};

use crate::error::GitError;

/// Result of a 3-way text merge.
#[derive(Debug, Clone)]
pub enum MergeResult {
    /// All changes merged cleanly — no conflicts.
    Clean(Vec<u8>),
    /// Conflicts remain — output contains diff3-style markers.
    Conflict(Vec<u8>),
}

/// Strategy for resolving conflict hunks in a 3-way text merge.
///
/// Corresponds to git's built-in merge drivers from `.gitattributes`:
/// - [`Diff3`](Self::Diff3) — default; mark conflicts with diff3 markers
/// - [`Union`](Self::Union) — place both sides one after another (`merge=union`)
/// - [`Ours`](Self::Ours) — always prefer the `ours` side (`merge=ours`)
/// - [`Theirs`](Self::Theirs) — always prefer the `theirs` side
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Default: keep both sides with diff3-style conflict markers.
    Diff3,
    /// `merge=union`: concatenate both sides line-by-line, no markers.
    /// Perfect for append-only files (logs, CHANGELOGs, events).
    Union,
    /// `merge=ours`: always pick the `ours` side in conflict hunks.
    Ours,
    /// Pick the `theirs` side in conflict hunks. Not a standard git driver,
    /// but mirrors `merge=ours` with the argument order flipped.
    Theirs,
}

/// Decide how to merge a file given its `.gitattributes` `merge=` driver name.
///
/// Returns the resolution strategy for the named driver:
/// - `"union"` → [`ConflictResolution::Union`]
/// - `"ours"` → [`ConflictResolution::Ours`]
/// - `"binary"` / `"-text"` / `"unset"` → returns `None` to signal "don't
///   attempt a 3-way text merge — treat as a hard conflict"
/// - `None`, `"text"`, or any unknown custom driver → [`ConflictResolution::Diff3`]
///
/// Unknown custom drivers fall back to diff3 rather than failing loudly, so
/// adding a `merge=my-thing` entry in `.gitattributes` won't break merges
/// even if maw doesn't recognize the driver.
#[must_use]
pub fn resolution_for_driver(driver: Option<&str>) -> Option<ConflictResolution> {
    match driver {
        None | Some("text") => Some(ConflictResolution::Diff3),
        Some("union") => Some(ConflictResolution::Union),
        Some("ours") => Some(ConflictResolution::Ours),
        Some("binary") => None,
        Some(_other) => Some(ConflictResolution::Diff3),
    }
}

/// Perform a 3-way text merge of `ours` and `theirs` against `base`.
///
/// Uses the same algorithm as `git merge-file --diff3`:
/// - Non-overlapping changes from both sides are combined.
/// - Overlapping (conflicting) changes get diff3-style markers with labels.
///
/// Labels are applied to conflict markers as:
///   `<<<<<<< {ours_label}` / `||||||| {base_label}` / `>>>>>>> {theirs_label}`
///
/// # Errors
/// Returns an error if the merge backend cannot process the supplied inputs.
pub fn merge_text(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
) -> Result<MergeResult, GitError> {
    merge_text_with_style(
        base,
        ours,
        theirs,
        ours_label,
        base_label,
        theirs_label,
        ConflictResolution::Diff3,
    )
}

/// Perform a 3-way text merge with a selectable conflict-resolution strategy.
///
/// This is the gitattributes-aware entry point. Callers pass the resolution
/// chosen for the file (via [`resolution_for_driver`]). When the strategy is
/// [`ConflictResolution::Union`], [`ConflictResolution::Ours`], or
/// [`ConflictResolution::Theirs`], conflict hunks are resolved automatically
/// and the result is always [`MergeResult::Clean`].
///
/// # Errors
/// Returns an error if the merge backend cannot process the supplied inputs.
pub fn merge_text_with_style(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
    resolution: ConflictResolution,
) -> Result<MergeResult, GitError> {
    let labels = Labels {
        ancestor: Some(base_label.as_bytes().as_bstr()),
        current: Some(ours_label.as_bytes().as_bstr()),
        other: Some(theirs_label.as_bytes().as_bstr()),
    };

    let default_marker_size = builtin_driver::text::Conflict::DEFAULT_MARKER_SIZE
        .try_into()
        .map_err(|_| GitError::BackendError {
            message: "gix default conflict marker size is invalid".to_string(),
        })?;
    let conflict = match resolution {
        ConflictResolution::Diff3 => builtin_driver::text::Conflict::Keep {
            style: ConflictStyle::Diff3,
            marker_size: default_marker_size,
        },
        ConflictResolution::Union => builtin_driver::text::Conflict::ResolveWithUnion,
        ConflictResolution::Ours => builtin_driver::text::Conflict::ResolveWithOurs,
        ConflictResolution::Theirs => builtin_driver::text::Conflict::ResolveWithTheirs,
    };

    let options = Options {
        conflict,
        ..Default::default()
    };

    let mut out = Vec::new();
    let mut input = InternedInput::new(&[][..], &[][..]);

    let gix_resolution =
        builtin_driver::text(&mut out, &mut input, labels, ours, base, theirs, options);

    match gix_resolution {
        Resolution::Complete | Resolution::CompleteWithAutoResolvedConflict => {
            Ok(MergeResult::Clean(out))
        }
        Resolution::Conflict => Ok(MergeResult::Conflict(out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_overlapping_edits_merge_cleanly() {
        let base = b"line1\nline2\nline3\n";
        let ours = b"LINE1\nline2\nline3\n";
        let theirs = b"line1\nline2\nLINE3\n";

        let result = merge_text(base, ours, theirs, "ours", "base", "theirs")
            .expect("non-overlapping merge should run");
        match result {
            MergeResult::Clean(merged) => {
                let s = String::from_utf8_lossy(&merged);
                assert!(s.contains("LINE1"), "ours edit missing: {s}");
                assert!(s.contains("LINE3"), "theirs edit missing: {s}");
                assert!(!s.contains("<<<<<<<"), "should not have markers: {s}");
            }
            MergeResult::Conflict(out) => {
                panic!(
                    "expected clean merge, got conflict:\n{}",
                    String::from_utf8_lossy(&out)
                );
            }
        }
    }

    #[test]
    fn same_line_conflict_produces_markers() {
        let base = b"key = original\n";
        let ours = b"key = ours\n";
        let theirs = b"key = theirs\n";

        let result = merge_text(base, ours, theirs, "ours", "base", "theirs")
            .expect("conflicting merge should run");
        match result {
            MergeResult::Clean(_) => panic!("expected conflict"),
            MergeResult::Conflict(out) => {
                let s = String::from_utf8_lossy(&out);
                assert!(s.contains("<<<<<<<"), "missing markers: {s}");
                assert!(s.contains("ours"), "missing ours label: {s}");
                assert!(s.contains("theirs"), "missing theirs label: {s}");
            }
        }
    }

    #[test]
    fn identical_changes_resolve_cleanly() {
        let base = b"line1\n";
        let ours = b"changed\n";
        let theirs = b"changed\n";

        let result = merge_text(base, ours, theirs, "ours", "base", "theirs")
            .expect("identical-change merge should run");
        match result {
            MergeResult::Clean(merged) => {
                assert_eq!(merged, b"changed\n");
            }
            MergeResult::Conflict(_) => panic!("identical changes should merge cleanly"),
        }
    }

    #[test]
    fn union_merge_resolves_conflict_with_both_sides() {
        // Two append-only sides with a base. Append to the end.
        let base = b"header\n";
        let ours = b"header\nours event 1\n";
        let theirs = b"header\ntheirs event 1\n";

        let result = merge_text_with_style(
            base,
            ours,
            theirs,
            "ours",
            "base",
            "theirs",
            ConflictResolution::Union,
        )
        .expect("union merge should run");

        match result {
            MergeResult::Clean(merged) => {
                let s = String::from_utf8_lossy(&merged);
                assert!(s.contains("ours event 1"), "missing ours: {s}");
                assert!(s.contains("theirs event 1"), "missing theirs: {s}");
                assert!(!s.contains("<<<<<<<"), "union should not have markers: {s}");
            }
            MergeResult::Conflict(out) => {
                panic!(
                    "union should not produce conflicts, got:\n{}",
                    String::from_utf8_lossy(&out)
                );
            }
        }
    }

    #[test]
    fn union_merge_handles_overlapping_edits() {
        // Both sides modify the same line differently — union concatenates the
        // conflict hunks.
        let base = b"key = original\n";
        let ours = b"key = ours\n";
        let theirs = b"key = theirs\n";

        let result = merge_text_with_style(
            base,
            ours,
            theirs,
            "ours",
            "base",
            "theirs",
            ConflictResolution::Union,
        )
        .expect("union overlap merge should run");

        match result {
            MergeResult::Clean(merged) => {
                let s = String::from_utf8_lossy(&merged);
                assert!(s.contains("key = ours"), "missing ours: {s}");
                assert!(s.contains("key = theirs"), "missing theirs: {s}");
                assert!(!s.contains("<<<<<<<"), "union should not have markers: {s}");
            }
            MergeResult::Conflict(_) => panic!("union should not produce conflicts"),
        }
    }

    #[test]
    fn ours_merge_picks_ours_side() {
        let base = b"line1\n";
        let ours = b"ours_version\n";
        let theirs = b"theirs_version\n";

        let result = merge_text_with_style(
            base,
            ours,
            theirs,
            "ours",
            "base",
            "theirs",
            ConflictResolution::Ours,
        )
        .expect("ours merge should run");

        match result {
            MergeResult::Clean(merged) => {
                let s = String::from_utf8_lossy(&merged);
                assert!(s.contains("ours_version"), "expected ours side: {s}");
                assert!(
                    !s.contains("theirs_version"),
                    "should not contain theirs: {s}"
                );
            }
            MergeResult::Conflict(_) => panic!("ours resolution should not conflict"),
        }
    }

    #[test]
    fn resolution_for_driver_mapping() {
        assert_eq!(resolution_for_driver(None), Some(ConflictResolution::Diff3));
        assert_eq!(
            resolution_for_driver(Some("text")),
            Some(ConflictResolution::Diff3)
        );
        assert_eq!(
            resolution_for_driver(Some("union")),
            Some(ConflictResolution::Union)
        );
        assert_eq!(
            resolution_for_driver(Some("ours")),
            Some(ConflictResolution::Ours)
        );
        assert_eq!(resolution_for_driver(Some("binary")), None);
        assert_eq!(
            resolution_for_driver(Some("my-custom-driver")),
            Some(ConflictResolution::Diff3),
            "unknown driver should fall back to diff3"
        );
    }
}
