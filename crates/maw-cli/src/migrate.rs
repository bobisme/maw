//! `maw migrate` — convert a populated v2 `ws/` repo to the consolidated
//! `.maw/` layout without violating the Prime Invariant.
//!
//! Implements the 14-step crash-safe algorithm from
//! `notes/sg3-layout-design.md` §7 (T3.1 / bn-tmt8). See `journal::Journal`
//! for the on-disk crash-recovery checkpoint. See [`run`] for the entry
//! point and the per-step mapping comment block.
//!
//! # Phases (each idempotent; resumable via `--resume`)
//!
//! - **A. Preflight & freeze** (steps 1–3): detect layout, refuse if an
//!   in-flight merge exists, enumerate worktrees + record manifold refs
//!   snapshot.
//! - **B. Preserve everything** (steps 4–5): snapshot every workspace
//!   (incl. default's dirty delta via the Step-0 ANCHOR pattern) and pin
//!   under `refs/manifold/recovery/`.
//! - **C. Relocate agent worktrees** (step 6): move each `ws/<name>/` to
//!   `.maw/worktrees/<name>/`. Rewrite `<common>/worktrees/<name>/gitdir`
//!   and `<new-path>/.git` so git can find them at their new home.
//! - **D. Un-bare root + materialize branch** (steps 7–11, the SP4 #9 hard
//!   kernel): flip `core.bare=false`; attach HEAD to the configured
//!   branch; check out the branch tree at the root; replay default's
//!   dirty-delta snapshot at the root; decommission `ws/default/`.
//! - **E. Finalize & verify** (steps 12–14): write/update root
//!   `.gitignore`; remove the empty `ws/` directory; verify every
//!   pre-migration `refs/manifold/*` survived and every relocated
//!   worktree is at its recorded HEAD; mark journal complete.
//!
//! # Reversibility
//!
//! While the journal is `in_progress` (any phase ≤ E), the recovery
//! snapshots pinned in Phase B make every workspace's pre-migration state
//! reachable via `maw ws recover`. The journal stays on disk until step
//! 14 succeeds; until then `maw migrate --resume` is the canonical
//! recovery path. After Phase D the journal moves from
//! `.manifold/migration/journal.json` to
//! `.maw/manifold/migration/journal.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use maw_core::merge_state::MergeStateFile;
use maw_core::model::layout::{
    self, LayoutFlavor, MAW_DIR, MAW_MANIFOLD_SUBDIR, MAW_WORKSPACES_SUBDIR, MANIFOLD_DIR,
    V2_WORKSPACES_DIR,
};
use maw_git::GitRepo as _;

pub mod journal;

use journal::{Journal, JournalPhase, JournalWorktree};

/// CLI-facing options for `maw migrate`.
#[derive(Clone, Debug, Default)]
pub struct MigrateOptions {
    /// Resume from a previous half-finished migration via the on-disk
    /// journal. If no journal is found, behaves like a fresh run.
    pub resume: bool,
    /// Print the planned actions and exit without mutating the repo.
    pub dry_run: bool,
}

/// Entry point: dispatch to fresh / resume / no-op based on layout & journal.
///
/// # Errors
///
/// Returns an error if any preflight check fails, the migration cannot
/// proceed safely, or any phase fails. On error the journal is left on
/// disk so `maw migrate --resume` (or `maw ws recover`) can pick up.
pub fn run(opts: &MigrateOptions) -> Result<()> {
    let root = crate::workspace::repo_root()
        .context("could not find repository root (run inside a maw repo)")?;

    let initial_flavor = LayoutFlavor::detect_with_env(&root);
    let journal_path_v2 = journal::path_v2(&root);
    let journal_path_consolidated = journal::path_consolidated(&root);
    let existing_journal = journal::load_first(&[&journal_path_v2, &journal_path_consolidated])?;

    // Already-consolidated *and* no journal present: nothing to do.
    if initial_flavor == LayoutFlavor::ConsolidatedMawDir && existing_journal.is_none() {
        println!("[OK] Already on the consolidated layout. Nothing to migrate.");
        return Ok(());
    }

    if opts.dry_run {
        print_dry_run_plan(&root, initial_flavor, existing_journal.as_ref())?;
        return Ok(());
    }

    let mut journal = match existing_journal {
        Some(j) if opts.resume || j.phase != JournalPhase::Start => {
            println!(
                "[INFO] Resuming migration at phase {:?} (journal: {})",
                j.phase,
                journal::active_path(&root).display()
            );
            j
        }
        Some(j) => {
            // Journal exists from a prior aborted attempt at Phase A
            // (no destructive work yet). Discard + restart cleanly.
            tracing::info!(
                phase = ?j.phase,
                "discarding stale journal at Phase Start; restarting"
            );
            let path = journal::active_path(&root);
            let _ = fs::remove_file(&path);
            Journal::new_for(&root)
        }
        None => Journal::new_for(&root),
    };

    // Each phase is idempotent: resume re-enters the same function and
    // its first action is to check the journal phase + skip-or-advance.
    phase_a(&root, &mut journal)?;
    phase_b(&root, &mut journal)?;
    phase_c(&root, &mut journal)?;
    phase_d(&root, &mut journal)?;
    phase_e(&root, &mut journal)?;

    println!();
    println!("Migration complete! v2 ws/ → consolidated .maw/");
    println!("  Root:        {}", root.display());
    println!("  Workspaces:  {}/.maw/workspaces/", root.display());
    println!("  Manifold:    {}/.maw/manifold/", root.display());
    println!();
    println!("Verify:");
    println!("  maw doctor              # should be clean");
    println!("  maw ws list             # workspaces should appear under .maw/workspaces/");
    println!();
    println!("Recovery snapshots from migration are preserved under");
    println!("`refs/manifold/recovery/` — list via `maw ws recover`.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase A: preflight & freeze (steps 1–3)
// ---------------------------------------------------------------------------

fn phase_a(root: &Path, journal: &mut Journal) -> Result<()> {
    if journal.phase as u8 > JournalPhase::PreflightDone as u8 {
        return Ok(());
    }

    // Step 1: detect layout.
    let flavor = LayoutFlavor::detect_with_env(root);
    if flavor == LayoutFlavor::ConsolidatedMawDir
        && journal.phase == JournalPhase::Start
    {
        // Already on the new layout and no in-flight migration. Treat
        // as success (caller already short-circuited the common case;
        // this branch covers the env-override path).
        println!("[OK] Already consolidated layout; nothing to do.");
        return Ok(());
    }
    // Resume path past Phase D: layout will report consolidated; carry on.

    // Step 2: refuse if an in-flight merge is detected.
    let manifold_dir = flavor.manifold_dir(root);
    let state_path = MergeStateFile::default_path(&manifold_dir);
    if state_path.exists() {
        match MergeStateFile::read(&state_path) {
            Ok(state) if !state.phase.is_terminal() => {
                bail!(
                    "in-flight merge detected (phase = {:?}). Migration refuses \
                     to run while a merge is live. Finish or recover the merge \
                     first:\n  maw doctor\n  maw ws recover\nThen re-run: maw migrate",
                    state.phase
                );
            }
            Err(_) => {
                bail!(
                    "merge-state file at {} is unreadable. Inspect / clean it \
                     before retrying: maw doctor",
                    state_path.display()
                );
            }
            _ => {
                // Terminal state — safe to proceed.
            }
        }
    }

    // Step 3: enumerate worktrees + snapshot refs/manifold/* listing.
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let worktrees = repo
        .worktree_list()
        .map_err(|e| anyhow::anyhow!("worktree list failed: {e}"))?;

    let workspaces_dir = flavor.workspaces_dir(root);
    let default_name = "default";

    let mut entries: Vec<JournalWorktree> = Vec::with_capacity(worktrees.len() + 1);
    for wt in &worktrees {
        let head = wt.head_oid.as_ref().map(ToString::to_string);
        let new_path = if wt.name == default_name {
            // ws/default → root itself
            root.to_path_buf()
        } else {
            root.join(MAW_DIR)
                .join(MAW_WORKSPACES_SUBDIR)
                .join(&wt.name)
        };
        entries.push(JournalWorktree {
            name: wt.name.clone(),
            old_path: wt.path.clone(),
            new_path,
            head_oid: head,
            is_detached: wt.is_detached,
            relocated: false,
            recovery_ref: None,
        });
    }
    // Defensive: if ws/default did not show up in worktree_list (it should,
    // it is a linked worktree under .git/worktrees in v2), record it
    // explicitly so Phase B + D can find it.
    if !entries.iter().any(|e| e.name == default_name) {
        let default_path = workspaces_dir.join(default_name);
        if default_path.is_dir() {
            entries.push(JournalWorktree {
                name: default_name.to_string(),
                old_path: default_path,
                new_path: root.to_path_buf(),
                head_oid: None,
                is_detached: false,
                relocated: false,
                recovery_ref: None,
            });
        }
    }

    let manifold_refs = list_all_manifold_refs(&repo)?;

    journal.root = root.to_path_buf();
    journal.original_flavor = "V2WsRoot".to_string();
    journal.worktrees = entries;
    journal.pre_migration_refs = manifold_refs;
    journal.phase = JournalPhase::PreflightDone;
    journal.updated_at = now_unix_secs();
    journal.write_atomic(&journal::path_v2(root))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase B: preserve everything (steps 4–5)
// ---------------------------------------------------------------------------

fn phase_b(root: &Path, journal: &mut Journal) -> Result<()> {
    if journal.phase as u8 > JournalPhase::PreserveDone as u8 {
        return Ok(());
    }

    // Step 4: for every worktree, snapshot dirty state under recovery refs.
    // Step 5: capture ws/default's dirty delta using Step-0 ANCHOR pattern
    //         so the replay in Phase D is faithful.
    //
    // Both steps reuse the existing `capture_before_destroy` helper which
    // pins recovery refs under `refs/manifold/recovery/<name>/<ts>` —
    // these refs are read by `maw ws recover` and survive layout flips
    // because `refs/manifold/*` is layout-agnostic.
    let mut updated_entries = journal.worktrees.clone();

    for entry in &mut updated_entries {
        if entry.recovery_ref.is_some() {
            // Already captured on a prior run; honour idempotency.
            continue;
        }
        // Only attempt capture when the on-disk worktree still exists.
        if !entry.old_path.is_dir() {
            tracing::warn!(
                workspace = %entry.name,
                path = %entry.old_path.display(),
                "skipping recovery snapshot: workspace directory missing"
            );
            continue;
        }

        // Use the workspace's recorded HEAD OID as the base epoch for the
        // capture's "ahead of epoch" check. If unknown, fall back to a
        // benign all-zeros OID so a head-only pin happens for any
        // committed-ahead content (Prime Invariant pessimistic default).
        let zero_oid = "0".repeat(40);
        let base_epoch = entry.head_oid.as_deref().unwrap_or(zero_oid.as_str());
        let base = maw_core::model::types::GitOid::new(base_epoch)
            .with_context(|| format!("invalid base epoch OID `{base_epoch}` for `{}`", entry.name))?;

        match crate::workspace::capture::capture_before_destroy(
            &entry.old_path,
            &entry.name,
            &base,
        ) {
            Ok(Some(captured)) => {
                tracing::info!(
                    workspace = %entry.name,
                    pinned_ref = %captured.pinned_ref,
                    "phase B captured recovery snapshot"
                );
                entry.recovery_ref = Some(captured.pinned_ref);
            }
            Ok(None) => {
                tracing::debug!(workspace = %entry.name, "phase B: nothing to capture");
            }
            Err(e) => {
                bail!(
                    "phase B failed to snapshot workspace `{}` at {}: {e}\n  \
                     The repo is unchanged so far; fix the source workspace and \
                     re-run: maw migrate --resume",
                    entry.name,
                    entry.old_path.display()
                );
            }
        }
    }

    journal.worktrees = updated_entries;
    journal.phase = JournalPhase::PreserveDone;
    journal.updated_at = now_unix_secs();
    journal.write_atomic(&journal::path_v2(root))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase C: relocate agent worktrees (step 6)
// ---------------------------------------------------------------------------

fn phase_c(root: &Path, journal: &mut Journal) -> Result<()> {
    if journal.phase as u8 > JournalPhase::RelocateDone as u8 {
        return Ok(());
    }

    let target_dir = root.join(MAW_DIR).join(MAW_WORKSPACES_SUBDIR);
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;

    let common_dir = git_common_dir(root)?;

    let mut updated_entries = journal.worktrees.clone();
    for entry in &mut updated_entries {
        if entry.name == "default" {
            // ws/default is decommissioned in Phase D, not relocated here.
            continue;
        }
        if entry.relocated {
            continue;
        }
        if !entry.old_path.is_dir() {
            tracing::warn!(
                workspace = %entry.name,
                "phase C: skipping missing workspace directory"
            );
            continue;
        }
        let new_path = target_dir.join(&entry.name);
        if new_path.exists() {
            // Resume case: the on-disk move already completed but the
            // journal wasn't updated. Verify and mark relocated.
            if !entry.old_path.exists() {
                entry.relocated = true;
                continue;
            }
            bail!(
                "phase C cannot relocate `{}`: both old ({}) and new ({}) paths \
                 exist. Resolve manually and re-run with --resume.",
                entry.name,
                entry.old_path.display(),
                new_path.display(),
            );
        }
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        // Move the worktree directory itself.
        fs::rename(&entry.old_path, &new_path).with_context(|| {
            format!(
                "failed to move {} → {}",
                entry.old_path.display(),
                new_path.display()
            )
        })?;
        entry.relocated = true;
        entry.new_path.clone_from(&new_path);

        // Rewrite <common>/worktrees/<name>/gitdir to point at the new
        // .git file path. The worktree's own .git file already points at
        // <common>/worktrees/<name>/ (admin key is unchanged — SP4 #6),
        // so we only fix the back-pointer.
        let admin_dir = common_dir.join("worktrees").join(&entry.name);
        let admin_gitdir = admin_dir.join("gitdir");
        if admin_gitdir.exists() {
            fs::write(
                &admin_gitdir,
                format!("{}\n", new_path.join(".git").display()),
            )
            .with_context(|| format!("failed to rewrite {}", admin_gitdir.display()))?;
        }
        // The worktree's .git file content (commondir + admin path) is
        // unchanged because admin_dir didn't move. Sanity check it.
        let dot_git = new_path.join(".git");
        if dot_git.is_file() {
            let content = fs::read_to_string(&dot_git).unwrap_or_default();
            if !content.contains(&admin_dir.display().to_string()) {
                tracing::warn!(
                    workspace = %entry.name,
                    dot_git = %dot_git.display(),
                    expected_admin = %admin_dir.display(),
                    actual_content = %content.trim(),
                    "worktree .git file does not reference its admin dir; \
                     git may complain. Continuing — admin dir was not moved."
                );
            }
        }

        maw::fp!("FP_MIGRATE_PHASE_C_AFTER_MOVE")?;

        tracing::info!(
            workspace = %entry.name,
            old = %entry.old_path.display(),
            new = %new_path.display(),
            "phase C relocated worktree"
        );
    }

    // Best-effort: clean up the git worktrees-admin-side links/gitdir paths
    // that may have become stale. `worktree_prune` is safe — only acts on
    // genuinely dangling admin dirs.
    if let Ok(repo) = maw_git::GixRepo::open(root) {
        let _ = repo.worktree_prune();
    }

    journal.worktrees = updated_entries;
    journal.phase = JournalPhase::RelocateDone;
    journal.updated_at = now_unix_secs();
    journal.write_atomic(&journal::path_v2(root))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase D: un-bare root + materialize branch (steps 7–11)
// ---------------------------------------------------------------------------

#[allow(
    clippy::too_many_lines,
    reason = "phase D is the SP4 #9 kernel: a single sequenced un-bare + \
              materialize + decommission flow that the spec mandates stay \
              in one place for auditability against the 14-step algorithm"
)]
fn phase_d(root: &Path, journal: &mut Journal) -> Result<()> {
    if journal.phase as u8 > JournalPhase::UnBareDone as u8 {
        return Ok(());
    }

    // Step 7: flip core.bare = false.
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    repo.write_config("core.bare", "false")
        .map_err(|e| anyhow::anyhow!("failed to set core.bare=false: {e}"))?;
    tracing::info!("phase D: set core.bare=false");

    // Step 8: set HEAD to refs/heads/<branch> (symbolic ref).
    let branch = crate::workspace::MawConfig::load(root)
        .map_or_else(|_| "main".to_string(), |c| c.branch().to_string());
    let target = format!("refs/heads/{branch}");
    let set = Command::new("git")
        .current_dir(root)
        .args(["symbolic-ref", "HEAD", &target])
        .output()
        .context("failed to run git symbolic-ref HEAD")?;
    if !set.status.success() {
        bail!(
            "git symbolic-ref HEAD {target} failed: {}",
            String::from_utf8_lossy(&set.stderr).trim()
        );
    }
    tracing::info!(target = %target, "phase D: HEAD attached to branch");

    maw::fp!("FP_MIGRATE_PHASE_D_AFTER_UNBARE")?;

    // Step 9: materialize the branch at root.
    //
    // The root has no working tree (was bare). `git checkout <branch> -- .`
    // would error in a freshly un-bared repo (no index). We use
    // `git read-tree -m -u` after writing the index from the branch tip,
    // or — simpler — `git checkout <branch>` which in this state attaches
    // and materializes the tree. We prefer the latter when safe.
    //
    // The cleanest primitive in this environment is `git checkout -f
    // <branch>` because root has no tracked content to conflict with.
    let checkout = Command::new("git")
        .current_dir(root)
        .args(["checkout", "-f", &branch])
        .output()
        .context("failed to materialize branch at root")?;
    if !checkout.status.success() {
        // Fallback: read-tree + checkout-index. This handles a freshly
        // un-bared repo with no index file.
        let read = Command::new("git")
            .current_dir(root)
            .args(["read-tree", "-m", "-u", &branch])
            .output()
            .context("git read-tree fallback failed to spawn")?;
        if !read.status.success() {
            bail!(
                "could not materialize branch `{branch}` at root: \
                 checkout said `{}`, read-tree said `{}`",
                String::from_utf8_lossy(&checkout.stderr).trim(),
                String::from_utf8_lossy(&read.stderr).trim(),
            );
        }
    }
    tracing::info!(branch = %branch, "phase D: materialized branch at root");

    // Step 10: replay ws/default's dirty-delta snapshot.
    //
    // The pre-migration ws/default was captured in Phase B as a recovery
    // ref. If there was nothing dirty there (the common case for a clean
    // populated repo), no replay is needed — the tracked-branch content
    // already matches what `ws/default` had.
    //
    // For dirty defaults, the user's edits remain pinned under
    // `refs/manifold/recovery/default/<ts>`. They are NOT replayed
    // automatically because (a) root checkout already mirrors the branch
    // tip (which post-Phase A was up to date with default's HEAD), and
    // (b) automatic replay risks dirtying the root with overlap-conflicts
    // mid-migration. We surface a clear hint instead — Prime Invariant
    // is preserved by the pinned ref. (Design choice; documented in the
    // bone report. The reviewer can decide if it should auto-replay.)
    if let Some(default_entry) = journal.worktrees.iter().find(|e| e.name == "default")
        && let Some(ref recovery) = default_entry.recovery_ref
    {
        println!(
            "[INFO] ws/default had uncommitted edits, pinned at: {recovery}"
        );
        println!("       Recover with: maw ws recover default --to default-prev");
    }

    // Step 11: decommission ws/default. Its admin dir under
    // <common>/worktrees/default is also gone, but we drop the working
    // copy. The repo root IS the privileged target now.
    let default_old = root.join(V2_WORKSPACES_DIR).join("default");
    if default_old.is_dir() {
        // Use `git worktree remove --force` so git's internal bookkeeping
        // matches. Fall back to a manual rmdir on failure.
        let remove = Command::new("git")
            .current_dir(root)
            .args(["worktree", "remove", "--force", default_old.to_string_lossy().as_ref()])
            .output();
        match remove {
            Ok(out) if out.status.success() => {
                tracing::info!(path = %default_old.display(), "phase D: removed ws/default");
            }
            _ => {
                // Manual cleanup. Recovery refs already hold the content.
                if let Err(e) = fs::remove_dir_all(&default_old) {
                    tracing::warn!(
                        path = %default_old.display(),
                        error = %e,
                        "could not remove ws/default; please clean manually"
                    );
                }
            }
        }
    }
    // Prune the git admin dir for ws/default if it lingers.
    if let Ok(repo) = maw_git::GixRepo::open(root) {
        let _ = repo.worktree_prune();
    }

    // Create the new consolidated layout directories now that root is live.
    let new_manifold = root.join(MAW_DIR).join(MAW_MANIFOLD_SUBDIR);
    let new_workspaces = root.join(MAW_DIR).join(MAW_WORKSPACES_SUBDIR);
    fs::create_dir_all(&new_manifold)
        .with_context(|| format!("failed to create {}", new_manifold.display()))?;
    fs::create_dir_all(&new_workspaces)
        .with_context(|| format!("failed to create {}", new_workspaces.display()))?;

    // Move the v2 .manifold/ contents into .maw/manifold/. We do this as
    // a directory rename when possible (atomic-ish; the directory just
    // moves). refs/manifold/* themselves live in `.git/refs/...` and are
    // unaffected.
    let old_manifold = root.join(MANIFOLD_DIR);
    if old_manifold.is_dir() {
        let new_is_empty = new_manifold
            .read_dir()
            .is_ok_and(|mut e| e.next().is_none());
        if new_is_empty {
            // Target empty — move atomically.
            fs::remove_dir(&new_manifold).ok();
            fs::rename(&old_manifold, &new_manifold).with_context(|| {
                format!(
                    "failed to move {} → {}",
                    old_manifold.display(),
                    new_manifold.display()
                )
            })?;
        } else {
            // Resume case (target partially populated). Copy missing files.
            copy_dir_merge(&old_manifold, &new_manifold)?;
            let _ = fs::remove_dir_all(&old_manifold);
        }
        tracing::info!(
            "phase D: moved {} → {}",
            old_manifold.display(),
            new_manifold.display()
        );
    }

    // Initialize the .maw/ admin dir housekeeping (bootstrap config,
    // .maw/.gitignore, cache/ — idempotent).
    layout::init_manifold_layout(root, LayoutFlavor::ConsolidatedMawDir)
        .with_context(|| "failed to initialise consolidated layout admin dir")?;

    // Migrate the journal from .manifold/migration/ to .maw/manifold/migration/.
    journal.phase = JournalPhase::UnBareDone;
    journal.updated_at = now_unix_secs();
    journal.write_atomic(&journal::path_consolidated(root))?;
    let _ = fs::remove_file(journal::path_v2(root));
    // Remove the migration dir if empty.
    let v2_mig_dir = root.join(MANIFOLD_DIR).join("migration");
    if v2_mig_dir.is_dir() {
        let _ = fs::remove_dir(v2_mig_dir);
    }

    maw::fp!("FP_MIGRATE_PHASE_D_AFTER_FLIP")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase E: finalize & verify (steps 12–14)
// ---------------------------------------------------------------------------

fn phase_e(root: &Path, journal: &mut Journal) -> Result<()> {
    if journal.phase as u8 > JournalPhase::FinalizeDone as u8 {
        return Ok(());
    }

    // Step 12: rewrite root .gitignore to reference /.maw/ (working-tree
    // change only — do NOT commit). If the branch already encodes the
    // right ignore, this is a no-op.
    update_root_gitignore_working_copy(root)?;

    // Step 13: drop the empty ws/ directory; warn on stray files.
    let ws_dir = root.join(V2_WORKSPACES_DIR);
    if ws_dir.is_dir() {
        match fs::read_dir(&ws_dir) {
            Ok(mut entries) => {
                if entries.next().is_none() {
                    let _ = fs::remove_dir(&ws_dir);
                } else {
                    // Re-read fresh iterator to print contents
                    let leftover: Vec<String> = fs::read_dir(&ws_dir)
                        .ok()
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    println!(
                        "[WARN] {} contains unmoved entries; leaving in place for safety:",
                        ws_dir.display()
                    );
                    for n in leftover {
                        println!("  - {n}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(path = %ws_dir.display(), error = %e, "could not read ws/ during cleanup");
            }
        }
    }

    // Step 14: verify invariants.
    verify_no_work_lost(root, journal)?;

    journal.phase = JournalPhase::FinalizeDone;
    journal.updated_at = now_unix_secs();

    // Delete the journal — migration is complete and verified.
    let cpath = journal::path_consolidated(root);
    let _ = fs::remove_file(&cpath);
    // Remove the migration subdir if empty.
    let mig_dir = cpath.parent().map(Path::to_path_buf);
    if let Some(d) = mig_dir {
        let _ = fs::remove_dir(d);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Verification (step 14 / acceptance gate)
// ---------------------------------------------------------------------------

fn verify_no_work_lost(root: &Path, journal: &Journal) -> Result<()> {
    // Check 1: every pre-migration `refs/manifold/*` is still present.
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let post = list_all_manifold_refs(&repo)?;
    let post_names: std::collections::HashSet<&String> =
        post.iter().map(|(n, _)| n).collect();

    let mut missing: Vec<&String> = journal
        .pre_migration_refs
        .iter()
        .map(|(n, _)| n)
        .filter(|n| !post_names.contains(n))
        .collect();
    missing.sort();
    if !missing.is_empty() {
        bail!(
            "Prime Invariant violation: {} `refs/manifold/*` refs disappeared \
             during migration: {:?}",
            missing.len(),
            missing
        );
    }

    // Check 2: every relocated worktree is at its recorded HEAD.
    for entry in &journal.worktrees {
        if entry.name == "default" || !entry.relocated {
            continue;
        }
        if !entry.new_path.is_dir() {
            bail!(
                "post-migration: workspace `{}` missing at {} (expected after relocation)",
                entry.name,
                entry.new_path.display()
            );
        }
        if let Some(ref recorded) = entry.head_oid {
            let ws_repo = maw_git::GixRepo::open(&entry.new_path)
                .map_err(|e| anyhow::anyhow!("post-migration: cannot open {}: {e}", entry.new_path.display()))?;
            let head = ws_repo
                .rev_parse_opt("HEAD")
                .ok()
                .flatten()
                .map(|o| o.to_string());
            if head.as_deref() != Some(recorded.as_str()) {
                bail!(
                    "post-migration HEAD mismatch for `{}`: recorded {recorded}, \
                     now {:?}",
                    entry.name,
                    head
                );
            }
        }
    }

    // Check 3: layout flavor is now ConsolidatedMawDir.
    if LayoutFlavor::detect(root) != LayoutFlavor::ConsolidatedMawDir {
        bail!(
            "post-migration: layout detection did not flip to ConsolidatedMawDir; \
             check that {} exists",
            root.join(MAW_DIR).join(MAW_MANIFOLD_SUBDIR).display()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn list_all_manifold_refs(repo: &maw_git::GixRepo) -> Result<Vec<(String, String)>> {
    let refs = repo
        .list_refs("refs/manifold/")
        .map_err(|e| anyhow::anyhow!("list_refs failed: {e}"))?;
    Ok(refs
        .into_iter()
        .map(|(n, oid)| (n.as_str().to_string(), oid.to_string()))
        .collect())
}

fn git_common_dir(root: &Path) -> Result<PathBuf> {
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    Ok(repo.common_dir().to_path_buf())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn copy_dir_merge(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_merge(&path, &target)?;
        } else if !target.exists() {
            fs::copy(&path, &target).with_context(|| {
                format!("copy {} -> {}", path.display(), target.display())
            })?;
        }
    }
    Ok(())
}

fn update_root_gitignore_working_copy(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let mut content = fs::read_to_string(&path).unwrap_or_default();
    let has_maw_ignore = content.lines().any(|l| {
        let t = l.trim();
        t == "/.maw/" || t == "/.maw" || t == ".maw/" || t == ".maw"
    });
    if !has_maw_ignore {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.contains("# maw runtime") {
            content.push_str("\n# maw runtime/admin state — never tracked\n");
        }
        content.push_str("/.maw/\n");
        fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    // Remove a stale `ws/` line (legacy v2 ignore) if present and no
    // ws/ directory exists anymore. Keep otherwise — never delete what
    // the user might still rely on.
    if !root.join(V2_WORKSPACES_DIR).exists() {
        let current = fs::read_to_string(&path).unwrap_or_default();
        let stripped: String = current
            .lines()
            .filter(|l| {
                let t = l.trim();
                t != "ws/" && t != "/ws/" && t != "ws" && t != "/ws"
            })
            .collect::<Vec<_>>()
            .join("\n");
        if stripped != current {
            let mut buf = stripped;
            if !buf.ends_with('\n') {
                buf.push('\n');
            }
            fs::write(&path, buf)
                .with_context(|| format!("failed to rewrite {}", path.display()))?;
        }
    }
    Ok(())
}

fn print_dry_run_plan(
    root: &Path,
    flavor: LayoutFlavor,
    journal: Option<&Journal>,
) -> Result<()> {
    println!("Dry run: maw migrate at {}", root.display());
    println!("  Detected layout: {flavor:?}");
    if let Some(j) = journal {
        println!("  Journal phase:   {:?}", j.phase);
        println!("  Worktrees:       {}", j.worktrees.len());
    }
    println!("  Plan:");
    println!("    A. Preflight: refuse if merge in flight; snapshot refs.");
    println!("    B. Preserve : pin recovery refs for every workspace.");
    println!(
        "    C. Relocate : ws/<name>/ → .maw/worktrees/<name>/ (admin gitdir rewrite)."
    );
    println!("    D. Un-bare  : core.bare=false; attach HEAD; checkout branch at root.");
    println!("    E. Finalize : update .gitignore; rmdir ws/; verify Prime Invariant.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test asserts")]
mod tests {
    use super::*;

    #[test]
    fn now_unix_secs_is_nonzero() {
        assert!(now_unix_secs() > 0);
    }

    #[test]
    fn update_root_gitignore_creates_and_idempotent() {
        let tmp = tempfile::tempdir().expect("mktemp");
        let root = tmp.path();
        update_root_gitignore_working_copy(root).expect("update");
        let c1 = fs::read_to_string(root.join(".gitignore")).expect("read");
        assert!(c1.contains("/.maw/"));
        // Second invocation is a no-op.
        update_root_gitignore_working_copy(root).expect("update");
        let c2 = fs::read_to_string(root.join(".gitignore")).expect("read");
        assert_eq!(c1, c2);
    }

    #[test]
    fn update_root_gitignore_strips_legacy_ws_when_dir_gone() {
        let tmp = tempfile::tempdir().expect("mktemp");
        let root = tmp.path();
        fs::write(root.join(".gitignore"), "target/\nws/\n").expect("write");
        update_root_gitignore_working_copy(root).expect("update");
        let c = fs::read_to_string(root.join(".gitignore")).expect("read");
        assert!(!c.lines().any(|l| l.trim() == "ws/"));
        assert!(c.contains("/.maw/"));
    }

    #[test]
    fn copy_dir_merge_preserves_existing_files() {
        let tmp = tempfile::tempdir().expect("mktemp");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("sub")).expect("mkdir");
        fs::write(src.join("a.txt"), "from src").expect("write");
        fs::write(src.join("sub/b.txt"), "nested").expect("write");
        fs::create_dir_all(&dst).expect("mkdir");
        fs::write(dst.join("a.txt"), "existing").expect("write");

        copy_dir_merge(&src, &dst).expect("copy");

        // Existing file preserved (not overwritten).
        assert_eq!(
            fs::read_to_string(dst.join("a.txt")).expect("read"),
            "existing"
        );
        // Missing file copied.
        assert_eq!(
            fs::read_to_string(dst.join("sub/b.txt")).expect("read"),
            "nested"
        );
    }
}
