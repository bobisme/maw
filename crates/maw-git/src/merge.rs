//! Text 3-way merge using gix-merge's built-in text driver.
//!
//! Provides [`merge_text`] — a pure-Rust 3-way merge that cleanly merges
//! non-overlapping edits and produces diff3-style conflict markers for
//! true conflicts. No git CLI subprocess required.

use gix::bstr::ByteSlice;
use gix::diff::blob::intern::InternedInput;
use gix::merge::blob::builtin_driver::text::{ConflictStyle, Labels, Options};
use gix::merge::blob::builtin_driver;
use gix::merge::blob::Resolution;

use crate::error::GitError;

/// Result of a 3-way text merge.
#[derive(Debug, Clone)]
pub enum MergeResult {
    /// All changes merged cleanly — no conflicts.
    Clean(Vec<u8>),
    /// Conflicts remain — output contains diff3-style markers.
    Conflict(Vec<u8>),
}

/// Perform a 3-way text merge of `ours` and `theirs` against `base`.
///
/// Uses the same algorithm as `git merge-file --diff3`:
/// - Non-overlapping changes from both sides are combined.
/// - Overlapping (conflicting) changes get diff3-style markers with labels.
///
/// Labels are applied to conflict markers as:
///   `<<<<<<< {ours_label}` / `||||||| {base_label}` / `>>>>>>> {theirs_label}`
pub fn merge_text(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
) -> Result<MergeResult, GitError> {
    let labels = Labels {
        ancestor: Some(base_label.as_bytes().as_bstr()),
        current: Some(ours_label.as_bytes().as_bstr()),
        other: Some(theirs_label.as_bytes().as_bstr()),
    };

    let options = Options {
        conflict: builtin_driver::text::Conflict::Keep {
            style: ConflictStyle::Diff3,
            marker_size: builtin_driver::text::Conflict::DEFAULT_MARKER_SIZE
                .try_into()
                .unwrap(),
        },
        ..Default::default()
    };

    let mut out = Vec::new();
    let mut input = InternedInput::new(&[][..], &[][..]);

    let resolution = builtin_driver::text(
        &mut out,
        &mut input,
        labels,
        ours,
        base,
        theirs,
        options,
    );

    match resolution {
        Resolution::Complete => Ok(MergeResult::Clean(out)),
        Resolution::CompleteWithAutoResolvedConflict => Ok(MergeResult::Clean(out)),
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

        let result = merge_text(base, ours, theirs, "ours", "base", "theirs").unwrap();
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

        let result = merge_text(base, ours, theirs, "ours", "base", "theirs").unwrap();
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

        let result = merge_text(base, ours, theirs, "ours", "base", "theirs").unwrap();
        match result {
            MergeResult::Clean(merged) => {
                assert_eq!(merged, b"changed\n");
            }
            MergeResult::Conflict(_) => panic!("identical changes should merge cleanly"),
        }
    }
}
