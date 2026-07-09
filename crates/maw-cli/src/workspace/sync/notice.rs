//! One-time auto-rebase notices (bn-1abp).
//!
//! When the sibling auto-rebase orchestrator (bn-3vf5/bn-103k) rewrites a
//! workspace that an agent may be actively working in, the agent gets no
//! signal — files change underneath them ("file modified since read"
//! rejections). This module records a small JSON notice in the workspace's
//! manifold artifacts directory at rebase time; the next workspace-scoped
//! command (`maw exec <ws> -- ...`) prints a short, one-time note and
//! consumes the file so it prints exactly once.
//!
//! The notice is advisory: every failure path here logs and continues —
//! it must never abort a merge or block a command.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::auto_rebase::OverlapHint;

/// File name of the notice inside `artifacts/ws/<name>/`.
const NOTICE_FILE: &str = "auto-rebase-notice.json";

/// Recorded when the sibling auto-rebase orchestrator advances a
/// workspace's refs (and usually its worktree) during someone else's merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoRebaseNotice {
    /// The workspace's base epoch before the rebase (full OID).
    pub old_epoch: String,
    /// The epoch the workspace was rebased onto (full OID).
    pub new_epoch: String,
    /// Workspaces whose merge triggered the rebase.
    pub merge_sources: Vec<String>,
    /// Number of workspace commits replayed onto the new epoch.
    pub replayed: usize,
    /// Number of conflict-as-data entries recorded by the rebase (0 = clean).
    pub conflicts: usize,
    /// Whether the files on disk were synchronized to the rebased HEAD.
    /// `false` means refs advanced but the worktree update was skipped.
    pub worktree_updated: bool,
    /// bn-2cvx: semantic-risk hint — set when the epoch range this
    /// workspace was rebased over touches at least one path the workspace
    /// itself also touches. `None` when there was no overlap (or it could
    /// not be computed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<OverlapHint>,
}

/// Path of the notice file for `ws_name` (layout-aware).
fn notice_path(root: &Path, ws_name: &str) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join(NOTICE_FILE)
}

/// Record a notice for `ws_name`. Overwrites any previous unconsumed notice
/// (the newest rebase is the one the agent needs to know about; `old_epoch`
/// of the first rebase is preserved so the agent's frame of reference —
/// "the epoch I started from" — survives back-to-back merges).
///
/// Never fails loudly: errors are logged via `tracing::warn!` and swallowed.
pub fn write_notice(root: &Path, ws_name: &str, notice: &AutoRebaseNotice) {
    let path = notice_path(root, ws_name);
    let merged = match read_notice(&path) {
        // A previous notice was never consumed: keep its old_epoch so the
        // printed message still spans the full distance the workspace moved.
        Some(prev) => AutoRebaseNotice {
            old_epoch: prev.old_epoch,
            merge_sources: {
                let mut all = prev.merge_sources;
                for src in &notice.merge_sources {
                    if !all.contains(src) {
                        all.push(src.clone());
                    }
                }
                all
            },
            replayed: notice.replayed,
            conflicts: notice.conflicts.max(prev.conflicts),
            worktree_updated: notice.worktree_updated,
            new_epoch: notice.new_epoch.clone(),
            // bn-2cvx: keep the latest rebase's overlap hint — it describes
            // the paths touched by the epoch range just replayed over,
            // which is only meaningful relative to the newest rebase.
            overlap: notice.overlap.clone(),
        },
        None => notice.clone(),
    };

    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&merged)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!(
            workspace = %ws_name,
            path = %path.display(),
            error = %e,
            "failed to record auto-rebase notice"
        );
    }
}

fn read_notice(path: &Path) -> Option<AutoRebaseNotice> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Read and DELETE the pending notice for `ws_name`, if any.
///
/// The delete is what makes the printed note one-time. If the delete fails
/// the notice is treated as not-consumed (returns `None`) so we never spam
/// the same note forever on a read-only filesystem.
pub fn take_notice(root: &Path, ws_name: &str) -> Option<AutoRebaseNotice> {
    let path = notice_path(root, ws_name);
    let notice = read_notice(&path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Some(notice),
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                path = %path.display(),
                error = %e,
                "failed to consume auto-rebase notice; suppressing it to avoid repeats"
            );
            None
        }
    }
}

/// Render the one-time message for a consumed notice (1-2 lines).
#[must_use]
pub fn render_notice(ws_name: &str, notice: &AutoRebaseNotice) -> String {
    use std::fmt::Write as _;

    let short = |oid: &str| oid.get(..12).unwrap_or(oid).to_string();
    let sources = if notice.merge_sources.is_empty() {
        "another workspace".to_string()
    } else {
        notice.merge_sources.join(", ")
    };
    let mut msg = format!(
        "NOTE: workspace '{ws_name}' was auto-rebased {} -> epoch {} during merge of {sources} ({} commit(s) replayed",
        short(&notice.old_epoch),
        short(&notice.new_epoch),
        notice.replayed,
    );
    if notice.conflicts > 0 {
        let _ = write!(msg, ", {} conflict(s)", notice.conflicts);
    }
    msg.push_str(").\n");
    if notice.worktree_updated {
        msg.push_str("  Files on disk changed — re-read any open files before editing.");
    } else {
        let _ = write!(
            msg,
            "  Refs advanced but files on disk were NOT updated — run `maw ws sync {ws_name}` to reconcile."
        );
    }
    if notice.conflicts > 0 {
        let _ = write!(msg, " Resolve conflicts: maw ws resolve {ws_name} --list");
    }
    // bn-2cvx: semantic-risk hint — textually clean does not mean safe.
    if let Some(overlap) = &notice.overlap
        && overlap.count > 0
    {
        let _ = write!(
            msg,
            "\n  This rebase also touched {} file(s) this workspace touches — re-run its tests before merging",
            overlap.count
        );
        if !overlap.sample_paths.is_empty() {
            let _ = write!(msg, ": {}", overlap.sample_paths.join(", "));
        }
        msg.push('.');
    }
    msg
}

/// Print (to stderr) and consume the pending auto-rebase notice for
/// `ws_name`, if one exists. Safe to call unconditionally.
pub fn print_notice_if_any(root: &Path, ws_name: &str) {
    if let Some(notice) = take_notice(root, ws_name) {
        eprintln!("{}", render_notice(ws_name, &notice));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AutoRebaseNotice {
        AutoRebaseNotice {
            old_epoch: "a".repeat(40),
            new_epoch: "b".repeat(40),
            merge_sources: vec!["merger".to_string()],
            replayed: 2,
            conflicts: 0,
            worktree_updated: true,
            overlap: None,
        }
    }

    #[test]
    fn render_clean_notice_mentions_epochs_sources_and_reread() {
        let msg = render_notice("sib", &sample());
        assert!(msg.contains("auto-rebased"));
        assert!(msg.contains(&"a".repeat(12)));
        assert!(msg.contains(&"b".repeat(12)));
        assert!(msg.contains("merger"));
        assert!(msg.contains("2 commit(s) replayed"));
        assert!(msg.contains("re-read any open files"));
        // 1-2 lines, per bn-1abp.
        assert!(msg.lines().count() <= 2, "notice too long:\n{msg}");
    }

    #[test]
    fn render_conflict_notice_points_at_resolve() {
        let mut n = sample();
        n.conflicts = 1;
        let msg = render_notice("sib", &n);
        assert!(msg.contains("1 conflict(s)"));
        assert!(msg.contains("maw ws resolve sib --list"));
    }

    #[test]
    fn render_refs_only_notice_points_at_sync() {
        let mut n = sample();
        n.worktree_updated = false;
        let msg = render_notice("sib", &n);
        assert!(msg.contains("NOT updated"));
        assert!(msg.contains("maw ws sync sib"));
    }

    #[test]
    fn write_then_take_consumes_exactly_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Make the root look like a v2 manifold repo so LayoutFlavor
        // resolves manifold_dir to <root>/.manifold.
        std::fs::create_dir_all(tmp.path().join(".manifold")).expect("mkdir");
        let n = sample();
        write_notice(tmp.path(), "sib", &n);
        let taken = take_notice(tmp.path(), "sib").expect("notice should exist");
        assert_eq!(taken, n);
        assert!(
            take_notice(tmp.path(), "sib").is_none(),
            "second take must find nothing"
        );
    }

    #[test]
    fn overwrite_preserves_original_old_epoch_and_unions_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".manifold")).expect("mkdir");
        let first = sample();
        write_notice(tmp.path(), "sib", &first);
        let second = AutoRebaseNotice {
            old_epoch: "b".repeat(40),
            new_epoch: "c".repeat(40),
            merge_sources: vec!["other".to_string()],
            replayed: 3,
            conflicts: 0,
            worktree_updated: true,
            overlap: None,
        };
        write_notice(tmp.path(), "sib", &second);
        let taken = take_notice(tmp.path(), "sib").expect("notice");
        assert_eq!(taken.old_epoch, "a".repeat(40), "first old_epoch kept");
        assert_eq!(taken.new_epoch, "c".repeat(40), "latest new_epoch kept");
        assert_eq!(taken.merge_sources, vec!["merger", "other"]);
        assert_eq!(taken.replayed, 3);
    }

    // -----------------------------------------------------------------
    // bn-2cvx: overlap hint carried in the notice JSON + rendering.
    // -----------------------------------------------------------------

    #[test]
    fn render_notice_with_overlap_mentions_hint_and_paths() {
        let mut n = sample();
        n.overlap = Some(OverlapHint {
            count: 2,
            sample_paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
        });
        let msg = render_notice("sib", &n);
        assert!(msg.contains("2 file(s) this workspace touches"));
        assert!(msg.contains("re-run its tests before merging"));
        assert!(msg.contains("src/lib.rs"));
        assert!(msg.contains("src/main.rs"));
    }

    #[test]
    fn render_notice_without_overlap_omits_hint() {
        let msg = render_notice("sib", &sample());
        assert!(!msg.contains("re-run its tests before merging"));
    }

    #[test]
    fn write_then_take_round_trips_overlap_hint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".manifold")).expect("mkdir");
        let mut n = sample();
        n.overlap = Some(OverlapHint {
            count: 1,
            sample_paths: vec!["shared.txt".to_string()],
        });
        write_notice(tmp.path(), "sib", &n);
        let taken = take_notice(tmp.path(), "sib").expect("notice should exist");
        assert_eq!(taken.overlap, n.overlap);
    }
}
