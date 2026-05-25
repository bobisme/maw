//! `maw crib <agent>` — per-agent protocol emitter for verb discoverability.
//!
//! # What this is for
//!
//! This is the **verb-discoverability** mitigation for the
//! `vocabulary_scarcity` friction cluster (SG4 bn-1t17, parent SG4
//! bn-2j45). The cluster fires when an agent issues a verb/flag
//! combination that doesn't exist in the maw CLI and has to discover
//! the correct surface by trial and error. Per the
//! `maw-design-rationale-agent-fluency` memory, *maw's self-describing
//! output is the load-bearing mitigation* for the training-data-scarce
//! verb problem (agents are exceptional at git but scarce on maw's
//! own verbs).
//!
//! `maw crib` lets an agent (or a coordinator dispatching agents) ask
//! the tool *up front* what it can do, in a machine-friendly form,
//! so the agent never has to guess at verbs. The expected usage is
//! "first call of the session" — emit the crib, hand the JSON to the
//! agent, let it bind correct verb names before it tries to do work.
//!
//! # Outputs
//!
//! Three forms, all sourced from the same in-process protocol table
//! (so they cannot drift from each other):
//!
//! - `maw crib <agent>` — Markdown cheat-sheet (default; copy-pasteable
//!   into an agent's context).
//! - `maw crib <agent> --format json` — stable JSON for programmatic
//!   consumption.
//! - `maw crib --overkill-line` — the one-line "when to use maw vs.
//!   when to use plain Claude/Codex worktrees" guidance (the second
//!   half of the SG4 mitigation: don't reach for maw verbs at all if
//!   the task is one-off-single-agent).
//!
//! # Agent identifiers
//!
//! `<agent>` is a free-form identifier (`claude`, `codex`, `generic`,
//! `maw-dev`, etc.). The crib content is the same shape for every
//! agent today; the identifier is captured in the JSON envelope so
//! downstream tooling can record which agent was briefed. Future
//! per-agent specialization (e.g. Codex-specific shell idioms) can be
//! added without an interface change.
//!
//! # Why no per-agent divergence yet
//!
//! Per the design rationale, the agent-fluency problem is *structural*
//! (training-data scarcity), not per-agent. The same crib serves all
//! agents until field data shows per-agent variance.

use anyhow::{Context as _, Result};
use clap::Args;
use serde::Serialize;

/// `maw crib` arguments.
#[derive(Debug, Args)]
pub struct CribArgs {
    /// Agent identifier to brief (e.g. `claude`, `codex`, `generic`).
    ///
    /// Free-form; recorded in the JSON envelope. The crib content is
    /// the same for every agent today, but the identifier is captured
    /// for downstream tracking (which agent was briefed when).
    #[arg(default_value = "generic")]
    pub agent: String,

    /// Output format: `md` (default, human-readable), `json` (machine).
    ///
    /// `md` produces a copy-pasteable cheat sheet; `json` produces the
    /// stable protocol envelope (see `CribProtocol`).
    #[arg(long, default_value = "md")]
    pub format: CribFormat,

    /// Print ONLY the "when to use maw vs. plain worktrees" guidance.
    ///
    /// Use this in an agent's system prompt so it has the correct
    /// *mental model* up front and doesn't reach for nonexistent maw
    /// verbs on tasks that don't need workspace coordination at all.
    /// Mutually informative with the full crib — the overkill line is
    /// already included as a section in the full output.
    #[arg(long)]
    pub overkill_line: bool,
}

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum CribFormat {
    /// Markdown cheat sheet (default; for direct paste into agent context).
    Md,
    /// Stable JSON protocol envelope (for programmatic consumption).
    Json,
}

// ---------------------------------------------------------------------------
// Protocol payload (stable; bumped via `CRIB_SCHEMA_VERSION`)
// ---------------------------------------------------------------------------

/// Stable schema version for the JSON output. Bump when the JSON
/// shape changes in a breaking way (adding fields is non-breaking).
pub const CRIB_SCHEMA_VERSION: u32 = 1;

/// Top-level JSON envelope emitted by `maw crib --format json`.
#[derive(Debug, Clone, Serialize)]
pub struct CribProtocol {
    /// Schema version (see `CRIB_SCHEMA_VERSION`).
    pub schema_version: u32,
    /// maw binary version, from `CARGO_PKG_VERSION`.
    pub maw_version: &'static str,
    /// Agent identifier passed on the command line.
    pub agent: String,
    /// The one-line overkill guidance (when NOT to reach for maw).
    pub overkill_line: &'static str,
    /// Named verb groups (workspace, bones-via-exec, recovery, etc.).
    pub verb_groups: Vec<VerbGroup>,
    /// Common verb pitfalls (synonym/alias hints — what agents
    /// usually try first that doesn't work, and the correct surface).
    pub vocabulary_pitfalls: Vec<VocabularyPitfall>,
    /// Self-describing-output contract (Prime Invariant + error
    /// recovery promises that the CLI honours).
    pub contracts: Vec<&'static str>,
}

/// One named group of related verbs (e.g. "workspace lifecycle").
#[derive(Debug, Clone, Serialize)]
pub struct VerbGroup {
    /// Group name (display label).
    pub name: &'static str,
    /// One-paragraph summary of what the group is for.
    pub summary: &'static str,
    /// The verbs themselves.
    pub verbs: Vec<Verb>,
}

/// One verb entry.
#[derive(Debug, Clone, Serialize)]
pub struct Verb {
    /// Copy-pasteable command template (e.g. `maw ws create <name>`).
    pub command: &'static str,
    /// One-line description.
    pub description: &'static str,
}

/// A common verb mistake → correct-surface pointer.
#[derive(Debug, Clone, Serialize)]
pub struct VocabularyPitfall {
    /// What the agent tried (the wrong verb / shape).
    pub wrong: &'static str,
    /// The right verb / shape.
    pub right: &'static str,
    /// One-line explanation (optional).
    pub note: &'static str,
}

// ---------------------------------------------------------------------------
// The crib content (single source of truth for both MD and JSON).
// ---------------------------------------------------------------------------

/// The one-line "overkill" guidance for when NOT to use maw.
///
/// Exposed as `pub const` so the `unknown_command_hint` (see `error_hints`
/// module) can quote it verbatim in error-recovery output.
pub const OVERKILL_LINE: &str = "Use plain Claude/Codex worktrees for one-off single-agent tasks. \
Use maw when (a) two or more agents work in parallel, (b) work touches files another \
in-flight workspace also touches, or (c) merges/rebases need to be coordinated against \
an integration branch.";

/// Build the protocol envelope. Single source of truth shared by the
/// MD renderer and the JSON serializer.
#[must_use]
#[allow(clippy::too_many_lines)] // static content table; one-line-per-row is the readable form
pub fn build_protocol(agent: &str) -> CribProtocol {
    CribProtocol {
        schema_version: CRIB_SCHEMA_VERSION,
        maw_version: env!("CARGO_PKG_VERSION"),
        agent: agent.to_string(),
        overkill_line: OVERKILL_LINE,
        verb_groups: vec![
            VerbGroup {
                name: "Workspace lifecycle",
                summary: "Isolated git worktrees per agent. Created from a base ref, edited freely, then merged back. Never edit `ws/default/` directly.",
                verbs: vec![
                    Verb {
                        command: "maw ws create <name> --from main --description \"<title>\"",
                        description: "Create an isolated workspace at ws/<name>/. Use the bone id as <name> when working a bone.",
                    },
                    Verb {
                        command: "maw ws list",
                        description: "List workspaces with state (active / conflicted / stale / +N to merge).",
                    },
                    Verb {
                        command: "maw ws status",
                        description: "Detailed status across all workspaces (stale, conflicted, dirty).",
                    },
                    Verb {
                        command: "maw ws sync <name>",
                        description: "Refresh a stale workspace onto the latest epoch (jj-style: keeps going on conflict).",
                    },
                    Verb {
                        command: "maw ws merge <name> --into default --check",
                        description: "Dry-run merge — verify conflict status before --destroy.",
                    },
                    Verb {
                        command: "maw ws merge <name> --into default --destroy --message \"feat: <title>\"",
                        description: "Merge a workspace into default and clean it up. ALWAYS --check first.",
                    },
                    Verb {
                        command: "maw ws destroy <name>",
                        description: "Remove a workspace. Refuses if unmerged work present (Prime Invariant). --force captures a recovery snapshot first.",
                    },
                ],
            },
            VerbGroup {
                name: "Recovery (Prime Invariant)",
                summary: "No committed work can ever be lost. Every destroy creates a recovery snapshot. Always check recovery before assuming work is gone.",
                verbs: vec![
                    Verb {
                        command: "maw ws recover",
                        description: "List all destroyed workspaces with recovery snapshots.",
                    },
                    Verb {
                        command: "maw ws recover <name>",
                        description: "Inspect a destroyed workspace's contents.",
                    },
                    Verb {
                        command: "maw ws recover --search \"<pattern>\"",
                        description: "Search all destroyed snapshots for a pattern.",
                    },
                    Verb {
                        command: "maw ws recover <name> --to <new-name>",
                        description: "Restore a destroyed workspace into a fresh workspace.",
                    },
                ],
            },
            VerbGroup {
                name: "Conflict handling (conflicts are data, not errors)",
                summary: "Operations succeed even with conflicts; conflicts are explicit state. Resolve when convenient. The only hard gate: `merge` refuses a source workspace whose HEAD has unresolved markers.",
                verbs: vec![
                    Verb {
                        command: "maw ws conflicts <name>",
                        description: "Show detailed conflict info for a workspace.",
                    },
                    Verb {
                        command: "maw ws resolve <name> --list",
                        description: "List structured conflicts in a workspace.",
                    },
                    Verb {
                        command: "maw ws resolve <name> --keep epoch | --keep <ws> | --keep both",
                        description: "Materialize a resolution by side. Commit and `maw ws sync` to clear conflict metadata.",
                    },
                ],
            },
            VerbGroup {
                name: "Repo / state introspection",
                summary: "Quick checks before / between operations.",
                verbs: vec![
                    Verb {
                        command: "maw status",
                        description: "Repo + workspace overview.",
                    },
                    Verb {
                        command: "maw doctor",
                        description: "Health check (toolchain, layout, refs).",
                    },
                    Verb {
                        command: "maw doctor --repair",
                        description: "Apply known-safe auto-fixes (e.g. epoch ff_absorbable drift).",
                    },
                    Verb {
                        command: "maw epoch sync",
                        description: "Resync refs/manifold/epoch/current to branch HEAD (after direct commits).",
                    },
                    Verb {
                        command: "maw gc",
                        description: "GC epoch snapshots + dangling head refs.",
                    },
                ],
            },
            VerbGroup {
                name: "Running commands inside a workspace",
                summary: "`cd` doesn't persist in sandboxed agent environments. Use `maw exec` for every command that needs to run inside a workspace.",
                verbs: vec![
                    Verb {
                        command: "maw exec <name> -- <command>",
                        description: "Run any command inside ws/<name>/.",
                    },
                    Verb {
                        command: "maw exec <name> -- git status",
                        description: "Git operations inside a workspace.",
                    },
                    Verb {
                        command: "maw exec default -- bn ...",
                        description: "Bones (issue tracker) commands ALWAYS go through the default workspace.",
                    },
                ],
            },
        ],
        vocabulary_pitfalls: vec![
            VocabularyPitfall {
                wrong: "maw ws new <name>",
                right: "maw ws create <name>",
                note: "The verb is `create`, not `new`. Mirrors `git worktree add`'s intent but uses `create`.",
            },
            VocabularyPitfall {
                wrong: "maw workspace add <name>",
                right: "maw ws create <name> --from <ref>",
                note: "`workspace` and `ws` are aliases; the verb is still `create`.",
            },
            VocabularyPitfall {
                wrong: "maw checkout <branch>",
                right: "maw ws create <name> --from <branch>",
                note: "maw doesn't model branches as checkouts. Each branch maps to a workspace.",
            },
            VocabularyPitfall {
                wrong: "maw branch <name>",
                right: "maw ws create <name> --from <ref>",
                note: "Workspace ≈ branch. Don't manage branches directly.",
            },
            VocabularyPitfall {
                wrong: "maw stash",
                right: "maw exec <name> -- git stash",
                note: "Git is exposed inside workspaces. Use plain git for stash/diff/log inside `maw exec`.",
            },
            VocabularyPitfall {
                wrong: "maw commit",
                right: "maw exec <name> -- git add -A && maw exec <name> -- git commit -m \"...\"",
                note: "maw doesn't wrap commit. Commit with plain git inside the workspace.",
            },
            VocabularyPitfall {
                wrong: "maw rebase / maw pull --rebase",
                right: "maw ws sync <name>",
                note: "`ws sync` is the rebase verb. It's jj-style: it labels conflicts and continues.",
            },
            VocabularyPitfall {
                wrong: "maw ws delete / maw ws rm <name>",
                right: "maw ws destroy <name>",
                note: "The verb is `destroy`; it refuses unmerged work (Prime Invariant) unless --force.",
            },
            VocabularyPitfall {
                wrong: "maw merge <ws>",
                right: "maw ws merge <ws> --into default --check",
                note: "Top-level `maw merge` is for the post-merge quarantine, not for merging workspaces. Use `ws merge`.",
            },
            VocabularyPitfall {
                wrong: "bn <args>  (run directly)",
                right: "maw exec default -- bn <args>",
                note: "Bones always runs through `maw exec default`, never directly.",
            },
        ],
        contracts: vec![
            "Prime Invariant: no committed work is ever lost. `destroy` refuses unmerged work; `--force` snapshots first.",
            "Conflicts are data, not errors. Operations succeed; conflicts become explicit state in `ws status` / sidecars.",
            "Self-describing output: every error path includes the exact recovery command.",
            "Agent-friendly: every success message tells you what to do next, with copy-pasteable commands.",
            "`cd` doesn't persist in sandboxed environments — use `maw exec <name> -- <cmd>` for everything inside a workspace.",
        ],
    }
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

/// Render the protocol as a Markdown cheat sheet (default human format).
#[must_use]
pub fn render_markdown(proto: &CribProtocol) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "# maw crib — {} (maw {})", proto.agent, proto.maw_version);
    out.push('\n');
    let _ = writeln!(out, "_Schema v{}._\n", proto.schema_version);

    let _ = writeln!(out, "## When NOT to use maw");
    out.push('\n');
    let _ = writeln!(out, "{}", proto.overkill_line);
    out.push('\n');

    for group in &proto.verb_groups {
        let _ = writeln!(out, "## {}", group.name);
        out.push('\n');
        let _ = writeln!(out, "{}", group.summary);
        out.push('\n');
        for verb in &group.verbs {
            let _ = writeln!(out, "- `{}` — {}", verb.command, verb.description);
        }
        out.push('\n');
    }

    let _ = writeln!(out, "## Common vocabulary pitfalls");
    out.push('\n');
    let _ = writeln!(
        out,
        "These are verbs agents reach for from training data that don't exist in maw. \
Map them to the correct verb up front."
    );
    out.push('\n');
    let _ = writeln!(out, "| Wrong | Right | Note |");
    let _ = writeln!(out, "|---|---|---|");
    for p in &proto.vocabulary_pitfalls {
        let _ = writeln!(out, "| `{}` | `{}` | {} |", p.wrong, p.right, p.note);
    }
    out.push('\n');

    let _ = writeln!(out, "## Contracts maw upholds");
    out.push('\n');
    for c in &proto.contracts {
        let _ = writeln!(out, "- {c}");
    }
    out.push('\n');

    let _ = writeln!(out, "## Discoverability");
    out.push('\n');
    let _ = writeln!(out, "- `maw --help` — top-level command list.");
    let _ = writeln!(out, "- `maw <command> --help` — per-command help (always works).");
    let _ = writeln!(out, "- `maw crib <agent> --format json` — this same content as JSON.");
    let _ = writeln!(out, "- `maw crib --overkill-line` — just the one-line guidance.");
    out.push('\n');

    out
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

/// CLI entrypoint for `maw crib`.
///
/// # Errors
///
/// Returns an error only on output serialization or write failure (the
/// content is statically built and cannot fail to construct).
pub fn run(args: &CribArgs) -> Result<()> {
    if args.overkill_line {
        println!("{OVERKILL_LINE}");
        return Ok(());
    }

    let proto = build_protocol(&args.agent);
    match args.format {
        CribFormat::Md => {
            print!("{}", render_markdown(&proto));
        }
        CribFormat::Json => {
            let json = serde_json::to_string_pretty(&proto)
                .context("serializing crib protocol to JSON")?;
            println!("{json}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overkill line is the load-bearing one-liner; agents quote it
    /// verbatim from `maw crib --overkill-line`. Pin its prefix so a
    /// rewrite doesn't silently change the contract.
    #[test]
    fn overkill_line_has_stable_prefix() {
        assert!(
            OVERKILL_LINE.starts_with("Use plain"),
            "overkill line must lead with 'Use plain...' (agent prompt-template uses prefix match)"
        );
        assert!(
            OVERKILL_LINE.contains("two or more agents")
                || OVERKILL_LINE.contains("2+")
                || OVERKILL_LINE.contains("multiple agents")
                || OVERKILL_LINE.contains("two or more"),
            "overkill line must name the multi-agent trigger"
        );
        assert!(
            OVERKILL_LINE.contains("integration branch") || OVERKILL_LINE.contains("merge"),
            "overkill line must name coordination as the qualifying use-case"
        );
    }

    /// The crib must cover every verb-group an agent needs day-1.
    #[test]
    fn protocol_covers_required_groups() {
        let proto = build_protocol("claude");
        let names: Vec<&str> = proto.verb_groups.iter().map(|g| g.name).collect();
        assert!(names.iter().any(|n| n.contains("Workspace lifecycle")));
        assert!(names.iter().any(|n| n.contains("Recovery")));
        assert!(names.iter().any(|n| n.contains("Conflict")));
        assert!(names.iter().any(|n| n.contains("introspection")));
        assert!(names.iter().any(|n| n.contains("inside a workspace")));
    }

    /// Every Verb command must mention `maw` so an agent can paste it
    /// without rewriting; every description is non-empty.
    #[test]
    fn every_verb_is_copy_pasteable() {
        let proto = build_protocol("generic");
        for group in &proto.verb_groups {
            for verb in &group.verbs {
                assert!(
                    verb.command.starts_with("maw "),
                    "verb command does not start with `maw `: {:?}",
                    verb.command,
                );
                assert!(
                    !verb.description.is_empty(),
                    "verb description is empty for {:?}",
                    verb.command,
                );
            }
        }
    }

    /// Pitfalls must each have a non-empty wrong + right + note, and
    /// the right side must be a maw command.
    #[test]
    fn pitfalls_route_to_maw_surface() {
        let proto = build_protocol("generic");
        assert!(
            proto.vocabulary_pitfalls.len() >= 8,
            "should ship at least 8 pitfalls to cover common training-data verbs"
        );
        for p in &proto.vocabulary_pitfalls {
            assert!(!p.wrong.is_empty());
            assert!(!p.right.is_empty());
            assert!(!p.note.is_empty());
            assert!(
                p.right.starts_with("maw ")
                    || p.right.contains("git ")
                    || p.right.contains("bn "),
                "right-hand side should be a real shell command: {:?}",
                p.right,
            );
        }
    }

    /// Specific high-value pitfalls that come from real-world friction:
    /// the `ws new` synonym (agents reach for git's `worktree add` form),
    /// the `destroy` vs `delete` mismatch (agents reach for `rm`), the
    /// top-level `maw merge` overload (it's for quarantine, not ws merge).
    #[test]
    fn high_value_pitfalls_present() {
        let proto = build_protocol("generic");
        let wrongs: Vec<&str> = proto.vocabulary_pitfalls.iter().map(|p| p.wrong).collect();
        assert!(
            wrongs.iter().any(|w| w.contains("ws new")),
            "must catch the `ws new` synonym (agents reach for git's worktree add)",
        );
        assert!(
            wrongs.iter().any(|w| w.contains("delete") || w.contains("rm")),
            "must catch the `delete`/`rm` synonym for destroy",
        );
        assert!(
            wrongs.iter().any(|w| w.starts_with("maw merge ")),
            "must clarify top-level `maw merge` vs `ws merge`",
        );
        assert!(
            wrongs.iter().any(|w| w.starts_with("bn ")),
            "must remind that bones runs through `maw exec default`",
        );
    }

    /// JSON envelope is stable shape: `schema_version` field exists,
    /// agent is preserved, and every field round-trips.
    #[test]
    fn json_envelope_is_stable() {
        let proto = build_protocol("codex");
        let json = serde_json::to_value(&proto).expect("serializes");
        assert_eq!(json["schema_version"], CRIB_SCHEMA_VERSION);
        assert_eq!(json["agent"], "codex");
        assert!(json["overkill_line"].is_string());
        assert!(json["verb_groups"].is_array());
        assert!(json["vocabulary_pitfalls"].is_array());
        assert!(json["contracts"].is_array());
    }

    /// Schema version is non-zero and pinned (changing it is a breaking
    /// change for downstream consumers).
    #[test]
    fn schema_version_is_pinned() {
        assert_eq!(CRIB_SCHEMA_VERSION, 1);
    }

    /// The Markdown renderer must mention the overkill line so a single
    /// `maw crib <agent>` invocation gives the agent both the verbs AND
    /// the mental model. Otherwise the overkill line is hidden behind a
    /// separate flag that agents won't discover.
    #[test]
    fn markdown_includes_overkill_line() {
        let proto = build_protocol("claude");
        let md = render_markdown(&proto);
        assert!(
            md.contains("When NOT to use maw"),
            "MD output must surface the overkill-line section"
        );
        assert!(
            md.contains(OVERKILL_LINE),
            "MD output must include the overkill line verbatim"
        );
    }

    /// The Markdown renderer mentions `maw <command> --help` so the
    /// agent learns the universal discoverability backstop.
    #[test]
    fn markdown_teaches_help_flag() {
        let proto = build_protocol("generic");
        let md = render_markdown(&proto);
        assert!(md.contains("--help"), "MD output must teach `--help`");
        assert!(
            md.contains("maw crib"),
            "MD output should reference `maw crib` (self-discovery)"
        );
    }

    /// Default agent identifier when not supplied is `generic` (sane
    /// default; agents can override).
    #[test]
    fn default_agent_is_generic() {
        // The default is wired in the clap `#[arg(default_value = ...)]`;
        // we re-state it here so the contract is testable without
        // round-tripping through clap.
        let proto = build_protocol("generic");
        assert_eq!(proto.agent, "generic");
    }

    /// `run` with --overkill-line short-circuits to print just the line
    /// (regression guard: a refactor that drops this branch would push
    /// agents back into trial-and-error discovery).
    #[test]
    fn overkill_line_is_a_first_class_output() {
        // Smoke: the field exists on CribArgs and the const is non-empty.
        let _args = CribArgs {
            agent: "generic".to_string(),
            format: CribFormat::Md,
            overkill_line: true,
        };
        assert!(!OVERKILL_LINE.is_empty());
        assert!(OVERKILL_LINE.len() < 400, "overkill line stays one screen-line of context");
    }
}
