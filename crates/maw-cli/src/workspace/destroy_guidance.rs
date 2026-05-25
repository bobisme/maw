// TODO(bn-c6l3 follow-up): wire-up deferred. This module ships as a building
// block but is NOT invoked from `create.rs` yet because a merge conflict with
// bn-29fi (destroy-prevention cues) on the refusal-text path required choosing
// one side; `--keep epoch` preserved bn-29fi's cues. A follow-up bone should
// integrate both: take the structured DestroyRefusal from this module AND
// preserve bn-29fi's `--dry-run` / `merge --destroy` recommendation lines.
// The 3 integration tests in tests/destroy_gate.rs are `#[ignore]`d pending
// that wire-up; the 11 unit tests in this module's own `mod tests` are green.

#![allow(dead_code)] // wire-up deferred — see TODO above

//! Self-describing refusal payload for `maw ws destroy` (SG4 / bn-c6l3).
//!
//! Targets the `ws_destroy_refused`
//! ([`maw_bench_metrics::attribution::MawVerbAttribution::WsDestroyRefused`])
//! friction cluster. Pre-fix, the refusal message asked the agent to
//! either inspect the workspace (extra turn) or re-issue `--force`
//! (which the message framed as the natural next step, encouraging
//! data-loss-shaped behavior). The agent had to *decide* between two
//! paths with no first-class signal about which one preserved work.
//!
//! Post-fix, the refusal is a structured payload built around the
//! [`LifecycleState`] safe-cleanup vocabulary
//! ([`super::lifecycle::LifecycleState`], bn-221b). The renderer:
//!
//! - Names the workspace's lifecycle state explicitly
//!   (`committed-unintegrated`, `dirty-uncommitted`, etc.) so the
//!   agent doesn't have to re-derive it from free-text.
//! - Leads with the **safe** next command (merge, or commit-then-
//!   merge) — the path that integrates work without invoking
//!   `--force`.
//! - Includes the `--force` command with an explicit Prime-Invariant
//!   reassurance: "snapshot is captured and recoverable via
//!   `maw ws recover <name>`" — so the agent doesn't need a second
//!   turn (or a second tool call) to be confident `--force` is safe.
//! - Emits a `--format json` payload so machine consumers can branch
//!   on `lifecycle_state` / `recommended_action` directly instead of
//!   regex-matching the human text.
//!
//! # Invariant
//!
//! The refusal **still refuses** — this module never weakens the
//! Prime-Invariant guard. It only changes the *shape* of the output
//! so the agent's first follow-up turn is the right one.

use serde::Serialize;

use super::lifecycle::LifecycleState;

/// Structured payload for a `maw ws destroy` refusal.
///
/// Built once at refusal time, then rendered as either human text
/// (default) or JSON (`--format json`). The JSON form is the
/// contract for machine consumers; the field names match this struct
/// 1:1 via serde.
#[derive(Debug, Clone, Serialize)]
pub struct DestroyRefusal {
    /// Workspace that was refused.
    pub workspace: String,
    /// Lifecycle state of the workspace, from the safe-cleanup
    /// vocabulary. Serializes as a kebab-case slug (e.g.
    /// `committed-unintegrated`, `dirty-uncommitted`).
    pub lifecycle_state: LifecycleState,
    /// Number of patches the workspace has against its base epoch
    /// (committed + uncommitted, mirrors the legacy `touched_count`).
    pub touched_count: usize,
    /// Number of commits on the workspace HEAD ahead of its base
    /// epoch. `0` when the workspace's work is only uncommitted.
    pub commits_ahead: u32,
    /// Number of dirty (uncommitted) files in the working tree.
    pub dirty_count: usize,
    /// The safe next-action command, written so the agent can paste
    /// it without re-deriving any names. This is what the renderer
    /// surfaces *first*.
    pub recommended_action: String,
    /// Optional short label for the `recommended_action`
    /// (`merge-and-destroy`, `commit-then-merge`, `inspect`). Lets
    /// JSON consumers branch on the action without parsing the
    /// command string.
    pub recommended_action_kind: RecommendedAction,
    /// The `--force` escape hatch, separated from the recommended
    /// action so the renderer can label it "alternative" instead of
    /// "next step".
    pub force_command: String,
    /// Reassurance text the renderer prints alongside `force_command`
    /// so the agent doesn't need a second turn to verify the
    /// Prime-Invariant guarantee. Static; pulled from the Prime
    /// Invariant docstring in `ws/default/AGENTS.md`.
    pub force_safety_note: String,
    /// Inspection command for agents that want to see *what* is in
    /// the workspace before choosing the safe-vs-force path. Surfaced
    /// as a third "if you want to look first" line.
    pub inspect_command: String,
}

/// Named kinds of recommended action, so machine consumers don't
/// have to parse `recommended_action` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecommendedAction {
    /// Workspace has committed work — agent should merge it in
    /// (and pass `--destroy` to atomically clean up afterwards).
    MergeAndDestroy,
    /// Workspace has only uncommitted edits — agent should commit
    /// inside the workspace first, then merge.
    CommitThenMerge,
    /// State is uncommon (e.g. conflicted, missing) — fall back to
    /// inspection rather than prescribing a wrong action.
    Inspect,
}

impl RecommendedAction {
    /// Stable slug used in JSON output. Matches the serde rename so
    /// JSON and tests agree.
    ///
    /// Currently consumed only by tests, but kept on the public
    /// surface (mirrors [`super::lifecycle::LifecycleState::slug`])
    /// so future text renderers can use it without re-deriving the
    /// mapping.
    #[allow(dead_code)]
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::MergeAndDestroy => "merge-and-destroy",
            Self::CommitThenMerge => "commit-then-merge",
            Self::Inspect => "inspect",
        }
    }
}

impl DestroyRefusal {
    /// Build a refusal payload from the destroy-time signals.
    ///
    /// `dirty_count` is from [`maw_core::backend::WorkspaceStatus::dirty_count`];
    /// `touched_count` is the union of dirty + committed-against-base
    /// (already computed by `destroy()` via `compute_patchset`);
    /// `commits_ahead` is from [`maw_core::model::types::WorkspaceInfo::commits_ahead`].
    ///
    /// The lifecycle state is derived locally rather than via
    /// [`LifecycleState::classify`] because the destroy path does NOT
    /// consult the conflict-marker / staleness signals — it's already
    /// past those gates by the time it reaches the unmerged-changes
    /// check. The mapping is:
    /// - `commits_ahead > 0`           → `CommittedUnintegrated`
    /// - `dirty_count > 0`             → `DirtyUncommitted`
    /// - otherwise (touched > 0 only via deleted-and-readded etc.) →
    ///   `DirtyUncommitted` (conservative; the agent's next step is
    ///   still "look at what changed before destroying").
    #[must_use]
    pub fn new(
        workspace: &str,
        touched_count: usize,
        commits_ahead: u32,
        dirty_count: usize,
    ) -> Self {
        let lifecycle_state = if commits_ahead > 0 {
            LifecycleState::CommittedUnintegrated
        } else {
            LifecycleState::DirtyUncommitted
        };

        let (recommended_action, recommended_action_kind) = match lifecycle_state {
            LifecycleState::CommittedUnintegrated => (
                format!("maw ws merge {workspace} --into default --destroy"),
                RecommendedAction::MergeAndDestroy,
            ),
            LifecycleState::DirtyUncommitted => (
                format!(
                    "maw exec {workspace} -- git add -A && \
                     maw exec {workspace} -- git commit -m \"wip: <message>\" && \
                     maw ws merge {workspace} --into default --destroy"
                ),
                RecommendedAction::CommitThenMerge,
            ),
            // Defensive fallthrough — the classifier above only
            // returns the two states above for this call site, but if
            // a future signal is added the inspect path is the safe
            // default.
            _ => (
                format!("maw ws touched {workspace} --format json"),
                RecommendedAction::Inspect,
            ),
        };

        Self {
            workspace: workspace.to_string(),
            lifecycle_state,
            touched_count,
            commits_ahead,
            dirty_count,
            recommended_action,
            recommended_action_kind,
            force_command: format!("maw ws destroy {workspace} --force"),
            force_safety_note: format!(
                "--force captures a recovery snapshot first (Prime Invariant: \
                 no committed work is ever lost). Recover with: \
                 maw ws recover {workspace}"
            ),
            inspect_command: format!("maw ws touched {workspace} --format json"),
        }
    }

    /// Render the refusal as human-readable text. Lines are indented
    /// two spaces to match the existing `bail!` output convention in
    /// `workspace/create.rs` so agents that already pattern-match the
    /// old layout still parse cleanly.
    #[must_use]
    pub fn render_text(&self) -> String {
        let Self {
            workspace,
            touched_count,
            recommended_action,
            force_command,
            force_safety_note,
            inspect_command,
            ..
        } = self;
        let state_slug = self.lifecycle_state.slug();
        // Lead line names the state from the safe-cleanup vocabulary.
        // Sub-lines are ordered: SAFE first, FORCE second, INSPECT
        // third — the agent's "first attempt is the right one" only
        // if the safe path is the most prominent.
        format!(
            "Workspace '{workspace}' has {touched_count} unmerged change(s) \
             (state: {state_slug}). Refusing destroy to avoid data loss.\n  \
             Recommended: {recommended_action}\n  \
             Or force-destroy: {force_command}\n    \
             ({force_safety_note})\n  \
             Inspect first: {inspect_command}"
        )
    }

    /// Render the refusal as pretty-printed JSON suitable for
    /// `--format json` consumers. Keys mirror the struct's serde
    /// names, so the contract is "what this struct serializes to".
    ///
    /// # Errors
    /// Returns the underlying `serde_json` error if serialization
    /// fails (should not happen in practice — all fields are owned
    /// strings / primitive newtypes).
    pub fn render_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_unintegrated_leads_with_merge_and_destroy() {
        let r = DestroyRefusal::new("alice", 3, 2, 0);
        assert_eq!(r.lifecycle_state, LifecycleState::CommittedUnintegrated);
        assert_eq!(r.recommended_action_kind, RecommendedAction::MergeAndDestroy);
        assert!(r.recommended_action.starts_with("maw ws merge alice"));
        assert!(r.recommended_action.contains("--into default"));
        assert!(r.recommended_action.contains("--destroy"));
    }

    #[test]
    fn dirty_uncommitted_leads_with_commit_then_merge() {
        let r = DestroyRefusal::new("bob", 2, 0, 2);
        assert_eq!(r.lifecycle_state, LifecycleState::DirtyUncommitted);
        assert_eq!(r.recommended_action_kind, RecommendedAction::CommitThenMerge);
        assert!(r.recommended_action.contains("git add -A"));
        assert!(r.recommended_action.contains("git commit"));
        assert!(r.recommended_action.contains("maw ws merge bob"));
    }

    #[test]
    fn committed_takes_priority_over_dirty_when_both_present() {
        // Mixed case: agent has both committed and uncommitted work.
        // The lifecycle vocabulary priority (committed > dirty) holds
        // because committed work is the more dangerous thing to lose.
        let r = DestroyRefusal::new("carol", 5, 1, 3);
        assert_eq!(r.lifecycle_state, LifecycleState::CommittedUnintegrated);
        assert_eq!(r.recommended_action_kind, RecommendedAction::MergeAndDestroy);
    }

    #[test]
    fn force_command_includes_workspace_name() {
        let r = DestroyRefusal::new("dave", 1, 0, 1);
        assert_eq!(r.force_command, "maw ws destroy dave --force");
    }

    #[test]
    fn force_safety_note_cites_prime_invariant_and_recover_cmd() {
        let r = DestroyRefusal::new("eve", 1, 1, 0);
        assert!(
            r.force_safety_note.contains("Prime Invariant"),
            "force_safety_note must reassure with Prime Invariant; got: {}",
            r.force_safety_note
        );
        assert!(
            r.force_safety_note.contains("recovery snapshot"),
            "force_safety_note must mention recovery snapshot; got: {}",
            r.force_safety_note
        );
        assert!(
            r.force_safety_note.contains("maw ws recover eve"),
            "force_safety_note must include exact recovery command; got: {}",
            r.force_safety_note
        );
    }

    #[test]
    fn render_text_lists_safe_path_before_force_path() {
        let r = DestroyRefusal::new("frank", 1, 1, 0);
        let text = r.render_text();
        let safe_pos = text.find("Recommended:").expect("Recommended: line present");
        let force_pos = text
            .find("Or force-destroy:")
            .expect("force-destroy line present");
        assert!(
            safe_pos < force_pos,
            "Safe path must appear before force path in the rendered text:\n{text}"
        );
    }

    #[test]
    fn render_text_uses_safe_cleanup_vocabulary_slug() {
        let r = DestroyRefusal::new("greg", 1, 1, 0);
        let text = r.render_text();
        assert!(
            text.contains("committed-unintegrated"),
            "Refusal text must name the safe-cleanup vocabulary state; got:\n{text}"
        );

        let r2 = DestroyRefusal::new("greg", 1, 0, 1);
        let text2 = r2.render_text();
        assert!(
            text2.contains("dirty-uncommitted"),
            "Refusal text must name dirty-uncommitted for uncommitted work; got:\n{text2}"
        );
    }

    #[test]
    fn render_text_includes_force_safety_note_inline() {
        let r = DestroyRefusal::new("harry", 1, 1, 0);
        let text = r.render_text();
        // Agent must see the Prime-Invariant reassurance in the same
        // refusal turn — otherwise it would need a second turn (or a
        // separate `maw ws recover --help`) to confirm `--force` is
        // safe, which is the friction the bone exists to eliminate.
        assert!(
            text.contains("Prime Invariant"),
            "render_text must inline the Prime-Invariant reassurance; got:\n{text}"
        );
    }

    #[test]
    fn render_json_round_trips_and_contains_lifecycle_state_slug() {
        let r = DestroyRefusal::new("ivy", 4, 2, 0);
        let json = r.render_json().expect("json renders");
        // Parseable.
        let v: serde_json::Value = serde_json::from_str(&json).expect("parses");
        // Carries the lifecycle slug as kebab-case (from
        // LifecycleState's serde rename_all).
        assert_eq!(
            v["lifecycle_state"].as_str(),
            Some("committed-unintegrated"),
            "JSON must serialize lifecycle_state as kebab-case slug; got: {json}"
        );
        // Carries the recommended-action kind as a parseable slug.
        assert_eq!(
            v["recommended_action_kind"].as_str(),
            Some("merge-and-destroy"),
            "JSON must serialize recommended_action_kind; got: {json}"
        );
        // Carries the counts.
        assert_eq!(v["touched_count"].as_u64(), Some(4));
        assert_eq!(v["commits_ahead"].as_u64(), Some(2));
        assert_eq!(v["dirty_count"].as_u64(), Some(0));
        // Workspace name preserved.
        assert_eq!(v["workspace"].as_str(), Some("ivy"));
    }

    #[test]
    fn render_json_dirty_uncommitted_carries_commit_then_merge_kind() {
        let r = DestroyRefusal::new("jane", 1, 0, 1);
        let json = r.render_json().expect("json renders");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(
            v["lifecycle_state"].as_str(),
            Some("dirty-uncommitted")
        );
        assert_eq!(
            v["recommended_action_kind"].as_str(),
            Some("commit-then-merge")
        );
    }

    #[test]
    fn recommended_action_slug_matches_serde() {
        // Belt-and-braces: the manual slug() and the serde rename
        // must stay in sync. If a future variant breaks this, the
        // JSON contract breaks silently — guard it.
        for kind in [
            RecommendedAction::MergeAndDestroy,
            RecommendedAction::CommitThenMerge,
            RecommendedAction::Inspect,
        ] {
            let slug = kind.slug();
            let json = serde_json::to_string(&kind).expect("serializes");
            assert_eq!(json, format!("\"{slug}\""));
        }
    }
}
