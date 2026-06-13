use std::path::Path;

use anyhow::{Result, bail};

use maw_core::model::types::{BaseEpoch, WorkspaceId};
use maw_core::refs as manifold_refs;
use maw_git::GitRepo as _;

use crate::workspace::DEFAULT_WORKSPACE;

pub(super) fn is_default_workspace(name: &str) -> bool {
    name == DEFAULT_WORKSPACE
}

pub(super) fn workspace_name_from_cwd(root: &Path, cwd: &Path) -> String {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let ws_root = flavor.workspaces_dir(root);
    let Ok(relative) = cwd.strip_prefix(&ws_root) else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let Some(component) = relative.components().next() else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let std::path::Component::Normal(name) = component else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let Some(name) = name.to_str() else {
        return DEFAULT_WORKSPACE.to_string();
    };

    if WorkspaceId::new(name).is_ok() {
        name.to_owned()
    } else {
        DEFAULT_WORKSPACE.to_string()
    }
}

/// Count commits reachable from HEAD but not from `epoch_oid` inside a workspace.
///
/// Returns the number of committed-but-not-yet-merged commits in the workspace.
/// A result > 0 means the workspace has committed work that should be merged
/// before syncing; syncing over it would wipe those commits.
///
/// Returns `None` if git fails for any reason (invalid repo, unknown OID, etc.).
/// Callers MUST treat `None` as "has committed work" (i.e. refuse to sync) to
/// prevent data loss when the workspace state cannot be determined.
///
/// # Ahead-count correctness and epoch-ref desync (bn-1qtj)
///
/// The `base` argument **must** be the workspace's recorded creation/sync epoch
/// ref (`refs/manifold/epoch/ws/<name>`), not the current epoch. The staleness
/// logic in [`maw_core::backend::git::GitWorktreeBackend::list`] self-heals a
/// lagging epoch ref when the workspace HEAD already equals or descends from
/// the current epoch — so by the time this function is called on an `is_stale`
/// workspace, the ref is either genuine (HEAD is below the current epoch and
/// the count is real workspace work) or it has already been corrected and
/// staleness was cleared. There is therefore no false-ahead path after the
/// self-heal runs.
//
// Takes a [`BaseEpoch`] explicitly (not a bare `&str` or `CurrentEpoch`) so
// that the compiler catches accidental swaps. See bn-18dj for the bug this
// newtype is meant to prevent: passing the current epoch here would silently
// return 0 on stale workspaces and wipe their local commits on sync.
pub fn committed_ahead_of_epoch(ws_path: &Path, base: &BaseEpoch) -> Option<u32> {
    let repo = maw_git::GixRepo::open(ws_path).ok()?;
    let base_oid = repo.rev_parse_opt(base.as_str()).ok().flatten()?;
    let head_oid = repo.rev_parse_opt("HEAD").ok().flatten()?;
    repo.count_commits_between(base_oid, head_oid).ok()
}

pub(super) fn workspace_has_uncommitted_changes(ws_path: &Path) -> Result<bool> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    // HEAD→worktree, including *staged* changes — the true `git status
    // --porcelain` set. The plain `status()` is index→worktree only, so a
    // `git add`-ed file whose worktree copy still equals the staged blob is
    // invisible to it; this is the data-loss gate before `git checkout
    // --detach <epoch>`, so under-reporting here lets sync clobber/orphan
    // staged work (bn-pfh7 class — Prime Invariant: no staged work is lost).
    let entries = repo
        .status_head_to_worktree()
        .map_err(|e| anyhow::anyhow!("status failed in {}: {e}", ws_path.display()))?;
    if !entries.is_empty() {
        return Ok(true);
    }
    Ok(false)
}

/// Return the list of commit OID hex strings reachable from `head_oid` but
/// not from `target_oid_str` in the workspace at `ws_path`.
///
/// Used to build the commit list in the ancestor-refusal error message.
/// Returns `None` if the workspace cannot be opened or the target cannot be
/// resolved (callers fall back to a generic "(unknown)" display).
fn commits_ahead_of_target_hex(
    ws_path: &Path,
    head_oid: maw_git::types::GitOid,
    target_oid_str: &str,
) -> Option<Vec<String>> {
    let repo = maw_git::GixRepo::open(ws_path).ok()?;
    let target_oid = repo.rev_parse_opt(target_oid_str).ok().flatten()?;
    if head_oid == target_oid {
        return Some(Vec::new());
    }
    let oids = repo.walk_commits(target_oid, head_oid, false).ok()?;
    Some(oids.into_iter().map(|oid| format!("{oid}")).collect())
}

/// Whether the sync actually executed a checkout or was safely skipped.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SyncOutcome {
    /// HEAD was successfully moved to the target epoch.
    Synced,
    /// HEAD moved between the caller's decision and the checkout; sync was
    /// aborted to preserve the new commit. The caller's command proceeds
    /// against the current (advanced) HEAD — this is always safe.
    SkippedHeadMoved,
}

/// Sync a single worktree to the given epoch commit.
///
/// Uses `git checkout --detach <epoch>` inside the worktree to update it.
/// This is safe because workspace changes are captured by the merge engine
/// via snapshot before any merge, so uncommitted changes are not lost
/// during the normal workflow. However, this function is only called
/// explicitly by the user/agent via `maw ws sync`.
///
/// `expected_head_hex` — when `Some(hex_oid)`, the checkout is a
/// compare-and-swap: if HEAD has moved to a different OID since the caller's
/// decision (TOCTOU), the sync is SKIPPED and `Ok(SyncOutcome::SkippedHeadMoved)`
/// is returned. The caller's command then proceeds against the now-current HEAD
/// — this is always safe.
///
/// Pass `None` to disable the CAS guard. Used by callers that already hold the
/// workspace lock and re-read HEAD themselves (sibling auto-rebase in
/// `auto_rebase.rs`).
///
/// Emits a success line to stdout. Internal callers that need silence
/// (sibling auto-rebase, bn-3vf5) should call [`sync_worktree_to_epoch_quiet`]
/// instead so the merge summary stays clean.
pub(super) fn sync_worktree_to_epoch(
    root: &Path,
    ws_name: &str,
    epoch_oid: &str,
    expected_head_hex: Option<&str>,
) -> Result<SyncOutcome> {
    sync_worktree_to_epoch_inner(root, ws_name, epoch_oid, expected_head_hex, true)
}

/// Quiet variant of [`sync_worktree_to_epoch`]: same effect, no stdout output.
///
/// Used by the sibling auto-rebase orchestrator so its per-sibling summary is
/// the only line emitted for each sibling — the CLI sync paths still use the
/// chatty wrapper above.
///
/// No CAS guard — callers already hold the workspace lock and re-read HEAD
/// themselves (`auto_rebase.rs`).
pub(super) fn sync_worktree_to_epoch_quiet(
    root: &Path,
    ws_name: &str,
    epoch_oid: &str,
) -> Result<SyncOutcome> {
    sync_worktree_to_epoch_inner(root, ws_name, epoch_oid, None, false)
}

#[expect(
    clippy::too_many_lines,
    reason = "bn-29z8: adds CAS guard + ancestor-refusal pre-flight; sequential steps that should not be split"
)]
fn sync_worktree_to_epoch_inner(
    root: &Path,
    ws_name: &str,
    epoch_oid: &str,
    expected_head_hex: Option<&str>,
    announce: bool,
) -> Result<SyncOutcome> {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let ws_path = flavor.workspace_path(root, ws_name);
    if !ws_path.exists() {
        bail!("Workspace directory does not exist: {}", ws_path.display());
    }

    // Safety: refuse to sync if the workspace has any uncommitted changes.
    // `git checkout --detach` can clobber staged/unstaged tracked edits, and
    // untracked files may become orphaned or conflict with the new tree.
    let is_dirty = workspace_has_uncommitted_changes(&ws_path).map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by sync. \
             Commit or stash first.\n  \
             Check: git -C {} status",
            ws_path.display()
        );
    }

    // bn-29z8 Defect A + B: open the repo once for both the CAS guard and
    // the ancestor-refusal pre-flight.
    let repo = maw_git::GixRepo::open(&ws_path)
        .map_err(|e| anyhow::anyhow!("Failed to open repo at {}: {e}", ws_path.display()))?;

    let current_head = repo
        .rev_parse_opt("HEAD")
        .map_err(|e| anyhow::anyhow!("Failed to rev-parse HEAD in workspace '{ws_name}': {e}"))?;

    // bn-29z8 Defect B (CAS): If the caller captured the HEAD OID at decision
    // time and passed it as `expected_head_hex`, re-read HEAD right now (under
    // the workspace lock if the caller acquired one) and abort the sync if HEAD
    // has moved.
    //
    // This closes the TOCTOU window between the caller's ahead-check and the
    // checkout: a concurrent `git commit` that landed between those two
    // operations will have updated HEAD to a new OID, so the comparison detects
    // the move and we SKIP the sync — preserving the new commit.
    //
    // The failpoint FP_AUTO_SYNC_BEFORE_CHECKOUT fires here (between the CAS
    // decision and the actual checkout) so tests can simulate the race without
    // real thread scheduling. An error action aborts the sync exactly like a
    // HEAD-moved race — the commit is preserved and the caller proceeds with
    // the current HEAD.
    if let Err(e) = maw::fp!("FP_AUTO_SYNC_BEFORE_CHECKOUT") {
        eprintln!(
            "note: auto-sync for workspace '{ws_name}' skipped (failpoint FP_AUTO_SYNC_BEFORE_CHECKOUT): {e}"
        );
        return Ok(SyncOutcome::SkippedHeadMoved);
    }

    if let Some(expected_hex) = expected_head_hex {
        match &current_head {
            None => {
                // Cannot read HEAD — workspace might be in a mid-operation
                // state. Skip the sync to be safe.
                eprintln!(
                    "note: auto-sync for workspace '{ws_name}' skipped \
                     (HEAD unreadable; concurrent operation in progress)"
                );
                return Ok(SyncOutcome::SkippedHeadMoved);
            }
            Some(actual_head) => {
                let actual_hex = format!("{actual_head}");
                if actual_hex != expected_hex {
                    eprintln!(
                        "note: auto-sync for workspace '{ws_name}' skipped — \
                         HEAD moved from {} to {} between decision and checkout \
                         (concurrent commit landed). \
                         Proceeding with command against current HEAD.",
                        &expected_hex[..12],
                        &actual_hex[..12],
                    );
                    return Ok(SyncOutcome::SkippedHeadMoved);
                }
            }
        }
    }

    // bn-29z8 Defect A (refusal): if HEAD is NOT an ancestor-or-equal of the
    // target epoch, fast-forwarding to the epoch would orphan the divergent
    // commits. This is the exact scenario the sigil incident (bn-3d4a) hit:
    // HEAD had a fresh commit, the auto-sync silently fast-forwarded HEAD to
    // epoch, and the commit disappeared without any error.
    //
    // Safe cases:
    //   HEAD == epoch               → already there, nothing to do.
    //   HEAD is an ancestor of epoch → epoch contains HEAD's history; a
    //                                  fast-forward to epoch is safe.
    //   HEAD is on a branch ref     → `git checkout --detach` can't orphan it;
    //                                  the branch ref stays, so the commits are
    //                                  reachable. Change-branch workspaces
    //                                  (created with `--change`) fall here.
    //
    // Unsafe case:
    //   HEAD is detached AND NOT an ancestor of epoch → HEAD has diverged
    //   commits with NO branch reference protecting them. A checkout to epoch
    //   would abandon them (git only warns for detached HEAD). REFUSE loudly
    //   and name the commits.
    //
    // Note: is_ancestor(ancestor=HEAD, descendant=epoch) answers "is HEAD an
    // ancestor of epoch?" — exactly what we need.
    let head_is_detached = repo.head_is_detached().unwrap_or(true); // safe default: assume detached
    if head_is_detached && let Some(ref head_oid) = current_head {
        let target_oid = repo.rev_parse_opt(epoch_oid).map_err(|e| {
            anyhow::anyhow!("Failed to rev-parse epoch {epoch_oid} in workspace '{ws_name}': {e}")
        })?;
        if let Some(target_oid) = target_oid {
            let is_equal = head_oid == &target_oid;
            // is_ancestor(ancestor=HEAD, descendant=epoch) → HEAD reachable from epoch
            let head_is_ancestor_of_epoch =
                repo.is_ancestor(*head_oid, target_oid).unwrap_or(false);
            if !is_equal && !head_is_ancestor_of_epoch {
                // HEAD has commits not reachable from epoch. Collecting the
                // orphaned SHAs for the error message lets the operator
                // identify exactly which commits are at risk.
                let head_hex = format!("{head_oid}");
                let orphaned: Vec<String> =
                    commits_ahead_of_target_hex(&ws_path, *head_oid, epoch_oid)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|h| h[..12].to_string())
                        .collect();
                let orphaned_list = if orphaned.is_empty() {
                    format!("(at least {})", &head_hex[..12])
                } else {
                    orphaned.join(", ")
                };
                bail!(
                    "Refusing to sync workspace '{ws_name}': HEAD ({}) has commit(s) not in \
                     the target epoch's history — syncing would orphan them.\n  \
                     Orphaned commit(s): {orphaned_list}\n  \
                     Fix: maw ws sync {ws_name}  (replays committed work onto the new epoch)",
                    &head_hex[..12],
                );
            }
        }
        // If the target epoch OID cannot be resolved, allow the checkout to
        // proceed — git will fail with a clear, actionable error message.
    }

    // Detach HEAD at the new epoch to sync the workspace.
    // Native gix path: checkout_detach = checkout_tree + set_head (+ reflog).
    // The ancestor-refusal guard above (bn-29z8 Defect A) guarantees HEAD is
    // an ancestor of epoch_oid, so no commits are at risk of orphaning here.
    let epoch_oid_typed = {
        let repo2 = maw_git::GixRepo::open(&ws_path).map_err(|e| {
            anyhow::anyhow!("Failed to re-open repo for checkout in workspace '{ws_name}': {e}")
        })?;
        repo2
            .rev_parse(epoch_oid)
            .map_err(|e| anyhow::anyhow!("Failed to resolve epoch '{epoch_oid}': {e}"))?
    };

    let ws_repo_for_checkout = maw_git::GixRepo::open(&ws_path).map_err(|e| {
        anyhow::anyhow!("Failed to open repo for checkout in workspace '{ws_name}': {e}")
    })?;
    ws_repo_for_checkout
        .checkout_detach(epoch_oid_typed, &ws_path)
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to sync workspace '{ws_name}': {e}\n  \
                 Manual fix: git -C {} checkout --detach {epoch_oid}",
                ws_path.display()
            )
        })?;

    // Update the per-workspace creation epoch ref to the new epoch.
    // After sync, the workspace is rebased onto the new epoch, so
    // the epoch ref should reflect the new base.
    //
    // bn-1qtj: A failed write leaves the workspace permanently stale-by-ref
    // (every subsequent `maw exec` prints a stale warning, and
    // `committed_ahead_of_epoch` counts epoch commits as workspace work).
    // Retry once; if the second attempt still fails, emit a loud stderr
    // WARNING with the exact ref, the OID it should hold, and a copy-pasteable
    // fix command so the operator can repair it manually.
    if let Ok(oid) = maw_core::model::types::GitOid::new(epoch_oid) {
        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
        let write_result = manifold_refs::write_ref(root, &epoch_ref, &oid).or_else(|_first_err| {
            // Retry once before escalating to a loud warning.
            manifold_refs::write_ref(root, &epoch_ref, &oid)
        });
        if let Err(e) = write_result {
            tracing::warn!(
                workspace = %ws_name,
                epoch_ref = %epoch_ref,
                oid = %oid,
                error = %e,
                "failed to update workspace epoch ref after sync — downstream commands may see a stale epoch"
            );
            eprintln!(
                "WARNING: failed to update epoch ref for workspace '{ws_name}' (retried once): {e}"
            );
            eprintln!("  Ref '{epoch_ref}' should hold OID {oid} but could not be written.");
            eprintln!(
                "  Without this ref the workspace will appear stale on every subsequent `maw exec`."
            );
            eprintln!(
                "  Manual fix: git -C {} update-ref {} {}",
                root.display(),
                epoch_ref,
                oid
            );
        }
    }

    if announce {
        println!(
            "  \u{2713} {ws_name} - synced to epoch {}",
            &epoch_oid[..12]
        );
    }
    Ok(SyncOutcome::Synced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn detects_workspace_name_from_workspace_path() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/ws/agent-1/src");
        assert_eq!(workspace_name_from_cwd(root, cwd), "agent-1");
    }

    #[test]
    fn falls_back_to_default_outside_workspace_tree() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/docs");
        assert_eq!(workspace_name_from_cwd(root, cwd), "default");
    }

    #[test]
    fn falls_back_to_default_for_invalid_workspace_segment() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/ws/not_valid");
        assert_eq!(workspace_name_from_cwd(root, cwd), "default");
    }

    #[test]
    fn detects_default_workspace_name() {
        assert!(is_default_workspace("default"));
        assert!(!is_default_workspace("agent-1"));
    }

    // -----------------------------------------------------------------------
    // bn-29z8: unit tests for ancestor-refusal (Defect A) and CAS guard
    // (Defect B) inside sync_worktree_to_epoch.
    //
    // These tests spin up real git repos in tempdir rather than using the
    // full `maw` binary, so they can invoke the Rust functions directly and
    // assert on `SyncOutcome` / error messages without subprocess overhead.
    // -----------------------------------------------------------------------

    /// Helper: run a git command in `dir`, panic on failure.
    fn git_test(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "git {} failed:\n{stderr}",
            args.join(" "),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Create a minimal maw-style repo and return the epoch₀ OID.
    ///
    /// Sets up:
    /// - `git init` + initial commit
    /// - `.manifold/` structure + config
    /// - `ws/default/` worktree
    /// - `refs/manifold/epoch/current` pointing to epoch₀
    fn init_maw_repo(dir: &Path) -> String {
        git_test(dir, &["init"]);
        git_test(dir, &["config", "user.name", "Test"]);
        git_test(dir, &["config", "user.email", "test@localhost"]);
        git_test(dir, &["config", "commit.gpgsign", "false"]);
        git_test(dir, &["checkout", "-B", "main"]);

        std::fs::write(dir.join(".gitignore"), "ws/\n.manifold/\n").expect("write .gitignore");
        git_test(dir, &["add", ".gitignore"]);
        git_test(dir, &["commit", "-m", "epoch0"]);
        let epoch0 = git_test(dir, &["rev-parse", "HEAD"]);

        git_test(dir, &["config", "core.bare", "true"]);
        let idx = dir.join(".git").join("index");
        if idx.exists() {
            std::fs::remove_file(&idx).expect("remove index");
        }

        let manifold = dir.join(".manifold");
        std::fs::create_dir_all(manifold.join("epochs")).expect("create .manifold/epochs");
        std::fs::create_dir_all(manifold.join("artifacts").join("ws"))
            .expect("create .manifold/artifacts/ws");
        std::fs::write(manifold.join("config.toml"), "[repo]\nbranch = \"main\"\n")
            .expect("write config.toml");

        git_test(dir, &["update-ref", "refs/manifold/epoch/current", &epoch0]);
        git_test(
            dir,
            &["update-ref", "refs/manifold/epoch/ws/default", &epoch0],
        );

        let ws_dir = dir.join("ws");
        std::fs::create_dir_all(&ws_dir).expect("create ws/");
        let default_ws = ws_dir.join("default");
        git_test(
            dir,
            &[
                "worktree",
                "add",
                "--detach",
                default_ws.to_str().expect("path to str"),
                &epoch0,
            ],
        );

        epoch0
    }

    /// Create a non-default workspace at `ws/<name>/` and register the epoch ref.
    fn create_ws_test(root: &Path, name: &str, epoch: &str) -> std::path::PathBuf {
        let ws_path = root.join("ws").join(name);
        git_test(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                ws_path.to_str().expect("path to str"),
                epoch,
            ],
        );
        git_test(
            root,
            &[
                "update-ref",
                &format!("refs/manifold/epoch/ws/{name}"),
                epoch,
            ],
        );
        ws_path
    }

    /// Advance epoch: commit a file in `ws/default/`, update epoch refs.
    fn advance_epoch_test(root: &Path, fname: &str, content: &str) -> String {
        let default_ws = root.join("ws").join("default");
        std::fs::write(default_ws.join(fname), content).expect("write epoch file");
        git_test(&default_ws, &["add", "-A"]);
        git_test(&default_ws, &["commit", "-m", &format!("epoch: {fname}")]);
        let new_epoch = git_test(&default_ws, &["rev-parse", "HEAD"]);
        git_test(
            root,
            &["update-ref", "refs/manifold/epoch/current", &new_epoch],
        );
        git_test(root, &["update-ref", "refs/heads/main", &new_epoch]);
        git_test(
            root,
            &["update-ref", "refs/manifold/epoch/ws/default", &new_epoch],
        );
        new_epoch
    }

    // bn-29z8 Defect A: if HEAD has a commit not in the target epoch's ancestry,
    // sync_worktree_to_epoch must REFUSE with an error naming the orphaned SHA.
    #[test]
    fn sync_refuses_when_head_has_commits_not_in_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);

        // Create workspace at epoch0.
        let ws_path = create_ws_test(root, "feat", &epoch0);

        // Add a commit in the workspace (not in the default/epoch branch).
        std::fs::write(ws_path.join("work.txt"), "precious\n").expect("write work.txt");
        git_test(&ws_path, &["add", "work.txt"]);
        git_test(&ws_path, &["commit", "-m", "feat: precious commit"]);
        let commit_sha = git_test(&ws_path, &["rev-parse", "HEAD"]);

        // Advance the epoch (in default workspace, NOT in feat workspace).
        let new_epoch = advance_epoch_test(root, "epoch.txt", "advance\n");

        // Now HEAD (commit_sha) is NOT an ancestor of new_epoch.
        // Calling sync should REFUSE.
        let result = sync_worktree_to_epoch(root, "feat", &new_epoch, None);
        let err = result.expect_err("sync must refuse when HEAD has unmerged commits");
        let msg = err.to_string();

        assert!(
            msg.contains("Refusing to sync workspace 'feat'"),
            "expected refusal message, got: {msg}"
        );
        assert!(
            msg.contains("would orphan"),
            "expected 'would orphan' in message, got: {msg}"
        );
        // The error should name the orphaned commit (at least the first 12 chars).
        let short_sha = &commit_sha[..12];
        assert!(
            msg.contains(short_sha),
            "expected orphaned SHA {short_sha} in message, got: {msg}"
        );
        assert!(
            msg.contains("maw ws sync feat"),
            "expected remediation hint 'maw ws sync feat', got: {msg}"
        );

        // HEAD must not have changed.
        let head_after = git_test(&ws_path, &["rev-parse", "HEAD"]);
        assert_eq!(
            head_after, commit_sha,
            "HEAD must not change when sync is refused"
        );
    }

    // bn-29z8 Defect A (regression): a normal stale+clean fast-forward
    // (HEAD == base_epoch, which IS an ancestor of new_epoch) must still work.
    #[test]
    fn sync_succeeds_for_clean_fast_forward() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);

        create_ws_test(root, "feat", &epoch0);

        // Advance epoch — 'feat' is now stale but clean (no commits of its own).
        let new_epoch = advance_epoch_test(root, "epoch.txt", "advance\n");

        let result =
            sync_worktree_to_epoch(root, "feat", &new_epoch, None).expect("clean fast-forward");
        assert_eq!(
            result,
            SyncOutcome::Synced,
            "clean fast-forward should return Synced"
        );

        let ws_path = root.join("ws").join("feat");
        let head_after = git_test(&ws_path, &["rev-parse", "HEAD"]);
        assert_eq!(
            head_after, new_epoch,
            "HEAD should equal new epoch after sync"
        );
    }

    // bn-29z8 Defect B (CAS): if expected_head_hex doesn't match current HEAD,
    // sync must return SkippedHeadMoved without touching HEAD.
    #[test]
    fn sync_skips_when_expected_head_does_not_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);

        let ws_path = create_ws_test(root, "feat", &epoch0);

        // Advance epoch to make the workspace stale.
        let new_epoch = advance_epoch_test(root, "epoch.txt", "advance\n");

        // Simulate: the caller captured epoch0 as expected_head at decision
        // time, but then a concurrent commit landed (HEAD moved to commit_sha).
        std::fs::write(ws_path.join("concurrent.txt"), "committed\n")
            .expect("write concurrent.txt");
        git_test(&ws_path, &["add", "concurrent.txt"]);
        git_test(&ws_path, &["commit", "-m", "concurrent commit"]);
        let commit_sha = git_test(&ws_path, &["rev-parse", "HEAD"]);

        // Call sync with the OLD expected_head (epoch0) — should be skipped.
        let result = sync_worktree_to_epoch(root, "feat", &new_epoch, Some(&epoch0))
            .expect("CAS skip returns Ok");
        assert_eq!(
            result,
            SyncOutcome::SkippedHeadMoved,
            "sync should return SkippedHeadMoved when expected_head doesn't match"
        );

        // HEAD must not have changed — the concurrent commit is preserved.
        let head_after = git_test(&ws_path, &["rev-parse", "HEAD"]);
        assert_eq!(
            head_after, commit_sha,
            "concurrent commit must survive CAS skip"
        );
    }

    // bn-29z8 Defect B (CAS): when expected_head matches current HEAD and
    // HEAD is an ancestor of epoch, sync should proceed normally.
    #[test]
    fn sync_proceeds_when_expected_head_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);

        create_ws_test(root, "feat", &epoch0);
        let new_epoch = advance_epoch_test(root, "epoch.txt", "advance\n");

        // Pass epoch0 as expected_head — it matches current HEAD.
        let result = sync_worktree_to_epoch(root, "feat", &new_epoch, Some(&epoch0))
            .expect("sync succeeds when expected_head matches");
        assert_eq!(result, SyncOutcome::Synced);
    }

    // bn-29z8: failpoint FP_AUTO_SYNC_BEFORE_CHECKOUT aborts the sync
    // cleanly — HEAD is preserved, caller can proceed.
    #[cfg(feature = "failpoints")]
    #[test]
    fn sync_fp_auto_sync_before_checkout_aborts_cleanly() {
        use maw_core::failpoints::{self, FailpointAction};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);

        let ws_path = create_ws_test(root, "feat", &epoch0);
        let new_epoch = advance_epoch_test(root, "epoch.txt", "advance\n");

        // Record HEAD before the sync attempt.
        let head_before = git_test(&ws_path, &["rev-parse", "HEAD"]);

        // Arm the failpoint.
        failpoints::set(
            "FP_AUTO_SYNC_BEFORE_CHECKOUT",
            FailpointAction::Error("injected by test".into()),
        );
        let result = sync_worktree_to_epoch(root, "feat", &new_epoch, None);
        failpoints::clear("FP_AUTO_SYNC_BEFORE_CHECKOUT");

        // Sync should return SkippedHeadMoved (abort path), not an error.
        let outcome = result.expect("failpoint should cause a clean skip, not propagate an error");
        assert_eq!(
            outcome,
            SyncOutcome::SkippedHeadMoved,
            "failpoint should produce SkippedHeadMoved"
        );

        // HEAD must not have changed.
        let head_after = git_test(&ws_path, &["rev-parse", "HEAD"]);
        assert_eq!(
            head_after, head_before,
            "HEAD must not change when failpoint fires"
        );
    }
}
