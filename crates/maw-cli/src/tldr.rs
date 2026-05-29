//! `maw tldr` — a short, affirmative quick-reference of common commands.
//!
//! Follows the `<tool> tldr` convention (cf. `bn tldr`).
//!
//! # What this is for
//!
//! This is the **verb-discoverability** mitigation for the
//! `vocabulary_scarcity` friction cluster (SG4 bn-1t17): agents are
//! exceptional at git but scarce on maw's own verbs, so the tool
//! describes its common surface up front. `maw tldr` answers "what are
//! the commands I actually reach for, and how do I spell them?" — it is
//! deliberately a short cheat-sheet of *affirmative* usage, not a manual
//! and not a list of anti-patterns.
//!
//! The same text is appended to `maw --help` (see [`quick_reference`]),
//! sourced from one table so the two can never drift.
//!
//! Renamed from `maw crib` (2026-05-29, bn-zg7c) to adopt the
//! `<tool> tldr` convention; `crib` remains a hidden alias.

use anyhow::Result;

/// One task-oriented block: a short imperative title and the commands
/// an agent pastes to do it. `cmds` entries may carry a trailing
/// `\t# comment` which the renderer aligns as an inline note.
struct Block {
    title: &'static str,
    cmds: &'static [&'static str],
}

/// The affirmative quick-reference. Common commands only, grouped by the
/// task an agent is trying to accomplish. No anti-patterns, no essays.
const BLOCKS: &[Block] = &[
    Block {
        title: "Create a workspace (isolated git worktree)",
        cmds: &[
            "maw ws create <name> --from main",
            "maw ws create <bone-id> --from main --description \"<title>\"",
        ],
    },
    Block {
        title: "See what's going on",
        cmds: &[
            "maw status\t# repo + workspace overview",
            "maw ws list\t# workspaces with state (active / stale / conflicted / +N to merge)",
            "maw ws diff <name>\t# a workspace's changes vs the epoch",
        ],
    },
    Block {
        title: "Run a command inside a workspace (cd doesn't persist)",
        cmds: &[
            "maw exec <name> -- <command>",
            "maw exec <name> -- git add -A && maw exec <name> -- git commit -m \"feat: ...\"",
            "maw exec default -- bn <args>\t# bones always runs through the default workspace",
        ],
    },
    Block {
        title: "Refresh a stale workspace onto the latest epoch",
        cmds: &["maw ws sync <name>"],
    },
    Block {
        title: "Merge work into default",
        cmds: &[
            "maw ws merge <name> --into default --check\t# dry-run first",
            "maw ws merge <name> --into default --destroy --message \"feat: <title>\"",
        ],
    },
    Block {
        title: "Recover work from a destroyed workspace (nothing is ever lost)",
        cmds: &[
            "maw ws recover\t# list destroyed workspaces",
            "maw ws recover <name>\t# inspect its contents",
            "maw ws recover <name> --to <new-name>\t# restore it",
        ],
    },
    Block {
        title: "Resolve conflicts (conflicts are state, not failure)",
        cmds: &[
            "maw ws conflicts <name>\t# what conflicts, and where",
            "maw ws resolve <name> --keep epoch | --keep <name> | --keep both",
        ],
    },
    Block {
        title: "Check substrate health",
        cmds: &[
            "maw doctor",
            "maw doctor --repair\t# apply known-safe auto-fixes",
        ],
    },
];

/// Render the quick reference as plain text in the `bn tldr` style.
///
/// A `QUICK REFERENCE` banner, then each block as a two-space-indented
/// title followed by six-space-indented commands. Inline `\t# comment`
/// notes are kept on the command line.
#[must_use]
pub fn render() -> String {
    use std::fmt::Write as _;
    let mut out = String::from("QUICK REFERENCE\n");
    for block in BLOCKS {
        let _ = write!(out, "\n  {}\n\n", block.title);
        for cmd in block.cmds {
            match cmd.split_once('\t') {
                Some((command, note)) => {
                    let _ = writeln!(out, "      {command}  {note}");
                }
                None => {
                    let _ = writeln!(out, "      {cmd}");
                }
            }
        }
    }
    out
}

/// The same quick reference, formatted for appending to `maw --help`.
/// Returned owned so it can be attached to the clap command at runtime.
#[must_use]
pub fn quick_reference() -> String {
    format!(
        "{}\nSee 'maw <command> --help' for more information on a specific command.",
        render()
    )
}

/// CLI entrypoint for `maw tldr`.
///
/// # Errors
///
/// Infallible in practice (content is static); returns `Result` to match
/// the command-dispatch signature.
pub fn run() -> Result<()> {
    print!("{}", render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command in the cheat-sheet must be copy-pasteable: it starts
    /// with `maw ` so an agent can paste it without rewriting.
    #[test]
    fn every_command_is_copy_pasteable() {
        for block in BLOCKS {
            assert!(!block.title.is_empty(), "block title must be non-empty");
            assert!(
                !block.cmds.is_empty(),
                "block must have at least one command"
            );
            for cmd in block.cmds {
                let command = cmd.split_once('\t').map_or(*cmd, |(c, _)| c);
                assert!(
                    command.trim_start().starts_with("maw "),
                    "command must start with `maw `: {cmd:?}"
                );
            }
        }
    }

    /// The rendered cheat-sheet must cover the day-1 verb surface.
    #[test]
    fn render_covers_core_verbs() {
        let out = render();
        assert!(out.starts_with("QUICK REFERENCE"));
        for needle in [
            "maw ws create",
            "maw ws merge",
            "maw ws recover",
            "maw exec",
            "maw ws sync",
            "maw doctor",
        ] {
            assert!(out.contains(needle), "tldr must mention `{needle}`");
        }
    }

    /// bn-232g regression guard: the tldr is affirmative. The retracted
    /// "overkill / when NOT to use maw" framing must never reappear, and
    /// the simplified output must not regrow the anti-pattern table.
    #[test]
    fn output_is_affirmative_only() {
        let out = render();
        assert!(
            !out.contains("overkill"),
            "retracted overkill framing must not appear"
        );
        assert!(!out.contains("When NOT"), "no when-NOT-to-use section");
        assert!(!out.contains("Wrong"), "no wrong/right pitfalls table");
        assert!(
            !out.to_lowercase().contains("don't"),
            "tldr lists what to do, not what not to do"
        );
    }

    /// `--help` and `maw tldr` share one source so they cannot drift.
    #[test]
    fn quick_reference_wraps_render() {
        let qr = quick_reference();
        assert!(qr.contains(&render()));
        assert!(qr.contains("maw <command> --help"));
    }
}
