//! bn-1q6z: PATH-shim factory for `git` / `jj` real-LLM chaos.
//!
//! The chaos seam in [`crate::Substrate::arm_chaos`] only fires when the
//! adapter's own scripted-driver path (`merge()`, etc.) is the caller. In
//! the **real-LLM agent path** the agent invokes `git` / `jj` itself via
//! Bash, NOT via the adapter — so chaos doesn't reach those invocations.
//!
//! This module materializes wrapper-script shims (`git`, `jj`) into a
//! tempdir and exposes the dir's path so the harness can prepend it to
//! the agent subprocess's `PATH`. The shims default to **passthrough**
//! (one `exec` to the real binary) and only kill-with-probability when
//! `MAW_BENCH_CHAOS_KILL_PROB` is set in the agent's env. The harness
//! gates that env var through [`maw_bench::harness::BenchConfig::chaos_env`]
//! (the bn-3hzt seam), so a non-chaos run is byte-identical to today
//! (the shim is on PATH but inert; the no-kill path is one `exec` call).
//!
//! # Shape
//!
//! ```text
//! <shim_dir>/
//!   ├── git    (bash, +x)  — exec'd in the agent's PATH lookup for `git`
//!   └── jj     (bash, +x)  — exec'd in the agent's PATH lookup for `jj`
//! ```
//!
//! The shim contents are embedded via `include_str!` from
//! `crates/maw-bench-adapters/src/shim/{git,jj}-shim` so the audit
//! surface is the bash scripts themselves (the Rust here just copies
//! bytes to disk and chmods +x). The bash logic is the load-bearing
//! part; see the script headers for the semantics.
//!
//! # Env contract (read by the shim scripts)
//!
//! - `MAW_BENCH_CHAOS_KILL_PROB` (float in [0,1]): per-invocation kill
//!   probability. Unset or `"0"` ⇒ passthrough (no fork, no kill).
//! - `MAW_BENCH_CHAOS_KILL_MS` (int ≥ 1): milliseconds to sleep before
//!   delivering SIGKILL when the roll fires. Default 50.
//! - `_MAW_SHIM_DIR` (path): the shim dir itself; the shim strips this
//!   from PATH before looking up the real binary. Set automatically by
//!   [`ShimSet::path_env_overlay`].
//! - `_MAW_SHIM_REAL_GIT` / `_MAW_SHIM_REAL_JJ` (path): explicit
//!   override for the real binary path (test-only; production uses
//!   PATH minus shim dir).

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::{Result, SubstrateError};

/// Embedded shim sources. The `include_str!` bytes are the audit
/// surface — the on-disk shim is a byte-for-byte copy of these.
const GIT_SHIM_SRC: &str = include_str!("git-shim");
const JJ_SHIM_SRC: &str = include_str!("jj-shim");

/// Filename for the git shim inside the materialized shim dir.
const GIT_SHIM_NAME: &str = "git";
/// Filename for the jj shim inside the materialized shim dir.
const JJ_SHIM_NAME: &str = "jj";

/// Env var names the shim scripts read. Centralized here so the Rust
/// side and the bash side agree (drift here is a silent miscompile).
pub mod env_keys {
    /// Per-invocation kill probability, float in `[0, 1]`. Unset ⇒
    /// passthrough.
    pub const CHAOS_KILL_PROB: &str = "MAW_BENCH_CHAOS_KILL_PROB";
    /// Milliseconds to sleep before SIGKILL when the roll fires.
    pub const CHAOS_KILL_MS: &str = "MAW_BENCH_CHAOS_KILL_MS";
    /// Shim dir path — stripped from `PATH` by the shim before it
    /// resolves the real binary.
    pub const SHIM_DIR: &str = "_MAW_SHIM_DIR";
    /// Test-only override for the real git binary.
    pub const REAL_GIT: &str = "_MAW_SHIM_REAL_GIT";
    /// Test-only override for the real jj binary.
    pub const REAL_JJ: &str = "_MAW_SHIM_REAL_JJ";
}

/// Materialized shim set living on disk under a tempdir.
///
/// Owns the tempdir guard so the shim dir survives until the
/// `ShimSet` is dropped (typically tied to the adapter's lifetime).
/// Construct via [`ShimSet::materialize_in`] (caller-owned dir) or
/// [`ShimSet::materialize_temp`] (private tempdir).
pub struct ShimSet {
    /// Directory containing the `git` and `jj` shim scripts.
    dir: PathBuf,
    /// Tempdir guard, if the shim dir lives under a private tempdir.
    /// `None` when the caller supplied their own dir.
    _tmp: Option<TempDir>,
}

impl ShimSet {
    /// Materialize the shim set under a fresh tempdir. The tempdir is
    /// removed when `self` is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError::Io`] if the tempdir cannot be
    /// created or the shim files cannot be written / chmod'd.
    pub fn materialize_temp() -> Result<Self> {
        let tmp = tempfile::Builder::new()
            .prefix("maw-bench-shim-")
            .tempdir()
            .map_err(|e| SubstrateError::Io(format!("shim tempdir: {e}")))?;
        let dir = tmp.path().to_path_buf();
        write_shims(&dir)?;
        Ok(Self {
            dir,
            _tmp: Some(tmp),
        })
    }

    /// Materialize the shim set into a caller-supplied directory. The
    /// directory is created if missing; the caller owns its lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError::Io`] on any fs failure.
    pub fn materialize_in(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).map_err(|e| {
            SubstrateError::Io(format!("shim dir {}: {e}", dir.display()))
        })?;
        write_shims(&dir)?;
        Ok(Self { dir, _tmp: None })
    }

    /// Absolute path to the shim dir (the value prepended to `PATH`).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Build the `(PATH, _MAW_SHIM_DIR)` env-overlay pair for the
    /// agent subprocess. `orig_path` is typically `std::env::var("PATH")`;
    /// the returned `PATH` value is `<shim_dir>:<orig_path>`.
    ///
    /// The caller folds this into [`maw_bench::agent::AgentConfig::extra_env`]
    /// (the bn-3hzt seam) so the spawned `claude -p` inherits it.
    ///
    /// The second pair (`_MAW_SHIM_DIR`) lets the shim strip itself
    /// from `PATH` before resolving the real binary, avoiding an
    /// infinite-loop exec.
    #[must_use]
    pub fn path_env_overlay(&self, orig_path: &str) -> [(String, String); 2] {
        let prepended = if orig_path.is_empty() {
            self.dir.display().to_string()
        } else {
            format!("{}:{}", self.dir.display(), orig_path)
        };
        [
            ("PATH".to_string(), prepended),
            (
                env_keys::SHIM_DIR.to_string(),
                self.dir.display().to_string(),
            ),
        ]
    }

    /// Convenience: build the full chaos-enabling env (PATH overlay
    /// PLUS the two kill-knob vars). Returns a `Vec<(String,String)>`
    /// so the caller can extend their `extra_env` map directly.
    ///
    /// `orig_path` is the agent subprocess's existing `PATH` value;
    /// `kill_prob` is `[0,1]` (clamped); `kill_ms` is `>=1` (clamped).
    #[must_use]
    pub fn chaos_env_overlay(
        &self,
        orig_path: &str,
        kill_prob: f64,
        kill_ms: u32,
    ) -> Vec<(String, String)> {
        let prob = kill_prob.clamp(0.0, 1.0);
        let ms = kill_ms.max(1);
        let mut out: Vec<(String, String)> = self.path_env_overlay(orig_path).to_vec();
        out.push((env_keys::CHAOS_KILL_PROB.to_string(), format!("{prob}")));
        out.push((env_keys::CHAOS_KILL_MS.to_string(), format!("{ms}")));
        out
    }
}

/// Write the embedded shim sources into `dir/{git,jj}` and chmod +x.
fn write_shims(dir: &Path) -> Result<()> {
    write_one(dir, GIT_SHIM_NAME, GIT_SHIM_SRC)?;
    write_one(dir, JJ_SHIM_NAME, JJ_SHIM_SRC)?;
    Ok(())
}

fn write_one(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, contents).map_err(|e| {
        SubstrateError::Io(format!("write {}: {e}", path.display()))
    })?;
    set_executable(&path)?;
    Ok(())
}

/// chmod +x on Unix; no-op on Windows (the shim is bash and Windows
/// isn't a supported benchmark host, but we keep the cfg gate
/// explicit so the crate compiles on Windows for `cargo check`).
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| SubstrateError::Io(format!("stat {}: {e}", path.display())))?
        .permissions();
    // 0o755: owner rwx, group/other rx — same as bash defaults.
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
        .map_err(|e| SubstrateError::Io(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Shim files are materialized and executable on Unix.
    #[test]
    fn materialize_creates_two_executable_scripts() {
        let shim = ShimSet::materialize_temp().expect("materialize");
        let git = shim.dir().join("git");
        let jj = shim.dir().join("jj");
        assert!(git.exists(), "git shim missing at {git:?}");
        assert!(jj.exists(), "jj shim missing at {jj:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = fs::metadata(&git).unwrap().permissions().mode();
            assert!(m & 0o111 != 0, "git shim not executable (mode={m:o})");
            let m = fs::metadata(&jj).unwrap().permissions().mode();
            assert!(m & 0o111 != 0, "jj shim not executable (mode={m:o})");
        }
    }

    /// path_env_overlay produces PATH=<shim>:<orig> + _MAW_SHIM_DIR.
    #[test]
    fn path_env_overlay_prepends_shim_dir() {
        let shim = ShimSet::materialize_temp().expect("materialize");
        let overlay = shim.path_env_overlay("/usr/bin:/bin");
        assert_eq!(overlay[0].0, "PATH");
        let expected_prefix = format!("{}:", shim.dir().display());
        assert!(
            overlay[0].1.starts_with(&expected_prefix),
            "PATH overlay should start with shim dir: {}",
            overlay[0].1
        );
        assert!(overlay[0].1.ends_with("/usr/bin:/bin"));
        assert_eq!(overlay[1].0, "_MAW_SHIM_DIR");
        assert_eq!(overlay[1].1, shim.dir().display().to_string());
    }

    /// chaos_env_overlay clamps prob to [0,1] and ms to >=1.
    #[test]
    fn chaos_env_overlay_clamps_extreme_values() {
        let shim = ShimSet::materialize_temp().expect("materialize");
        let out = shim.chaos_env_overlay("", 5.0, 0);
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert_eq!(map.get("MAW_BENCH_CHAOS_KILL_PROB").map(String::as_str), Some("1"));
        assert_eq!(map.get("MAW_BENCH_CHAOS_KILL_MS").map(String::as_str), Some("1"));
        let out = shim.chaos_env_overlay("", -0.5, 1_000_000);
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert_eq!(map.get("MAW_BENCH_CHAOS_KILL_PROB").map(String::as_str), Some("0"));
        assert_eq!(map.get("MAW_BENCH_CHAOS_KILL_MS").map(String::as_str), Some("1000000"));
    }

    /// Default passthrough: with no chaos env, invoking the git shim
    /// is equivalent to invoking the real git (modulo PATH lookup).
    /// Skips when git isn't on the test host's PATH.
    #[test]
    fn passthrough_runs_real_git_when_chaos_unset() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let shim = ShimSet::materialize_temp().expect("materialize");
        // Use the OS PATH minus our shim dir for the real-git lookup.
        let orig_path = std::env::var("PATH").unwrap_or_default();
        let out = Command::new(shim.dir().join("git"))
            .arg("--version")
            .env("PATH", &orig_path)
            .env("_MAW_SHIM_DIR", shim.dir())
            // explicitly unset chaos: pass empty string
            .env_remove("MAW_BENCH_CHAOS_KILL_PROB")
            .output()
            .expect("spawn shim");
        assert!(out.status.success(), "shim passthrough exit: {:?}", out.status);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with("git version"),
            "expected real git output; got: {stdout}"
        );
    }
}
