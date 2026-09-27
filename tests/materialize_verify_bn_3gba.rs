//! Post-materialization `worktree == HEAD` verification (bn-3gba).
//!
//! Origin: field report 3 (bn-p3m9). Two freshly created workspaces
//! materialized with HEAD and index correct at the trunk tip but a working tree
//! carrying byte-exact stale-epoch blobs. The mechanism is still unreproduced;
//! `workspace::materialize_verify` is the mechanism-independent defense that
//! turns the class from silent corruption into a caught, repaired and recorded
//! event.
//!
//! This file owns the **no-false-positives** half of the contract, and runs in
//! the default `just check` lane: a clean `ws create` / `ws sync` says nothing
//! and writes no artifact, and every path where dirty state is preserved ON
//! PURPOSE (a dirty sibling during a merge auto-rebase; a dirty sibling during
//! an FF-absorb) keeps its uncommitted bytes byte-for-byte. A repair that eats
//! real work would be far worse than the bug it guards, so the skip-scoping is
//! pinned here where every developer runs it.
//!
//! The **catch** half — fault-injecting the bn-p3m9 signature via
//! `MAW_FP=FP_CREATE_AFTER_MATERIALIZE=corrupt:<abs-path>` and asserting the
//! detect / preserve / repair / record chain — lives in
//! `crates/maw-cli/tests/materialize_verify_bn_3gba.rs`. It needs a
//! `--features failpoints` binary (the `fp!()` site compiles to nothing
//! otherwise), so it rides `just sg1-materialize-check` →
//! `.github/workflows/dst-faithful.yml` rather than `just check`.

mod manifold_common;

use std::path::{Path, PathBuf};

use manifold_common::{TestRepo, git_ok};

/// Substring of the loud stderr WARNING the verifier prints on divergence.
const WARNING_MARKER: &str = "did not materialize cleanly";

/// Uncommitted bytes a deliberately-dirty sibling must still hold after the
/// operation under test. If a repair ever eats these, the defense has become
/// the bug.
const PRECIOUS: &str = "PRECIOUS uncommitted agent work\n";

/// The materialize-repair artifact directory for a workspace.
fn artifact_dir(root: &Path, ws: &str) -> PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws)
        .join("materialize-repair")
}

/// Every `*.json` materialize-repair artifact written for `ws`.
fn artifact_files(root: &Path, ws: &str) -> Vec<PathBuf> {
    let dir = artifact_dir(root, ws);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    out.sort();
    out
}

/// Recovery refs pinned for `ws` by the post-materialization verifier.
fn materialize_recovery_refs(root: &Path, ws: &str) -> Vec<String> {
    git_ok(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/manifold/recovery/{ws}"),
        ],
    )
    .lines()
    .map(str::trim)
    .filter(|l| l.contains("/materialize-"))
    .map(ToOwned::to_owned)
    .collect()
}

// ---------------------------------------------------------------------------
// No false positives on the clean paths
// ---------------------------------------------------------------------------

/// A clean `ws create` and a clean fast-forward `ws sync` must be SILENT: no
/// WARNING, no artifact, no recovery ref. The verifier runs on every one of
/// these, so a false positive here would fire constantly.
#[test]
fn clean_create_and_ff_sync_are_silent() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn hello() {}\n")]);

    let out = repo.maw_raw(&["ws", "create", "alpha"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "create failed: {stderr}{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !stderr.contains(WARNING_MARKER),
        "a clean create must not warn:\n{stderr}"
    );

    // Advance the epoch so `alpha` (which has no local commits) takes the
    // fast-forward sync path — the second instrumented call site.
    repo.add_file("default", "src/main.rs", "fn main() {}\n");
    repo.advance_epoch("chore: advance epoch");

    let out = repo.maw_raw(&["ws", "sync", "alpha"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "sync failed: {stderr}{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !stderr.contains(WARNING_MARKER),
        "a clean fast-forward sync must not warn:\n{stderr}"
    );

    // The workspace really is clean at HEAD (the property being asserted).
    assert!(
        repo.dirty_files("alpha").is_empty(),
        "alpha should be clean at HEAD after a fast-forward sync"
    );

    assert!(
        artifact_files(repo.root(), "alpha").is_empty(),
        "no divergence happened, so no artifact may be written"
    );
    assert!(
        materialize_recovery_refs(repo.root(), "alpha").is_empty(),
        "no divergence happened, so no recovery ref may be pinned"
    );
}

/// **The skip-logic proof.** A sibling workspace holding INTENTIONAL
/// uncommitted edits must come out of a merge's sibling auto-rebase with those
/// bytes untouched. The auto-rebase classifies it `SkippedDirty`, which is NOT
/// a clean-at-commit contract, so the verifier must never run — a repair here
/// would silently revert an agent's live work.
#[test]
fn dirty_sibling_edits_survive_merge_auto_rebase_untouched() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("shared.txt", "epoch content\n"),
        ("keeper.txt", "keeper base\n"),
    ]);

    repo.maw_ok(&["ws", "create", "keeper"]);
    repo.maw_ok(&["ws", "create", "mover"]);

    // `keeper` has UNCOMMITTED work — the state that must survive verbatim.
    repo.modify_file("keeper", "keeper.txt", PRECIOUS);

    // `mover` commits and merges, advancing the epoch and triggering the
    // post-merge sibling auto-rebase over `keeper`.
    repo.add_file("mover", "shared.txt", "mover content\n");
    repo.git_in_workspace("mover", &["add", "-A"]);
    repo.git_in_workspace("mover", &["commit", "-m", "feat: mover work"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "mover",
        "--into",
        "default",
        "--message",
        "merge mover",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "merge failed:\n{stderr}{}",
        String::from_utf8_lossy(&out.stdout)
    );

    assert_eq!(
        repo.read_file("keeper", "keeper.txt").as_deref(),
        Some(PRECIOUS),
        "the dirty sibling's uncommitted bytes must survive the merge untouched"
    );
    assert!(
        !stderr.contains(WARNING_MARKER),
        "a deliberately-dirty sibling is not divergence:\n{stderr}"
    );
    assert!(
        artifact_files(repo.root(), "keeper").is_empty(),
        "no repair may be recorded for a deliberately-dirty sibling"
    );
    assert!(
        materialize_recovery_refs(repo.root(), "keeper").is_empty(),
        "no pre-repair snapshot may be taken for a deliberately-dirty sibling"
    );
}

/// The same skip logic on the FF-absorb path: a commit lands directly on trunk
/// (outside maw), so the next merge must absorb the drift; a sibling holding
/// uncommitted edits on a DISJOINT path is classified
/// `SiblingPlan::FastForward` (dirty, re-checked under its lock) and keeps them on purpose.
#[test]
fn dirty_sibling_edits_survive_ff_absorb_untouched() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("trunk-only.txt", "trunk base\n"),
        ("keeper.txt", "keeper base\n"),
        ("shared.txt", "shared base\n"),
    ]);

    repo.maw_ok(&["ws", "create", "keeper"]);
    repo.maw_ok(&["ws", "create", "mover"]);

    repo.modify_file("keeper", "keeper.txt", PRECIOUS);

    // A commit made directly on `main`, outside maw: `refs/heads/main` is now
    // ahead of `refs/manifold/epoch/current`, arming the FF-absorb path. The
    // touched path is disjoint from `keeper`'s dirty path, so the FF-absorb
    // safety predicate lets the absorb through.
    let default_ws = repo.default_workspace();
    std::fs::write(default_ws.join("trunk-only.txt"), "trunk updated\n").expect("write trunk file");
    git_ok(&default_ws, &["add", "-A"]);
    git_ok(
        &default_ws,
        &["commit", "-m", "chore: out-of-maw trunk commit"],
    );
    let trunk_tip = git_ok(&default_ws, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &trunk_tip]);

    repo.add_file("mover", "shared.txt", "mover content\n");
    repo.git_in_workspace("mover", &["add", "-A"]);
    repo.git_in_workspace("mover", &["commit", "-m", "feat: mover work"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "mover",
        "--into",
        "default",
        "--message",
        "merge mover",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "merge (with FF absorb) failed:\n{stderr}{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("Absorbed"),
        "expected the FF-absorb path to run:\n{stderr}"
    );

    assert_eq!(
        repo.read_file("keeper", "keeper.txt").as_deref(),
        Some(PRECIOUS),
        "FF-absorb must preserve a dirty sibling's uncommitted bytes"
    );
    assert!(
        artifact_files(repo.root(), "keeper").is_empty(),
        "no repair may be recorded for a dirty FF-absorb sibling"
    );
}
