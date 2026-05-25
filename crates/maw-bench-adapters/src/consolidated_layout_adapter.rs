//! SP5 spike adapter: simulates the **proposed consolidated `.maw/` layout**
//! (bone bn-2kgu, parent SG3 bn-2yh1). Sibling of
//! [`crate::ws_layout_adapter`]; together they isolate the *layout* variable
//! for SP5's directional ergonomics read.
//!
//! # Why path-translation simulation (not a wrapped real-maw)
//!
//! The actual T3.2 (bn-2sw3) implementation is the downstream xl bone this
//! spike *gates*. SP5 cannot wait for T3.2 to run — that defeats the
//! direction-before-commitment point. So this adapter **simulates** the
//! consolidated layout by building it from primitives (plain `git
//! worktree`) using the same engine as [`crate::ws_layout_adapter`]. The
//! only delta between the two is the **on-disk path shape**, which is
//! exactly what SP5 is asking about.
//!
//! Rationale (filed under "adapter approach" in the spike report):
//! - **Simulated, not path-translation-over-real-maw.** Wrapping a real-maw
//!   binary with a chroot/bind-mount layer would require either (a) running
//!   maw with a config knob that doesn't exist yet, or (b) a fragile
//!   bind-mount trick. Both bias the measurement with implementation
//!   artifacts unrelated to the layout question. The simulation is
//!   load-bearing-honest: the maw merge engine is layout-agnostic
//!   (SP4 verdict, `notes/layout-engine-impact.md`), so a git-worktree
//!   engine under each layout shape is a fair proxy for SP5's purpose.
//! - **The naming-decision override (bone description) applies here.**
//!   Per the 2026-05-25 terminology decision, this simulation uses
//!   `.maw/workspaces/` not `.maw/worktrees/`. The SG3 design doc
//!   (`notes/sg3-layout-design.md` §1.3) is being amended in the T3.2
//!   downstream; SP5 honors the override verbatim.
//!
//! # Layout this adapter materializes
//!
//! ```text
//! <root>/                       NORMAL checkout (core.bare=false). IS the
//!                               merge target. README.md, src/ live here
//!                               directly (no <root>/ws/default).
//! <root>/.git/                  git data (non-bare)
//! <root>/.maw/                  maw admin dir (hidden)
//! <root>/.maw/.gitignore        tracks config.toml, ignores runtime
//! <root>/.maw/config.toml       bootstrap config
//! <root>/.maw/manifold/         (simulated) maw metadata — relocated
//!                               from <root>/.manifold/
//! <root>/.maw/workspaces/<name>/  agent worktrees (the relocated ws/<name>)
//! <root>/.maw/cache/            reserved (no top-level dirs later)
//! ```
//!
//! Per pre-reg §3.1 Pilot rule, SP5 output never sets bars / never feeds
//! publication.

use std::fs;
use std::path::PathBuf;

use maw_scenario::{BaseRef, WsId};
use tempfile::TempDir;

use crate::proc_util;
use crate::{Result, StateSnapshot, StepOutcome, Substrate, SubstrateError};

/// SP5 adapter: simulates the **proposed consolidated `.maw/` layout**.
/// See module docs for layout shape + rationale.
pub struct ConsolidatedLayoutAdapter {
    root: PathBuf,
    /// `.maw/workspaces/` directory. All agent workspaces live as
    /// subdirs here, mirroring the proposed T3.2 layout.
    workspaces_dir: PathBuf,
    /// `.maw/manifold/` directory (replaces v2's `<root>/.manifold/`).
    manifold_dir: PathBuf,
    _tmp: Option<TempDir>,
}

impl ConsolidatedLayoutAdapter {
    /// Build a fresh substrate under a private tempdir.
    pub fn new() -> Result<Self> {
        let tmp = tempfile::tempdir().map_err(|e| SubstrateError::Io(format!("tempdir: {e}")))?;
        Self::new_in(tmp.path().to_path_buf(), Some(tmp))
    }

    /// Build into a caller-owned root.
    pub fn new_in(root: PathBuf, owned_tmp: Option<TempDir>) -> Result<Self> {
        let maw_admin = root.join(".maw");
        let workspaces_dir = maw_admin.join("workspaces");
        let manifold_dir = maw_admin.join("manifold");
        let cache_dir = maw_admin.join("cache");

        for d in [&maw_admin, &workspaces_dir, &manifold_dir, &cache_dir] {
            fs::create_dir_all(d).map_err(|e| SubstrateError::Io(e.to_string()))?;
        }

        // .maw/.gitignore — tracks config.toml, ignores runtime per the
        // bone description.
        fs::write(
            maw_admin.join(".gitignore"),
            "# .maw/ admin dir — ignore runtime, track config\n\
             /workspaces/\n\
             /manifold/\n\
             /cache/\n\
             !/config.toml\n\
             !/.gitignore\n",
        )
        .map_err(|e| SubstrateError::Io(e.to_string()))?;

        // .maw/config.toml — bootstrap config (location fixed).
        fs::write(
            maw_admin.join("config.toml"),
            "# maw config (SP5 simulation; T3.2 will define schema)\n\
             [layout]\n\
             default_workspace = \"\"  # root checkout is the privileged target\n\
             workspaces_dir = \".maw/workspaces\"\n",
        )
        .map_err(|e| SubstrateError::Io(e.to_string()))?;

        // .maw/manifold/ placeholder (matches v2 .manifold/ semantics —
        // empty here; the SP5 measurement is structural, not behavioral).
        fs::write(
            manifold_dir.join("PLACEHOLDER"),
            "maw metadata placeholder (relocated from <root>/.manifold/)\n",
        )
        .map_err(|e| SubstrateError::Io(e.to_string()))?;

        // 1. Init a NORMAL (non-bare) git repo at root. The root IS the
        //    integration / privileged target — there is no ws/default.
        proc_util::run("git", &["init", "-b", "main"], &root)?;
        proc_util::run("git", &["config", "user.name", "bench"], &root)?;
        proc_util::run("git", &["config", "user.email", "bench@localhost"], &root)?;
        // Tracked seed. .maw/ is in gitignore so the admin runtime is invisible
        // to git. config.toml IS tracked via .maw/.gitignore's bang-rule above.
        fs::write(root.join("README.md"), "bench repo\n")
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        fs::write(
            root.join(".gitignore"),
            "/.maw/\n!/.maw/.gitignore\n!/.maw/config.toml\n",
        )
        .map_err(|e| SubstrateError::Io(e.to_string()))?;
        proc_util::run("git", &["add", "-A"], &root)?;
        proc_util::run("git", &["commit", "-m", "init"], &root)?;

        Ok(Self {
            root,
            workspaces_dir,
            manifold_dir,
            _tmp: owned_tmp,
        })
    }

    /// Path an agent workspace lives at under this layout.
    #[must_use]
    pub fn ws_dir(&self, ws: &WsId) -> PathBuf {
        self.workspaces_dir.join(&ws.0)
    }

    /// The hidden admin dir, exposed for inspection by the SP5 pilot.
    #[must_use]
    pub fn maw_admin_dir(&self) -> PathBuf {
        self.root.join(".maw")
    }
}

impl Substrate for ConsolidatedLayoutAdapter {
    fn arm_name(&self) -> &'static str {
        "sp5-consolidated-layout"
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
        // `git worktree add` from the integration root (== <root>).
        proc_util::run(
            "git",
            &["worktree", "add", "-b", &ws.0, &dir_str, base_ref],
            &self.root,
        )?;
        proc_util::run("git", &["config", "user.name", "bench"], &dir)?;
        proc_util::run("git", &["config", "user.email", "bench@localhost"], &dir)?;
        Ok(StepOutcome {
            ok: true,
            notes: format!(
                ".maw/workspaces/{}/ created (layout=consolidated, depth={})",
                ws.0,
                super::ws_layout_adapter::ws_path_depth_components(&dir, &self.root)
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
            notes: format!(
                "wrote .maw/workspaces/{}/{} ({} bytes)",
                ws.0,
                path,
                content.len()
            ),
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
            notes: format!("commit '{msg}' in .maw/workspaces/{}", ws.0),
            ..StepOutcome::default()
        })
    }

    fn merge(&mut self, srcs: &[WsId], target: &str, destroy_sources: bool) -> Result<StepOutcome> {
        // Consolidated: the root IS the integration target (no ws/default).
        if !(target == "default" || target == "main") {
            return Err(SubstrateError::Refused(format!(
                "consolidated adapter integration is <root> (==default==main), got target={target}"
            )));
        }
        proc_util::run("git", &["checkout", "main"], &self.root)?;
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
        let result = proc_util::run("git", &arg_refs, &self.root);
        match result {
            Ok(_) => {
                let mut outcome = StepOutcome {
                    ok: true,
                    advanced_integration: true,
                    notes: format!("consolidated merged {} sources into <root>", srcs.len()),
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
                let _ = proc_util::run_lenient("git", &["merge", "--abort"], &self.root);
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "consolidated merge conflicted; aborted".into(),
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
                notes: format!("rebased .maw/workspaces/{} onto main", ws.0),
                ..StepOutcome::default()
            }),
            Err(SubstrateError::SubprocessFailed { stderr, .. })
                if stderr.contains("CONFLICT") || stderr.contains("could not apply") =>
            {
                let _ = proc_util::run_lenient("git", &["rebase", "--abort"], &dir);
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: format!(".maw/workspaces/{} rebase conflicted; aborted", ws.0),
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
        let porcelain = proc_util::run("git", &["worktree", "list", "--porcelain"], &self.root)?;
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
                let msg = proc_util::run("git", &["log", "-1", "--format=%s", b], &self.root)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                snap.live_workspaces.insert(b.to_string(), msg);
            }
        }
        // Integrated files: walk root (skipping .git AND .maw — the latter
        // is the runtime admin dir, intentionally hidden).
        collect_files_skip_maw(&self.root, &self.root, &mut snap.integrated_files)?;
        let _ = self.manifold_dir.as_path();
        Ok(snap)
    }

    fn cleanup(&mut self) -> Result<()> {
        self._tmp.take();
        Ok(())
    }
}

impl ConsolidatedLayoutAdapter {
    fn destroy_inner(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        let dir_str = dir.to_string_lossy().into_owned();
        args.push(&dir_str);
        let _ = proc_util::run("git", &args, &self.root);
        let _ = proc_util::run_lenient("git", &["branch", "-D", &ws.0], &self.root);
        Ok(StepOutcome {
            ok: true,
            notes: format!("destroyed .maw/workspaces/{}", ws.0),
            ..StepOutcome::default()
        })
    }
}

/// File walker that skips `.git` AND `.maw/` (the consolidated admin dir).
/// SP5 cares about the integrated-content view; the consolidated layout's
/// design says `.maw/` is gitignored runtime state — invisible to the
/// integration head's file enumeration.
fn collect_files_skip_maw(
    root: &std::path::Path,
    base: &std::path::Path,
    out: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    for entry in fs::read_dir(root)
        .map_err(|e| SubstrateError::Io(format!("read_dir {}: {e}", root.display())))?
    {
        let entry = entry.map_err(|e| SubstrateError::Io(e.to_string()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str == ".jj" || name_str == ".maw" {
            continue;
        }
        let ft = entry
            .file_type()
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        if ft.is_dir() {
            collect_files_skip_maw(&path, base, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|e| SubstrateError::Io(e.to_string()))?
                .to_string_lossy()
                .to_string();
            let content = fs::read_to_string(&path).unwrap_or_else(|_| String::from("<binary>"));
            out.insert(rel, content);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consolidated_layout_smoke() {
        let mut s = ConsolidatedLayoutAdapter::new().expect("substrate");
        let ws = WsId::slot(0);
        s.create_workspace(&ws, &BaseRef::Main).expect("create");
        let dir = s.ws_dir(&ws);
        assert!(
            dir.exists(),
            ".maw/workspaces dir must exist at {}",
            dir.display()
        );
        // Depth = 3 from root: .maw / workspaces / <ws>
        assert_eq!(
            super::super::ws_layout_adapter::ws_path_depth_components(&dir, s.root()),
            3,
            "expected depth=3 for .maw/workspaces/<name>"
        );
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
        // .maw/ is NOT in the integrated file view (it's runtime admin).
        assert!(
            !snap.integrated_files.keys().any(|k| k.starts_with(".maw")),
            ".maw/ runtime dir must be invisible to integrated file view"
        );
    }

    #[test]
    fn consolidated_layout_tracks_config_and_gitignore() {
        let s = ConsolidatedLayoutAdapter::new().expect("substrate");
        let admin = s.maw_admin_dir();
        assert!(admin.join(".gitignore").exists());
        assert!(admin.join("config.toml").exists());
        // Reserved cache dir is present (no top-level dirs reserved later).
        assert!(admin.join("cache").is_dir());
    }
}
