//! Tool-version capture for the §6.4 reproducibility manifest.
//!
//! Every [`crate::BenchRun`] embeds a [`crate::RunManifest`] which
//! records the versions of the external binaries the agent invoked
//! (`maw`, `git`, `jj`). Populating these is what makes
//! binary/substrate version-skew confounds catchable at-source — see
//! `notes/sg3-no-go-rootcause.md` (bn-2ert) for the 2026-05-26 SG3
//! NO-GO root-cause where an empty `maw_version` field forced a 20-run
//! forensic trace to identify v0.61.0 vs post-T3.2 binary skew.
//!
//! # Design choices
//!
//! - **Capture once per run, not per turn.** Each `--version` shell-out
//!   is ms-fast; one set per [`crate::BenchHarness::run`] suffices and
//!   the manifest is built once at run end anyway.
//! - **Soft on errors.** If a binary is missing or returns non-zero, we
//!   record a literal `error: <message>` string into the manifest
//!   instead of panicking. Empty-string is reserved for "we deliberately
//!   did not look" (e.g. a NoopSubstrate self-test that doesn't need
//!   any external binary).
//! - **No PATH games.** Captured via [`std::process::Command`] using the
//!   inherited `$PATH` — the same lookup the agent's `Bash` tool calls
//!   will use, so the manifest reflects what the agent actually invoked.
//!
//! See [`capture_tool_version`] for the unit API.

use std::process::Command;

/// Captured version of one external tool.
///
/// `version` is the stdout's first line, trimmed. `error` is `Some`
/// when capture failed (binary missing, non-zero exit). Either field
/// being populated is mutually exclusive with the other being
/// meaningful, but we keep them as separate fields so a downstream
/// analyst can distinguish "ran but said nothing" from "did not run".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolVersion {
    /// Tool name as invoked (e.g. `"maw"`, `"git"`, `"jj"`).
    pub name: String,
    /// First line of `<tool> --version` stdout, trimmed. Empty if
    /// `error.is_some()`.
    pub version: String,
    /// Populated iff capture failed. The string is the literal error
    /// message we store into the manifest (so a forensic reader sees
    /// `"error: No such file or directory (os error 2)"` instead of
    /// an empty string — the §6.4 contract permits this).
    pub error: Option<String>,
}

impl ToolVersion {
    /// Render to the string we write into the §6.4 manifest field.
    /// Either the trimmed version line OR `"error: <msg>"`.
    #[must_use]
    pub fn manifest_string(&self) -> String {
        self.error
            .as_ref()
            .map_or_else(|| self.version.clone(), |err| format!("error: {err}"))
    }
}

/// Shell out to `<tool> --version` and capture a [`ToolVersion`].
///
/// On failure to spawn (binary not on `$PATH`), the returned value
/// carries the OS error message in `.error`. On a non-zero exit, we
/// keep stderr's first line as the error so the run record retains
/// diagnostic context. Never panics; never blocks (the underlying
/// `Command::output` returns when the child writes EOF, which
/// `--version` does immediately).
#[must_use]
pub fn capture_tool_version(tool: &str) -> ToolVersion {
    match Command::new(tool).arg("--version").output() {
        Ok(o) if o.status.success() => ToolVersion {
            name: tool.to_string(),
            version: String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
            error: None,
        },
        Ok(o) => {
            // Exited non-zero. Treat as captured-with-error; surface
            // the first line of stderr (or the exit code) as context.
            let stderr = String::from_utf8_lossy(&o.stderr);
            let snippet = stderr.lines().next().unwrap_or("").trim().to_string();
            let err = if snippet.is_empty() {
                format!("exit {:?}", o.status.code())
            } else {
                snippet
            };
            ToolVersion {
                name: tool.to_string(),
                version: String::new(),
                error: Some(err),
            }
        }
        Err(e) => ToolVersion {
            name: tool.to_string(),
            version: String::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Triple of `(maw, git, jj)` versions, captured in that order. Order
/// is deterministic so a determinism test can byte-compare two
/// captures from the same host without worrying about HashMap ordering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedVersions {
    /// `maw --version`. Populating this field is the bn-f5zu fix —
    /// previously the manifest left it empty, which masked the 2026-05-26
    /// SG3 NO-GO version-skew (`notes/sg3-no-go-rootcause.md`).
    pub maw: ToolVersion,
    /// `git --version`. Already populated pre-bn-f5zu via
    /// `detect_tool_version`; reshaped here as part of the same helper
    /// so the three external-binary fields are captured uniformly.
    pub git: ToolVersion,
    /// `jj --version`. Same provenance as `git`.
    pub jj: ToolVersion,
}

/// Capture all three external-tool versions in one call. The harness
/// calls this once per [`crate::BenchHarness::run`] when building the
/// manifest. Cost is three `Command::output` calls — measured in low
/// single-digit milliseconds even on a cold cache.
#[must_use]
pub fn capture_versions() -> CapturedVersions {
    CapturedVersions {
        maw: capture_tool_version("maw"),
        git: capture_tool_version("git"),
        jj: capture_tool_version("jj"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capturing a binary that exists yields a non-empty version
    /// string and no error. We use `sh` because every supported host
    /// has it and `--version` works (busybox sh, bash, dash all
    /// honour it or fail predictably; even when sh's `--version` is
    /// non-zero, we get a deterministic ToolVersion shape).
    #[test]
    fn capture_existing_tool_yields_some_output() {
        // `git --version` is reliably available on any host where the
        // maw test suite runs.
        let tv = capture_tool_version("git");
        // Either the version captured cleanly OR (on a host without
        // git on $PATH) we recorded an error. Both shapes are
        // acceptable; what matters is non-panic and well-formed.
        assert_eq!(tv.name, "git");
        if tv.error.is_none() {
            assert!(
                tv.version.starts_with("git version"),
                "expected `git version ...`; got {:?}",
                tv.version
            );
        }
    }

    /// Capturing a definitely-missing binary records the error rather
    /// than panicking.
    #[test]
    fn capture_missing_tool_records_error_no_panic() {
        let tv = capture_tool_version("definitely-no-such-binary-bn-f5zu");
        assert_eq!(tv.name, "definitely-no-such-binary-bn-f5zu");
        assert!(tv.version.is_empty());
        assert!(tv.error.is_some(), "expected error; got {tv:?}");
    }

    /// `manifest_string` renders the error path verbatim with an
    /// `error:` prefix so the §6.4 field is self-describing.
    #[test]
    fn manifest_string_renders_error_prefix() {
        let tv = ToolVersion {
            name: "maw".to_string(),
            version: String::new(),
            error: Some("not found".to_string()),
        };
        assert_eq!(tv.manifest_string(), "error: not found");
    }

    /// `manifest_string` for a happy-path capture returns just the
    /// version (no `error:` prefix).
    #[test]
    fn manifest_string_renders_clean_version() {
        let tv = ToolVersion {
            name: "maw".to_string(),
            version: "maw 0.61.0".to_string(),
            error: None,
        };
        assert_eq!(tv.manifest_string(), "maw 0.61.0");
    }

    /// `capture_versions` returns the three fields in the documented
    /// order regardless of which binaries exist. Smoke test for the
    /// aggregate API.
    #[test]
    fn capture_versions_returns_all_three_fields() {
        let v = capture_versions();
        assert_eq!(v.maw.name, "maw");
        assert_eq!(v.git.name, "git");
        assert_eq!(v.jj.name, "jj");
    }
}
