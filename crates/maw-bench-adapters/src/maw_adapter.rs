//! `maw ws ...` subprocess adapter (SG2 arm 1).
//!
//! Shells out to the `maw` binary on PATH. Test invocations under
//! `cargo test` can override the binary location via the
//! `MAW_BENCH_BIN` env var (preferred — the default workspace install is
//! `0.61.0` and tests want to run against whatever maw is locally
//! installed, not a stale `target/debug` from another workspace).
//!
//! # The maw substrate, in one paragraph
//!
//! maw v2 is a bare-repo layout: `<root>/.git/` plus `<root>/ws/` for
//! workspaces. `maw ws create` makes a worktree under `ws/<name>`;
//! `maw ws merge` runs the merge engine and advances `ws/default`'s
//! HEAD (the "epoch"); `maw ws destroy` snapshots into
//! `refs/manifold/recovery/<name>/` then removes the worktree;
//! `maw ws sync` rebases stale workspaces onto the current epoch.
//!
//! # Asymmetry vs the other two arms (justified in parity table)
//!
//! - `maw ws create` writes manifold state files (`.manifold/...`). This
//!   is essential to the maw contract (without it merge/sync/recover
//!   cannot function). NOT an extra "bias" step.
//! - `maw ws merge` advances the epoch ref. This IS the maw contract.
//!   The worktrees+convention arm uses a merge commit; jj uses an
//!   integrated change. All three "advance the integration head" — same
//!   abstract operation, different substrate-native artifact.
//! - `maw ws destroy` captures recovery snapshots even without `--force`.
//!   This is the Prime Invariant in action; without it, maw would be a
//!   different substrate.

use std::fs;
use std::path::PathBuf;

use maw_scenario::{BaseRef, WsId};
use tempfile::TempDir;

use crate::proc_util;
use crate::{Result, StateSnapshot, StepOutcome, Substrate, SubstrateError};

/// `maw` subprocess adapter.
pub struct MawAdapter {
    root: PathBuf,
    /// Resolved `maw` binary path (`MAW_BENCH_BIN` override or PATH lookup).
    maw_bin: String,
    _tmp: Option<TempDir>,
}

impl MawAdapter {
    /// Build a fresh maw substrate inside a private tempdir using the
    /// `maw` binary on PATH (or `MAW_BENCH_BIN`).
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError`] if `maw` is missing or `maw init` fails.
    pub fn new() -> Result<Self> {
        let tmp = tempfile::tempdir().map_err(|e| SubstrateError::Io(format!("tempdir: {e}")))?;
        Self::new_in(tmp.path().to_path_buf(), Some(tmp))
    }

    /// Build into a caller-owned root.
    ///
    /// # Errors
    ///
    /// Same as [`Self::new`].
    pub fn new_in(root: PathBuf, owned_tmp: Option<TempDir>) -> Result<Self> {
        let maw_bin = std::env::var("MAW_BENCH_BIN").unwrap_or_else(|_| "maw".to_string());

        // maw v2 expects an existing git repo with at least one commit before
        // `maw init` runs (it converts the layout in place to the bare model).
        // SP3 §Auth pattern: bootstrap a normal repo under /tmp, then upgrade.
        // 1. git init + identity + seed commit on `main`.
        proc_util::run("git", &["init", "-b", "main"], &root)?;
        proc_util::run("git", &["config", "user.name", "bench"], &root)?;
        proc_util::run("git", &["config", "user.email", "bench@localhost"], &root)?;
        std::fs::write(root.join("README.md"), "bench repo\n")
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        proc_util::run("git", &["add", "-A"], &root)?;
        proc_util::run("git", &["commit", "-m", "init"], &root)?;
        // 2. `maw init` (transforms layout to bare v2).
        let init_out = proc_util::run(&maw_bin, &["init"], &root);
        match init_out {
            Ok(_) => {}
            Err(e @ SubstrateError::BinaryNotFound(_)) => return Err(e),
            Err(SubstrateError::SubprocessFailed { stderr, .. }) => {
                return Err(SubstrateError::Refused(format!(
                    "maw init failed: {stderr}"
                )));
            }
            Err(e) => return Err(e),
        }

        Ok(Self {
            root,
            maw_bin,
            _tmp: owned_tmp,
        })
    }

    fn ws_dir(&self, ws: &WsId) -> PathBuf {
        self.root.join("ws").join(&ws.0)
    }

    fn default_dir(&self) -> PathBuf {
        self.root.join("ws").join("default")
    }
}

impl Substrate for MawAdapter {
    fn arm_name(&self) -> &'static str {
        "maw"
    }

    fn root(&self) -> &PathBuf {
        &self.root
    }

    fn create_workspace(&mut self, ws: &WsId, base: &BaseRef) -> Result<StepOutcome> {
        // Map BaseRef → --from flag. Epoch == current default; Main is the
        // configured project branch (maw init sets it to `main`).
        let from = match base {
            BaseRef::Main => "main",
            BaseRef::Epoch => "main", // maw resolves epoch via its own state; --from main is the documented value for fresh ws
        };
        proc_util::run(
            &self.maw_bin,
            &["ws", "create", &ws.0, "--from", from],
            &self.root,
        )?;
        Ok(StepOutcome {
            ok: true,
            notes: format!("maw ws create {}", ws.0),
            ..StepOutcome::default()
        })
    }

    fn edit_file(&mut self, ws: &WsId, path: &str, content: &str) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| SubstrateError::Io(e.to_string()))?;
        }
        fs::write(&target, content).map_err(|e| SubstrateError::Io(e.to_string()))?;
        Ok(StepOutcome {
            ok: true,
            notes: format!("wrote {path} ({} bytes) in ws/{}", content.len(), ws.0),
            ..StepOutcome::default()
        })
    }

    fn commit(&mut self, ws: &WsId, msg: &str) -> Result<StepOutcome> {
        // maw uses ordinary git inside the workspace; the equivalent of
        // `maw exec <ws> -- git add -A && git commit -m ...`.
        let dir = self.ws_dir(ws);
        // Workspace worktrees inherit identity from parent repo config in
        // most maw setups; set it explicitly here for hermeticity.
        let _ = proc_util::run("git", &["config", "user.name", "bench"], &dir);
        let _ = proc_util::run("git", &["config", "user.email", "bench@localhost"], &dir);
        proc_util::run("git", &["add", "-A"], &dir)?;
        let status = std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(&dir)
            .env("GIT_AUTHOR_NAME", "bench")
            .env("GIT_AUTHOR_EMAIL", "bench@localhost")
            .env("GIT_COMMITTER_NAME", "bench")
            .env("GIT_COMMITTER_EMAIL", "bench@localhost")
            .output()
            .map_err(|e| SubstrateError::Io(format!("git commit: {e}")))?;
        let stdout = String::from_utf8_lossy(&status.stdout);
        if !status.status.success() && !stdout.contains("nothing to commit") {
            return Err(SubstrateError::SubprocessFailed {
                code: status.status.code(),
                stderr: String::from_utf8_lossy(&status.stderr).to_string(),
            });
        }
        Ok(StepOutcome {
            ok: true,
            notes: format!("commit '{msg}' in ws/{}", ws.0),
            ..StepOutcome::default()
        })
    }

    fn merge(&mut self, srcs: &[WsId], target: &str, destroy_sources: bool) -> Result<StepOutcome> {
        let mut args: Vec<String> = vec!["ws".into(), "merge".into()];
        for s in srcs {
            args.push(s.0.clone());
        }
        args.push("--into".into());
        args.push(target.to_string());
        if destroy_sources {
            args.push("--destroy".into());
        }
        // Pin a non-interactive commit message so maw doesn't probe an
        // editor under -p. (`-m` is widely supported in `maw ws merge`.)
        args.push("--message".into());
        args.push(format!(
            "merge: {}",
            srcs.iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = proc_util::run(&self.maw_bin, &arg_refs, &self.root);
        match result {
            Ok(out) => Ok(StepOutcome {
                ok: true,
                advanced_integration: true,
                notes: format!("maw ws merge → {}", out.lines().last().unwrap_or("")),
                ..StepOutcome::default()
            }),
            Err(SubstrateError::SubprocessFailed { stderr, .. })
                if stderr.contains("conflict") || stderr.contains("has_conflicts") =>
            {
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "maw ws merge: conflicts surfaced as data".into(),
                    ..StepOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn sync(&mut self, ws: &WsId) -> Result<StepOutcome> {
        // `maw ws sync` is the maw equivalent of rebase + epoch refresh.
        let _ = ws;
        let result = proc_util::run(&self.maw_bin, &["ws", "sync"], &self.root);
        match result {
            Ok(_) => Ok(StepOutcome {
                ok: true,
                notes: "maw ws sync".into(),
                ..StepOutcome::default()
            }),
            Err(SubstrateError::SubprocessFailed { stderr, .. }) if stderr.contains("conflict") => {
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "maw ws sync surfaced conflicts (resolve via maw ws resolve)".into(),
                    ..StepOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn destroy(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome> {
        let mut args = vec!["ws", "destroy", &ws.0];
        if force {
            args.push("--force");
        }
        proc_util::run(&self.maw_bin, &args, &self.root)?;
        Ok(StepOutcome {
            ok: true,
            notes: format!("maw ws destroy {} (force: {force})", ws.0),
            ..StepOutcome::default()
        })
    }

    fn state_snapshot(&self) -> Result<StateSnapshot> {
        let mut snap = StateSnapshot {
            integration_head: Some("default".to_string()),
            ..StateSnapshot::default()
        };
        // List live workspaces via `maw ws list`. Output format:
        // "<name>\t<path>" one per line. We parse minimally — anything else
        // changes per release and that drift is itself worth catching.
        let list = proc_util::run(&self.maw_bin, &["ws", "list"], &self.root)?;
        for line in list.lines() {
            let name = line.split('\t').next().unwrap_or("").trim();
            if name.is_empty() || name == "default" {
                continue;
            }
            // Terminal commit message of that workspace branch.
            let msg = proc_util::run(
                "git",
                &["log", "-1", "--format=%s", name],
                &self.root.join(".git"),
            )
            .unwrap_or_default()
            .trim()
            .to_string();
            snap.live_workspaces.insert(name.to_string(), msg);
        }
        // Destroyed workspaces: `maw ws recover` lists them. Output is one
        // workspace per line in the same `<name>\t<path-or-meta>` form. If
        // the command isn't available in the installed binary, treat the
        // list as empty (it's optional substrate metadata, not required
        // for equivalence — recovery refs themselves live in
        // `refs/manifold/recovery/*` which we DON'T expose in the
        // substrate-neutral snapshot).
        if let Ok(recover) = proc_util::run(&self.maw_bin, &["ws", "recover"], &self.root) {
            for line in recover.lines() {
                let name = line.split_whitespace().next().unwrap_or("").to_string();
                if !name.is_empty() && !name.starts_with('-') && name != "Workspace" {
                    snap.destroyed_workspaces.push(name);
                }
            }
            snap.destroyed_workspaces.sort();
            snap.destroyed_workspaces.dedup();
        }
        // Integrated files: walk ws/default/.
        super::worktrees_adapter_collect_files(
            &self.default_dir(),
            &self.default_dir(),
            &mut snap.integrated_files,
        )?;
        Ok(snap)
    }

    fn cleanup(&mut self) -> Result<()> {
        self._tmp.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maw_available() -> bool {
        let bin = std::env::var("MAW_BENCH_BIN").unwrap_or_else(|_| "maw".to_string());
        std::process::Command::new(&bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn minimal_battery_smoke() {
        if !maw_available() {
            eprintln!("skipping: maw not on PATH (set MAW_BENCH_BIN)");
            return;
        }
        let mut s = match MawAdapter::new() {
            Ok(s) => s,
            Err(SubstrateError::BinaryNotFound(_)) => {
                eprintln!("skipping: maw missing");
                return;
            }
            Err(e) => panic!("adapter create: {e}"),
        };
        let ws = WsId::slot(0);
        s.create_workspace(&ws, &BaseRef::Main).expect("create");
        s.edit_file(&ws, "src/lib.rs", "pub fn alpha() {}\n")
            .expect("edit");
        s.commit(&ws, "feat: alpha").expect("commit");
        let merge = s.merge(&[ws.clone()], "default", true).expect("merge");
        assert!(merge.ok);
        assert!(merge.advanced_integration);
        let snap = s.state_snapshot().expect("snapshot");
        assert!(
            snap.integrated_files
                .get("src/lib.rs")
                .map_or(false, |c| c.contains("alpha"))
        );
    }
}
