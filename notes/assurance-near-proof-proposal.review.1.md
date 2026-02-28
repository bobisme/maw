## What’s missing / hand-wavy in the proposal (must be made explicit)

1. **“Dirty working copy” after COMMIT is not “user changes.”**
   After COMMIT advances `refs/heads/<branch>`, the default worktree’s `HEAD` (a symref) now points at the _new_ epoch even though the files on disk are still at the _old_ epoch. Git then reports the entire old checkout as “local modifications.”
   If you do the naïve thing (“stash; checkout; pop”), you can accidentally **re-apply the entire old epoch** and silently undo the update to the new epoch. The proposal needs to call this out and specify the correct primitive.

2. **Your invariants/claims need concrete definitions.**
   Specifically: what counts as “work” (staged/unstaged/untracked; ignored?), what counts as “lost” (unreachable by _which_ refs?), and what “recovery” means (CLI surface, artifact paths, guarantees about discoverability).

3. **You must prohibit destructive operations when you cannot capture.**
   Current behavior in post-merge destroy continues even when status/capture fails. That is the opposite of “provably solid.” The proposal should require that **any operation that can discard user work must be gated by successful capture** (or must abort/skip).

4. **The proposal wants a “preserve-checkout-replay” helper, but it doesn’t define replay semantics precisely.**
   You need a deterministic spec: inputs (base epoch, target ref), outputs (updated worktree + applied user deltas), failure outcomes (rollback vs conflict state), and required artifacts.

5. **No contract docs.**
   You need a repo document that is the “source of truth” for guarantees and failure model. Otherwise every future change regresses safety.

---

## Patch 1: remove `git checkout --force` in default workspace update and make it “no-loss” with snapshot + replay

This directly eliminates the destructive primitive you called out and fixes the subtle “stale checkout looks dirty” trap by basing the user-delta extraction on `epoch_before` from the merge-state.

It also ensures cleanup does not fail the merge: this function now **never returns Err for operational failures** (it warns and returns `Ok(())`), so the merge command won’t die after COMMIT succeeded.

```diff
diff --git a/src/workspace/merge.rs b/src/workspace/merge.rs
--- a/src/workspace/merge.rs
+++ b/src/workspace/merge.rs
@@
-fn update_default_workspace( default_ws_path: &Path, branch: &str, text_mode: bool, ) -> Result<()> { // Checkout the branch by name so default stays attached to it. // The COMMIT phase already advanced refs/heads/{branch} to the new epoch, // so checking out the branch updates the working tree AND keeps HEAD // attached (not detached).
-let output = std::process::Command::new("git") .args(["checkout", "--force", branch]) .current_dir(default_ws_path) .output() .context("Failed to update default workspace")?; if output.status.success() { if text_mode { println!(" Default workspace updated to new epoch."); } } else { let stderr = String::from_utf8_lossy(&output.stderr); bail!( "Failed to update default workspace to new epoch: {}\n \ The merge COMMIT succeeded (refs are updated), but the default workspace \ working copy could not be checked out.\n \ To fix: git -C {} checkout {branch}", stderr.trim(), default_ws_path.display(), ); } Ok(()) }
+fn update_default_workspace(
+    default_ws_path: &Path,
+    branch: &str,
+    text_mode: bool,
+) -> Result<()> {
+    // SAFETY NOTE (critical):
+    //
+    // During COMMIT we advance `refs/heads/{branch}` to the new epoch commit. Since
+    // the default workspace HEAD is a *symref* to that branch, `HEAD` now resolves
+    // to the new epoch even though the working copy files are still at the old epoch.
+    //
+    // A naive `stash; checkout; stash pop` performed *after* the ref move will treat
+    // the entire old epoch checkout as "local changes" and will re-apply it, undoing
+    // the update to the new epoch.
+    //
+    // To preserve only the user's edits, we:
+    //  1) Read `epoch_before` from `.manifold/merge-state.json` (still present in CLEANUP).
+    //  2) Extract two patches that represent only user work:
+    //     - staged patch:   `git diff --cached --binary <epoch_before>`
+    //     - unstaged patch: `git diff --binary` (working tree vs index)
+    //  3) Capture a full snapshot for recovery *iff* either patch is non-empty.
+    //  4) `git reset --hard {branch}` to materialize the new epoch in the worktree.
+    //  5) Re-apply patches onto the new epoch via `git apply --3way`.
+    //
+    // If replay fails, we roll back to the captured snapshot so the user's working copy
+    // remains intact (merge COMMIT already succeeded; cleanup must not lose work).
+
+    fn warn(text_mode: bool, msg: &str) {
+        // In JSON mode we must not print to stdout; stderr is always safe.
+        if text_mode {
+            eprintln!(" WARNING: {msg}");
+        } else {
+            eprintln!("WARNING: {msg}");
+        }
+    }
+
+    // Helper: run a git command and return its Output, but never fail the merge.
+    fn git_output(cwd: &Path, args: &[&str]) -> Option<std::process::Output> {
+        Command::new("git").args(args).current_dir(cwd).output().ok()
+    }
+
+    fn git_apply_from_stdin(
+        cwd: &Path,
+        mut args: Vec<&str>,
+        patch: &[u8],
+    ) -> Option<std::process::Output> {
+        args.push("-");
+        let mut child = Command::new("git")
+            .args(args)
+            .current_dir(cwd)
+            .stdin(Stdio::piped())
+            .stdout(Stdio::piped())
+            .stderr(Stdio::piped())
+            .spawn()
+            .ok()?;
+        if let Some(mut stdin) = child.stdin.take() {
+            let _ = stdin.write_all(patch);
+        }
+        child.wait_with_output().ok()
+    }
+
+    // Determine epoch_before (base for patches) from merge-state.
+    let root = match repo_root() {
+        Ok(r) => r,
+        Err(e) => {
+            warn(
+                text_mode,
+                &format!("Default workspace not updated: could not determine repo root: {e}"),
+            );
+            return Ok(());
+        }
+    };
+    let manifold_dir = root.join(".manifold");
+    let merge_state_path = MergeStateFile::default_path(&manifold_dir);
+    let epoch_before = match MergeStateFile::read(&merge_state_path) {
+        Ok(state) => state.epoch_before.oid().clone(),
+        Err(e) => {
+            warn(
+                text_mode,
+                &format!(
+                    "Default workspace not updated: failed to read merge-state (needed for epoch_before): {e}"
+                ),
+            );
+            warn(
+                text_mode,
+                &format!(
+                    "HINT: Merge COMMIT succeeded. To update manually: git -C {} reset --hard {branch}",
+                    default_ws_path.display()
+                ),
+            );
+            return Ok(());
+        }
+    };
+
+    // Compute user patches (staged + unstaged).
+    let patch_index = match git_output(
+        default_ws_path,
+        &["diff", "--cached", "--binary", epoch_before.as_str()],
+    ) {
+        Some(o) if o.status.success() => o,
+        Some(o) => {
+            warn(
+                text_mode,
+                &format!(
+                    "Default workspace not updated: `git diff --cached` failed: {}",
+                    String::from_utf8_lossy(&o.stderr).trim()
+                ),
+            );
+            return Ok(());
+        }
+        None => {
+            warn(text_mode, "Default workspace not updated: failed to run `git diff --cached`");
+            return Ok(());
+        }
+    };
+    let patch_worktree = match git_output(default_ws_path, &["diff", "--binary"]) {
+        Some(o) if o.status.success() => o,
+        Some(o) => {
+            warn(
+                text_mode,
+                &format!(
+                    "Default workspace not updated: `git diff` failed: {}",
+                    String::from_utf8_lossy(&o.stderr).trim()
+                ),
+            );
+            return Ok(());
+        }
+        None => {
+            warn(text_mode, "Default workspace not updated: failed to run `git diff`");
+            return Ok(());
+        }
+    };
+
+    let has_index_patch = !patch_index.stdout.is_empty();
+    let has_worktree_patch = !patch_worktree.stdout.is_empty();
+    let needs_preserve = has_index_patch || has_worktree_patch;
+
+    // Fast-path: no local edits → safe hard reset to branch tip.
+    if !needs_preserve {
+        let out = match git_output(default_ws_path, &["reset", "--hard", branch]) {
+            Some(o) => o,
+            None => {
+                warn(
+                    text_mode,
+                    "Failed to update default workspace: could not run `git reset --hard`",
+                );
+                return Ok(());
+            }
+        };
+        if out.status.success() {
+            if text_mode {
+                println!(" Default workspace updated to new epoch.");
+            }
+        } else {
+            warn(
+                text_mode,
+                &format!(
+                    "Failed to update default workspace to new epoch: {}",
+                    String::from_utf8_lossy(&out.stderr).trim()
+                ),
+            );
+            warn(
+                text_mode,
+                &format!(
+                    "HINT: Merge COMMIT succeeded. Manual: git -C {} reset --hard {branch}",
+                    default_ws_path.display()
+                ),
+            );
+        }
+        return Ok(());
+    }
+
+    // Capture recovery snapshot before destructive update.
+    let capture = match capture_before_destroy(default_ws_path, DEFAULT_WORKSPACE, &epoch_before) {
+        Ok(Some(c)) => c,
+        Ok(None) => {
+            // This shouldn't happen when `needs_preserve == true`.
+            warn(
+                text_mode,
+                "Default workspace had local edits but capture returned None; refusing to update for safety.",
+            );
+            return Ok(());
+        }
+        Err(e) => {
+            warn(
+                text_mode,
+                &format!(
+                    "Default workspace not updated: failed to capture recovery snapshot before rewrite: {e}"
+                ),
+            );
+            warn(
+                text_mode,
+                &format!(
+                    "HINT: Merge COMMIT succeeded. Manual (risking local edits): git -C {} reset --hard {branch}",
+                    default_ws_path.display()
+                ),
+            );
+            return Ok(());
+        }
+    };
+
+    // Best-effort: write patch + metadata artifacts for humans/tools.
+    let timestamp = super::now_timestamp_iso8601();
+    let safe_ts = timestamp.replace(':', "-");
+    let artifact_dir = manifold_dir
+        .join("artifacts")
+        .join("rewrite")
+        .join(DEFAULT_WORKSPACE)
+        .join(&safe_ts);
+    if std::fs::create_dir_all(&artifact_dir).is_ok() {
+        let _ = std::fs::write(artifact_dir.join("index.patch"), &patch_index.stdout);
+        let _ = std::fs::write(artifact_dir.join("worktree.patch"), &patch_worktree.stdout);
+        #[derive(Serialize)]
+        struct DefaultRewriteArtifact {
+            workspace: String,
+            branch: String,
+            epoch_before: String,
+            snapshot_oid: String,
+            snapshot_ref: String,
+        }
+        let meta = DefaultRewriteArtifact {
+            workspace: DEFAULT_WORKSPACE.to_string(),
+            branch: branch.to_string(),
+            epoch_before: epoch_before.as_str().to_string(),
+            snapshot_oid: capture.snapshot_oid.clone().unwrap_or_default(),
+            snapshot_ref: capture.snapshot_ref.clone().unwrap_or_default(),
+        };
+        let _ = std::fs::write(
+            artifact_dir.join("meta.json"),
+            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
+        );
+    }
+
+    // Materialize the new epoch in the worktree.
+    let out = match git_output(default_ws_path, &["reset", "--hard", branch]) {
+        Some(o) => o,
+        None => {
+            warn(
+                text_mode,
+                "Failed to update default workspace: could not run `git reset --hard`",
+            );
+            return Ok(());
+        }
+    };
+    if !out.status.success() {
+        warn(
+            text_mode,
+            &format!(
+                "Failed to update default workspace to new epoch: {}",
+                String::from_utf8_lossy(&out.stderr).trim()
+            ),
+        );
+        warn(
+            text_mode,
+            &format!(
+                "Local edits preserved:\n  snapshot_ref: {}\n  snapshot_oid: {}\n  artifacts: {}",
+                capture.snapshot_ref.as_deref().unwrap_or("<unknown>"),
+                capture.snapshot_oid.as_deref().unwrap_or("<unknown>"),
+                artifact_dir.display()
+            ),
+        );
+        warn(
+            text_mode,
+            &format!(
+                "Manual update (will discard local edits): git -C {} reset --hard {branch}",
+                default_ws_path.display()
+            ),
+        );
+        return Ok(());
+    }
+
+    // Replay staged edits.
+    if has_index_patch {
+        let out = git_apply_from_stdin(
+            default_ws_path,
+            vec!["apply", "--index", "--3way"],
+            &patch_index.stdout,
+        );
+        if !matches!(out, Some(ref o) if o.status.success()) {
+            let stderr = match out {
+                Some(ref o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
+                None => "<spawn failed>".to_string(),
+            };
+            warn(
+                text_mode,
+                &format!("Failed to replay staged edits onto new epoch: {stderr}"),
+            );
+            warn(
+                text_mode,
+                &format!(
+                    "Rolling back ws/default to snapshot so local work stays visible: {}",
+                    capture.snapshot_oid.as_deref().unwrap_or("<unknown>")
+                ),
+            );
+            if let Some(ref oid) = capture.snapshot_oid {
+                let _ = git_output(default_ws_path, &["reset", "--hard", oid]);
+            }
+            warn(text_mode, &format!("Recovery artifacts: {}", artifact_dir.display()));
+            return Ok(());
+        }
+    }
+
+    // Replay unstaged edits.
+    if has_worktree_patch {
+        let out = git_apply_from_stdin(
+            default_ws_path,
+            vec!["apply", "--3way"],
+            &patch_worktree.stdout,
+        );
+        if !matches!(out, Some(ref o) if o.status.success()) {
+            let stderr = match out {
+                Some(ref o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
+                None => "<spawn failed>".to_string(),
+            };
+            warn(
+                text_mode,
+                &format!("Failed to replay unstaged edits onto new epoch: {stderr}"),
+            );
+            warn(
+                text_mode,
+                &format!(
+                    "Rolling back ws/default to snapshot so local work stays visible: {}",
+                    capture.snapshot_oid.as_deref().unwrap_or("<unknown>")
+                ),
+            );
+            if let Some(ref oid) = capture.snapshot_oid {
+                let _ = git_output(default_ws_path, &["reset", "--hard", oid]);
+            }
+            warn(text_mode, &format!("Recovery artifacts: {}", artifact_dir.display()));
+            return Ok(());
+        }
+    }
+
+    if text_mode {
+        println!(" Default workspace updated to new epoch (local edits preserved).");
+    }
+    Ok(())
+}
```

**What this guarantees immediately:**

- No more `checkout --force` that can blow away a dirty default worktree.
- If the default worktree had staged/unstaged edits, those edits are:
  - snapshotted into `refs/manifold/recovery/default/<timestamp>` (via your existing capture code), and
  - extracted as explicit `index.patch` / `worktree.patch` artifacts in `.manifold/artifacts/rewrite/default/<timestamp>/`.

- If replay fails, the working copy is rolled back to the snapshot so the user doesn’t experience “my edits disappeared”.

---

## Patch 2: do not destroy a workspace if we can’t prove we captured it first

This is the other major “loss” vector in your proposal: continuing with destroy if status/capture fails. That must be forbidden.

```diff
diff --git a/src/workspace/merge.rs b/src/workspace/merge.rs
--- a/src/workspace/merge.rs
+++ b/src/workspace/merge.rs
@@
-let base_epoch = match backend.status(&ws_id) { Ok(status) => status.base_epoch, Err(e) => { eprintln!( " WARNING: Could not get status for '{ws_name}': {e}.\n \ Skipping capture — proceeding with destroy anyway." ); eprintln!( " HINT: If '{ws_name}' had unmerged work, it may be gone.\n \ Check git reflog for recovery options." ); match backend.destroy(&ws_id) { Ok(()) => { if text_mode { println!(" Destroyed: {ws_name}"); } } Err(e2) => eprintln!(" WARNING: Failed to destroy '{ws_name}': {e2}"), } continue; } };
+let base_epoch = match backend.status(&ws_id) { Ok(status) => status.base_epoch, Err(e) => { eprintln!( " WARNING: Could not get status for '{ws_name}': {e}.\n \ Refusing to destroy '{ws_name}' for safety (no base_epoch => no capture)." ); eprintln!( " HINT: Resolve manually, then destroy explicitly if desired:\n \   maw ws status {ws_name}\n \   maw ws destroy {ws_name}" ); continue; } };
@@
-let capture_result = capture_before_destroy(&ws_path, ws_name, base_epoch.oid()); let capture = match capture_result { Ok(c) => c, Err(e) => { eprintln!( " WARNING: Failed to capture state for '{ws_name}' before destroy: {e}" ); eprintln!( " HINT: To attempt manual recovery, run: \ git -C {ws_path} stash list", ws_path = ws_path.display() ); // Emit recovery hint but continue with destroy — merge already succeeded.
+let capture_result = capture_before_destroy(&ws_path, ws_name, base_epoch.oid()); let capture = match capture_result { Ok(c) => c, Err(e) => { eprintln!( " WARNING: Failed to capture state for '{ws_name}' before destroy: {e}" ); eprintln!( " HINT: Refusing to destroy '{ws_name}' for safety. Inspect/work out recovery manually, then destroy explicitly if desired." ); continue; } };
```

This converts “can’t prove it’s safe” into “refuse to destroy”, which is what your proposal is implicitly demanding but the code wasn’t enforcing.

---

## Patch 3: add explicit assurance + recovery contract docs (no more implicit assumptions)

These are new files. They make the “near-proof” proposal operational by defining the claims/assumptions in-repo.

```diff
diff --git a/docs/assurance/claims.md b/docs/assurance/claims.md
new file mode 100644
--- /dev/null
+++ b/docs/assurance/claims.md
@@
+# Assurance claims
+
+This document is the *contract* maw must uphold. Anything not stated here is not guaranteed.
+
+## Definitions
+
+**Repo root**
+The directory containing `.manifold/` and the Git storage (`.git/` for non-bare, or the bare Git dir for bare layouts).
+
+**Epoch**
+The commit id that represents the current “shared truth” (maw’s operational head). In code this is typically `EpochId(GitOid)`.
+
+**User work**
+
+For a given workspace working copy:
+
+* **Committed work**: commits referenced by any Git ref (`refs/**`) or by a maw recovery ref.
+* **Uncommitted work (tracked)**: staged and/or unstaged changes to tracked paths.
+* **Uncommitted work (untracked)**: untracked (but not ignored) paths.
+
+Ignored files are explicitly *not* covered unless we later expand the contract.
+
+**Reachable**
+
+A commit is *reachable* if it is reachable from at least one durable ref under `refs/**` (including maw’s own namespaces). We do **not** rely on reflogs for correctness.
+
+## Failure model
+
+We assume:
+
+* Process crash at any instruction boundary.
+* Power loss at any syscall boundary (filesystem may reorder unless explicitly synced).
+* No adversarial disk corruption; Git behaves per documentation.
+
+## Global safety guarantees
+
+### G1 — No silent loss of committed work
+
+maw must never move a worktree HEAD away from a commit that is not already reachable **without first pinning it** under `refs/manifold/recovery/<workspace>/<timestamp>`.
+
+### G2 — No silent loss of uncommitted work
+
+Before any operation that can overwrite tracked working copy state (e.g. force checkout, hard reset, destructive worktree rewrite), maw must:
+
+1) Create a recovery snapshot pinned to `refs/manifold/recovery/<workspace>/<timestamp>`, and
+2) Write human-readable artifacts under `.manifold/artifacts/**` sufficient to restore the state.
+
+### G3 — Post-COMMIT cleanup is never allowed to “lose” the COMMIT
+
+If COMMIT succeeded (refs updated), subsequent failures must not:
+
+* make the merge appear unsuccessful without clearly stating COMMIT succeeded, or
+* destroy/overwrite any user work.
+
+### G4 — Recovery must be discoverable
+
+Every recovery snapshot created by maw must be discoverable by:
+
+* a deterministic filesystem location under `.manifold/artifacts/**`, and
+* a deterministic Git ref name under `refs/manifold/recovery/**`.
+
+## Required enforcement points
+
+The following code paths are *proof obligations* (must be covered by tests + failure injection):
+
+* Default workspace update after COMMIT (historically `git checkout --force`)
+* Post-merge destroy (`--destroy`)
+* Any “rewrite worktree to new epoch” operation
+* Any “detach/attach/reset” flow that can orphan commits
+
diff --git a/docs/assurance/working-copy.md b/docs/assurance/working-copy.md
new file mode 100644
--- /dev/null
+++ b/docs/assurance/working-copy.md
@@
+# Working copy rewrite semantics (preserve / checkout / replay)
+
+## Why the naïve approach is wrong in maw
+
+After a merge COMMIT, maw advances `refs/heads/<branch>` to a new epoch. If the default
+workspace HEAD is a symref to that branch, then:
+
+* `HEAD` resolves to the new epoch immediately, *even if the files on disk are still at the old epoch*.
+* Git reports the old checkout as “local modifications”.
+
+If you then do:
+
+1. `git stash`
+2. `git checkout <branch>`
+3. `git stash pop`
+
+the stash contains the *entire old checkout* (not just the user’s edits), and popping it
+can silently revert you back to the old epoch.
+
+## Correct primitive (spec)
+
+Inputs:
+
+* `base_epoch`: the commit the files-on-disk currently represent (for merge cleanup this is `epoch_before`)
+* `target_ref`: branch name / ref that points to the new epoch
+
+Algorithm:
+
+1. Extract **user deltas only**:
+   * staged delta: `git diff --cached --binary <base_epoch>`
+   * unstaged delta: `git diff --binary` (working tree vs index)
+2. If either delta is non-empty, create a recovery snapshot:
+   * Pin snapshot under `refs/manifold/recovery/<ws>/<timestamp>`
+   * Write artifacts under `.manifold/artifacts/rewrite/<ws>/<timestamp>/`
+3. Materialize target:
+   * `git reset --hard <target_ref>`
+4. Replay:
+   * Apply staged delta with `git apply --index --3way -`
+   * Apply unstaged delta with `git apply --3way -`
+5. Failure handling:
+   * If replay fails, restore the snapshot (`git reset --hard <snapshot_oid>`) so no work disappears.
+
+Output:
+
+* Either:
+  * updated working copy with deltas applied, or
+  * original working copy restored + explicit recovery information emitted.
+
diff --git a/docs/assurance/recovery-contract.md b/docs/assurance/recovery-contract.md
new file mode 100644
--- /dev/null
+++ b/docs/assurance/recovery-contract.md
@@
+# Recovery contract
+
+This defines what “recoverable” means operationally.
+
+## Recovery surfaces
+
+### Git refs
+
+Any maw-created recovery snapshot must be pinned as:
+
+* `refs/manifold/recovery/<workspace>/<timestamp>`
+
+The snapshot commit id must be printed or recorded in `.manifold/artifacts/**`.
+
+### Filesystem artifacts
+
+Any maw-created recovery snapshot must write a directory:
+
+* `.manifold/artifacts/rewrite/<workspace>/<timestamp>/`
+
+with (at minimum):
+
+* `meta.json` (machine-readable)
+* `index.patch` (may be empty)
+* `worktree.patch` (may be empty)
+
+## Required warning content
+
+If maw cannot safely complete an operation that would otherwise rewrite/destroy state, it must:
+
+* clearly state the operation was not performed (or was rolled back),
+* state whether the merge COMMIT succeeded (if applicable),
+* print the recovery ref + snapshot oid,
+* print the artifact directory.
+
diff --git a/docs/assurance/assurance-near-proof-proposal.md b/docs/assurance/assurance-near-proof-proposal.md
new file mode 100644
--- /dev/null
+++ b/docs/assurance/assurance-near-proof-proposal.md
@@
+# Assurance / near-proof proposal (revised)
+
+This is the implementation-facing version of `assurance-near-proof-proposal.md`.
+It removes implicit assumptions and pins each guarantee to concrete mechanisms.
+
+## Core goals
+
+* Make “lost work” *structurally hard*.
+* Any state-changing operation must be replayable and recoverable.
+* Git remains the concrete persistence layer; maw is the operational/storage layer (jj-like intent).
+
+## Failure model (explicit)
+
+* Crash at any boundary; operations may stop after any Git or filesystem step.
+* No reliance on reflog for correctness.
+
+## Guarantees (normative)
+
+See:
+
+* `docs/assurance/claims.md`
+* `docs/assurance/recovery-contract.md`
+* `docs/assurance/working-copy.md`
+
+## Phase 0 (immediate “stop losing work”)
+
+1) Eliminate any `git checkout --force` / hard reset without capture.
+2) Do not destroy workspaces if status/capture fails.
+3) Ensure post-COMMIT failures never mask that COMMIT succeeded.
+
+Acceptance tests (must exist):
+
+* Default workspace dirty (staged + unstaged), epoch advances, update preserves edits.
+* Replay failure rolls back to snapshot and leaves artifacts.
+* Destroy-after-merge refuses to destroy if capture fails.
+
+## Phase 1 (DST / failpoints)
+
+* Introduce failpoints at:
+  * before/after merge-state write
+  * before/after each ref update in COMMIT
+  * before/after each destructive filesystem operation (rm -rf, worktree prune)
+* Deterministic state-machine test harness that:
+  * enumerates failpoint stops
+  * asserts `claims.md` invariants for each stop
+
+## Phase 2 (algebraic structure)
+
+To move toward “jj-like operational layer”:
+
+* Represent each operation as an element of an *append-only* oplog (monoid under concatenation).
+* Materialized refs/worktrees are derived views.
+* Recovery becomes “recompute view from oplog”, not “best-effort heuristic”.
```

---

## What to do next (non-patch, but required to get to “provably solid”)

If you want the jj-like “operational layer” strength, the hard question is:

**Are you willing to treat worktrees as _derived cache_, not authoritative state?**
If yes, then the real invariant becomes: “operation log + content store is durable; worktrees are reconstructable.” That’s where algebraic structure actually pays off.

Your proposal is already headed that direction (merge-state journal, deterministic engine). The next concrete step is to make the oplog authoritative and make every mutation a logged, replayable op.

But the patches above fix the _known_ real loss vectors now, without rewriting the world.
