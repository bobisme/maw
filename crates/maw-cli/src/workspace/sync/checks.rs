use std::path::Path;

use anyhow::{Result, bail};

use maw_core::model::types::{BaseEpoch, WorkspaceId};
use maw_core::refs as manifold_refs;
use maw_git::GitRepo as _;
use maw_git::types::{FileStatus, StatusEntry};

use crate::workspace::DEFAULT_WORKSPACE;
use crate::workspace::materialize_verify::{
    MaterializeOp, PreOverwriteGuard, format_divergent_pairs, preserve_divergence_before_overwrite,
    verify_clean_materialization,
};

// Re-export the per-workspace rebase lock at crate scope so `maw ws clean`
// (crate::workspace::clean) can take the SAME lock as sync/rebase without
// touching the private `sync/mod.rs` module declaration (bn-auu5). The `lock`
// module is private to `sync`, but `checks` is a sibling within `sync` and so
// can name it.
pub use super::lock::WorkspaceRebaseLock;

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
    Ok(!dirty_status_entries(ws_path)?.is_empty())
}

/// Return the dirty [`StatusEntry`] set (HEAD→worktree, including staged
/// changes) for the workspace at `ws_path`.
///
/// This is the same status computation [`workspace_has_uncommitted_changes`]
/// uses to decide whether to refuse — exposed separately so refusal sites
/// can name the offending paths instead of just reporting "dirty" (bn-3rst).
///
/// See [`workspace_has_uncommitted_changes`] for why HEAD→worktree (not
/// index→worktree) is required here (bn-pfh7 class).
pub(super) fn dirty_status_entries(ws_path: &Path) -> Result<Vec<StatusEntry>> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    repo.status_head_to_worktree()
        .map_err(|e| anyhow::anyhow!("status failed in {}: {e}", ws_path.display()))
}

/// Maximum number of dirty paths listed verbatim in a refusal message before
/// falling back to a "...and N more" summary line.
const MAX_DIRTY_PATHS_SHOWN: usize = 10;

const fn status_letter(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Modified => "M",
        FileStatus::Added => "A",
        FileStatus::Deleted => "D",
        FileStatus::Untracked => "??",
        FileStatus::Renamed => "R",
    }
}

/// Render a capped, human-readable list of dirty paths for refusal messages:
/// up to [`MAX_DIRTY_PATHS_SHOWN`] entries, one per line prefixed with a
/// git-style status letter (`M`/`A`/`D`/`??`/`R`), followed by an
/// `...and N more` summary line if the set was truncated.
///
/// Returns an empty string for an empty slice so callers can splice the
/// result directly into a message without a conditional blank line.
pub(super) fn format_dirty_paths(entries: &[StatusEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = entries
        .iter()
        .take(MAX_DIRTY_PATHS_SHOWN)
        .map(|e| format!("  {} {}", status_letter(e.status), e.path))
        .collect();
    if entries.len() > MAX_DIRTY_PATHS_SHOWN {
        lines.push(format!(
            "  ...and {} more",
            entries.len() - MAX_DIRTY_PATHS_SHOWN
        ));
    }
    lines.join("\n")
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
    let dirty_entries = dirty_status_entries(&ws_path).map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if !dirty_entries.is_empty() {
        // bn-auu5: when any of the offending paths are untracked scratch, point
        // at `maw ws clean` — a guard-friendly, snapshot-backed way to remove
        // them (mess field report: environment safety hooks block rm/git clean).
        let has_untracked = dirty_entries
            .iter()
            .any(|e| matches!(e.status, FileStatus::Untracked | FileStatus::Added));
        let clean_hint = if has_untracked {
            format!(
                "\n  Untracked scratch can be removed safely (with a recovery snapshot): \
                 maw ws clean {ws_name}"
            )
        } else {
            String::new()
        };
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by sync. \
             Commit or stash first.\n\
             {}\n  \
             Check: git -C {} status{}",
            format_dirty_paths(&dirty_entries),
            ws_path.display(),
            clean_hint,
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

    // bn-154g: the checkout below materializes EVERY entry of the target tree
    // with `overwrite_existing = true` — it flattens the whole worktree, not
    // just the epoch delta. The dirty pre-check above proved the worktree is
    // clean *by status*, and status is exactly what the bn-p3m9 corruption
    // class hides from (the index stat cache reports a stale file as
    // unmodified). So a workspace can reach this line believing it is clean,
    // carrying stale bytes that the next statement destroys.
    //
    // The post-checkout `verify_clean_materialization` below CANNOT see that:
    // by the time it runs the divergence is gone and the tree compares clean —
    // right outcome, destroyed evidence, no snapshot. Detect it HERE, before
    // the overwrite, so the pre-overwrite bytes get pinned, warned about and
    // recorded like every other Prime-Invariant site.
    //
    // Placed after the CAS and ancestor guards on purpose: those paths return
    // WITHOUT touching the worktree, so there is nothing to preserve and no
    // reason to pay the detector's cost.
    match preserve_divergence_before_overwrite(
        root,
        ws_name,
        &ws_path,
        MaterializeOp::SyncFastForward,
    ) {
        // Clean (the overwhelmingly common case), or pinned + reported. Either
        // way the checkout may proceed — for `Pinned` the checkout IS the repair.
        PreOverwriteGuard::Proceed | PreOverwriteGuard::Pinned(_) => {}
        // Prime Invariant, fail-safe: divergence found but NOT preservable.
        // Refuse rather than overwrite unsnapshotted bytes. The worktree, HEAD
        // and the epoch ref are all left exactly as they were.
        PreOverwriteGuard::Blocked { paths, error } => {
            bail!(
                "Refusing to sync workspace '{ws_name}': its working tree silently disagrees \
                 with HEAD on {} tracked path(s), and maw could NOT snapshot those bytes before \
                 the sync checkout would overwrite them.\n\
                 {}\n  \
                 Snapshot failed: {error}\n  \
                 `git status` reports this workspace clean because the index stat cache masks \
                 the difference (the bn-p3m9 corruption class).\n  \
                 maw never overwrites bytes it has not first made recoverable, so the sync was \
                 aborted with the worktree untouched.\n  \
                 Fix: copy the listed files aside, then re-run: maw ws sync {ws_name}",
                paths.len(),
                format_divergent_pairs(&paths),
            );
        }
    }

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

    // bn-3gba: this is the fast-forward sync path — no local commits were
    // replayed (the caller routes committed-ahead workspaces to
    // `rebase_workspace`) and the dirty pre-check above proved the worktree was
    // clean on entry. So the contract here is exactly "clean worktree at
    // `epoch_oid`". Assert it; WARN + repair from HEAD if the checkout did not
    // fully land (the bn-p3m9 class). Never fails the sync.
    verify_clean_materialization(root, ws_name, &ws_path, MaterializeOp::SyncFastForward);

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

    // -----------------------------------------------------------------------
    // bn-3rst: sync refusal names the offending dirty paths.
    // -----------------------------------------------------------------------

    // bn-3rst: a dirty workspace (one modified tracked file + one untracked
    // file) must have both paths named in the sync refusal, with the correct
    // status letter for each.
    #[test]
    fn sync_refusal_names_dirty_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);

        let ws_path = create_ws_test(root, "feat", &epoch0);

        // Modify a tracked file (present since init_maw_repo commits
        // .gitignore) and add an untracked scratch file.
        std::fs::write(ws_path.join(".gitignore"), "ws/\n.manifold/\nextra\n")
            .expect("modify .gitignore");
        std::fs::write(ws_path.join("scratch.txt"), "untracked\n").expect("write scratch.txt");

        let result = sync_worktree_to_epoch(root, "feat", &epoch0, None);
        let err = result.expect_err("sync must refuse on a dirty workspace");
        let msg = err.to_string();

        assert!(
            msg.contains("Workspace 'feat' has uncommitted changes that would be lost by sync."),
            "expected refusal message, got: {msg}"
        );
        assert!(
            msg.contains("M .gitignore"),
            "expected modified tracked path with 'M' marker, got: {msg}"
        );
        // Note: `status_head_to_worktree` is HEAD-relative, not
        // index-relative, so a new untracked file surfaces as `Added` (`A`)
        // rather than `FileStatus::Untracked` (`??`) — it doesn't exist in
        // HEAD either way (see the dead-`FileStatus::Untracked` note in
        // init.rs). `format_dirty_paths` still maps `Untracked` to `??` for
        // any caller that does produce it (e.g. `status.rs`'s index-relative
        // view), covered separately by `format_dirty_paths_lists_entries_under_cap`.
        assert!(
            msg.contains("A scratch.txt"),
            "expected untracked path with 'A' marker (HEAD-relative status), got: {msg}"
        );
        // bn-auu5: untracked scratch present → refusal points at `maw ws clean`.
        assert!(
            msg.contains("maw ws clean feat"),
            "expected `maw ws clean` hint for the untracked subset, got: {msg}"
        );
    }

    // bn-auu5: when the ONLY dirty paths are tracked modifications (no
    // untracked), the refusal must NOT emit the `maw ws clean` hint (clean
    // wouldn't help — it never touches tracked files).
    #[test]
    fn sync_refusal_omits_clean_hint_when_no_untracked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let epoch0 = init_maw_repo(root);
        let ws_path = create_ws_test(root, "feat", &epoch0);

        // Only a tracked modification, no untracked files.
        std::fs::write(ws_path.join(".gitignore"), "ws/\n.manifold/\nextra\n")
            .expect("modify tracked .gitignore");

        let result = sync_worktree_to_epoch(root, "feat", &epoch0, None);
        let msg = result
            .expect_err("sync must refuse on a dirty workspace")
            .to_string();
        assert!(
            msg.contains("M .gitignore"),
            "expected the tracked modification listed, got: {msg}"
        );
        assert!(
            !msg.contains("maw ws clean"),
            "clean hint must be omitted when there is no untracked subset, got: {msg}"
        );
    }

    // bn-3rst: format_dirty_paths lists every entry, one per line with its
    // status letter, when the set is at or under the cap.
    #[test]
    fn format_dirty_paths_lists_entries_under_cap() {
        let entries = vec![
            StatusEntry {
                path: "src/a.rs".to_string(),
                status: FileStatus::Modified,
            },
            StatusEntry {
                path: "src/b.rs".to_string(),
                status: FileStatus::Added,
            },
            StatusEntry {
                path: "src/c.rs".to_string(),
                status: FileStatus::Deleted,
            },
            StatusEntry {
                path: "src/d.rs".to_string(),
                status: FileStatus::Untracked,
            },
            StatusEntry {
                path: "src/e.rs".to_string(),
                status: FileStatus::Renamed,
            },
        ];

        let formatted = format_dirty_paths(&entries);

        assert_eq!(
            formatted,
            "  M src/a.rs\n  A src/b.rs\n  D src/c.rs\n  ?? src/d.rs\n  R src/e.rs"
        );
        assert!(
            !formatted.contains("more"),
            "no truncation expected under the cap, got: {formatted}"
        );
    }

    // bn-3rst: when the dirty set exceeds MAX_DIRTY_PATHS_SHOWN, only the
    // first N are listed verbatim and the rest are summarized.
    #[test]
    fn format_dirty_paths_truncates_over_cap() {
        let entries: Vec<StatusEntry> = (0..13)
            .map(|i| StatusEntry {
                path: format!("file{i}.txt"),
                status: FileStatus::Modified,
            })
            .collect();

        let formatted = format_dirty_paths(&entries);
        let lines: Vec<&str> = formatted.lines().collect();

        assert_eq!(
            lines.len(),
            MAX_DIRTY_PATHS_SHOWN + 1,
            "expected {MAX_DIRTY_PATHS_SHOWN} listed paths + 1 summary line, got: {formatted}"
        );
        for (i, line) in lines.iter().enumerate().take(MAX_DIRTY_PATHS_SHOWN) {
            assert!(
                line.contains(&format!("file{i}.txt")),
                "expected file{i}.txt in line {i}, got: {formatted}"
            );
        }
        assert_eq!(lines[MAX_DIRTY_PATHS_SHOWN], "  ...and 3 more");
    }

    // bn-3rst: an empty dirty set formats to an empty string so callers can
    // splice it into a message without a stray blank line.
    #[test]
    fn format_dirty_paths_empty_for_no_entries() {
        assert_eq!(format_dirty_paths(&[]), "");
    }

    // -----------------------------------------------------------------------
    // bn-154g: the sync fast-forward checkout rewrites the ENTIRE worktree, so
    // hidden (index-stat-cache-masked) divergence must be pinned BEFORE it
    // runs. The post-checkout verify cannot see the divergence — the checkout
    // already destroyed it — which is exactly the gap black-box validation
    // found at /tmp/maw-validate/t9: correct outcome, no WARNING, no artifact,
    // no snapshot of the bytes maw overwrote.
    // -----------------------------------------------------------------------

    /// Tracked file the injection poisons.
    const VICTIM: &str = "victim.txt";
    /// Committed content.
    const GOOD: &str = "pub fn answer() -> u32 { 42 }\n";
    /// Stale bytes, SAME byte length as `GOOD` so even a size-only stat
    /// comparison cannot separate them — the strongest form of the mask.
    const STALE: &str = "pub fn answer() -> u32 { 17 }\n";

    fn recovery_refs(root: &Path, ws: &str) -> Vec<String> {
        git_test(
            root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/manifold/recovery/{ws}"),
            ],
        )
        .lines()
        .map(str::to_owned)
        .filter(|l| !l.is_empty())
        .collect()
    }

    fn artifact_files(root: &Path, ws: &str) -> Vec<std::path::PathBuf> {
        let dir = crate::workspace::materialize_verify::artifact_dir(root, ws);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        out.sort();
        out
    }

    /// Reproduce the bn-p3m9 / t9 injection: stale bytes on a tracked path that
    /// every status-shaped query reports as clean.
    ///
    /// Recipe, in the order the real corrupter produces it:
    ///
    /// 1. back-date the victim so the index records an unambiguously old mtime
    ///    (an entry whose mtime equals the index's own is "racily clean" and
    ///    gets re-hashed, which would accidentally rescue a stat-based check);
    /// 2. `set_head_detached` + `unstage_all` + `update-index --refresh`, so
    ///    the index holds HEAD's CORRECT blob OID stamped with that stat data;
    /// 3. overwrite the file with the same-length stale bytes and restore the
    ///    back-dated mtime, so `(size, mtime)` still match the index entry.
    ///
    /// `core.checkStat = minimal` narrows git's comparison to size+mtime. It is
    /// a real, documented setting (recommended on filesystems with unstable
    /// ctime/ino) and the only part of the fingerprint that cannot be
    /// reproduced portably — `ctime` cannot be set from userspace.
    ///
    /// `core.trustCTime = false` must be set ALONGSIDE it, and is load-bearing
    /// for the workspace-dirty check specifically. gix compares `ctime.secs`
    /// whenever `trust_ctime` is on, **independently of `check_stat`**
    /// (`gix_index::entry::stat::Stat::matches`), where git's `minimal` drops
    /// ctime entirely. Writing the stale bytes always bumps ctime, so without
    /// this the mask survives only while the whole injection lands inside one
    /// wall-clock second — a fixture that passes on an idle machine and flakes
    /// under a loaded `just check`. With it, the mask is deterministic.
    ///
    /// Returns whether the mask actually took effect.
    fn poison_with_stat_cache_mask(ws: &Path) -> bool {
        use std::fs::FileTimes;
        use std::time::{Duration, SystemTime};

        git_test(ws, &["config", "core.checkStat", "minimal"]);
        git_test(ws, &["config", "core.trustctime", "false"]);

        let victim = ws.join(VICTIM);
        let backdated = SystemTime::now() - Duration::from_mins(10);
        let times = FileTimes::new()
            .set_accessed(backdated)
            .set_modified(backdated);
        std::fs::File::options()
            .write(true)
            .open(&victim)
            .expect("open victim")
            .set_times(times)
            .expect("back-date victim");

        let repo = maw_git::GixRepo::open(ws).expect("open workspace repo");
        let head = repo.rev_parse("HEAD").expect("rev-parse HEAD");
        repo.set_head_detached(head).expect("set_head_detached");
        repo.unstage_all().expect("unstage_all");
        git_test(ws, &["update-index", "--refresh"]);

        std::fs::write(&victim, STALE).expect("write stale bytes");
        std::fs::File::options()
            .write(true)
            .open(&victim)
            .expect("reopen victim")
            .set_times(times)
            .expect("restore back-dated mtime");

        // Run the query twice: the first can legitimately make git write back a
        // refreshed index, and it is the SECOND, settled answer that the
        // production dirty check will see.
        git_test(ws, &["status", "--porcelain"]).is_empty()
            && git_test(ws, &["status", "--porcelain"]).is_empty()
    }

    /// Build a repo whose `feat` workspace is clean, stale (so `sync` takes the
    /// fast-forward path) and holds the tracked victim file at its committed
    /// content. Returns `(ws_path, target_epoch)`.
    fn setup_stale_clean_workspace(root: &Path) -> (std::path::PathBuf, String) {
        init_maw_repo(root);
        // Commit the victim on trunk, then base `feat` on it.
        let epoch1 = advance_epoch_test(root, VICTIM, GOOD);
        let ws_path = create_ws_test(root, "feat", &epoch1);
        // Advance trunk again so `feat` is stale → sync takes the FF path.
        let new_epoch = advance_epoch_test(root, "other.txt", "advance\n");
        (ws_path, new_epoch)
    }

    /// (a) The regression. A stat-cache-masked stale file must be PINNED,
    /// WARNED about and RECORDED before the sync checkout flattens it — and
    /// the sync must still land the correct content.
    #[test]
    fn sync_ff_pins_hidden_divergence_before_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (ws_path, new_epoch) = setup_stale_clean_workspace(root);

        assert!(
            recovery_refs(root, "feat").is_empty(),
            "fixture must start with no recovery refs"
        );

        let masked = poison_with_stat_cache_mask(&ws_path);
        assert!(
            masked,
            "the fixture failed to produce the mask: `git status --porcelain` already reports \
             the divergence, so this run would NOT be testing what it claims. Fix the fixture \
             (see poison_with_stat_cache_mask) rather than weakening the assertion — a \
             status-based detector passing here would be a false green on the exact class this \
             test exists to catch."
        );
        assert_eq!(
            std::fs::read_to_string(ws_path.join(VICTIM)).expect("read victim"),
            STALE,
            "the injection must leave the stale bytes on disk"
        );

        let outcome =
            sync_worktree_to_epoch(root, "feat", &new_epoch, None).expect("clean fast-forward");
        assert_eq!(outcome, SyncOutcome::Synced);

        // The sync outcome is still correct: HEAD at the new epoch, worktree
        // materialized from it.
        assert_eq!(git_test(&ws_path, &["rev-parse", "HEAD"]), new_epoch);
        assert_eq!(
            std::fs::read_to_string(ws_path.join(VICTIM)).expect("read repaired victim"),
            GOOD,
            "the FF checkout must still land the committed content"
        );
        assert!(
            ws_path.join("other.txt").exists(),
            "the epoch delta must be materialized too"
        );

        // ...and this time the pre-overwrite bytes were preserved.
        let refs = recovery_refs(root, "feat");
        assert_eq!(
            refs.len(),
            1,
            "the pre-overwrite bytes must be pinned to exactly one recovery ref, got: {refs:?}"
        );
        let pinned = &refs[0];
        assert!(
            pinned.contains("/materialize-"),
            "pin must land in the materialize namespace: {pinned}"
        );
        assert_eq!(
            git_test(root, &["show", &format!("{pinned}:{VICTIM}")]),
            STALE.trim_end(),
            "the pin must hold the PRE-overwrite (stale) bytes verbatim"
        );

        // ...and recorded as a JSON artifact naming the path and the mode.
        let artifacts = artifact_files(root, "feat");
        assert_eq!(
            artifacts.len(),
            1,
            "expected one artifact, got {artifacts:?}"
        );
        let json = std::fs::read_to_string(&artifacts[0]).expect("read artifact");
        let record: crate::workspace::materialize_verify::MaterializeRepairRecord =
            serde_json::from_str(&json).expect("artifact must deserialize");
        assert_eq!(
            record.repair_mode,
            crate::workspace::materialize_verify::RepairMode::PreservedBeforeOverwrite,
            "the sync-FF record must say the bytes were pinned BEFORE the overwrite"
        );
        assert_eq!(
            record.operation,
            crate::workspace::materialize_verify::MaterializeOp::SyncFastForward
        );
        assert_eq!(record.paths.len(), 1, "{:?}", record.paths);
        assert_eq!(record.paths[0].path, VICTIM);
        assert_eq!(record.paths[0].status, "M");
        assert_eq!(record.preserved_ref.as_deref(), Some(pinned.as_str()));
    }

    /// (b) The negative control: a genuinely clean fast-forward sync must
    /// produce no warning artifact and no recovery ref. Without this, "pin
    /// everything" would trivially satisfy the test above — and every sync in
    /// the fleet would litter refs.
    #[test]
    fn sync_ff_clean_workspace_pins_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (ws_path, new_epoch) = setup_stale_clean_workspace(root);

        // Same index rewrite the corrupter performs, WITHOUT the stale bytes —
        // so a detector keyed on "the index was rewritten" would false-positive
        // here.
        let repo = maw_git::GixRepo::open(&ws_path).expect("open workspace repo");
        let head = repo.rev_parse("HEAD").expect("rev-parse HEAD");
        repo.set_head_detached(head).expect("set_head_detached");
        repo.unstage_all().expect("unstage_all");

        let outcome =
            sync_worktree_to_epoch(root, "feat", &new_epoch, None).expect("clean fast-forward");
        assert_eq!(outcome, SyncOutcome::Synced);

        assert!(
            recovery_refs(root, "feat").is_empty(),
            "a clean sync must pin nothing: {:?}",
            recovery_refs(root, "feat")
        );
        assert!(
            artifact_files(root, "feat").is_empty(),
            "a clean sync must write no artifact: {:?}",
            artifact_files(root, "feat")
        );
        assert_eq!(
            std::fs::read_to_string(ws_path.join(VICTIM)).expect("read victim"),
            GOOD,
            "the sync must still land the committed content"
        );
    }

    /// (c) A legitimately dirty workspace must still be REFUSED with its paths
    /// named — the pre-overwrite guard runs after that refusal and must not
    /// change it. If the guard ever moved ahead of the dirty check it would
    /// pin every uncommitted edit on every refused sync.
    #[test]
    fn sync_ff_dirty_workspace_still_refuses_and_pins_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (ws_path, new_epoch) = setup_stale_clean_workspace(root);

        std::fs::write(ws_path.join(VICTIM), "REAL uncommitted agent work\n")
            .expect("dirty the victim");
        std::fs::write(ws_path.join("scratch.txt"), "untracked\n").expect("write scratch");

        let msg = sync_worktree_to_epoch(root, "feat", &new_epoch, None)
            .expect_err("sync must refuse on a dirty workspace")
            .to_string();
        assert!(
            msg.contains("Workspace 'feat' has uncommitted changes that would be lost by sync."),
            "the dirty refusal must be unchanged, got: {msg}"
        );
        assert!(
            msg.contains(&format!("M {VICTIM}")),
            "expected the modified path named, got: {msg}"
        );
        assert!(
            msg.contains("A scratch.txt"),
            "expected the untracked path named, got: {msg}"
        );

        assert_eq!(
            std::fs::read_to_string(ws_path.join(VICTIM)).expect("read victim"),
            "REAL uncommitted agent work\n",
            "a refused sync must not touch the worktree"
        );
        assert!(
            recovery_refs(root, "feat").is_empty(),
            "a refused sync must not pin anything: {:?}",
            recovery_refs(root, "feat")
        );
        assert!(
            artifact_files(root, "feat").is_empty(),
            "a refused sync must write no artifact"
        );
    }

    /// The fail-safe. If hidden divergence is found but the snapshot cannot be
    /// taken, the sync must REFUSE rather than let the checkout flatten bytes
    /// that were never made recoverable. The Prime Invariant is unconditional —
    /// it applies even to bytes maw believes are wrong.
    ///
    /// The capture is made to fail with a git-native directory/file ref
    /// conflict: occupying `refs/manifold/recovery/feat` with a ref of its own
    /// makes the nested `refs/manifold/recovery/feat/materialize-<ts>` write
    /// impossible. That is deterministic AND process-local, unlike the
    /// `FP_CLEAN_CAPTURE_BEFORE_PIN` failpoint, whose registry is global and
    /// would leak into the tests running beside this one.
    #[test]
    fn sync_ff_refuses_when_hidden_divergence_cannot_be_pinned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (ws_path, new_epoch) = setup_stale_clean_workspace(root);
        let stale_head = git_test(&ws_path, &["rev-parse", "HEAD"]);

        assert!(poison_with_stat_cache_mask(&ws_path), "mask must take");

        git_test(
            root,
            &["update-ref", "refs/manifold/recovery/feat", &stale_head],
        );

        let result = sync_worktree_to_epoch(root, "feat", &new_epoch, None);

        let msg = result
            .expect_err("an unpreservable divergence must abort the sync")
            .to_string();
        assert!(
            msg.contains("Refusing to sync workspace 'feat'"),
            "expected a refusal, got: {msg}"
        );
        assert!(
            msg.contains(&format!("M {VICTIM}")),
            "the refusal must name the divergent path, got: {msg}"
        );
        assert!(
            msg.contains("could NOT snapshot"),
            "the refusal must say why it aborted, got: {msg}"
        );

        // Nothing was touched: not the bytes, not HEAD.
        assert_eq!(
            std::fs::read_to_string(ws_path.join(VICTIM)).expect("read victim"),
            STALE,
            "the unpreservable bytes must survive the refusal"
        );
        assert_eq!(
            git_test(&ws_path, &["rev-parse", "HEAD"]),
            stale_head,
            "a refused sync must not move HEAD"
        );
    }

    /// The guard must not fire on the paths that return WITHOUT touching the
    /// worktree: a CAS skip leaves the workspace exactly as it was, so there is
    /// nothing to preserve and no reason to pay the detector's cost.
    #[test]
    fn sync_ff_cas_skip_pins_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (ws_path, new_epoch) = setup_stale_clean_workspace(root);
        let stale_head = git_test(&ws_path, &["rev-parse", "HEAD"]);

        assert!(poison_with_stat_cache_mask(&ws_path), "mask must take");

        // Expected HEAD that does not match → CAS skip before the checkout.
        let bogus = "0".repeat(40);
        let outcome = sync_worktree_to_epoch(root, "feat", &new_epoch, Some(&bogus))
            .expect("CAS skip returns Ok");
        assert_eq!(outcome, SyncOutcome::SkippedHeadMoved);

        assert_eq!(
            git_test(&ws_path, &["rev-parse", "HEAD"]),
            stale_head,
            "a CAS skip must not move HEAD"
        );
        assert_eq!(
            std::fs::read_to_string(ws_path.join(VICTIM)).expect("read victim"),
            STALE,
            "a CAS skip must not touch the worktree"
        );
        assert!(
            recovery_refs(root, "feat").is_empty(),
            "no overwrite happened, so nothing should be pinned: {:?}",
            recovery_refs(root, "feat")
        );
    }
}
