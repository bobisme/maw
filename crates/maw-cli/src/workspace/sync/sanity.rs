//! Post-merge sanity checks (bn-2upt / bn-c5ui).
//!
//! Originally written inline in `sync::rebase` to guard three-way overlap
//! merges during workspace rebase. Extracted here so that
//! `workspace::resolve_structured` can run the same checks on per-hunk
//! resolution outputs before auto-committing them.
//!
//! # Design
//!
//! Two independent checks are composed in [`run_post_merge_sanity`]:
//!
//! 1. **Size-delta** ([`check_size_delta`]): catches the case where the merged
//!    output is dramatically larger than the sum of both sides' additions —
//!    which signals that the diff algorithm confused large blocks of content and
//!    duplicated them.
//!
//! 2. **AST-parse** ([`check_ast_parse`]): when both input sides parse cleanly
//!    under a supported tree-sitter grammar but the merged output does *not*,
//!    report a [`SanityFailure::AstParse`]. The check is intentionally lenient:
//!    unsupported file types and pre-broken inputs are silently accepted.

use std::path::Path;

use anyhow::Result;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for the post-merge sanity check (bn-2upt).
///
/// Built from [`maw_core::config::MergeConfig`] and passed through the merge
/// machinery so per-merge helpers can decide whether a "clean" output looks
/// implausible.
#[derive(Clone, Copy, Debug)]
pub struct PostMergeSanityConfig {
    /// Maximum allowed `merged_size / expected_size` ratio before the
    /// size-delta check flags the merge.
    pub size_ratio_max: f64,
}

impl PostMergeSanityConfig {
    /// Build from a [`maw_core::config::MergeConfig`].
    pub(crate) const fn from_merge(cfg: &maw_core::config::MergeConfig) -> Self {
        Self {
            size_ratio_max: cfg.post_rebase_size_ratio_max,
        }
    }

    /// Disabled config: size check never trips. Used by test callers that
    /// explicitly opt out.
    #[allow(
        dead_code,
        reason = "used by tests that construct merge machinery directly"
    )]
    pub(crate) const fn disabled() -> Self {
        Self {
            size_ratio_max: f64::INFINITY,
        }
    }
}

impl Default for PostMergeSanityConfig {
    fn default() -> Self {
        Self::from_merge(&maw_core::config::MergeConfig::default())
    }
}

// ---------------------------------------------------------------------------
// Failure type
// ---------------------------------------------------------------------------

/// Why a merge output was flagged as suspicious by the post-merge sanity check.
#[derive(Clone, Debug)]
pub enum SanityFailure {
    /// The merged blob's byte length exceeded
    /// `size_ratio_max * expected_size`, where `expected_size` is the
    /// upper bound for a legitimate clean merge:
    /// `max(ours, theirs) + (ours - base) + (theirs - base)` (saturating).
    SizeDelta {
        merged_len: usize,
        /// `max(base, ours, theirs)` — informational context.
        max_input: usize,
        /// `max(ours, theirs) + (ours-base) + (theirs-base)`.
        expected_size: usize,
        ratio: f64,
    },
    /// Both inputs parsed cleanly under the file's tree-sitter grammar but
    /// the merged output did not. Strong signal of structured-merge corruption.
    AstParse { reason: &'static str },
}

impl std::fmt::Display for SanityFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeDelta {
                merged_len,
                max_input,
                expected_size,
                ratio,
            } => write!(
                f,
                "merged output is {merged_len} bytes; \
                 {ratio:.2}x larger than the expected upper bound \
                 ({expected_size} bytes; largest input was {max_input} bytes)"
            ),
            Self::AstParse { reason } => write!(
                f,
                "tree-sitter parse of the merged output reported {reason} \
                 even though both inputs parsed cleanly"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure checks
// ---------------------------------------------------------------------------

/// Pure-function size-delta check (bn-2upt).
///
/// Compares `merged.len()` against the expected upper bound for a legitimate
/// merge: `max(ours, theirs) + (ours - base) + (theirs - base)` (saturating).
/// Returns `Err(SanityFailure::SizeDelta)` if the ratio exceeds
/// `size_ratio_max`.
///
/// Pure: no I/O, no allocation beyond the failure-payload struct itself.
#[allow(
    clippy::cast_precision_loss,
    reason = "blob sizes far below f64 mantissa headroom; ratio is for thresholding only"
)]
pub fn check_size_delta(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    merged: &[u8],
    size_ratio_max: f64,
) -> Result<(), SanityFailure> {
    let max_input = ours.len().max(theirs.len()).max(base.len());
    let ours_added = ours.len().saturating_sub(base.len());
    let theirs_added = theirs.len().saturating_sub(base.len());
    let expected = ours.len().max(theirs.len()) + ours_added + theirs_added;
    if expected == 0 {
        if merged.is_empty() {
            return Ok(());
        }
        return Err(SanityFailure::SizeDelta {
            merged_len: merged.len(),
            max_input,
            expected_size: expected,
            ratio: f64::INFINITY,
        });
    }
    let ratio = (merged.len() as f64) / (expected as f64);
    if ratio > size_ratio_max {
        return Err(SanityFailure::SizeDelta {
            merged_len: merged.len(),
            max_input,
            expected_size: expected,
            ratio,
        });
    }
    Ok(())
}

/// AST-parse sanity check (bn-2upt).
///
/// Returns `Err(SanityFailure::AstParse)` only when:
///   * The path matches a supported tree-sitter language; AND
///   * Both `ours` and `theirs` parsed without errors; AND
///   * The merged blob did NOT parse without errors.
///
/// In every other case (unsupported language, an input already had parse
/// errors, the merge also parses cleanly) we return `Ok(())`. This avoids
/// false positives on languages we don't have a grammar for and on inputs
/// that were already broken.
#[cfg(feature = "ast-merge")]
pub fn check_ast_parse(
    path: &Path,
    ours: &[u8],
    theirs: &[u8],
    merged: &[u8],
) -> Result<(), SanityFailure> {
    use maw::merge::ast_merge::{AstLanguage, AstParseStatus, parse_status};

    let Some(lang) = AstLanguage::from_path(path) else {
        return Ok(());
    };

    let ours_status = parse_status(ours, lang);
    let theirs_status = parse_status(theirs, lang);
    if ours_status != AstParseStatus::Clean || theirs_status != AstParseStatus::Clean {
        return Ok(());
    }

    match parse_status(merged, lang) {
        AstParseStatus::Clean => Ok(()),
        AstParseStatus::HasErrors => Err(SanityFailure::AstParse {
            reason: "syntax errors",
        }),
        AstParseStatus::Unparseable => Err(SanityFailure::AstParse {
            reason: "an unrecoverable parse failure",
        }),
    }
}

/// Stub for builds without the `ast-merge` feature: skip the AST check.
#[cfg(not(feature = "ast-merge"))]
pub fn check_ast_parse(
    _path: &Path,
    _ours: &[u8],
    _theirs: &[u8],
    _merged: &[u8],
) -> Result<(), SanityFailure> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Composed check
// ---------------------------------------------------------------------------

/// Compose the size-delta and AST-parse checks. Order: cheapest first.
///
/// Returns the first failure found, or `Ok(())` when both checks pass.
pub fn run_post_merge_sanity(
    path: &Path,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    merged: &[u8],
    cfg: PostMergeSanityConfig,
) -> Result<(), SanityFailure> {
    check_size_delta(base, ours, theirs, merged, cfg.size_ratio_max)?;
    check_ast_parse(path, ours, theirs, merged)?;
    Ok(())
}
