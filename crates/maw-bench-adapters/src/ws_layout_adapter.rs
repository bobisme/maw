//! SP5 spike adapter: simulates the **current v2 `ws/` layout** using plain
//! `git worktree`. Sibling of [`crate::consolidated_layout_adapter`]; together
//! these isolate the *layout* variable for SP5's directional ergonomics read.
//!
//! # Why a separate adapter (not [`crate::maw_adapter`])
//!
//! The existing `MawAdapter` exercises the real `maw` binary, which means a
//! `maw` arm vs a `maw-with-different-layout` arm would conflate the layout
//! change with maw's own substrate machinery (merge engine, recovery refs,
//! epoch tracking). SP5 wants the **layout-only delta**.
//!
//! This adapter therefore reuses the [`crate::worktrees_adapter`] engine
//! shape (plain `git worktree`) but lays out the workspaces under the v2
//! shape:
//!
//! ```text
//! <root>/repo.git/         bare repo (object store)
//! <root>/ws/default/       integration worktree (privileged target)
//! <root>/ws/<name>/        agent worktrees
//! <root>/.manifold/        (simulated) maw metadata dir (empty placeholder)
//! ```
//!
//! Its twin, [`crate::consolidated_layout_adapter::ConsolidatedLayoutAdapter`],
//! uses identical engine semantics but the consolidated `.maw/` layout.
//! Difference between the two BenchRuns = layout effect (driver, engine, and
//! seed held constant).
//!
//! # NOT measured here
//!
//! - Maw-specific behaviors (epoch refs, recovery snapshots, claim files) —
//!   out of scope; this is layout-only.
//! - Real-LLM agent ergonomics — SP5 is MockAgent-only by the spike's HARD
//!   RULE. The structural metrics this adapter records (path depth, file
//!   counts, hidden-vs-visible workspace dir) are the directional signal.
//!
//! # Per pre-reg §3.1 Pilot rule
//!
//! Output of this adapter feeds the SP5 spike only — never the SG2/SG3/SG4
//! publication numbers or bars. The adapter is build-gated behind the
//! `bench` feature.

use std::fs;
use std::path::PathBuf;

use maw_scenario::{BaseRef, WsId};
use tempfile::TempDir;

use crate::proc_util;
use crate::{Result, StateSnapshot, StepOutcome, Substrate, SubstrateError};

/// SP5 adapter: simulates the **current v2 `ws/` layout**. See module docs.
pub struct WsLayoutAdapter {
    root: PathBuf,
    bare_dir: PathBuf,
    integration_dir: PathBuf,
    _tmp: Option<TempDir>,
}

impl WsLayoutAdapter {
    /// Build a fresh substrate under a private tempdir.
    pub fn new() -> Result<Self> {
        let tmp = tempfile::tempdir().map_err(|e| SubstrateError::Io(format!("tempdir: {e}")))?;
        Self::new_in(tmp.path().to_path_buf(), Some(tmp))
    }

    /// Build into a caller-owned root.
    pub fn new_in(root: PathBuf, owned_tmp: Option<TempDir>) -> Result<Self> {
        let bare_dir = root.join("repo.git");
        let ws_dir = root.join("ws");
        let integration_dir = ws_dir.join("default");
        let manifold_dir = root.join(".manifold");

        fs::create_dir_all(&ws_dir).map_err(|e| SubstrateError::Io(e.to_string()))?;
        fs::create_dir_all(&manifold_dir).map_err(|e| SubstrateError::Io(e.to_string()))?;
        // .manifold/ placeholder file so the dir is non-empty (matches maw
        // populating .manifold/ during init). Empty file — content irrelevant
        // for the SP5 structural read.
        fs::write(
            manifold_dir.join("PLACEHOLDER"),
            "maw metadata placeholder\n",
        )
        .map_err(|e| SubstrateError::Io(e.to_string()))?;

        // 1. Bare repo as the canonical object store.
        proc_util::run("git", &["init", "--bare", "repo.git"], &root)?;
        // 2. Clone into ws/default as the integration worktree (== maw's
        //    "privileged target").
        proc_util::run("git", &["clone", "repo.git", "ws/default"], &root)?;
        // 3. Pin identity and seed.
        proc_util::run("git", &["config", "user.name", "bench"], &integration_dir)?;
        proc_util::run(
            "git",
            &["config", "user.email", "bench@localhost"],
            &integration_dir,
        )?;
        fs::write(integration_dir.join("README.md"), "bench repo\n")
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        // Per v2 layout: root `.gitignore` contains `ws/` so the
        // worktree dir isn't tracked. Matches `ws/default/AGENTS.md`
        // "ws/ is gitignored" line and gives equivalence-check parity
        // with the consolidated adapter's `.gitignore`.
        fs::write(integration_dir.join(".gitignore"), "/ws/\n")
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        proc_util::run("git", &["add", "-A"], &integration_dir)?;
        proc_util::run("git", &["commit", "-m", "init"], &integration_dir)?;
        proc_util::run(
            "git",
            &["push", "-u", "origin", "HEAD:refs/heads/main"],
            &integration_dir,
        )?;
        proc_util::run(
            "git",
            &["symbolic-ref", "HEAD", "refs/heads/main"],
            &bare_dir,
        )?;

        Ok(Self {
            root,
            bare_dir,
            integration_dir,
            _tmp: owned_tmp,
        })
    }

    /// Path an agent workspace lives at under this layout.
    #[must_use]
    pub fn ws_dir(&self, ws: &WsId) -> PathBuf {
        self.root.join("ws").join(&ws.0)
    }
}

impl Substrate for WsLayoutAdapter {
    fn arm_name(&self) -> &'static str {
        "sp5-ws-layout"
    }

    fn root(&self) -> &PathBuf {
        &self.root
    }

    fn create_workspace(&mut self, ws: &WsId, base: &BaseRef) -> Result<StepOutcome> {
        let base_ref = match base {
            BaseRef::Main | BaseRef::Epoch => "main",
        };
        let dir = self.ws_dir(ws);
        let dir_str = dir.to_string_lossy().into_owned();
        proc_util::run(
            "git",
            &["worktree", "add", "-b", &ws.0, &dir_str, base_ref],
            &self.integration_dir,
        )?;
        proc_util::run("git", &["config", "user.name", "bench"], &dir)?;
        proc_util::run("git", &["config", "user.email", "bench@localhost"], &dir)?;
        Ok(StepOutcome {
            ok: true,
            notes: format!(
                "ws/{}/ created (layout=ws, depth={})",
                ws.0,
                ws_path_depth_components(&dir, &self.root)
            ),
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
            notes: format!("wrote ws/{}/{} ({} bytes)", ws.0, path, content.len()),
            ..StepOutcome::default()
        })
    }

    fn commit(&mut self, ws: &WsId, msg: &str) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
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
        // Engine: octopus merge into ws/default (the privileged target).
        // Layout-equivalent to maw ws merge under v2; here we measure
        // engine-equivalent semantics so the only delta vs the consolidated
        // adapter is the path shape.
        if !(target == "default" || target == "main") {
            return Err(SubstrateError::Refused(format!(
                "ws-layout adapter integration is ws/default, got target={target}"
            )));
        }
        proc_util::run("git", &["checkout", "main"], &self.integration_dir)?;
        let msg = format!(
            "merge: {}",
            srcs.iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut args: Vec<String> = vec!["merge".into(), "--no-ff".into(), "-m".into(), msg];
        for s in srcs {
            args.push(s.0.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = proc_util::run("git", &arg_refs, &self.integration_dir);
        match result {
            Ok(_) => {
                let mut outcome = StepOutcome {
                    ok: true,
                    advanced_integration: true,
                    notes: format!("ws-layout merged {} sources into ws/default", srcs.len()),
                    ..StepOutcome::default()
                };
                if destroy_sources {
                    for s in srcs {
                        let _ = self.destroy_inner(s, false);
                    }
                    outcome.notes.push_str("; sources destroyed");
                }
                Ok(outcome)
            }
            Err(SubstrateError::SubprocessFailed { stderr, .. })
                if stderr.contains("CONFLICT") || stderr.contains("Automatic merge failed") =>
            {
                let _ = proc_util::run_lenient("git", &["merge", "--abort"], &self.integration_dir);
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "ws-layout merge conflicted; aborted".into(),
                    ..StepOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn sync(&mut self, ws: &WsId) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        let result = proc_util::run("git", &["rebase", "main"], &dir);
        match result {
            Ok(_) => Ok(StepOutcome {
                ok: true,
                notes: format!("rebased ws/{} onto main", ws.0),
                ..StepOutcome::default()
            }),
            Err(SubstrateError::SubprocessFailed { stderr, .. })
                if stderr.contains("CONFLICT") || stderr.contains("could not apply") =>
            {
                let _ = proc_util::run_lenient("git", &["rebase", "--abort"], &dir);
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: format!("rebase ws/{} conflicted; aborted", ws.0),
                    ..StepOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn destroy(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome> {
        self.destroy_inner(ws, force)
    }

    fn state_snapshot(&self) -> Result<StateSnapshot> {
        let mut snap = StateSnapshot {
            integration_head: Some("default".to_string()),
            ..StateSnapshot::default()
        };
        let porcelain = proc_util::run(
            "git",
            &["worktree", "list", "--porcelain"],
            &self.integration_dir,
        )?;
        for block in porcelain.split("\n\n") {
            let mut branch_opt: Option<&str> = None;
            for line in block.lines() {
                if let Some(b) = line.strip_prefix("branch ") {
                    branch_opt = Some(b.trim_start_matches("refs/heads/"));
                }
            }
            if let Some(b) = branch_opt {
                if b == "main" {
                    continue;
                }
                let msg = proc_util::run(
                    "git",
                    &["log", "-1", "--format=%s", b],
                    &self.integration_dir,
                )
                .unwrap_or_default()
                .trim()
                .to_string();
                snap.live_workspaces.insert(b.to_string(), msg);
            }
        }
        crate::worktrees_adapter_collect_files(
            &self.integration_dir,
            &self.integration_dir,
            &mut snap.integrated_files,
        )?;
        let _ = self.bare_dir.as_path();
        Ok(snap)
    }

    fn cleanup(&mut self) -> Result<()> {
        self._tmp.take();
        Ok(())
    }
}

impl WsLayoutAdapter {
    fn destroy_inner(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        // git worktree remove identifies by path or branch; pass abs path.
        let dir_str = dir.to_string_lossy().into_owned();
        args.push(&dir_str);
        let _ = proc_util::run("git", &args, &self.integration_dir);
        let _ = proc_util::run_lenient("git", &["branch", "-D", &ws.0], &self.integration_dir);
        Ok(StepOutcome {
            ok: true,
            notes: format!("destroyed ws/{}", ws.0),
            ..StepOutcome::default()
        })
    }
}

/// Path depth (number of components from `base` to `path`). Used by the
/// SP5 spike's structural-ergonomics metric (deeper paths = more chars
/// per command, more depth for `cd`/`find` mental models).
#[must_use]
pub fn ws_path_depth_components(path: &std::path::Path, base: &std::path::Path) -> usize {
    path.strip_prefix(base)
        .map(|p| p.components().count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_layout_smoke() {
        let mut s = WsLayoutAdapter::new().expect("substrate");
        let ws = WsId::slot(0);
        s.create_workspace(&ws, &BaseRef::Main).expect("create");
        // Workspace dir is at ws/<name>/ (depth = 2 from root).
        let dir = s.ws_dir(&ws);
        assert!(dir.exists(), "ws dir must exist at {}", dir.display());
        assert_eq!(ws_path_depth_components(&dir, s.root()), 2);
        s.edit_file(&ws, "src/lib.rs", "pub fn alpha() {}\n")
            .expect("edit");
        s.commit(&ws, "feat: alpha").expect("commit");
        let m = s.merge(&[ws.clone()], "default", true).expect("merge");
        assert!(m.ok);
        assert!(m.advanced_integration);
        let snap = s.state_snapshot().expect("snapshot");
        assert!(snap.live_workspaces.is_empty());
        assert!(
            snap.integrated_files
                .get("src/lib.rs")
                .map_or(false, |c| c.contains("alpha"))
        );
    }
}
