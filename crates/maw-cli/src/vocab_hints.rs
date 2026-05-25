//! Vocabulary-scarcity recovery hints for `maw` CLI errors.
//!
//! # What this module does
//!
//! When clap rejects a verb (e.g. `maw ws new`, `maw checkout`, `maw stash`),
//! it prints `error: unrecognized subcommand 'X'` and exits. That output
//! is what feeds the `vocabulary_scarcity` friction cluster (SG4
//! `MawVerbAttribution::VocabularyScarcity`): the agent has to discover
//! the correct surface by trial and error.
//!
//! This module classifies the rejected verb against a small table of
//! "common training-data verbs an agent might reach for" and emits a
//! ONE-LINE recovery hint pointing at the right maw verb (or telling
//! the agent it's reaching for plain-git territory, which lives inside
//! `maw exec`). The hint is the same single source of truth used by
//! `maw crib` (see `crib.rs::VocabularyPitfall`), so the
//! tool's error-recovery story is symmetrical with its briefing story.
//!
//! # Why a separate module (not inline in main.rs)
//!
//! The hint table is data, not control flow. Keeping it in its own
//! module lets us:
//!   - unit-test the classifier without parsing real CLI input;
//!   - share the table with `maw crib` if we ever want to derive one
//!     from the other (today they are intentionally two flat tables —
//!     duplication is cheap, drift is loud because the unit tests
//!     here pin the hint surface).
//!
//! # Layout-agnostic
//!
//! The hints quote `maw ...` verbs only. They never quote `ws/` or
//! `.manifold/` path strings. T3.2's layout-flavor enum lives in
//! `workspace::workspace_path` / `workspaces_dir`; nothing here needs
//! it.

/// A one-line recovery hint for a rejected verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbHint {
    /// The right command (copy-pasteable).
    pub suggestion: &'static str,
    /// Short explanation appended after the suggestion.
    pub note: &'static str,
}

impl VerbHint {
    /// Render as a single line for printing under a clap error.
    #[must_use]
    pub fn render(&self) -> String {
        format!("  did you mean: `{}`  ({})", self.suggestion, self.note)
    }
}

/// Classify a rejected top-level token against the known pitfall table.
///
/// `args` is the slice of command-line tokens after the binary name
/// (e.g. for `maw ws new alice`, pass `["ws", "new", "alice"]`).
///
/// Returns `None` for tokens the classifier doesn't recognize —
/// callers fall back to clap's default error in that case.
///
/// # Examples
///
/// ```
/// use maw_cli::vocab_hints::classify_rejected_verb;
///
/// // `maw ws new` is a training-data verb (git worktree add → "new"); maw uses `create`.
/// let h = classify_rejected_verb(&["ws", "new"]).unwrap();
/// assert!(h.suggestion.contains("maw ws create"));
///
/// // `maw checkout` is a git verb; maw maps it to ws creation.
/// assert!(classify_rejected_verb(&["checkout"]).is_some());
///
/// // Genuinely unknown / typos return None — caller falls back to clap default.
/// assert!(classify_rejected_verb(&["xyzzy"]).is_none());
/// ```
#[must_use]
pub fn classify_rejected_verb(args: &[&str]) -> Option<VerbHint> {
    match args {
        // ws-level synonyms — "new" is the training-data verb (git's
        // `worktree add` lands a new worktree; agents map "new ws" → "ws new").
        ["ws" | "workspace", "new", ..] => Some(VerbHint {
            suggestion: "maw ws create <name> --from main --description \"...\"",
            note: "the verb is `create`, not `new`",
        }),
        ["ws" | "workspace", "add", ..] => Some(VerbHint {
            suggestion: "maw ws create <name> --from <ref>",
            note: "the verb is `create`; `add` is a git-worktree idiom",
        }),
        ["ws" | "workspace", "delete" | "rm" | "remove", ..] => Some(VerbHint {
            suggestion: "maw ws destroy <name>",
            note: "the verb is `destroy`; refuses unmerged work (Prime Invariant) unless --force",
        }),
        ["ws" | "workspace", "rebase", ..] => Some(VerbHint {
            suggestion: "maw ws sync <name>",
            note: "`sync` is the jj-style rebase verb — keeps going on conflict",
        }),

        // Top-level verbs that don't exist — agents reach for git muscle memory.
        ["checkout", ..] => Some(VerbHint {
            suggestion: "maw ws create <name> --from <branch>",
            note: "maw doesn't model checkout — each branch is a workspace",
        }),
        ["branch", ..] => Some(VerbHint {
            suggestion: "maw ws create <name> --from <ref>",
            note: "maw doesn't model branches — workspace ≈ branch",
        }),
        ["stash", ..] => Some(VerbHint {
            suggestion: "maw exec <name> -- git stash",
            note: "git is exposed inside workspaces; use plain git inside `maw exec`",
        }),
        ["commit", ..] => Some(VerbHint {
            suggestion: "maw exec <name> -- git add -A && maw exec <name> -- git commit -m \"...\"",
            note: "maw doesn't wrap commit; commit with plain git inside the workspace",
        }),
        ["rebase", ..] | ["pull", "--rebase", ..] => Some(VerbHint {
            suggestion: "maw ws sync <name>",
            note: "use `ws sync` to refresh a stale workspace onto the latest epoch",
        }),
        ["log" | "diff" | "show", ..] => Some(VerbHint {
            suggestion: "maw exec <name> -- git log|diff|show",
            note: "git introspection runs inside the workspace via `maw exec`",
        }),
        ["clone", ..] => Some(VerbHint {
            suggestion: "git clone <url> && cd <repo> && maw init",
            note: "clone is plain git; then bootstrap maw on top with `maw init`",
        }),
        ["fetch", ..] => Some(VerbHint {
            suggestion: "maw pull --manifold  # (or plain `git fetch` inside a workspace)",
            note: "use `maw pull --manifold` for Manifold state; plain git fetch for refs",
        }),

        // Bones reach-for: agents sometimes try `bn` as a top-level verb.
        ["bn" | "bone" | "bones", ..] => Some(VerbHint {
            suggestion: "maw exec default -- bn <args>",
            note: "bones always runs through `maw exec default`",
        }),

        // `maw merge ...` is for quarantine, not workspace merge — high-friction overload.
        ["merge", target, ..] if !is_quarantine_subverb(target) => Some(VerbHint {
            suggestion: "maw ws merge <ws> --into default --check",
            note: "top-level `merge` is for post-merge quarantines; use `ws merge`",
        }),

        _ => None,
    }
}

/// Top-level `maw merge` has real subcommands (`list`, `promote`,
/// `abandon`). Don't hijack those with a "did you mean ws merge" hint.
fn is_quarantine_subverb(token: &str) -> bool {
    matches!(
        token,
        "list" | "promote" | "abandon" | "help" | "--help" | "-h"
    )
}

/// Tail-line appended to every rejected-verb error so even unknown
/// tokens learn the universal discoverability backstops.
pub const UNIVERSAL_DISCOVERY_TAIL: &str =
    "  tip: `maw --help` lists all verbs; `maw crib <agent>` emits a cheat sheet.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_new_routes_to_create() {
        let h = classify_rejected_verb(&["ws", "new"]).expect("classified");
        assert!(h.suggestion.contains("maw ws create"));
        assert!(h.note.contains("create"));
    }

    #[test]
    fn workspace_new_alias_routes_to_create() {
        let h = classify_rejected_verb(&["workspace", "new"]).expect("classified");
        assert!(h.suggestion.contains("maw ws create"));
    }

    #[test]
    fn delete_rm_remove_all_route_to_destroy() {
        for verb in ["delete", "rm", "remove"] {
            let h = classify_rejected_verb(&["ws", verb]).expect("classified");
            assert!(
                h.suggestion.contains("maw ws destroy"),
                "{verb} should route to destroy, got {:?}",
                h.suggestion,
            );
        }
    }

    #[test]
    fn ws_rebase_routes_to_sync() {
        let h = classify_rejected_verb(&["ws", "rebase"]).expect("classified");
        assert!(h.suggestion.contains("maw ws sync"));
    }

    #[test]
    fn checkout_routes_to_ws_create() {
        let h = classify_rejected_verb(&["checkout", "main"]).expect("classified");
        assert!(h.suggestion.contains("maw ws create"));
    }

    #[test]
    fn branch_routes_to_ws_create() {
        let h = classify_rejected_verb(&["branch"]).expect("classified");
        assert!(h.suggestion.contains("maw ws create"));
    }

    #[test]
    fn stash_routes_to_maw_exec_git() {
        let h = classify_rejected_verb(&["stash"]).expect("classified");
        assert!(h.suggestion.contains("maw exec"));
        assert!(h.suggestion.contains("git stash"));
    }

    #[test]
    fn commit_routes_to_maw_exec_git_commit() {
        let h = classify_rejected_verb(&["commit"]).expect("classified");
        assert!(h.suggestion.contains("maw exec"));
        assert!(h.suggestion.contains("git commit"));
    }

    #[test]
    fn rebase_top_level_routes_to_ws_sync() {
        let h = classify_rejected_verb(&["rebase"]).expect("classified");
        assert!(h.suggestion.contains("maw ws sync"));
    }

    #[test]
    fn bn_top_level_routes_to_maw_exec_default() {
        let h = classify_rejected_verb(&["bn", "list"]).expect("classified");
        assert!(h.suggestion.contains("maw exec default"));
        assert!(h.suggestion.contains("bn"));
    }

    #[test]
    fn top_level_merge_with_workspace_name_routes_to_ws_merge() {
        // `maw merge alice` looks to an agent like "merge workspace alice"
        // but is actually a typo for `ws merge alice`. The quarantine
        // verbs are `merge list/promote/abandon`; those must NOT trigger.
        let h = classify_rejected_verb(&["merge", "alice"]).expect("classified");
        assert!(h.suggestion.contains("maw ws merge"));
    }

    #[test]
    fn quarantine_subverbs_are_not_hijacked() {
        // These are real `maw merge ...` subcommands; the classifier
        // must NOT shadow them with a "did you mean ws merge" hint.
        for sub in ["list", "promote", "abandon", "help"] {
            assert!(
                classify_rejected_verb(&["merge", sub]).is_none(),
                "merge {sub} is a real quarantine subverb, not a typo",
            );
        }
    }

    #[test]
    fn genuinely_unknown_verb_returns_none() {
        assert!(classify_rejected_verb(&["xyzzy"]).is_none());
        assert!(classify_rejected_verb(&["ws", "xyzzy"]).is_none());
        assert!(classify_rejected_verb(&[]).is_none());
    }

    #[test]
    fn hint_renders_as_single_line() {
        let h = classify_rejected_verb(&["ws", "new"]).expect("ws new should classify");
        let line = h.render();
        assert!(line.starts_with("  did you mean: "));
        assert!(
            !line.contains('\n'),
            "hint render must be a single line (printed under clap's error)",
        );
    }

    #[test]
    fn universal_tail_is_short_and_actionable() {
        assert!(UNIVERSAL_DISCOVERY_TAIL.contains("maw --help"));
        assert!(UNIVERSAL_DISCOVERY_TAIL.contains("maw crib"));
        assert!(
            !UNIVERSAL_DISCOVERY_TAIL.contains('\n'),
            "tail must be a single line",
        );
    }

    /// The two-prong contract: every classified hint includes both a
    /// concrete suggestion AND a one-line note (so the agent gets
    /// "what to do" + "why" in the same screen-line of context).
    #[test]
    fn every_hint_has_suggestion_and_note() {
        for args in [
            &["ws", "new"][..],
            &["ws", "delete"][..],
            &["checkout"][..],
            &["branch"][..],
            &["stash"][..],
            &["commit"][..],
            &["rebase"][..],
            &["bn", "list"][..],
            &["merge", "alice"][..],
            &["clone"][..],
            &["fetch"][..],
        ] {
            let h = classify_rejected_verb(args).expect("should be classified");
            assert!(!h.suggestion.is_empty());
            assert!(!h.note.is_empty());
        }
    }
}
