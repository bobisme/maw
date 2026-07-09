//! Bin-startup preflight checks for the SG2/SG3 eval binaries.
//!
//! # Why a preflight exists
//!
//! The 2026-05-26 SG3 layout-eval NO-GO traced to a binary/substrate
//! version skew: the eval ran against installed `maw` v0.61.0 while
//! the substrate was the post-T3.2 `.maw/` layout (see
//! `notes/sg3-no-go-rootcause.md`, bn-2ert). A populated
//! [`maw_bench::RunManifest::maw_version`] (bn-f5zu) makes the
//! confound recoverable post-hoc; this preflight is the
//! *complementary* defence — surface drift **at-source** before the
//! agent ever runs.
//!
//! # What it does
//!
//! - Shells out to `maw --version` (the same binary the agent will
//!   invoke through `Bash`).
//! - Compares against [`env!("CARGO_PKG_VERSION")`] embedded into the
//!   eval binary at build time (the workspace version of the source
//!   tree the operator built from).
//! - If the installed `maw` is older than the source tree, emits a
//!   single-line `WARN:` to stderr with the exact remediation command.
//!
//! **Warning-only by design.** Operators sometimes intentionally bench
//! an older binary (e.g. to A/B a fix's effect). Hard-erroring would
//! gate legitimate workflow. The warning is loud enough that an
//! operator who DIDN'T mean to do this notices before reading
//! 20-run aggregates.
//!
//! # Cost
//!
//! One `Command::output` (low ms). Run once per binary invocation, at
//! `main` entry. Not in the hot path.

use std::cmp::Ordering;

use maw_bench::capture_tool_version;

/// Outcome of comparing installed-vs-source `maw` versions. Surfaced
/// as the return value of [`check_maw_version_skew`] so callers can
/// log differently (or, in tests, assert on the variant) without
/// having to grep stderr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// Installed `maw` version parsed and matches (or exceeds) the
    /// source tree version. Quiet — no stderr output.
    InSync {
        /// `maw --version` first line, trimmed.
        installed: String,
        /// `env!("CARGO_PKG_VERSION")` at bin-build time.
        source: String,
    },
    /// Installed `maw` is older than the source. A `WARN:` line was
    /// emitted to stderr; the carrying fields let test assertions
    /// exercise the formatting without parsing stderr.
    SkewDetected {
        /// Installed version (semver triple, e.g. `0.61.0`).
        installed: String,
        /// Source version embedded at compile time.
        source: String,
        /// Exact `WARN:` line written to stderr.
        warning_line: String,
    },
    /// Could not capture `maw --version` (binary missing on `$PATH`
    /// or returned non-zero). Operator may have deliberately omitted
    /// `maw` (e.g. running the Mock pilot which never invokes it);
    /// we emit a `WARN:` so it's surfaced but do not error.
    CaptureFailed {
        /// The capture-error message.
        error: String,
        /// Source version embedded at compile time.
        source: String,
        /// Exact `WARN:` line written to stderr.
        warning_line: String,
    },
    /// Installed `maw --version` did not match the expected
    /// `maw <semver>` shape — likely an experimental fork. Warns
    /// (don't gate) and proceeds.
    UnparsableVersion {
        /// Raw `maw --version` first line.
        installed: String,
        /// Source version embedded at compile time.
        source: String,
        /// Exact `WARN:` line written to stderr.
        warning_line: String,
    },
}

impl PreflightOutcome {
    /// True iff the outcome represents a clean match. Lets callers
    /// branch on `matches!(...)` without naming the inner fields.
    #[must_use]
    pub fn is_in_sync(&self) -> bool {
        matches!(self, Self::InSync { .. })
    }
}

/// Parse a semver triple out of `maw --version` output. `maw` prints
/// `maw <version>` (one line). Returns `None` if neither the
/// `maw <semver>` shape nor a bare `<semver>` is matched.
fn parse_maw_version(line: &str) -> Option<(u32, u32, u32)> {
    let trimmed = line.trim();
    // Tolerate either `maw 0.61.0` (current) or a bare `0.61.0`.
    let candidate = trimmed.strip_prefix("maw ").unwrap_or(trimmed);
    // Drop any trailing whitespace / +metadata / pre-release tags.
    let core = candidate.split([' ', '-', '+']).next().unwrap_or(candidate);
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts.next()?.parse::<u32>().ok()?;
    Some((major, minor, patch))
}

/// Compare two semver triples lexicographically.
fn cmp_semver(a: (u32, u32, u32), b: (u32, u32, u32)) -> Ordering {
    a.cmp(&b)
}

/// Run the preflight, emitting a `WARN:` line to stderr on drift.
///
/// `source_version` is conventionally `env!("CARGO_PKG_VERSION")` from
/// the caller's `main` — we accept it as a parameter rather than
/// reading it here so each bin can record its own build-time version
/// (also lets unit tests exercise the comparator without rebuilding
/// the crate).
///
/// Returns the [`PreflightOutcome`] so a caller can assert on it.
/// Never errors; never panics.
pub fn check_maw_version_skew(source_version: &str) -> PreflightOutcome {
    check_maw_version_skew_with(source_version, capture_tool_version("maw").version, None)
}

/// Internal seam — testable version of [`check_maw_version_skew`].
/// Accepts the captured `maw --version` line and an optional
/// pre-formatted capture error (test code passes `Some("...")` to
/// simulate a missing binary).
pub fn check_maw_version_skew_with(
    source_version: &str,
    captured_version: String,
    capture_error: Option<&str>,
) -> PreflightOutcome {
    if let Some(err) = capture_error {
        let warning_line = format!(
            "WARN: could not capture installed maw version ({err}); \
             source tree is v{source_version}. \
             Install with 'just install' if you intended to bench a built maw."
        );
        eprintln!("{warning_line}");
        return PreflightOutcome::CaptureFailed {
            error: err.to_string(),
            source: source_version.to_string(),
            warning_line,
        };
    }
    if captured_version.is_empty() {
        // Shouldn't happen (capture_tool_version returns error in this
        // case), but be defensive.
        let warning_line = format!(
            "WARN: 'maw --version' returned no output; \
             source tree is v{source_version}. \
             Install with 'just install' if you intended to bench a built maw."
        );
        eprintln!("{warning_line}");
        return PreflightOutcome::CaptureFailed {
            error: "empty output".to_string(),
            source: source_version.to_string(),
            warning_line,
        };
    }

    let Some(installed_triple) = parse_maw_version(&captured_version) else {
        let warning_line = format!(
            "WARN: installed maw version '{captured_version}' did not match the \
             expected 'maw <major>.<minor>.<patch>' shape; source tree is v{source_version}. \
             Results may reflect an experimental fork."
        );
        eprintln!("{warning_line}");
        return PreflightOutcome::UnparsableVersion {
            installed: captured_version,
            source: source_version.to_string(),
            warning_line,
        };
    };

    let Some(source_triple) = parse_maw_version(source_version) else {
        // Source CARGO_PKG_VERSION is always a valid semver per Cargo's
        // own validation; this arm is unreachable in practice but kept
        // for total-function correctness.
        return PreflightOutcome::InSync {
            installed: captured_version,
            source: source_version.to_string(),
        };
    };

    match cmp_semver(installed_triple, source_triple) {
        Ordering::Less => {
            let warning_line = format!(
                "WARN: installed maw v{}.{}.{} is older than source v{}.{}.{}; \
                 results may reflect outdated behavior. Run 'just install' to update.",
                installed_triple.0,
                installed_triple.1,
                installed_triple.2,
                source_triple.0,
                source_triple.1,
                source_triple.2,
            );
            eprintln!("{warning_line}");
            PreflightOutcome::SkewDetected {
                installed: format!(
                    "{}.{}.{}",
                    installed_triple.0, installed_triple.1, installed_triple.2
                ),
                source: format!(
                    "{}.{}.{}",
                    source_triple.0, source_triple.1, source_triple.2
                ),
                warning_line,
            }
        }
        Ordering::Equal | Ordering::Greater => PreflightOutcome::InSync {
            installed: captured_version,
            source: source_version.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matched versions are silent (in_sync outcome).
    #[test]
    fn matched_versions_are_in_sync() {
        let out = check_maw_version_skew_with("0.61.0", "maw 0.61.0".to_string(), None);
        assert!(out.is_in_sync(), "expected InSync; got {out:?}");
    }

    /// Installed newer than source is in_sync (we don't warn on
    /// forward drift — operator may be running an installed
    /// post-release against an older checkout).
    #[test]
    fn installed_newer_than_source_is_in_sync() {
        let out = check_maw_version_skew_with("0.61.0", "maw 0.62.0".to_string(), None);
        assert!(out.is_in_sync(), "expected InSync; got {out:?}");
    }

    /// Installed older than source emits a SkewDetected with the
    /// canonical remediation text.
    #[test]
    fn older_installed_emits_skew_warning() {
        let out = check_maw_version_skew_with("0.62.0", "maw 0.61.0".to_string(), None);
        match out {
            PreflightOutcome::SkewDetected {
                installed,
                source,
                warning_line,
            } => {
                assert_eq!(installed, "0.61.0");
                assert_eq!(source, "0.62.0");
                assert!(
                    warning_line.contains("v0.61.0 is older than source v0.62.0"),
                    "warning text: {warning_line}"
                );
                assert!(
                    warning_line.contains("just install"),
                    "warning text: {warning_line}"
                );
            }
            other => panic!("expected SkewDetected; got {other:?}"),
        }
    }

    /// Capture-error path emits a CaptureFailed outcome with a
    /// `WARN:` line.
    #[test]
    fn capture_error_path_emits_capture_failed() {
        let out = check_maw_version_skew_with(
            "0.62.0",
            String::new(),
            Some("No such file or directory (os error 2)"),
        );
        match out {
            PreflightOutcome::CaptureFailed { warning_line, .. } => {
                assert!(
                    warning_line.contains("could not capture installed maw version"),
                    "warning text: {warning_line}"
                );
                assert!(
                    warning_line.contains("just install"),
                    "warning text: {warning_line}"
                );
            }
            other => panic!("expected CaptureFailed; got {other:?}"),
        }
    }

    /// Unparsable installed-version output warns but does not error.
    #[test]
    fn unparsable_installed_version_warns() {
        let out =
            check_maw_version_skew_with("0.62.0", "experimental-fork-build".to_string(), None);
        match out {
            PreflightOutcome::UnparsableVersion { warning_line, .. } => {
                assert!(
                    warning_line.contains("did not match"),
                    "warning text: {warning_line}"
                );
            }
            other => panic!("expected UnparsableVersion; got {other:?}"),
        }
    }

    /// Parser tolerates the `maw <semver>` shape and the bare-semver
    /// shape (some future maw versions might drop the prefix).
    #[test]
    fn parser_tolerates_prefix_and_bare_semver() {
        assert_eq!(parse_maw_version("maw 0.61.0"), Some((0, 61, 0)));
        assert_eq!(parse_maw_version("0.61.0"), Some((0, 61, 0)));
        assert_eq!(parse_maw_version("maw 1.2.3"), Some((1, 2, 3)));
        // Tolerates trailing whitespace / pre-release / build metadata.
        assert_eq!(parse_maw_version("maw 0.61.0\n"), Some((0, 61, 0)));
        assert_eq!(parse_maw_version("maw 0.61.0-rc.1"), Some((0, 61, 0)));
        assert_eq!(parse_maw_version("maw 0.61.0+sha.abc"), Some((0, 61, 0)));
    }

    /// Parser rejects non-semver inputs.
    #[test]
    fn parser_rejects_non_semver() {
        assert_eq!(parse_maw_version("not a version"), None);
        assert_eq!(parse_maw_version("maw"), None);
        assert_eq!(parse_maw_version("maw 1.2"), None);
        assert_eq!(parse_maw_version("maw a.b.c"), None);
    }

    /// `check_maw_failpoints_advisory` always emits a stderr line
    /// naming the feature flag + chaos mode (it's advisory; reliable
    /// feature-detection from outside the binary is impossible).
    #[test]
    fn failpoints_advisory_always_emits_named_warning() {
        let out = check_maw_failpoints_advisory();
        assert!(
            out.warning_line.contains("--features failpoints"),
            "advisory should name the feature flag: {}",
            out.warning_line
        );
        assert!(
            out.warning_line.contains("chaos"),
            "advisory should name chaos: {}",
            out.warning_line
        );
    }
}

// ---------------------------------------------------------------------------
// bn-3hzt: failpoints-feature advisory
// ---------------------------------------------------------------------------

/// Outcome of [`check_maw_failpoints_advisory`]. Always carries an
/// advisory line because we cannot reliably feature-detect
/// `--features failpoints` from the binary's `--version` output — the
/// env-bridge is silently a no-op without the feature, so a chaos
/// invocation against a stock binary would just produce zero crashes
/// and look like a clean run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailpointsAdvisory {
    /// Exact `WARN:` (advisory) line written to stderr.
    pub warning_line: String,
}

/// bn-3hzt: emit a stderr advisory that chaos mode requires the
/// installed `maw` binary to be built with `--features failpoints`.
///
/// Reliable feature-detection from outside the process is impossible
/// — `init_from_env` is a no-op without the feature, so `MAW_FP=...`
/// silently does nothing, and `maw --version` doesn't emit feature
/// flags. The honest move is a loud advisory at chaos invocation time
/// with the exact remediation command. Operators running chaos
/// against a stock binary will see zero crash events in their run
/// JSONs and (with this advisory in scrollback) immediately know why.
///
/// # Effects
///
/// Writes one `WARN:` line to stderr. Never errors. Never panics.
#[must_use]
pub fn check_maw_failpoints_advisory() -> FailpointsAdvisory {
    let warning_line = "WARN: --chaos=on requires the installed `maw` binary to be built with \
         --features failpoints (otherwise MAW_FP is silently a no-op and chaos \
         produces zero crashes). Install with: cargo install --path crates/maw-cli \
         --features failpoints --force"
        .to_string();
    eprintln!("{warning_line}");
    FailpointsAdvisory { warning_line }
}
