//! `jj workspace` adapter (SG2 arm 3 — pre-reg §1.3 + SP3 §1).
//!
//! Uses **colocated** `jj git init --colocate` so the adapter's
//! integration-head bytes are reachable by ordinary `git log` (consistent
//! with the other two arms' `git`-based snapshot machinery).
//!
//! # SP3 opfork-wedge observability is preserved
//!
//! Per the SP3 reproduction memo (`notes/agent-benchmark-feasibility.md`
//! §1) the jj arm exists in SG2 *because* concurrent multi-workspace use
//! still wedges on jj 0.41.0 — that is the load-bearing finding the
//! publication leads with. This adapter does NOT install workarounds
//! (no `jj op integrate` retry loops, no divergence pre-resolution).
//! Wedges surface as [`SubstrateError::SubprocessFailed`] with the
//! `sibling of the working copy's operation` envelope, exactly as an
//! agent would see them — that is the point.
//!
//! See `tests/jj_opfork_wedge.rs` (`#[ignore]`-gated; runs only with
//! `cargo test --ignored`) for the SP3 reproduction lock-in test.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use maw_scenario::{BaseRef, FaultSpec, WsId};
use tempfile::TempDir;

use crate::proc_util;
use crate::{Result, StateSnapshot, StepOutcome, Substrate, SubstrateError};

/// `jj` substrate adapter.
pub struct JjAdapter {
    root: PathBuf,
    integration_dir: PathBuf,
    _tmp: Option<TempDir>,
    /// bn-3hzt chaos seam — armed `FaultSpec` consumed by the **next**
    /// `merge()` call. `jj` has no failpoint hooks, so the
    /// parity-equivalent chaos is SIGKILL-mid-merge of the spawned
    /// `jj new ...` subprocess (analogous to the worktrees adapter's
    /// `git merge` kill). One-shot — `merge()` consumes it.
    armed_chaos: Option<FaultSpec>,
}

impl JjAdapter {
    /// Build a fresh colocated jj repo under a tempdir, with an
    /// `integration` workspace (the equivalent of maw's `default`).
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError`] if `jj` is missing or any setup step
    /// fails.
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
        let repo_dir = root.join("repo");
        fs::create_dir_all(&repo_dir).map_err(|e| SubstrateError::Io(e.to_string()))?;

        // Stable jj user identity. jj reads $JJ_USER / $JJ_EMAIL.
        let env: &[(&str, &str)] = &[("JJ_USER", "bench"), ("JJ_EMAIL", "bench@localhost")];

        // Colocated init so `git log` works on the same object store.
        proc_util::run_envs("jj", &["git", "init", "--colocate"], &repo_dir, env)?;
        // Seed an initial commit on @ so subsequent ops have an ancestor.
        fs::write(repo_dir.join("README.md"), "bench repo\n")
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        proc_util::run_envs("jj", &["describe", "-m", "init"], &repo_dir, env)?;
        // Promote @ to a stable named change so the `main` bookmark
        // pins the integration history.
        proc_util::run_envs(
            "jj",
            &["bookmark", "create", "-r", "@", "main"],
            &repo_dir,
            env,
        )?;
        // Create a fresh @ child so future workspaces fork from a stable point.
        proc_util::run_envs("jj", &["new", "main", "-m", "wip"], &repo_dir, env)?;

        // The "integration" workspace == the default colocated working
        // copy at <repo>. We treat <repo> itself as the integration
        // workspace and never `jj workspace add` an extra one for it.
        let integration_dir = repo_dir;

        Ok(Self {
            root,
            integration_dir,
            _tmp: owned_tmp,
            armed_chaos: None,
        })
    }

    fn ws_dir(&self, ws: &WsId) -> PathBuf {
        // jj workspaces live as sibling directories per jj docs / SP3
        // setup. We place each one at <root>/ws/<id>.
        self.root.join("ws").join(&ws.0)
    }

    fn env() -> Vec<(&'static str, &'static str)> {
        vec![("JJ_USER", "bench"), ("JJ_EMAIL", "bench@localhost")]
    }
}

impl Substrate for JjAdapter {
    fn arm_name(&self) -> &'static str {
        "jj-workspaces"
    }

    fn root(&self) -> &PathBuf {
        &self.root
    }

    fn create_workspace(&mut self, ws: &WsId, base: &BaseRef) -> Result<StepOutcome> {
        // jj workspace add --name <id> <dir> -- creates a working copy at
        // <dir> with an empty @ on top of the current head. We then
        // explicitly `jj new main` inside the new workspace so all
        // workspaces fork from the same canonical base (mirrors what the
        // other two arms do with `git checkout -b <id> main`).
        let dir = self.ws_dir(ws);
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|e| SubstrateError::Io(e.to_string()))?;
        }
        let env = Self::env();
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
        let dir_str = dir.to_string_lossy().into_owned();
        proc_util::run_envs(
            "jj",
            &["workspace", "add", "--name", &ws.0, &dir_str],
            &self.integration_dir,
            &env_refs,
        )?;
        // Pin a fresh @ from main in the new workspace.
        let base_ref = match base {
            BaseRef::Main | BaseRef::Epoch => "main",
        };
        proc_util::run_envs("jj", &["new", base_ref, "-m", "wip"], &dir, &env_refs)?;
        Ok(StepOutcome {
            ok: true,
            notes: format!("jj workspace add {}", ws.0),
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
            notes: format!("wrote {path} ({} bytes) in jj-ws/{}", content.len(), ws.0),
            ..StepOutcome::default()
        })
    }

    fn commit(&mut self, ws: &WsId, msg: &str) -> Result<StepOutcome> {
        // In jj, `jj describe` sets @'s message; `jj new` then advances @
        // so subsequent edits land in a new commit. Together they emulate
        // git's "commit then move on".
        let dir = self.ws_dir(ws);
        let env = Self::env();
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
        proc_util::run_envs("jj", &["describe", "-m", msg], &dir, &env_refs)?;
        proc_util::run_envs("jj", &["new", "-m", "wip"], &dir, &env_refs)?;
        // Push the workspace's tip to a same-named bookmark so the
        // integration workspace can refer to it by stable name.
        // bookmark set is idempotent.
        proc_util::run_envs(
            "jj",
            &["bookmark", "set", &ws.0, "-r", "@-", "--allow-backwards"],
            &dir,
            &env_refs,
        )?;
        Ok(StepOutcome {
            ok: true,
            notes: format!("jj describe + new: '{msg}' in jj-ws/{}", ws.0),
            ..StepOutcome::default()
        })
    }

    fn merge(&mut self, srcs: &[WsId], target: &str, destroy_sources: bool) -> Result<StepOutcome> {
        if !(target == "main" || target == "default") {
            return Err(SubstrateError::Refused(format!(
                "jj adapter integration target must be 'main'/'default', got {target}"
            )));
        }
        let env = Self::env();
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
        // Build a `jj new` revset combining `main` plus each source
        // bookmark — this creates a merge commit on top of all of them.
        let mut revset = String::from("main");
        for s in srcs {
            revset.push_str(" | ");
            revset.push_str(&s.0);
        }
        let msg = format!(
            "merge: {}",
            srcs.iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        // bn-3hzt: chaos-kill seam. jj has no failpoint hooks; we
        // SIGKILL the `jj new` subprocess mid-merge to model
        // parity-equivalent chaos. The colocated working copy may be
        // left in a state where `jj op log` shows the partial op and
        // the next `jj` invocation has to reconcile it — exactly the
        // class of wedge the SP3 reproduction memo documents.
        if let Some(fault) = self.armed_chaos.take() {
            let killed = proc_util::run_with_chaos_kill(
                "jj",
                &["new", &revset, "-m", &msg],
                &self.integration_dir,
                &env_refs,
                Duration::from_millis(50),
            )?;
            let fault_label = match &fault {
                FaultSpec::Failpoint { name, .. } => name.as_str(),
                FaultSpec::None => "<none>",
            };
            return Ok(StepOutcome {
                ok: false,
                notes: format!(
                    "jj new: CHAOS-KILLED (SIGKILL-mid-merge, exit={:?}, fault={fault_label}); \
                     colocated working copy may be wedged",
                    killed.0.code()
                ),
                ..StepOutcome::default()
            });
        }

        // `jj new <revset> -m "..."` in the integration workspace.
        let result = proc_util::run_envs(
            "jj",
            &["new", &revset, "-m", &msg],
            &self.integration_dir,
            &env_refs,
        );
        match result {
            Ok(_) => {
                // Advance the `main` bookmark to the new merge commit (@-),
                // and squash the empty `wip` @ so the integration workspace's
                // tree IS the merged tree.
                let _ = proc_util::run_envs(
                    "jj",
                    &["bookmark", "set", "main", "-r", "@", "--allow-backwards"],
                    &self.integration_dir,
                    &env_refs,
                );
                // Force-update the colocated git HEAD (main) so `git log` /
                // file walking on the integration dir sees the merge.
                let _ =
                    proc_util::run_envs("jj", &["git", "export"], &self.integration_dir, &env_refs);
                let mut outcome = StepOutcome {
                    ok: true,
                    advanced_integration: true,
                    notes: format!("jj merged {} sources into main", srcs.len()),
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
                if stderr.contains("conflict") || stderr.contains("Conflict") =>
            {
                // jj records conflicts in the commit (first-class). The
                // adapter surfaces this as conflicted=true; sources are NOT
                // destroyed.
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "jj merge: first-class conflict recorded".into(),
                    ..StepOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn sync(&mut self, ws: &WsId) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        let env = Self::env();
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
        // jj's stale-WC dance: workspace update-stale first, then rebase @
        // onto main. We do NOT swallow `sibling of the working copy's
        // operation` errors — that is the SP3-observed wedge and the
        // benchmark needs to see it.
        let _ = proc_util::run_envs("jj", &["workspace", "update-stale"], &dir, &env_refs);
        let result =
            proc_util::run_envs("jj", &["rebase", "-s", "@", "-d", "main"], &dir, &env_refs);
        match result {
            Ok(_) => Ok(StepOutcome {
                ok: true,
                notes: "jj workspace update-stale + rebase".into(),
                ..StepOutcome::default()
            }),
            Err(SubstrateError::SubprocessFailed { stderr, .. })
                if stderr.contains("conflict") || stderr.contains("Conflict") =>
            {
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "jj rebase: first-class conflict recorded".into(),
                    ..StepOutcome::default()
                })
            }
            // Surface the wedge: jj's opfork failure mode is reported via
            // the sibling-of-the-working-copy envelope. We return it
            // *as-is* so T2.2 can count it as a substrate failure.
            Err(e) => Err(e),
        }
    }

    fn destroy(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome> {
        self.destroy_inner(ws, force)
    }

    fn state_snapshot(&self) -> Result<StateSnapshot> {
        let mut snap = StateSnapshot {
            integration_head: Some("main".to_string()),
            ..StateSnapshot::default()
        };
        let env = Self::env();
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
        // List workspaces. `jj workspace list -T name`-style; we parse the
        // default output which is `name: <change-id> [...] <desc>`.
        let list = proc_util::run_envs(
            "jj",
            &["workspace", "list"],
            &self.integration_dir,
            &env_refs,
        )?;
        for line in list.lines() {
            // `default: <change-id> ...` — the integration workspace is
            // named `default` by jj. Skip it.
            if let Some((name, rest)) = line.split_once(':') {
                let name = name.trim();
                if name == "default" || name.is_empty() {
                    continue;
                }
                // Best-effort terminal commit message: the bookmark of the
                // same name (set by `commit`) points at the last described
                // change.
                let msg = proc_util::run_envs(
                    "jj",
                    &[
                        "log",
                        "-r",
                        name,
                        "-T",
                        "description.first_line()",
                        "--no-graph",
                        "--no-pager",
                    ],
                    &self.integration_dir,
                    &env_refs,
                )
                .unwrap_or_default()
                .trim()
                .to_string();
                let _ = rest;
                snap.live_workspaces.insert(name.to_string(), msg);
            }
        }
        // jj has no "destroyed workspaces" concept distinct from
        // `forget`-ten workspaces; the op-log is the recovery surface and
        // we don't expose it in the substrate-neutral snapshot. The
        // parity table calls this out explicitly.
        //
        // Integrated files: walk the integration dir (the colocated git
        // tree is the source of truth post-`jj git export`).
        super::worktrees_adapter_collect_files(
            &self.integration_dir,
            &self.integration_dir,
            &mut snap.integrated_files,
        )?;
        Ok(snap)
    }

    fn cleanup(&mut self) -> Result<()> {
        self._tmp.take();
        Ok(())
    }

    /// bn-3hzt: arm SIGKILL-mid-merge chaos for the next `merge()`
    /// call. `jj` has no failpoint hooks; the parity-equivalent
    /// chaos at the substrate-process layer is to kill the
    /// currently-running `jj new` subprocess mid-flight. One-shot —
    /// `merge()` consumes it.
    fn arm_chaos(&mut self, fault: Option<&FaultSpec>) {
        self.armed_chaos = match fault {
            Some(FaultSpec::None) | None => None,
            Some(f @ FaultSpec::Failpoint { .. }) => Some(f.clone()),
        };
    }
}

impl JjAdapter {
    fn destroy_inner(&mut self, ws: &WsId, _force: bool) -> Result<StepOutcome> {
        let env = Self::env();
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
        // jj workspace forget <name> — removes the workspace's view from the
        // op-log (recoverable via `jj op restore`). The on-disk dir is left
        // alone by jj; we remove it ourselves to match the other arms'
        // post-destroy fs state.
        let dir = self.ws_dir(ws);
        let res = proc_util::run_envs(
            "jj",
            &["workspace", "forget", &ws.0],
            &self.integration_dir,
            &env_refs,
        );
        // If forget fails (workspace already gone), proceed to fs cleanup.
        let _ = res;
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        Ok(StepOutcome {
            ok: true,
            notes: format!("jj workspace forget {}", ws.0),
            ..StepOutcome::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jj_available() -> bool {
        std::process::Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn minimal_battery_smoke() {
        if !jj_available() {
            eprintln!("skipping: jj not on PATH (requires jj)");
            return;
        }
        let mut s = match JjAdapter::new() {
            Ok(s) => s,
            Err(SubstrateError::BinaryNotFound(_)) => {
                eprintln!("skipping: jj missing");
                return;
            }
            Err(e) => panic!("adapter create: {e}"),
        };
        let ws = WsId::slot(0);
        s.create_workspace(&ws, &BaseRef::Main).expect("create");
        s.edit_file(&ws, "src/lib.rs", "pub fn alpha() {}\n")
            .expect("edit");
        s.commit(&ws, "feat: alpha").expect("commit");
        let merge = s.merge(&[ws.clone()], "main", true).expect("merge");
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
