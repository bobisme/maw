use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use crate::format::OutputFormat;
use crate::workspace::lifecycle::{LifecycleSignals, LifecycleState};
use crate::workspace::templates::TemplateDefaults;
use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceState;

use maw::merge::quarantine::QUARANTINE_NAME_PREFIX;

use super::{DEFAULT_WORKSPACE, get_backend, metadata, repo_root};

#[derive(Serialize)]
pub struct WorkspaceInfo {
    pub(crate) name: String,
    pub(crate) is_default: bool,
    pub(crate) epoch: String,
    pub(crate) state: String,
    pub(crate) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) behind_epochs: Option<u32>,
    /// Commits in the workspace HEAD that haven't been merged into the epoch yet.
    /// Non-zero means "this workspace has work to merge".
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) commits_ahead: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) template_defaults: Option<TemplateDefaults>,
    /// Local branch this workspace is attached to for merge targeting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    /// Merge check result (only present when --check is used).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) merge_check: Option<MergeCheckSummary>,
    /// Number of unresolved rebase conflicts (0 = none).
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) rebase_conflicts: u32,
    /// Human-readable description of the workspace's purpose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    /// True when the workspace's worktree directory is gone from disk while
    /// registry/metadata still advertises it (bn-3fhj). The CLI registry can
    /// diverge from disk when a user manually deletes the worktree dir;
    /// surfacing this as a distinct state prevents misleading "ready to merge"
    /// guidance.
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) missing: bool,
    /// bn-242l (SG4 / `read_from_stale_workspace` mitigation): named
    /// safe-cleanup vocabulary slug for the workspace. Mirrors
    /// `WorkspaceEntry::lifecycle_state` from `maw ws status` and the
    /// `workspace_details[].lifecycle_state` field of `maw status --json`
    /// so all three discovery surfaces agree on a single enum vocabulary.
    /// Cluster `read_from_stale_workspace` fires when an agent reads
    /// `maw ws list` (or `status`/`diff`) output and its next op is
    /// inconsistent with a stale workspace — carrying the same named
    /// slug everywhere closes the prose-misread gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lifecycle_state: Option<LifecycleState>,
    /// bn-242l: exact recovery command for the workspace's current
    /// lifecycle state — `maw ws sync <name>`, `maw ws merge <name>
    /// --into default --check`, etc. Same shape as the load-bearing
    /// `fix_command` field of `maw status --json` so the agent's first
    /// attempt is the right one whether they read `status`, `ws list`,
    /// or `ws status`. Absent for `clean`/`integrated`/default workspaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fix_command: Option<String>,
}

/// Compact merge-check result for ws list output.
#[derive(Serialize)]
pub struct MergeCheckSummary {
    pub(crate) ready: bool,
    pub(crate) conflict_count: usize,
    pub(crate) stale: bool,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive fields by reference"
)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive fields by reference"
)]
const fn is_false(b: &bool) -> bool {
    !*b
}

/// Envelope for `maw ws list --format json` output.
#[derive(Serialize)]
pub struct WorkspaceListEnvelope {
    pub(crate) workspaces: Vec<WorkspaceInfo>,
    pub(crate) advice: Vec<Advice>,
}

/// A single advisory message (warning, info) embedded in structured output.
#[derive(Serialize)]
pub struct Advice {
    pub(crate) level: &'static str,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) details: Option<AdviceDetails>,
}

/// Extra details for an advice entry.
#[derive(Serialize)]
pub struct AdviceDetails {
    pub(crate) workspaces: Vec<String>,
    pub(crate) fix: String,
}

#[expect(
    clippy::too_many_lines,
    reason = "list command combines data collection, optional checks, and rendering"
)]
pub fn list(verbose: bool, check: bool, format: OutputFormat) -> Result<()> {
    let backend = get_backend()?;
    let backend_workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    if backend_workspaces.is_empty() {
        match format {
            OutputFormat::Text | OutputFormat::Pretty => println!("No workspaces found."),
            OutputFormat::Json => {
                let envelope = WorkspaceListEnvelope {
                    workspaces: vec![],
                    advice: vec![],
                };
                println!("{}", format.serialize(&envelope)?);
            }
        }
        return Ok(());
    }

    // Read metadata for all workspaces to get mode (ephemeral/persistent).
    let root = repo_root()?;

    // If --check requested, run merge checks for workspaces with pending commits.
    let merge_checks: HashMap<String, MergeCheckSummary> = if check {
        let mut checks = HashMap::new();
        for ws in &backend_workspaces {
            let name = ws.id.as_str().to_string();
            if name == DEFAULT_WORKSPACE || ws.commits_ahead == 0 {
                continue;
            }
            // bn-3fhj: skip merge-check for workspaces whose worktree dir is
            // gone from disk — the check would crash with the same "does not
            // exist" error that --check is meant to surface cleanly via the
            // MISSING state in list output.
            if !ws.path.exists() {
                continue;
            }
            if metadata::read(&root, ws.id.as_str()).is_ok_and(|meta| meta.branch.is_some()) {
                continue;
            }
            match super::merge::check_merge_result(std::slice::from_ref(&name)) {
                Ok(result) => {
                    checks.insert(
                        name,
                        MergeCheckSummary {
                            ready: result.ready,
                            conflict_count: result.conflicts.len(),
                            stale: result.stale,
                        },
                    );
                }
                Err(_) => {
                    // Check failed — mark as not ready with 0 conflicts.
                    checks.insert(
                        name,
                        MergeCheckSummary {
                            ready: false,
                            conflict_count: 0,
                            stale: false,
                        },
                    );
                }
            }
        }
        checks
    } else {
        HashMap::new()
    };

    // Convert backend workspace info to display structs
    let mut workspaces: Vec<WorkspaceInfo> = backend_workspaces
        .iter()
        .map(|ws| {
            let name = ws.id.as_str().to_string();
            let is_default = name == DEFAULT_WORKSPACE;
            let is_quarantine = name.starts_with(QUARANTINE_NAME_PREFIX);
            let behind = match &ws.state {
                WorkspaceState::Stale { behind_epochs } if !is_default => Some(*behind_epochs),
                _ => None,
            };
            // Read metadata for this workspace (defaults to ephemeral on error/missing).
            let ws_meta = metadata::read(&root, ws.id.as_str()).unwrap_or_default();
            let branch = ws_meta.branch;
            let ws_mode = if is_default {
                maw_core::model::types::WorkspaceMode::Persistent
            } else {
                ws_meta.mode
            };
            // bn-3fhj: registry/git can advertise a workspace whose worktree
            // dir is gone from disk. Detect that here so we don't claim "ready
            // to merge" when the merge would error with "does not exist".
            let missing = !is_default && !ws.path.exists();
            let rebase_conflicts = if missing {
                0
            } else {
                let ws_path = root.join("ws").join(ws.id.as_str());
                super::resolve::find_conflicted_files(&ws_path)
                    .map_or(0, |f| u32::try_from(f.len()).unwrap_or(u32::MAX))
            };
            // bn-242l: classify lifecycle state and compute the
            // exact fix command. Use the same signals/priority order
            // as `maw status --json` and `maw ws status` so the three
            // discovery surfaces cannot disagree. Skip classification
            // for the default workspace (it's not a candidate for the
            // stale/integrate-ready vocabulary) and for quarantine
            // workspaces (they have their own special-case wiring
            // and an opaque-lifecycle classifier would be misleading).
            let (lifecycle_state, fix_command) = if is_default || is_quarantine {
                (None, None)
            } else {
                let has_uncommitted = if missing {
                    false
                } else {
                    maw_git::GixRepo::open(&ws.path)
                        .ok()
                        .and_then(|repo| {
                            use maw_git::GitRepo as _;
                            repo.count_dirty_tracked().ok()
                        })
                        .is_some_and(|c| c > 0)
                };
                let signals = LifecycleSignals {
                    missing,
                    rebase_conflicts,
                    is_stale: ws.state.is_stale(),
                    commits_ahead: ws.commits_ahead,
                    has_uncommitted,
                    was_integrated: false,
                };
                let state = LifecycleState::classify(signals);
                let fix = state.fix_command(ws.id.as_str(), ws_mode.is_persistent());
                (Some(state), fix)
            };
            WorkspaceInfo {
                is_default,
                epoch: ws.epoch.as_str()[..12].to_string(),
                // Quarantine workspaces show as "quarantine" regardless of
                // their staleness state — they are a special class of workspace.
                state: if missing {
                    "MISSING".to_owned()
                } else if is_quarantine {
                    "quarantine".to_owned()
                } else if is_default {
                    "active".to_owned()
                } else if rebase_conflicts > 0 {
                    format!("conflicted ({rebase_conflicts} conflict(s))")
                } else if branch.is_some() && ws.commits_ahead > 0 {
                    format!("active (+{} on branch)", ws.commits_ahead)
                } else if ws.commits_ahead > 0 {
                    format!("active (+{} to merge)", ws.commits_ahead)
                } else {
                    format!("{}", ws.state)
                },
                mode: format!("{ws_mode}"),
                path: Some(ws.path.display().to_string()),
                behind_epochs: behind,
                commits_ahead: ws.commits_ahead,
                template: ws_meta.template.map(|t| t.to_string()),
                template_defaults: ws_meta.template_defaults,
                branch,
                merge_check: merge_checks.get(&name).map(|mc| MergeCheckSummary {
                    ready: mc.ready,
                    conflict_count: mc.conflict_count,
                    stale: mc.stale,
                }),
                rebase_conflicts,
                description: ws_meta.description,
                missing,
                lifecycle_state,
                fix_command,
                name,
            }
        })
        .collect();

    // Sort: default first, then alphabetical by name.
    workspaces.sort_by(|a, b| match (a.is_default, b.is_default) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });

    // Collect stale workspace warnings, split by mode (exclude quarantine workspaces).
    let stale_persistent: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| {
            ws.state.is_stale()
                && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX)
                && ws.id.as_str() != DEFAULT_WORKSPACE
        })
        .filter(|ws| metadata::read(&root, ws.id.as_str()).is_ok_and(|m| m.mode.is_persistent()))
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    let stale_ephemeral: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| {
            ws.state.is_stale()
                && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX)
                && ws.id.as_str() != DEFAULT_WORKSPACE
        })
        .filter(|ws| metadata::read(&root, ws.id.as_str()).map_or(true, |m| m.mode.is_ephemeral()))
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    // Combined stale list (exclude quarantine, for backwards compatibility)
    let stale_workspaces: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| {
            ws.state.is_stale()
                && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX)
                && ws.id.as_str() != DEFAULT_WORKSPACE
        })
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    let missing_workspaces: Vec<String> = workspaces
        .iter()
        .filter(|ws| ws.missing)
        .map(|ws| ws.name.clone())
        .collect();

    match format {
        OutputFormat::Text => print_list_text(
            &workspaces,
            &stale_workspaces,
            &stale_persistent,
            &stale_ephemeral,
            &missing_workspaces,
            verbose,
        ),
        OutputFormat::Pretty => print_list_pretty(
            &workspaces,
            &stale_workspaces,
            &stale_persistent,
            &stale_ephemeral,
            &missing_workspaces,
            format,
            verbose,
        ),
        OutputFormat::Json => print_list_json(
            workspaces,
            stale_workspaces,
            stale_persistent,
            stale_ephemeral,
            missing_workspaces,
            format,
        ),
    }

    Ok(())
}

/// Whether this workspace has unresolved rebase conflict markers in its HEAD
/// (bn-2l00). This is the single classification predicate shared by every
/// `ws list` render path; it is derived from `rebase_conflicts`, which is
/// computed via `resolve::find_conflicted_files` — the *same* source of truth
/// `ws status` and the `ws merge` gate use. Keeping all three readers behind
/// one predicate is what guarantees they can never disagree.
const fn is_conflicted(ws: &WorkspaceInfo) -> bool {
    ws.rebase_conflicts > 0 && !ws.missing
}

/// Decide the trailing `(...)` annotation for a workspace in text output.
///
/// Pulled out of `print_list_text` so the classification is unit-testable and
/// provably consistent with `ws status` (bn-2l00). A conflicted workspace must
/// never be classified "ready to merge" — the merge gate hard-refuses it.
fn text_annotation(ws: &WorkspaceInfo, check_annotation: Option<&str>) -> String {
    // bn-242l: append the named lifecycle slug as a `[lifecycle:<slug>]`
    // suffix so the prose carries the same canonical vocabulary the
    // JSON consumer reads from `lifecycle_state`. The cluster this
    // bone is funded to reduce (`read_from_stale_workspace`) fires
    // when an agent reads this output and misclassifies the state;
    // the slug suffix makes the misread mechanical — the token is
    // literally present in the line.
    let lifecycle_tag = ws
        .lifecycle_state
        .map(|state| format!(" [lifecycle:{}]", state.slug()))
        .unwrap_or_default();
    let base = if ws.missing {
        format!(" (MISSING — fix: maw ws destroy {} --force)", ws.name)
    } else if is_conflicted(ws) {
        // bn-2l00: a workspace whose HEAD still carries unresolved rebase
        // conflict markers is NOT ready to merge — `maw ws merge` hard-gates
        // it. Surface the conflicted state here so `ws list` stays consistent
        // with `ws status` / `ws conflicts` / the merge gate and never lures
        // context-free agents into a guaranteed-fail `--destroy` command.
        format!(
            " (conflicted: {} — resolve before merge; maw ws resolve {} --list)",
            ws.rebase_conflicts, ws.name
        )
    } else if ws.state.contains("stale") {
        " (stale)".to_string()
    } else if ws.branch.is_some() && ws.commits_ahead > 0 {
        format!(" (branch work +{})", ws.commits_ahead)
    } else if ws.commits_ahead > 0 {
        format!(" (ready to merge){}", check_annotation.unwrap_or(""))
    } else if ws.state == "quarantine" {
        " (quarantine)".to_string()
    } else {
        String::new()
    };
    format!("{base}{lifecycle_tag}")
}

/// Print workspace list in minimal text format (agent-friendly).
///
/// Output: name, absolute path, and state annotation only when actionable.
/// Default workspace is always first (sorted upstream).
fn print_list_text(
    workspaces: &[WorkspaceInfo],
    stale: &[String],
    stale_persistent: &[String],
    stale_ephemeral: &[String],
    missing: &[String],
    _verbose: bool,
) {
    for ws in workspaces {
        let path = ws.path.as_deref().unwrap_or("");
        let check_annotation = ws.merge_check.as_ref().map(|mc| {
            if mc.stale {
                " [stale]".to_string()
            } else if mc.ready {
                " [clean]".to_string()
            } else {
                format!(" [{} conflict(s)]", mc.conflict_count)
            }
        });
        let annotation = text_annotation(ws, check_annotation.as_deref());
        let desc_suffix = ws
            .description
            .as_deref()
            .map(|d| format!("\t# {d}"))
            .unwrap_or_default();
        let branch_suffix = ws
            .branch
            .as_deref()
            .map(|branch| format!("\tbranch={branch}"))
            .unwrap_or_default();
        println!(
            "{}\t{}{}{}{}",
            ws.name, path, annotation, branch_suffix, desc_suffix
        );
    }

    print_stale_warning_text(stale, stale_persistent, stale_ephemeral);
    print_missing_warning_text(missing);

    // bn-2l00: never advertise a `Merge ready: ... --destroy` line for a
    // workspace whose HEAD still has unresolved rebase conflict markers — the
    // merge gate hard-refuses it, so the suggestion is guaranteed to fail.
    // Instead, point at the resolve command, consistent with the merge gate.
    let mergeable: Vec<&str> = workspaces
        .iter()
        .filter(|ws| {
            ws.commits_ahead > 0 && ws.branch.is_none() && !ws.missing && !is_conflicted(ws)
        })
        .map(|ws| ws.name.as_str())
        .collect();
    if !mergeable.is_empty() {
        println!();
        for name in &mergeable {
            println!("Merge ready: maw ws merge {name} --into default --destroy");
        }
    }

    let conflicted: Vec<&WorkspaceInfo> =
        workspaces.iter().filter(|ws| is_conflicted(ws)).collect();
    if !conflicted.is_empty() {
        println!();
        for ws in &conflicted {
            println!(
                "Conflicted: {} has {} unresolved rebase conflict(s) — resolve before merge.",
                ws.name, ws.rebase_conflicts
            );
            println!(
                "  Fix: maw ws resolve {} --list, then --keep <side>",
                ws.name
            );
        }
    }
}

/// Print workspace list in colored, human-friendly format.
#[expect(
    clippy::too_many_lines,
    reason = "pretty renderer keeps all workspace categories in one output routine"
)]
fn print_list_pretty(
    workspaces: &[WorkspaceInfo],
    stale: &[String],
    stale_persistent: &[String],
    stale_ephemeral: &[String],
    missing: &[String],
    format: OutputFormat,
    verbose: bool,
) {
    let use_color = format.should_use_color();

    for ws in workspaces {
        let is_stale = ws.state.contains("stale");
        let is_persistent = ws.mode == "persistent";
        let conflicted = is_conflicted(ws);
        // bn-2l00: a conflicted workspace is NOT ready-to-merge. Don't paint it
        // with the cyan ready glyph — the merge gate hard-refuses it.
        let has_work = ws.commits_ahead > 0 && !ws.missing && !conflicted;
        let (glyph, name_style, reset) = if use_color {
            if ws.is_default {
                ("\u{25cf}", "\x1b[1;32m", "\x1b[0m") // Green bold for default
            } else if ws.missing || conflicted {
                // bn-2l00: conflicted shares the red-X treatment with missing —
                // both are "not ready, needs action" states.
                ("\u{2718}", "\x1b[1;31m", "\x1b[0m") // Red X for missing/conflicted
            } else if ws.state == "quarantine" {
                ("\u{26a0}", "\x1b[1;31m", "\x1b[0m") // Red bold for quarantine
            } else if is_stale {
                ("\u{25b2}", "\x1b[1;33m", "\x1b[0m") // Yellow for stale
            } else if has_work {
                ("\u{25b6}", "\x1b[1;36m", "\x1b[0m") // Cyan for ready-to-merge
            } else {
                ("\u{25cc}", "\x1b[90m", "\x1b[0m") // Gray for idle
            }
        } else if ws.is_default {
            ("\u{25cf}", "", "")
        } else if ws.missing || conflicted {
            ("\u{2718}", "", "")
        } else if ws.state == "quarantine" {
            ("\u{26a0}", "", "")
        } else if has_work {
            ("\u{25b6}", "", "")
        } else {
            ("\u{25cc}", "", "")
        };

        let mode_tag = if is_persistent { " [persistent]" } else { "" };
        let branch_tag = ws
            .branch
            .as_deref()
            .map(|branch| format!(" [branch: {branch}]"))
            .unwrap_or_default();
        let check_tag = ws
            .merge_check
            .as_ref()
            .map(|mc| {
                if mc.stale {
                    if use_color {
                        " \x1b[33m[stale]\x1b[0m".to_string()
                    } else {
                        " [stale]".to_string()
                    }
                } else if mc.ready {
                    if use_color {
                        " \x1b[32m[clean]\x1b[0m".to_string()
                    } else {
                        " [clean]".to_string()
                    }
                } else if use_color {
                    format!(" \x1b[31m[{} conflict(s)]\x1b[0m", mc.conflict_count)
                } else {
                    format!(" [{} conflict(s)]", mc.conflict_count)
                }
            })
            .unwrap_or_default();
        // bn-242l: named lifecycle slug tag, identical to the JSON
        // `lifecycle_state` field so prose-reading and JSON-reading
        // agents branch on the same vocabulary.
        let lifecycle_tag = ws
            .lifecycle_state
            .map(|state| {
                if use_color {
                    format!(" \x1b[35m[lifecycle:{}]\x1b[0m", state.slug())
                } else {
                    format!(" [lifecycle:{}]", state.slug())
                }
            })
            .unwrap_or_default();
        println!(
            "{} {}{}{} {} {}{}{}{}{}",
            glyph,
            name_style,
            ws.name,
            reset,
            ws.epoch,
            ws.state,
            mode_tag,
            branch_tag,
            check_tag,
            lifecycle_tag
        );

        if let Some(desc) = &ws.description {
            if use_color {
                println!("    \x1b[90m{desc}\x1b[0m");
            } else {
                println!("    {desc}");
            }
        }

        if ws.missing {
            let fix = format!("maw ws destroy {} --force", ws.name);
            if use_color {
                println!("    \x1b[31mworktree dir is gone — fix: {fix}\x1b[0m");
            } else {
                println!("    worktree dir is gone — fix: {fix}");
            }
        } else if is_conflicted(ws) {
            // bn-2l00: surface the conflict and the resolve path so this stays
            // consistent with `ws status` / the merge gate (which hard-refuses).
            let msg = format!(
                "{} unresolved rebase conflict(s) — resolve before merge: maw ws resolve {} --list",
                ws.rebase_conflicts, ws.name
            );
            if use_color {
                println!("    \x1b[31m{msg}\x1b[0m");
            } else {
                println!("    {msg}");
            }
        }

        if verbose {
            if let Some(path) = &ws.path {
                println!("    path: {path}");
            }
            if ws.is_default {
                println!("    default workspace");
            }
        }
    }

    // Stale warnings with mode-specific guidance.
    if !stale_persistent.is_empty() {
        println!();
        if use_color {
            println!(
                "\x1b[1;33m\u{25b2} STALE persistent workspace(s):\x1b[0m {}",
                stale_persistent.join(", ")
            );
        } else {
            println!(
                "\u{25b2} STALE persistent workspace(s): {}",
                stale_persistent.join(", ")
            );
        }
        for ws in stale_persistent {
            println!("  Fix: maw ws advance {ws}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        if use_color {
            println!(
                "\x1b[1;33m\u{25b2} WARNING: stale ephemeral workspace(s):\x1b[0m {}",
                stale_ephemeral.join(", ")
            );
        } else {
            println!(
                "\u{25b2} WARNING: stale ephemeral workspace(s): {}",
                stale_ephemeral.join(", ")
            );
        }
        println!(
            "  Ephemeral workspaces should be merged or destroyed — they survived an epoch advance."
        );
        println!(
            "  Fix: maw ws sync --all  (to sync) or maw ws merge <name> --into default (to merge and destroy)"
        );
    }

    if !missing.is_empty() {
        println!();
        if use_color {
            println!(
                "\x1b[1;31m\u{2718} MISSING worktree(s):\x1b[0m {}",
                missing.join(", ")
            );
        } else {
            println!("\u{2718} MISSING worktree(s): {}", missing.join(", "));
        }
        println!("  Worktree directory was removed but registry still tracks it.");
        for ws in missing {
            println!("  Fix: maw ws destroy {ws} --force");
        }
    }

    // Legacy: combined stale notice if nothing split above.
    if stale.is_empty() && !workspaces.is_empty() {
        // Nothing stale.
    } else if stale_persistent.is_empty() && stale_ephemeral.is_empty() && !stale.is_empty() {
        // Fallback for workspaces with unknown mode.
        println!();
        if use_color {
            println!(
                "\x1b[1;33m\u{25b2} WARNING:\x1b[0m {} stale workspace(s): {}",
                stale.len(),
                stale.join(", ")
            );
        } else {
            println!(
                "\u{25b2} WARNING: {} stale workspace(s): {}",
                stale.len(),
                stale.join(", ")
            );
        }
        println!("  Fix: maw ws sync --all");
    }

    if !workspaces.is_empty() {
        println!();
        if use_color {
            println!("\x1b[90mNext: maw exec <name> -- <command>\x1b[0m");
        } else {
            println!("Next: maw exec <name> -- <command>");
        }
    }
}

/// Print workspace list as JSON with stale-workspace advice.
fn print_list_json(
    workspaces: Vec<WorkspaceInfo>,
    stale_workspaces: Vec<String>,
    stale_persistent: Vec<String>,
    stale_ephemeral: Vec<String>,
    missing: Vec<String>,
    format: OutputFormat,
) {
    let mut advice = vec![];

    // bn-2l00: surface conflicted workspaces in structured advice too, so a
    // machine consumer steering off `ws list` gets the same "not ready —
    // resolve first" signal the human output gives, consistent with the merge
    // gate. (Per-workspace `rebase_conflicts`/`state` fields already carry the
    // raw signal; this advice mirrors the missing/stale advice pattern.)
    let conflicted: Vec<String> = workspaces
        .iter()
        .filter(|ws| is_conflicted(ws))
        .map(|ws| ws.name.clone())
        .collect();
    if !conflicted.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} workspace(s) conflicted — unresolved rebase conflict markers in HEAD; \
                 NOT ready to merge: {}",
                conflicted.len(),
                conflicted.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: conflicted,
                fix: "maw ws resolve <name> --list, then --keep <side>".to_string(),
            }),
        });
    }

    if !missing.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} workspace(s) MISSING — worktree dir gone, registry stale: {}",
                missing.len(),
                missing.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: missing,
                fix: "maw ws destroy <name> --force".to_string(),
            }),
        });
    }

    if !stale_persistent.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} stale persistent workspace(s): {} — run maw ws advance <name>",
                stale_persistent.len(),
                stale_persistent.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: stale_persistent,
                fix: "maw ws advance <name>".to_string(),
            }),
        });
    }

    if !stale_ephemeral.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} stale ephemeral workspace(s): {} — survived epoch advance; merge or destroy",
                stale_ephemeral.len(),
                stale_ephemeral.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: stale_ephemeral,
                fix: "maw ws sync --all".to_string(),
            }),
        });
    }

    // Fallback advice if stale workspaces exist but weren't categorized.
    if advice.is_empty() && !stale_workspaces.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} workspace(s) stale: {}",
                stale_workspaces.len(),
                stale_workspaces.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: stale_workspaces,
                fix: "maw ws sync --all".to_string(),
            }),
        });
    }

    let envelope = WorkspaceListEnvelope { workspaces, advice };

    match format.serialize(&envelope) {
        Ok(output) => println!("{output}"),
        Err(e) => {
            tracing::warn!("Failed to serialize to JSON: {e}");
        }
    }
}

/// Print stale workspace warnings for text output mode.
fn print_stale_warning_text(
    stale: &[String],
    stale_persistent: &[String],
    stale_ephemeral: &[String],
) {
    if !stale_persistent.is_empty() {
        println!();
        println!(
            "STALE persistent workspace(s): {}",
            stale_persistent.join(", ")
        );
        for ws in stale_persistent {
            println!("  Fix: maw ws advance {ws}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        println!(
            "WARNING: stale ephemeral workspace(s): {}",
            stale_ephemeral.join(", ")
        );
        println!("  Survived epoch advance — merge or destroy:");
        println!("  Fix: maw ws sync --all");
    }
    // Fallback for workspaces with unknown mode.
    if stale_persistent.is_empty() && stale_ephemeral.is_empty() && !stale.is_empty() {
        println!();
        println!(
            "WARNING: {} stale workspace(s): {}",
            stale.len(),
            stale.join(", ")
        );
        println!("  Fix: maw ws sync --all");
    }
}

/// Print MISSING-worktree warnings for text output mode (bn-3fhj).
fn print_missing_warning_text(missing: &[String]) {
    if missing.is_empty() {
        return;
    }
    println!();
    println!("MISSING worktree(s): {}", missing.join(", "));
    println!("  Worktree directory was removed but registry still tracks it.");
    for ws in missing {
        println!("  Fix: maw ws destroy {ws} --force");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_info(name: &str, commits_ahead: u32, rebase_conflicts: u32) -> WorkspaceInfo {
        // bn-242l: build a synthetic LifecycleSignals so the test
        // fixture mirrors what `list()` does at runtime — same
        // classifier output the production code path would compute.
        let signals = LifecycleSignals {
            missing: false,
            rebase_conflicts,
            is_stale: false,
            commits_ahead,
            has_uncommitted: false,
            was_integrated: false,
        };
        let lifecycle_state = LifecycleState::classify(signals);
        let fix_command = lifecycle_state.fix_command(name, false);
        WorkspaceInfo {
            name: name.to_string(),
            is_default: false,
            epoch: "abc123def456".to_string(),
            // `state` is built in `list()` exactly like `ws status` builds its
            // own entry state (status.rs): conflicted entries use this string.
            state: if rebase_conflicts > 0 {
                format!("conflicted ({rebase_conflicts} conflict(s))")
            } else {
                "ready".to_string()
            },
            mode: "ephemeral".to_string(),
            path: Some(format!("/tmp/ws/{name}")),
            behind_epochs: None,
            commits_ahead,
            template: None,
            template_defaults: None,
            branch: None,
            merge_check: None,
            rebase_conflicts,
            description: None,
            missing: false,
            lifecycle_state: Some(lifecycle_state),
            fix_command,
        }
    }

    /// Mirror of `ws status`'s per-workspace conflicted classification
    /// (status.rs `WorkspaceEntry`: `rebase_conflicts > 0`). The bug (bn-2l00)
    /// was that `ws list` disagreed with this. Keeping this mirror here makes
    /// the disagreement a compile-pinned, asserted invariant.
    fn status_says_conflicted(rebase_conflicts: u32) -> bool {
        rebase_conflicts > 0
    }

    /// bn-2l00 regression: a workspace with unresolved rebase conflicts must
    /// NEVER be classified "ready to merge" by `ws list`, and its
    /// classification must agree with what `ws status` reports.
    #[test]
    fn conflicted_workspace_not_classified_ready_to_merge() {
        // Has committed work AND unresolved rebase conflicts — exactly the
        // bn-2l00 repro state.
        let ws = ws_info("bob", 2, 1);

        // ws list classification.
        assert!(
            is_conflicted(&ws),
            "ws list must classify a workspace with rebase_conflicts>0 as conflicted"
        );

        // Parity with ws status: both readers derive from the SAME
        // `rebase_conflicts` source of truth and must never disagree.
        assert_eq!(
            is_conflicted(&ws),
            status_says_conflicted(ws.rebase_conflicts),
            "ws list and ws status conflicted classification must match"
        );

        let annotation = text_annotation(&ws, None);
        assert!(
            !annotation.contains("ready to merge"),
            "conflicted workspace must NOT be annotated 'ready to merge', got: {annotation:?}"
        );
        assert!(
            annotation.contains("conflicted"),
            "conflicted workspace annotation must surface the conflict, got: {annotation:?}"
        );
        assert!(
            annotation.contains("maw ws resolve"),
            "conflicted annotation must point at the resolve command, got: {annotation:?}"
        );

        // The `state` string `ws list` exposes (also in --format json) matches
        // what `ws status` exposes for the same conflicted workspace.
        assert_eq!(ws.state, "conflicted (1 conflict(s))");
    }

    /// Don't over-correct: a clean workspace with committed work must STILL be
    /// classified ready-to-merge.
    #[test]
    fn clean_workspace_still_ready_to_merge() {
        let ws = ws_info("alice", 1, 0);

        assert!(!is_conflicted(&ws));
        assert_eq!(
            is_conflicted(&ws),
            status_says_conflicted(ws.rebase_conflicts)
        );

        let annotation = text_annotation(&ws, None);
        assert!(
            annotation.contains("ready to merge"),
            "clean workspace with work must remain ready-to-merge, got: {annotation:?}"
        );
        assert!(!annotation.contains("conflicted"));
    }

    /// The `Merge ready: ... --destroy` suggestion line must never be emitted
    /// for a conflicted workspace (the bn-2l00 lure), but must still be emitted
    /// for clean ones. We assert on the mergeable-filter predicate directly.
    #[test]
    fn merge_ready_suggestion_excludes_conflicted() {
        let conflicted = ws_info("bob", 2, 1);
        let clean = ws_info("alice", 1, 0);

        let is_mergeable = |ws: &WorkspaceInfo| {
            ws.commits_ahead > 0 && ws.branch.is_none() && !ws.missing && !is_conflicted(ws)
        };

        assert!(
            !is_mergeable(&conflicted),
            "conflicted workspace must be excluded from the 'Merge ready' suggestion"
        );
        assert!(
            is_mergeable(&clean),
            "clean workspace with work must still get the 'Merge ready' suggestion"
        );
    }
}
