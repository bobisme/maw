use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde::Serialize;

use crate::format::OutputFormat;
use crate::ref_gc;
use crate::workspace;

// ---------------------------------------------------------------------------
// Git version check
// ---------------------------------------------------------------------------

/// Minimum supported git version. Features used by maw (e.g. `git worktree`
/// improvements, `--orphan` flag) require at least this version.
const MIN_GIT_VERSION: (u32, u32, u32) = (2, 40, 0);

/// Parse a git version string like "git version 2.47.1" into (major, minor, patch).
///
/// Tolerates extra suffixes (e.g. "2.47.1.windows.1" or "2.39.3 (Apple Git-146)").
fn parse_git_version(version_output: &str) -> Option<(u32, u32, u32)> {
    // Expect first line to start with "git version "
    let line = version_output.lines().next()?;
    let version_str = line.strip_prefix("git version ")?;

    // Split on '.' and parse up to 3 numeric components
    let mut parts = version_str.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    // Patch may contain extra suffixes (e.g. "1 (Apple Git-146)") — take digits only
    let patch: u32 = parts
        .next()
        .and_then(|s| {
            let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);

    Some((major, minor, patch))
}

/// Get the installed git version by running `git --version`.
///
/// Diagnostic carveout: the whole point of this check is to inspect the
/// user's installed git CLI binary. gix's compiled-in version is irrelevant
/// for the warn-if-old check.
fn get_git_version() -> Option<(u32, u32, u32)> {
    let output = Command::new("git").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_git_version(&stdout)
}

/// Emit a warning to stderr if the installed git version is below the minimum.
///
/// This is a no-op if git is not found or the version is at or above the minimum.
/// Intended to be called from `maw init` and other entry points as a soft check.
pub fn warn_git_version_if_old() {
    if let Some(version) = get_git_version()
        && version < MIN_GIT_VERSION
    {
        eprintln!(
            "WARNING: git {}.{}.{} detected; maw requires git {}.{}.{} or later. \
                 Some features may not work correctly.\n  \
                 Upgrade: https://git-scm.com/downloads",
            version.0,
            version.1,
            version.2,
            MIN_GIT_VERSION.0,
            MIN_GIT_VERSION.1,
            MIN_GIT_VERSION.2,
        );
    }
}

#[derive(Serialize)]
struct DoctorEnvelope {
    checks: Vec<DoctorCheck>,
    all_ok: bool,
    advice: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
}

fn print_check(check: &DoctorCheck) {
    let prefix = match check.status.as_str() {
        "ok" => "[OK]",
        "warn" => "[WARN]",
        "fail" => "[FAIL]",
        _ => "[???]",
    };
    println!("{} {}", prefix, check.message);
    if let Some(fix) = &check.fix {
        println!("       {fix}");
    }
}

#[allow(clippy::unnecessary_wraps)]
/// # Errors
///
/// Returns an error if repository checks or output serialization fail.
pub fn run(format: Option<OutputFormat>) -> Result<()> {
    run_with_repair(format, false)
}

/// Run all doctor checks (bn-1ieb).
///
/// When `repair = true`, attempt auto-fixes for checks with a known-safe
/// repair path. Currently the only such path is `ff_absorbable` epoch
/// drift, which is auto-advanced via
/// [`crate::workspace::epoch_drift::auto_advance_if_safe`] (the FF-absorb
/// safety predicate guarantees no in-flight workspace's diff3 base would
/// change). All other checks are run unchanged.
///
/// # Errors
///
/// Returns an error if output serialization fails. Individual check or
/// repair errors are reported in-band as `DoctorCheck` entries.
#[allow(clippy::unnecessary_wraps)]
pub fn run_with_repair(format: Option<OutputFormat>, repair: bool) -> Result<()> {
    let format = OutputFormat::resolve(format);
    let mut checks = Vec::new();

    checks.push(check_tool(
        "git",
        &["--version"],
        "https://git-scm.com/downloads",
    ));
    checks.push(check_git_version());

    let root = workspace::repo_root().ok();

    checks.push(check_manifold_initialized(root.as_deref()));
    checks.push(check_default_workspace(root.as_deref()));
    checks.push(check_lfs(root.as_deref()));
    checks.push(check_root_bare(root.as_deref()));
    checks.push(check_ghost_working_copy(root.as_deref()));
    checks.push(check_dangling_snapshots(root.as_deref()));
    let (abandoned, unpinned) = check_destroy_records(root.as_deref());
    checks.push(abandoned);
    checks.push(unpinned);
    checks.push(check_stale_head_refs(root.as_deref()));
    checks.push(check_merge_state(root.as_deref()));
    if repair {
        // Try the auto-advance BEFORE the drift check so the check reflects
        // the post-repair state. Push a repair receipt so the user sees
        // exactly what was advanced.
        if let Some(receipt) = try_repair_epoch_drift(root.as_deref()) {
            checks.push(receipt);
        }
    }
    checks.push(check_epoch_drift(root.as_deref()));
    checks.push(check_git_head());

    let all_ok = checks.iter().all(|c| c.status == "ok");

    match format {
        OutputFormat::Json => {
            let envelope = DoctorEnvelope {
                checks,
                all_ok,
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text | OutputFormat::Pretty => {
            println!("maw doctor");
            println!("==========");
            println!();

            for check in &checks {
                print_check(check);
            }

            println!();
            if all_ok {
                println!("All checks passed!");
            } else {
                println!("Some checks failed. See above for details.");
            }
        }
    }

    Ok(())
}

/// Attempt the epoch auto-advance when safe (bn-1ieb). Returns a
/// `DoctorCheck` "receipt" entry only when a meaningful action was taken
/// (advance succeeded) or when the user explicitly asked to repair but
/// the drift was non-auto-fixable (so they see the structured "skipped:
/// reason" message). Returns `None` for the in-sync / epoch-unset cases
/// to avoid noise.
fn try_repair_epoch_drift(root: Option<&Path>) -> Option<DoctorCheck> {
    use crate::workspace::epoch_drift::{
        AutoAdvanceOutcome, AutoAdvanceSkip, auto_advance_if_safe,
    };

    let root = root?;
    let config = crate::workspace::MawConfig::load(root).ok()?;
    let branch = config.branch();
    let default_ws = config.default_workspace();
    let backend = maw_core::backend::git::GitWorktreeBackend::new(root.to_path_buf());

    match auto_advance_if_safe(root, branch, default_ws, &backend) {
        Ok(AutoAdvanceOutcome::Advanced {
            report,
            new_epoch_short,
        }) => Some(DoctorCheck {
            name: "epoch repair".to_string(),
            status: "ok".to_string(),
            message: format!(
                "epoch repair: advanced epoch {} → {} ({} commit(s) absorbed on branch '{}').",
                report.epoch_short, new_epoch_short, report.ff_commit_count, report.branch,
            ),
            fix: None,
        }),
        Ok(AutoAdvanceOutcome::NoOp {
            reason: AutoAdvanceSkip::InSync | AutoAdvanceSkip::EpochUnset,
        }) => None,
        Ok(AutoAdvanceOutcome::NoOp {
            reason: AutoAdvanceSkip::FfBlocked(report),
        }) => Some(DoctorCheck {
            name: "epoch repair".to_string(),
            status: "warn".to_string(),
            message: format!(
                "epoch repair: skipped — branch '{}' ahead by {} commit(s) but blocked by workspace(s): {}",
                report.branch,
                report.ff_commit_count,
                report.blocking_workspaces.join(", "),
            ),
            fix: Some(format!(
                "Resolve first: maw ws merge {} --into default --check",
                report
                    .blocking_workspaces
                    .first()
                    .map_or("<ws>", String::as_str),
            )),
        }),
        Ok(AutoAdvanceOutcome::NoOp {
            reason: AutoAdvanceSkip::Diverged(report),
        }) => Some(DoctorCheck {
            name: "epoch repair".to_string(),
            status: "fail".to_string(),
            message: format!(
                "epoch repair: refused — epoch ({}) and branch '{}' ({}) have forked; auto-advance unsafe.",
                report.epoch_short, report.branch, report.branch_short,
            ),
            fix: Some("Investigate with: git log --oneline --all".to_string()),
        }),
        Err(e) => Some(DoctorCheck {
            name: "epoch repair".to_string(),
            status: "warn".to_string(),
            message: format!("epoch repair: classify/advance failed ({e})"),
            fix: None,
        }),
    }
}

/// Diagnostic carveout: probes the user's installed `<name>` binary (here:
/// "git", "git-lfs", etc.) to surface its presence and version in `maw doctor`.
/// This is intentionally a subprocess: it's checking the external tool, not
/// performing a repo operation.
fn check_tool(name: &str, args: &[&str], install_url: &str) -> DoctorCheck {
    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version = version.lines().next().unwrap_or("unknown").trim();
            DoctorCheck {
                name: name.to_string(),
                status: "ok".to_string(),
                message: format!("{name}: {version}"),
                fix: None,
            }
        }
        Ok(_) => DoctorCheck {
            name: name.to_string(),
            status: "fail".to_string(),
            message: format!("{name}: found but returned error"),
            fix: Some(format!("Install: {install_url}")),
        },
        Err(_) => DoctorCheck {
            name: name.to_string(),
            status: "fail".to_string(),
            message: format!("{name}: not found"),
            fix: Some(format!("Install: {install_url}")),
        },
    }
}

fn check_git_version() -> DoctorCheck {
    match get_git_version() {
        Some(version) if version >= MIN_GIT_VERSION => DoctorCheck {
            name: "git version".to_string(),
            status: "ok".to_string(),
            message: format!(
                "git version: {}.{}.{} (>= {}.{}.{})",
                version.0,
                version.1,
                version.2,
                MIN_GIT_VERSION.0,
                MIN_GIT_VERSION.1,
                MIN_GIT_VERSION.2
            ),
            fix: None,
        },
        Some(version) => DoctorCheck {
            name: "git version".to_string(),
            status: "warn".to_string(),
            message: format!(
                "git version: {}.{}.{} (minimum {}.{}.{} recommended)",
                version.0,
                version.1,
                version.2,
                MIN_GIT_VERSION.0,
                MIN_GIT_VERSION.1,
                MIN_GIT_VERSION.2
            ),
            fix: Some("Upgrade: https://git-scm.com/downloads".to_string()),
        },
        None => DoctorCheck {
            name: "git version".to_string(),
            status: "warn".to_string(),
            message: "git version: could not determine version".to_string(),
            fix: Some("Ensure git is installed: https://git-scm.com/downloads".to_string()),
        },
    }
}

fn check_manifold_initialized(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "manifold metadata".to_string(),
            status: "warn".to_string(),
            message: "manifold metadata: could not determine repo root".to_string(),
            fix: None,
        };
    };

    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let manifold = flavor.manifold_dir(root);
    if manifold.exists() {
        let label = match flavor {
            maw_core::model::layout::LayoutFlavor::ConsolidatedMawDir => ".maw/manifold/",
            maw_core::model::layout::LayoutFlavor::V2WsRoot => ".manifold/",
        };
        DoctorCheck {
            name: "manifold metadata".to_string(),
            status: "ok".to_string(),
            message: format!("manifold metadata: {label} exists"),
            fix: None,
        }
    } else {
        DoctorCheck {
            name: "manifold metadata".to_string(),
            status: "fail".to_string(),
            message: format!(
                "manifold metadata: {} is missing",
                manifold.strip_prefix(root).unwrap_or(&manifold).display()
            ),
            fix: Some("Run: maw init".to_string()),
        }
    }
}

fn check_default_workspace(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "warn".to_string(),
            message: "default workspace: could not determine repo root".to_string(),
            fix: None,
        };
    };

    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let default_ws = flavor.default_target_path(root, "default");

    // Consolidated layout: the root itself IS the default workspace; it
    // always exists (we're checking via the repo root), no worktree
    // registration is needed because the root is the primary worktree.
    if matches!(
        flavor,
        maw_core::model::layout::LayoutFlavor::ConsolidatedMawDir
    ) {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "ok".to_string(),
            message: format!(
                "default workspace: consolidated layout — root checkout at {}",
                default_ws.display()
            ),
            fix: None,
        };
    }

    if !default_ws.exists() {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "fail".to_string(),
            message: "default workspace: ws/default/ does not exist".to_string(),
            fix: Some("Run: maw init".to_string()),
        };
    }

    if !is_valid_default_worktree(root, &default_ws) {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "fail".to_string(),
            message: "default workspace: ws/default/ exists but is not a registered git worktree"
                .to_string(),
            fix: Some("Fix: maw init (repairs default workspace registration)".to_string()),
        };
    }

    let has_files = std::fs::read_dir(&default_ws).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
    });

    if has_files {
        DoctorCheck {
            name: "default workspace".to_string(),
            status: "ok".to_string(),
            message: "default workspace: ws/default/ exists with source files".to_string(),
            fix: None,
        }
    } else {
        DoctorCheck {
            name: "default workspace".to_string(),
            status: "warn".to_string(),
            message: "default workspace: ws/default/ exists but appears empty".to_string(),
            fix: Some("Run: maw init".to_string()),
        }
    }
}

fn is_valid_default_worktree(root: &Path, default_ws: &Path) -> bool {
    if !is_inside_worktree(default_ws) {
        return false;
    }

    is_registered_worktree(root, default_ws)
}

/// Returns true if `path` resolves to a git work tree.
///
/// Replaces `git rev-parse --is-inside-work-tree`. We open the repo at
/// `path` (which walks parents) and confirm the discovered `.git` dir is a
/// linked-worktree gitdir or a non-bare repo — i.e. there is a real working
/// tree associated with the path. Bare repos have no gitdir distinct from
/// their root, so we fall back to checking that `path` resolves under a
/// listed worktree's directory.
fn is_inside_worktree(path: &Path) -> bool {
    use maw_git::GitRepo as _;

    // A linked worktree has a `.git` file that points at
    // <common-dir>/worktrees/<name>/. If `.git` exists at the path (as either
    // a file or directory), it's a working tree.
    if path.join(".git").exists() {
        return true;
    }
    // Fallback: ask the repo for its worktree list and check if `path` is in it.
    let Ok(canon) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(repo) = maw_git::GixRepo::open(path) else {
        return false;
    };
    let Ok(worktrees) = repo.worktree_list() else {
        return false;
    };
    worktrees.iter().any(|wt| {
        let listed = std::fs::canonicalize(&wt.path).unwrap_or_else(|_| wt.path.clone());
        listed == canon
    })
}

/// Returns true if `ws_path` is a registered worktree of the repo at `root`.
///
/// Uses `GitRepo::worktree_list` and compares canonicalized paths.
fn is_registered_worktree(root: &Path, ws_path: &Path) -> bool {
    use maw_git::GitRepo as _;

    let Ok(repo) = maw_git::GixRepo::open(root) else {
        return false;
    };
    let Ok(worktrees) = repo.worktree_list() else {
        return false;
    };

    let ws_path = std::fs::canonicalize(ws_path).unwrap_or_else(|_| ws_path.to_path_buf());
    worktrees.iter().any(|wt| {
        let listed = std::fs::canonicalize(&wt.path).unwrap_or_else(|_| wt.path.clone());
        listed == ws_path
    })
}

fn check_root_bare(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "repo root".to_string(),
            status: "ok".to_string(),
            message: "repo root: could not check (no root)".to_string(),
            fix: None,
        };
    };

    // Consolidated layout: the root is intentionally a live checkout, so
    // "stray" project files are expected. Skip the bare-root strict check.
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    if matches!(
        flavor,
        maw_core::model::layout::LayoutFlavor::ConsolidatedMawDir
    ) {
        return DoctorCheck {
            name: "repo root".to_string(),
            status: "ok".to_string(),
            message: "repo root: consolidated layout (live checkout at root)".to_string(),
            fix: None,
        };
    }

    let stray = stray_root_entries(root);
    if stray.is_empty() {
        DoctorCheck {
            name: "repo root".to_string(),
            status: "ok".to_string(),
            message: "repo root: bare (no source files)".to_string(),
            fix: None,
        }
    } else {
        DoctorCheck {
            name: "repo root".to_string(),
            status: "fail".to_string(),
            message: format!(
                "repo root: {} unexpected file(s)/dir(s) — should be bare: {}",
                stray.len(),
                stray.join(", ")
            ),
            fix: Some("Fix: maw init (moves project files) or move/remove manually".to_string()),
        }
    }
}

const BARE_ROOT_ALLOWED: &[&str] = &[".git", ".manifold", ".maw", "repo.git", "ws"];

fn check_ghost_working_copy(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "legacy jj metadata".to_string(),
            status: "ok".to_string(),
            message: "legacy jj metadata: could not check (no root)".to_string(),
            fix: None,
        };
    };

    let ghost_wc = root.join(".jj").join("working_copy");
    if ghost_wc.exists() {
        DoctorCheck {
            name: "legacy jj metadata".to_string(),
            status: "warn".to_string(),
            message: "legacy jj metadata: .jj/working_copy/ exists at repo root".to_string(),
            fix: Some("Migration cleanup: rm -rf .jj/working_copy/".to_string()),
        }
    } else {
        DoctorCheck {
            name: "legacy jj metadata".to_string(),
            status: "ok".to_string(),
            message: "legacy jj metadata: none".to_string(),
            fix: None,
        }
    }
}

#[must_use]
pub fn stray_root_entries(root: &Path) -> Vec<String> {
    // Layout-aware: in the consolidated `.maw/` layout the repo root IS the
    // live default checkout, so every project file there is expected — nothing
    // is "stray". Only the bare/v2 layout keeps root metadata-only, where this
    // check applies. Without this guard `maw status` (which calls this directly)
    // reports every source file as a "Root extra" after `maw migrate` (bn-2h3g).
    // `maw doctor`'s repo-root check guards separately; this makes the guard
    // hold for every caller.
    if matches!(
        maw_core::model::layout::LayoutFlavor::detect_with_env(root),
        maw_core::model::layout::LayoutFlavor::ConsolidatedMawDir
    ) {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if BARE_ROOT_ALLOWED.contains(&name.as_str()) {
                None
            } else {
                Some(name)
            }
        })
        .collect()
}

fn check_dangling_snapshots(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "dangling snapshots".to_string(),
            status: "ok".to_string(),
            message: "dangling snapshots: could not check (no root)".to_string(),
            fix: None,
        };
    };

    match workspace::recover::find_dangling_snapshots(root) {
        Ok(dangling) if dangling.is_empty() => DoctorCheck {
            name: "dangling snapshots".to_string(),
            status: "ok".to_string(),
            message: "dangling snapshots: none".to_string(),
            fix: None,
        },
        Ok(dangling) => DoctorCheck {
            name: "dangling snapshots".to_string(),
            status: "warn".to_string(),
            message: format!(
                "dangling snapshots: {} orphaned recovery snapshot(s) (recovery refs left by \
                 crashed or completed merges, with no destroy record)",
                dangling.len()
            ),
            fix: Some(
                "Fix: maw ws recover --gc --dry-run  (preview), then: maw ws recover --gc"
                    .to_string(),
            ),
        },
        Err(_) => DoctorCheck {
            name: "dangling snapshots".to_string(),
            status: "ok".to_string(),
            message: "dangling snapshots: could not check (git error)".to_string(),
            fix: None,
        },
    }
}

/// SG4 / bn-29fi (destroy-prevention): surface workspaces whose
/// destroy-record + recovery snapshot is still on disk, distinct from
/// the "dangling snapshots" check (which targets ref-only leakage from
/// crashed merges).
///
/// Why this check exists:
/// - `dangling snapshots` warns when a `refs/manifold/recovery/...`
///   ref has no owning destroy-record (clean-up garbage).
/// - This check warns when destroyed workspaces have **valid** destroy
///   records that the agent may have forgotten about — the
///   "abandoned-with-snapshot" state from the safe-cleanup vocabulary.
///
/// The destroy-prevention impact is upstream: an agent who runs
/// `maw doctor` and sees "3 abandoned-with-snapshot workspace(s)" is
/// far more likely to `maw ws recover` the queued work BEFORE
/// destroying yet another workspace it will later need to recover.
/// Naming the queue makes it actionable.
///
/// Status is `warn` (not `fail`) because the data is preserved by the
/// Prime Invariant — this is a *prompt to drain the mergeback queue*,
/// not a corruption signal.
///
/// Returns two checks that share one classification pass (bn-3uou):
/// - `abandoned-with-snapshot`: destroyed workspaces whose recovery snapshot
///   ref is still present (recoverable; drain the mergeback queue).
/// - `destroy-record-unpinned`: destroyed workspaces whose destroy record
///   claims a recovery snapshot whose ref is gone (desynced — a later
///   `git gc --prune` could drop the object). Kept distinct so a swept-ref
///   state is surfaced rather than silently inflating the first count.
fn check_destroy_records(root: Option<&Path>) -> (DoctorCheck, DoctorCheck) {
    let abandoned_name = "abandoned-with-snapshot".to_string();
    let unpinned_name = "destroy-record-unpinned".to_string();

    let unknown = |name: String, reason: &str| DoctorCheck {
        name: name.clone(),
        status: "ok".to_string(),
        message: format!("{name}: could not check ({reason})"),
        fix: None,
    };

    let Some(root) = root else {
        return (
            unknown(abandoned_name, "no root"),
            unknown(unpinned_name, "no root"),
        );
    };

    let Ok(pinning) = workspace::recover::classify_destroyed_workspaces(root) else {
        return (
            unknown(abandoned_name, "read error"),
            unknown(unpinned_name, "read error"),
        );
    };

    (
        abandoned_with_snapshot_check(abandoned_name, &pinning.pinned),
        destroy_record_unpinned_check(unpinned_name, &pinning.unpinned),
    )
}

/// Join up to three names as a `a, b, c (+N more)` preview.
fn preview_names(names: &[String]) -> String {
    let head = names
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > 3 {
        format!("{head} (+{} more)", names.len() - 3)
    } else {
        head
    }
}

fn abandoned_with_snapshot_check(name: String, pinned: &[String]) -> DoctorCheck {
    if pinned.is_empty() {
        return DoctorCheck {
            name,
            status: "ok".to_string(),
            message: "abandoned-with-snapshot: no queued recovery snapshots".to_string(),
            fix: None,
        };
    }

    let first = pinned.first().expect("non-empty checked above");
    DoctorCheck {
        name,
        status: "warn".to_string(),
        message: format!(
            "abandoned-with-snapshot: {} destroyed workspace(s) retain a recovery snapshot \
             (destroy records with a still-pinned `refs/manifold/recovery/...` — the \
             `maw ws recover` audit trail; expected after `ws destroy`, no work is lost): {}",
            pinned.len(),
            preview_names(pinned)
        ),
        fix: Some(format!(
            "Restore one: maw ws recover {first} --to {first}-restored  |  Prune old ones \
             (ref + record together): maw gc --recovery-snapshots --dry-run, then maw gc \
             --recovery-snapshots"
        )),
    }
}

fn destroy_record_unpinned_check(name: String, unpinned: &[String]) -> DoctorCheck {
    if unpinned.is_empty() {
        return DoctorCheck {
            name,
            status: "ok".to_string(),
            message: "destroy-record-unpinned: none".to_string(),
            fix: None,
        };
    }

    DoctorCheck {
        name,
        status: "warn".to_string(),
        message: format!(
            "destroy-record-unpinned: {} destroyed workspace(s) have a destroy record that \
             claims a recovery snapshot whose ref is already gone (swept by an older `maw gc \
             --refs`, or manually deleted). The snapshot object is unpinned and a future \
             `git gc --prune` may drop it: {}",
            unpinned.len(),
            preview_names(unpinned)
        ),
        fix: Some(
            "Prune desynced records older than 30 days: maw gc --recovery-snapshots (preview \
             with --dry-run). CAUTION: --older-than 0 drains them all, but ALSO sweeps every \
             still-pinned recovery snapshot and its record — recently destroyed workspaces \
             become unrecoverable."
                .to_string(),
        ),
    }
}

/// Detect an orphaned / stuck merge-state file (bn-2wyh).
///
/// A killed / OOM'd / panicked / Ctrl-C'd `maw ws merge` leaves
/// `.manifold/merge-state.json` behind, which then blocks every future
/// merge with "merge already in progress". `maw doctor` must surface this
/// (it previously reported "All checks passed!" while merges were wedged)
/// and print the exact recovery command.
fn check_merge_state(root: Option<&Path>) -> DoctorCheck {
    use maw_core::merge_state::{
        DEFAULT_STALE_AFTER_SECS, MergeStateError, MergeStateFile, Staleness,
    };

    let name = "merge-state".to_string();
    let recovery_fix = "Fix: maw ws merge --abort".to_string();

    let Some(root) = root else {
        return DoctorCheck {
            name,
            status: "ok".to_string(),
            message: "merge-state: could not check (no root)".to_string(),
            fix: None,
        };
    };

    let state_path = MergeStateFile::default_path(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
    );
    let state = match MergeStateFile::read(&state_path) {
        Err(MergeStateError::NotFound(_)) => {
            return DoctorCheck {
                name,
                status: "ok".to_string(),
                message: "merge-state: no merge in progress".to_string(),
                fix: None,
            };
        }
        Err(e) => {
            // A corrupt merge-state file also wedges merges.
            return DoctorCheck {
                name,
                status: "fail".to_string(),
                message: format!("merge-state: file present but unreadable ({e})"),
                fix: Some(recovery_fix),
            };
        }
        Ok(s) => s,
    };

    if state.phase.is_terminal() {
        // A terminal state still on disk is leftover but harmless to clear.
        return DoctorCheck {
            name,
            status: "warn".to_string(),
            message: format!(
                "merge-state: leftover terminal state (phase: {})",
                state.phase
            ),
            fix: Some(recovery_fix),
        };
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match state.staleness(now, DEFAULT_STALE_AFTER_SECS) {
        Staleness::Live => DoctorCheck {
            name,
            status: "ok".to_string(),
            message: format!(
                "merge-state: a merge is actively running (phase: {}, pid: {})",
                state.phase,
                state
                    .owner_pid
                    .map_or_else(|| "?".to_string(), |p| p.to_string())
            ),
            fix: None,
        },
        Staleness::Orphaned => DoctorCheck {
            name,
            status: "fail".to_string(),
            message: format!(
                "merge-state: ORPHANED merge-state from an interrupted merge \
                 (phase: {}, owner process is gone). This blocks ALL future merges.",
                state.phase
            ),
            fix: Some(recovery_fix),
        },
        Staleness::Indeterminate => DoctorCheck {
            name,
            status: "warn".to_string(),
            message: format!(
                "merge-state: merge-state present (phase: {}) but owner liveness \
                 could not be confirmed. If no merge is running it is orphaned and \
                 blocks all future merges.",
                state.phase
            ),
            fix: Some(recovery_fix),
        },
    }
}

fn check_stale_head_refs(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "stale head refs".to_string(),
            status: "ok".to_string(),
            message: "stale head refs: could not check (no root)".to_string(),
            fix: None,
        };
    };

    match ref_gc::count_stale_head_refs(root) {
        Ok(0) => DoctorCheck {
            name: "stale head refs".to_string(),
            status: "ok".to_string(),
            message: "stale head refs: none".to_string(),
            fix: None,
        },
        Ok(count) => DoctorCheck {
            name: "stale head refs".to_string(),
            status: "warn".to_string(),
            message: format!(
                "stale head refs: found {count} head ref(s) for non-existent workspaces"
            ),
            fix: Some("Run: maw gc".to_string()),
        },
        Err(_) => DoctorCheck {
            name: "stale head refs".to_string(),
            status: "ok".to_string(),
            message: "stale head refs: could not check (git error)".to_string(),
            fix: None,
        },
    }
}

/// Detect drift between `refs/manifold/epoch/current` and the configured
/// branch HEAD (bn-1ieb / SG4 `epoch_sync_required` mitigation).
///
/// This is the proactive surface for the `epoch_sync_required` friction
/// cluster: when an agent runs `maw doctor` (or any tool that invokes the
/// check), drift is named with the exact recovery verb instead of waiting
/// for a downstream `maw ws merge` to fail with "Target branch has
/// diverged".
///
/// Status mapping:
/// - `in_sync` → `[OK]` "epoch is in sync".
/// - `ff_absorbable` → `[WARN]` "branch ahead of epoch by N commit(s);
///   safe to advance" + fix hint `maw epoch sync`.
/// - `ff_blocked` → `[WARN]` "auto-advance blocked by workspace(s) ...";
///   the merge will absorb when those workspaces resolve.
/// - `diverged` → `[FAIL]` "epoch and branch have forked".
///
/// `warn` (not `fail`) for the auto-advanceable case because the system is
/// not broken — the next `maw ws merge` would absorb it transparently via
/// the existing FF-absorb path. The check exists to short-circuit the
/// agent's discovery cost; the architectural auto-advance is still doing
/// most of the work.
fn check_epoch_drift(root: Option<&Path>) -> DoctorCheck {
    use crate::workspace::epoch_drift::{EpochDriftKind, classify_drift};

    let name = "epoch drift".to_string();

    let Some(root) = root else {
        return DoctorCheck {
            name,
            status: "ok".to_string(),
            message: "epoch drift: could not check (no root)".to_string(),
            fix: None,
        };
    };

    let Ok(config) = crate::workspace::MawConfig::load(root) else {
        return DoctorCheck {
            name,
            status: "ok".to_string(),
            message: "epoch drift: could not check (config unreadable)".to_string(),
            fix: None,
        };
    };
    let branch = config.branch();

    let backend = maw_core::backend::git::GitWorktreeBackend::new(root.to_path_buf());
    let report = match classify_drift(root, branch, &backend) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DoctorCheck {
                name,
                status: "ok".to_string(),
                message: "epoch drift: epoch ref not set (run `maw init`)".to_string(),
                fix: None,
            };
        }
        Err(e) => {
            return DoctorCheck {
                name,
                status: "warn".to_string(),
                message: format!("epoch drift: could not classify drift ({e})"),
                fix: None,
            };
        }
    };

    match report.kind {
        EpochDriftKind::InSync => DoctorCheck {
            name,
            status: "ok".to_string(),
            message: format!(
                "epoch drift: in sync ({} == '{branch}')",
                report.epoch_short
            ),
            fix: None,
        },
        EpochDriftKind::FfAbsorbable => DoctorCheck {
            name,
            status: "warn".to_string(),
            message: format!(
                "epoch drift: branch '{branch}' is {} commit(s) ahead of epoch \
                 ({} → {}); safe to auto-advance.",
                report.ff_commit_count, report.epoch_short, report.branch_short,
            ),
            fix: Some("Fix: maw epoch sync".to_string()),
        },
        EpochDriftKind::FfBlocked => DoctorCheck {
            name,
            status: "warn".to_string(),
            message: format!(
                "epoch drift: branch '{branch}' is {} commit(s) ahead of epoch \
                 ({} → {}), blocked by workspace(s) {}. The next `maw ws merge` \
                 will absorb the FF range once those workspaces are resolved.",
                report.ff_commit_count,
                report.epoch_short,
                report.branch_short,
                report.blocking_workspaces.join(", "),
            ),
            fix: Some(format!(
                "Fix: maw ws merge {} --into default --check  (resolve, then retry)",
                report
                    .blocking_workspaces
                    .first()
                    .map_or("<ws>", String::as_str),
            )),
        },
        EpochDriftKind::Diverged => DoctorCheck {
            name,
            status: "fail".to_string(),
            message: format!(
                "epoch drift: epoch ({}) and branch '{branch}' ({}) have forked. \
                 Manual recovery required — auto-advance is unsafe.",
                report.epoch_short, report.branch_short,
            ),
            fix: Some(
                "Fix: investigate with `git log --oneline --all`; \
                       reset branch or epoch ref deliberately."
                    .to_string(),
            ),
        },
    }
}

fn check_git_head() -> DoctorCheck {
    // Read the HEAD file directly to determine if it's symbolic. A symbolic
    // HEAD has the form "ref: refs/heads/<branch>\n"; a detached HEAD contains
    // only an OID. We read the common-dir HEAD because that's the canonical
    // location for the maw bare repo's branch pointer.
    let head_ref_name = crate::workspace::repo_root()
        .ok()
        .and_then(|root| read_symbolic_head_target(&root));

    head_ref_name.map_or_else(
        || {
            let root = crate::workspace::repo_root().unwrap_or_else(|_| ".".into());
            let branch = crate::workspace::MawConfig::load(&root)
                .map_or_else(|_| "main".to_string(), |c| c.branch().to_string());
            DoctorCheck {
                name: "git HEAD".to_string(),
                status: "fail".to_string(),
                message: "git HEAD: detached (git log may be stale)".to_string(),
                fix: Some(format!(
                    "Fix: git symbolic-ref HEAD refs/heads/{branch}  (or run: maw init)"
                )),
            }
        },
        |name| DoctorCheck {
            name: "git HEAD".to_string(),
            status: "ok".to_string(),
            message: format!("git HEAD: {name}"),
            fix: None,
        },
    )
}

/// Returns the symbolic ref target of HEAD (e.g. `refs/heads/main`), or `None`
/// if HEAD is detached or cannot be read.
///
/// Open the repo via gix so we honour the same common-dir discovery that
/// `git symbolic-ref HEAD` uses (worktrees, repo.git layout, etc.).
fn read_symbolic_head_target(root: &Path) -> Option<String> {
    let repo = maw_git::GixRepo::open(root).ok()?;
    let head_path = repo.common_dir().join("HEAD");
    let content = std::fs::read_to_string(&head_path).ok()?;
    let target = content.trim().strip_prefix("ref:")?;
    Some(format!("ref: {}", target.trim()))
}

// ---------------------------------------------------------------------------
// LFS check
// ---------------------------------------------------------------------------

#[cfg(not(feature = "lfs"))]
fn check_lfs(_root: Option<&Path>) -> DoctorCheck {
    DoctorCheck {
        name: "lfs".to_string(),
        status: "ok".to_string(),
        message: "lfs: feature disabled".to_string(),
        fix: None,
    }
}

#[cfg(feature = "lfs")]
fn check_lfs(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "lfs".to_string(),
            status: "ok".to_string(),
            message: "lfs: could not determine repo root".to_string(),
            fix: None,
        };
    };

    check_lfs_in(root)
}

#[cfg(feature = "lfs")]
fn check_lfs_in(root: &Path) -> DoctorCheck {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let ws_root = flavor.workspaces_dir(root);
    if !has_any_gitattributes(&ws_root) {
        return DoctorCheck {
            name: "lfs".to_string(),
            status: "ok".to_string(),
            message: "lfs: no LFS tracked patterns".to_string(),
            fix: None,
        };
    }

    // Count objects in .git/lfs/objects/
    let object_count = count_lfs_objects(&root.join(".git").join("lfs").join("objects"));

    let mut workspaces = 0usize;
    let mut files_checked = 0usize;
    let mut stubs: Vec<String> = Vec::new();

    let Ok(entries) = std::fs::read_dir(&ws_root) else {
        return DoctorCheck {
            name: "lfs".to_string(),
            status: "ok".to_string(),
            message: "lfs: could not read ws/".to_string(),
            fix: None,
        };
    };

    for entry in entries.flatten() {
        let ws_path = entry.path();
        if !ws_path.is_dir() {
            continue;
        }
        let ws_name = entry.file_name().to_string_lossy().to_string();
        workspaces += 1;

        let Ok(matcher) = maw_lfs::AttrsMatcher::from_workdir(&ws_path) else {
            continue;
        };

        scan_for_stubs(
            &ws_path,
            &ws_path,
            &matcher,
            &mut files_checked,
            &mut stubs,
            &ws_name,
        );
    }

    let stub_count = stubs.len();
    if stub_count == 0 {
        DoctorCheck {
            name: "lfs".to_string(),
            status: "ok".to_string(),
            message: format!(
                "lfs: {workspaces} workspace(s) healthy, {files_checked} LFS files checked, \
                 {object_count} object(s) in store"
            ),
            fix: None,
        }
    } else {
        let sample: Vec<String> = stubs.iter().take(5).cloned().collect();
        DoctorCheck {
            name: "lfs".to_string(),
            status: "warn".to_string(),
            message: format!(
                "lfs: {stub_count} pointer stub(s) detected (corrupted checkout), \
                 {object_count} object(s) in store: {}",
                sample.join(", ")
            ),
            fix: Some(
                "Run `maw ws sync <name>` on each stale workspace, or recreate the workspace."
                    .to_string(),
            ),
        }
    }
}

#[cfg(feature = "lfs")]
fn has_any_gitattributes(ws_root: &Path) -> bool {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == ".git" || name_str == ".manifold" {
                continue;
            }
            if path.is_file() && name_str == ".gitattributes" {
                return true;
            }
            if path.is_dir() && walk(&path) {
                return true;
            }
        }
        false
    }
    walk(ws_root)
}

#[cfg(feature = "lfs")]
fn count_lfs_objects(dir: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else if path.is_file() {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(dir, &mut count);
    count
}

#[cfg(feature = "lfs")]
fn scan_for_stubs(
    ws_root: &Path,
    dir: &Path,
    matcher: &maw_lfs::AttrsMatcher,
    files_checked: &mut usize,
    stubs: &mut Vec<String>,
    ws_name: &str,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str == ".manifold" {
            continue;
        }
        if path.is_dir() {
            scan_for_stubs(ws_root, &path, matcher, files_checked, stubs, ws_name);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = path.metadata() else { continue };
        if meta.len() > 1024 {
            continue;
        }
        let Ok(rel) = path.strip_prefix(ws_root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !matcher.is_lfs(&rel_str) {
            continue;
        }
        *files_checked += 1;
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if maw_lfs::looks_like_pointer(&bytes) {
            stubs.push(format!("{ws_name}/{rel_str}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stray_root_entries_flags_project_dotfiles() {
        let tmp = tempfile::tempdir().expect("operation should succeed");
        let root = tmp.path();
        std::fs::write(root.join(".git"), "gitdir: repo.git\n").expect("operation should succeed");
        std::fs::create_dir_all(root.join(".manifold")).expect("operation should succeed");
        std::fs::create_dir_all(root.join("repo.git")).expect("operation should succeed");
        std::fs::create_dir_all(root.join("ws")).expect("operation should succeed");
        std::fs::create_dir_all(root.join(".bones")).expect("operation should succeed");
        std::fs::create_dir_all(root.join("notes")).expect("operation should succeed");

        let mut stray = stray_root_entries(root);
        stray.sort();

        assert_eq!(stray, [".bones", "notes"]);
    }

    /// bn-2h3g: in the consolidated `.maw/` layout the root is the live
    /// default checkout, so project files there are NOT stray. `maw status`
    /// calls `stray_root_entries` directly; without the layout guard it
    /// reported every source file as a "Root extra" after `maw migrate`.
    #[test]
    fn stray_root_entries_empty_in_consolidated_layout() {
        let tmp = tempfile::tempdir().expect("operation should succeed");
        let root = tmp.path();
        // `.maw/manifold/` presence => ConsolidatedMawDir per detect_with_env.
        std::fs::create_dir_all(root.join(".maw").join("manifold"))
            .expect("operation should succeed");
        std::fs::create_dir_all(root.join("crates")).expect("operation should succeed");
        std::fs::write(root.join("Cargo.toml"), "").expect("operation should succeed");
        std::fs::write(root.join("AGENTS.md"), "").expect("operation should succeed");

        assert!(
            stray_root_entries(root).is_empty(),
            "consolidated layout root is a live checkout — nothing is stray"
        );
    }

    #[test]
    fn parse_standard_git_version() {
        assert_eq!(parse_git_version("git version 2.47.1"), Some((2, 47, 1)));
    }

    #[test]
    fn parse_git_version_two_components() {
        // Some distributions emit "git version 2.40" with no patch
        assert_eq!(parse_git_version("git version 2.40"), Some((2, 40, 0)));
    }

    #[test]
    fn parse_git_version_with_suffix() {
        // macOS Apple Git
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parse_git_version_windows() {
        assert_eq!(
            parse_git_version("git version 2.43.0.windows.1"),
            Some((2, 43, 0))
        );
    }

    #[test]
    fn parse_git_version_multiline() {
        // Only the first line matters
        assert_eq!(
            parse_git_version("git version 2.45.2\nsome extra info"),
            Some((2, 45, 2))
        );
    }

    #[test]
    fn parse_git_version_invalid() {
        assert_eq!(parse_git_version("not git output"), None);
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("git version "), None);
        assert_eq!(parse_git_version("git version abc.def.ghi"), None);
    }

    #[test]
    fn version_comparison_at_minimum() {
        let v = (2, 40, 0);
        assert!(v >= MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_above_minimum() {
        let v = (2, 47, 1);
        assert!(v >= MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_below_minimum() {
        let v = (2, 39, 3);
        assert!(v < MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_major_below() {
        let v = (1, 99, 99);
        assert!(v < MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_major_above() {
        let v = (3, 0, 0);
        assert!(v >= MIN_GIT_VERSION);
    }

    #[cfg(feature = "lfs")]
    #[test]
    fn check_lfs_detects_pointer_stub() {
        let tmp = tempfile::tempdir().expect("operation should succeed");
        let root = tmp.path();
        let ws_default = root.join("ws").join("default");
        std::fs::create_dir_all(&ws_default).expect("operation should succeed");
        std::fs::write(
            ws_default.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("operation should succeed");
        let pointer = "version https://git-lfs.github.com/spec/v1\n\
                       oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
                       size 12345\n";
        std::fs::write(ws_default.join("hero.bin"), pointer).expect("operation should succeed");

        let check = check_lfs(Some(root));
        assert_eq!(check.status, "warn", "got: {}", check.message);
        assert!(
            check.message.contains("pointer stub"),
            "message: {}",
            check.message
        );
        assert!(
            check.message.contains("default/hero.bin"),
            "message: {}",
            check.message
        );
        assert!(check.fix.is_some());
    }

    #[cfg(feature = "lfs")]
    #[test]
    fn check_lfs_no_gitattributes_is_ok() {
        let tmp = tempfile::tempdir().expect("operation should succeed");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("ws").join("default")).expect("operation should succeed");
        let check = check_lfs(Some(root));
        assert_eq!(check.status, "ok");
        assert!(check.message.contains("no LFS tracked patterns"));
    }

    #[cfg(feature = "lfs")]
    #[test]
    fn check_lfs_healthy_workspace() {
        let tmp = tempfile::tempdir().expect("operation should succeed");
        let root = tmp.path();
        let ws_default = root.join("ws").join("default");
        std::fs::create_dir_all(&ws_default).expect("operation should succeed");
        std::fs::write(
            ws_default.join(".gitattributes"),
            "*.bin filter=lfs -text\n",
        )
        .expect("operation should succeed");
        // A non-pointer (real binary content) file
        std::fs::write(ws_default.join("real.bin"), vec![0u8; 2048])
            .expect("operation should succeed");
        let check = check_lfs(Some(root));
        assert_eq!(check.status, "ok", "message: {}", check.message);
        assert!(check.message.contains("healthy"));
    }

    /// bn-1ieb: end-to-end doctor coverage for the `epoch_sync_required`
    /// mitigation. `check_epoch_drift` must surface the structured warning
    /// (status=warn, fix=maw epoch sync) for a real ff-absorbable drift —
    /// agents reading doctor output thus get the exact recovery verb
    /// without having to run a separate merge attempt to discover it.
    #[test]
    fn check_epoch_drift_warns_for_ff_absorbable_state() {
        let (dir, root, _epoch0) = maw_git::test_support::init_test_repo_with_commit();
        let _ = dir;

        // Set up manifold metadata + epoch ref pointing at the initial
        // commit; advance branch HEAD by 2 commits without advancing epoch.
        std::fs::create_dir_all(root.join(".manifold/epochs")).expect("mkdir manifold");
        let epoch0 = maw_git::test_support::git_capture(&root, &["rev-parse", "HEAD"]);
        let epoch0 = epoch0.trim();
        maw_core::refs::write_epoch_current(
            &root,
            &maw_core::model::types::GitOid::new(epoch0).expect("oid"),
        )
        .expect("write epoch_current");

        std::fs::write(root.join("ff1.txt"), "ff1").expect("write");
        let _ = maw_git::test_support::commit_all(&root, "ff1");
        std::fs::write(root.join("ff2.txt"), "ff2").expect("write");
        let _ = maw_git::test_support::commit_all(&root, "ff2");

        let check = check_epoch_drift(Some(&root));
        assert_eq!(
            check.status, "warn",
            "ff-absorbable drift must surface as [WARN], got: {check:?}"
        );
        assert!(
            check.message.contains("ahead of epoch"),
            "message must name the drift direction: {}",
            check.message
        );
        // The fix string is the load-bearing signal for the agent —
        // hardcode the exact verb the friction cluster names.
        let fix = check.fix.as_deref().unwrap_or("");
        assert!(
            fix.contains("maw epoch sync"),
            "fix must recommend `maw epoch sync` verbatim, got: {fix}"
        );
    }

    /// bn-1ieb: in-sync state must NOT raise a doctor warning (no false
    /// positives — otherwise the check becomes noise and agents start
    /// ignoring it).
    #[test]
    fn check_epoch_drift_ok_when_in_sync() {
        let (dir, root, _oid) = maw_git::test_support::init_test_repo_with_commit();
        let _ = dir;
        std::fs::create_dir_all(root.join(".manifold/epochs")).expect("mkdir manifold");
        let head = maw_git::test_support::git_capture(&root, &["rev-parse", "HEAD"]);
        let head = head.trim();
        maw_core::refs::write_epoch_current(
            &root,
            &maw_core::model::types::GitOid::new(head).expect("oid"),
        )
        .expect("write epoch_current");

        let check = check_epoch_drift(Some(&root));
        assert_eq!(check.status, "ok", "in-sync must be [OK]: {check:?}");
        assert!(check.fix.is_none(), "no fix needed when in sync");
    }

    /// bn-1ieb: when the epoch ref has never been set (pre-`maw init`),
    /// the check is informational, not a fail — `check_manifold_initialized`
    /// owns that diagnosis.
    #[test]
    fn check_epoch_drift_ok_when_epoch_unset() {
        let (dir, root, _oid) = maw_git::test_support::init_test_repo_with_commit();
        let _ = dir;
        // Deliberately do NOT write epoch_current.
        let check = check_epoch_drift(Some(&root));
        assert_eq!(
            check.status, "ok",
            "epoch-unset should not raise here: {check:?}"
        );
        assert!(
            check.message.contains("not set") || check.message.contains("unset"),
            "message should name the unset state: {}",
            check.message
        );
    }
}
