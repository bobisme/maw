//! `git worktree` + thin written convention. The **honest baseline** maw is
//! compared against (per SG2 pre-reg §1.3 arm 2).
//!
//! The convention is captured in `notes/sg2-worktrees-convention.md` (a
//! committed, agent-readable convention — not maw-style coordination). This
//! adapter encodes exactly the convention surface; it does NOT add maw-style
//! locks, claims, or state files that would bias metrics.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use maw_scenario::{BaseRef, WsId};
use tempfile::TempDir;

use crate::proc_util;
use crate::{Result, StateSnapshot, StepOutcome, Substrate, SubstrateError};

/// `git worktree` adapter implementing the SG2 thin coordination convention.
///
/// # Layout
///
/// ```text
/// <root>/repo.git/             ← bare repository (origin)
/// <root>/main/                 ← integration worktree (acts as "default")
/// <root>/<ws>/                 ← per-ws worktrees
/// <root>/.coord/<ws>.claim     ← convention: claim files per workspace
/// <root>/.coord/destroyed/<ws> ← archived claim + final commit for destroyed ws
/// ```
///
/// # Convention encoded here (not maw-equivalent on purpose)
///
/// - Workspace branch name == workspace id (`refs/heads/<id>`).
/// - Workspace dir == `<root>/<id>`.
/// - To merge, the adapter checks out `main`, runs `git merge --no-ff -m
///   "<msg>" <branches...>`. **Octopus merges fail on conflict** — that is
///   the substrate's actual behavior; the convention does NOT paper over it.
/// - Destroy = `git worktree remove` + `git branch -D <ws>` + claim archive.
/// - Sync = `git rebase main` from within the worktree (the convention's
///   only "agent must know git" requirement).
///
/// # What this adapter does NOT do (asymmetry justifications in parity table)
///
/// - No recovery refs (`refs/manifold/recovery/*`). The reflog is the only
///   recovery surface — exactly what bare worktrees give you.
/// - No epoch counter, no stale-flag, no claim broadcast. The claim file is
///   advisory only — the convention says agents check it; the adapter does
///   not enforce it.
/// - No pre-merge conflict check. Merge attempts that conflict leave the
///   integration branch with unresolved merge state, exactly as `git merge`
///   does in real life.
pub struct WorktreesConventionAdapter {
    root: PathBuf,
    integration_dir: PathBuf,
    coord_dir: PathBuf,
    // Owned tempdir so the substrate vanishes on Drop / cleanup().
    _tmp: Option<TempDir>,
}

impl WorktreesConventionAdapter {
    /// Build a fresh substrate under a private tempdir.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError`] if `git` is missing or any step of
    /// repo/worktree creation fails.
    pub fn new() -> Result<Self> {
        let tmp =
            tempfile::tempdir().map_err(|e| SubstrateError::Io(format!("tempdir: {e}")))?;
        Self::new_in(tmp.path().to_path_buf(), Some(tmp))
    }

    /// Build into a caller-owned directory (used by tests that want to keep
    /// the substrate after run end for inspection).
    ///
    /// # Errors
    ///
    /// Same as [`Self::new`].
    pub fn new_in(root: PathBuf, owned_tmp: Option<TempDir>) -> Result<Self> {
        let bare_dir = root.join("repo.git");
        let integration_dir = root.join("main");
        let coord_dir = root.join(".coord");
        let destroyed_dir = coord_dir.join("destroyed");
        fs::create_dir_all(&destroyed_dir).map_err(|e| SubstrateError::Io(e.to_string()))?;

        // 1. Create bare repo (the "origin"). Bare so worktrees attach to a
        //    canonical shared object store.
        proc_util::run("git", &["init", "--bare", "repo.git"], &root)?;
        // 2. Create the integration worktree against bare repo. We bootstrap
        //    it by cloning the bare repo into "main", then making the bare
        //    repo's HEAD point at main so subsequent `git worktree add` works.
        //    Clone first so we have a working tree to seed an initial commit.
        proc_util::run("git", &["clone", "repo.git", "main"], &root)?;
        // Pin identity inside the integration worktree.
        proc_util::run(
            "git",
            &["config", "user.name", "bench"],
            &integration_dir,
        )?;
        proc_util::run(
            "git",
            &["config", "user.email", "bench@localhost"],
            &integration_dir,
        )?;
        // 3. Seed an initial commit so branches have an ancestor.
        fs::write(integration_dir.join("README.md"), "bench repo\n")
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        proc_util::run("git", &["add", "README.md"], &integration_dir)?;
        proc_util::run(
            "git",
            &["commit", "-m", "init"],
            &integration_dir,
        )?;
        // 4. Push to the bare repo and force its HEAD to main so `worktree
        //    add` from the bare repo finds the right default branch.
        //    `git push` from the clone sets up `main` on the bare repo.
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
            integration_dir,
            coord_dir,
            _tmp: owned_tmp,
        })
    }

    fn ws_dir(&self, ws: &WsId) -> PathBuf {
        self.root.join(&ws.0)
    }

    fn claim_path(&self, ws: &WsId) -> PathBuf {
        self.coord_dir.join(format!("{}.claim", ws.0))
    }

    fn destroyed_dir(&self) -> PathBuf {
        self.coord_dir.join("destroyed")
    }

    fn write_claim(&self, ws: &WsId) -> Result<()> {
        let claim = format!(
            "# worktrees+convention claim file\nworkspace = {}\nbranch = {}\n",
            ws.0, ws.0
        );
        fs::write(self.claim_path(ws), claim)
            .map_err(|e| SubstrateError::Io(format!("claim write: {e}")))?;
        Ok(())
    }
}

impl Substrate for WorktreesConventionAdapter {
    fn arm_name(&self) -> &'static str {
        "git-worktrees-bare"
    }

    fn root(&self) -> &PathBuf {
        &self.root
    }

    fn create_workspace(&mut self, ws: &WsId, base: &BaseRef) -> Result<StepOutcome> {
        let base_ref = match base {
            BaseRef::Main | BaseRef::Epoch => "main",
        };
        // git worktree add -b <ws> <abs-dir> <base>
        // Pass an absolute path so the resulting worktree lives at
        // <root>/<ws> regardless of cwd (the rest of the adapter assumes
        // this layout via ws_dir()).
        let dir = self.ws_dir(ws);
        let dir_str = dir.to_string_lossy().into_owned();
        proc_util::run(
            "git",
            &[
                "worktree",
                "add",
                "-b",
                &ws.0,
                &dir_str,
                base_ref,
            ],
            &self.integration_dir,
        )?;
        // Pin per-worktree identity (the bare repo gitconfig isn't inherited
        // into per-worktree config; this is the convention requirement, not
        // an extra step — without it, commits fail).
        proc_util::run("git", &["config", "user.name", "bench"], &dir)?;
        proc_util::run("git", &["config", "user.email", "bench@localhost"], &dir)?;
        // Convention: write the claim file.
        self.write_claim(ws)?;
        Ok(StepOutcome {
            ok: true,
            notes: format!("git worktree add -b {} {}", ws.0, ws.0),
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
            notes: format!("wrote {path} ({} bytes)", content.len()),
            ..StepOutcome::default()
        })
    }

    fn commit(&mut self, ws: &WsId, msg: &str) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        proc_util::run("git", &["add", "-A"], &dir)?;
        // `git commit` exits non-zero if there's nothing to commit; treat
        // that as ok with `conflicted=false`.
        let status = std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(&dir)
            .env("GIT_AUTHOR_NAME", "bench")
            .env("GIT_AUTHOR_EMAIL", "bench@localhost")
            .env("GIT_COMMITTER_NAME", "bench")
            .env("GIT_COMMITTER_EMAIL", "bench@localhost")
            .output()
            .map_err(|e| SubstrateError::Io(format!("git commit: {e}")))?;
        let stderr = String::from_utf8_lossy(&status.stderr);
        let stdout = String::from_utf8_lossy(&status.stdout);
        if !status.status.success() && !stdout.contains("nothing to commit") {
            return Err(SubstrateError::SubprocessFailed {
                code: status.status.code(),
                stderr: stderr.to_string(),
            });
        }
        Ok(StepOutcome {
            ok: true,
            notes: format!("commit '{msg}'"),
            ..StepOutcome::default()
        })
    }

    fn merge(
        &mut self,
        srcs: &[WsId],
        target: &str,
        destroy_sources: bool,
    ) -> Result<StepOutcome> {
        // Convention: the integration worktree is always "main" (or the
        // single integration label the convention pins). Other targets are
        // not supported by this thin convention — accept "default" or "main"
        // as the same thing.
        if !(target == "main" || target == "default") {
            return Err(SubstrateError::Refused(format!(
                "worktrees+convention supports only 'main'/'default' integration target, got {target}"
            )));
        }
        // Make sure integration worktree is on main.
        proc_util::run("git", &["checkout", "main"], &self.integration_dir)?;
        // Build the merge args: `git merge --no-ff -m "..." <branches...>`.
        // This is an octopus merge; git refuses if any branch conflicts.
        let msg = format!("merge: {}", srcs.iter().map(|s| s.0.as_str()).collect::<Vec<_>>().join(", "));
        let mut args: Vec<String> =
            vec!["merge".into(), "--no-ff".into(), "-m".into(), msg];
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
                    notes: format!("merged {} sources into main", srcs.len()),
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
                // Abort the merge so the integration branch isn't left in a
                // half-merged state — this matches what an attentive git user
                // does manually; the convention documents this step.
                let _ = proc_util::run_lenient(
                    "git",
                    &["merge", "--abort"],
                    &self.integration_dir,
                );
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "merge conflicted; integration aborted (convention rule)".into(),
                    ..StepOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn sync(&mut self, ws: &WsId) -> Result<StepOutcome> {
        // Convention: `git rebase main` from within the worktree. May
        // conflict — convention records the conflict and tells the agent to
        // resolve.
        let dir = self.ws_dir(ws);
        let result = proc_util::run("git", &["rebase", "main"], &dir);
        match result {
            Ok(_) => Ok(StepOutcome {
                ok: true,
                notes: "rebased onto main".into(),
                ..StepOutcome::default()
            }),
            Err(SubstrateError::SubprocessFailed { stderr, .. })
                if stderr.contains("CONFLICT") || stderr.contains("could not apply") =>
            {
                let _ = proc_util::run_lenient("git", &["rebase", "--abort"], &dir);
                Ok(StepOutcome {
                    ok: true,
                    conflicted: true,
                    notes: "rebase conflicted; aborted (convention rule)".into(),
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
            integration_head: Some("main".to_string()),
            ..StateSnapshot::default()
        };
        // List worktrees: `git worktree list --porcelain`.
        let porcelain =
            proc_util::run("git", &["worktree", "list", "--porcelain"], &self.integration_dir)?;
        for block in porcelain.split("\n\n") {
            let mut path_opt: Option<&str> = None;
            let mut branch_opt: Option<&str> = None;
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path_opt = Some(p);
                } else if let Some(b) = line.strip_prefix("branch ") {
                    branch_opt = Some(b.trim_start_matches("refs/heads/"));
                }
            }
            if let (Some(p), Some(b)) = (path_opt, branch_opt) {
                if b == "main" {
                    continue;
                }
                // Workspace name is the branch (== id by convention).
                // Terminal commit message via `git log -1 --format=%s`.
                let msg = proc_util::run(
                    "git",
                    &["log", "-1", "--format=%s", b],
                    &self.integration_dir,
                )
                .unwrap_or_default()
                .trim()
                .to_string();
                snap.live_workspaces.insert(b.to_string(), msg);
                let _ = p; // path captured for future use; not part of snapshot
            }
        }
        // Destroyed list: file presence under .coord/destroyed/.
        if let Ok(rd) = fs::read_dir(self.destroyed_dir()) {
            for entry in rd.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    snap.destroyed_workspaces.push(name.to_string());
                }
            }
            snap.destroyed_workspaces.sort();
        }
        // Integrated files: walk the integration worktree (skipping .git).
        collect_files(&self.integration_dir, &self.integration_dir, &mut snap.integrated_files)?;
        Ok(snap)
    }

    fn cleanup(&mut self) -> Result<()> {
        // Drop the tempdir.
        self._tmp.take();
        Ok(())
    }
}

impl WorktreesConventionAdapter {
    fn destroy_inner(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome> {
        let dir = self.ws_dir(ws);
        // Convention: check the claim file; warn if missing but proceed
        // (claim file is advisory).
        let claim_existed = self.claim_path(ws).exists();
        // Remove worktree. `--force` propagates to allow removing a
        // worktree with uncommitted changes (the convention's only
        // safety-bypass surface).
        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(ws.0.as_str());
        proc_util::run("git", &args, &self.integration_dir)?;
        // Best-effort archive: capture branch tip in destroyed/. The
        // convention only requires *naming* the destroyed workspace; this
        // is the agent-readable surface.
        let archive = self.destroyed_dir().join(&ws.0);
        let tip = proc_util::run("git", &["rev-parse", &ws.0], &self.integration_dir)
            .unwrap_or_default();
        fs::write(&archive, tip).map_err(|e| SubstrateError::Io(e.to_string()))?;
        // Delete branch.
        let _ = proc_util::run_lenient("git", &["branch", "-D", &ws.0], &self.integration_dir);
        // Remove claim file.
        let _ = fs::remove_file(self.claim_path(ws));
        let _ = dir; // ensure unused warning suppressed
        Ok(StepOutcome {
            ok: true,
            notes: format!("destroyed {} (claim existed: {claim_existed})", ws.0),
            ..StepOutcome::default()
        })
    }
}

pub(crate) fn collect_files(
    root: &std::path::Path,
    base: &std::path::Path,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    for entry in
        fs::read_dir(root).map_err(|e| {
            SubstrateError::Io(format!("read_dir {}: {e}", root.display()))
        })?
    {
        let entry = entry.map_err(|e| SubstrateError::Io(e.to_string()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str == ".jj" {
            continue;
        }
        let ft = entry
            .file_type()
            .map_err(|e| SubstrateError::Io(e.to_string()))?;
        if ft.is_dir() {
            collect_files(&path, base, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|e| SubstrateError::Io(e.to_string()))?
                .to_string_lossy()
                .to_string();
            let content =
                fs::read_to_string(&path).unwrap_or_else(|_| String::from("<binary>"));
            out.insert(rel, content);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_adapter() -> WorktreesConventionAdapter {
        WorktreesConventionAdapter::new().expect("git substrate")
    }

    #[test]
    fn minimal_battery_smoke() {
        let mut s = new_adapter();
        let ws = WsId::slot(0);
        s.create_workspace(&ws, &BaseRef::Main).unwrap();
        s.edit_file(&ws, "src/lib.rs", "pub fn alpha() {}\n").unwrap();
        s.commit(&ws, "feat: alpha").unwrap();
        let merge = s.merge(&[ws.clone()], "main", true).unwrap();
        assert!(merge.ok);
        assert!(merge.advanced_integration);
        let snap = s.state_snapshot().unwrap();
        // After destroy=true, the source ws is gone.
        assert!(snap.live_workspaces.is_empty());
        assert!(snap.destroyed_workspaces.contains(&ws.0));
        // Integration head has the merged file.
        assert!(snap
            .integrated_files
            .get("src/lib.rs")
            .map_or(false, |c| c.contains("alpha")));
    }

    #[test]
    fn conflict_returns_conflicted_outcome() {
        let mut s = new_adapter();
        let a = WsId::slot(0);
        let b = WsId::slot(1);
        s.create_workspace(&a, &BaseRef::Main).unwrap();
        s.create_workspace(&b, &BaseRef::Main).unwrap();
        s.edit_file(&a, "shared.txt", "A wins\n").unwrap();
        s.commit(&a, "a").unwrap();
        s.edit_file(&b, "shared.txt", "B wins\n").unwrap();
        s.commit(&b, "b").unwrap();
        s.merge(&[a.clone()], "main", false).unwrap();
        let m = s.merge(&[b.clone()], "main", false).unwrap();
        assert!(m.conflicted, "second merge should conflict on shared.txt");
        assert!(m.ok, "conflict is data, not error");
    }
}
