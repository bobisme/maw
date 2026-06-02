//! bn-1lj2: merge integration-event oplog must be unified in the consolidated
//! `.maw/manifold/` layout — never split across a stray root `.manifold/`.
//!
//! The bug: `emit_integration_started` (and several other call sites) hardcoded
//! `root.join(".manifold")` instead of resolving the layout-aware manifold dir.
//! On a consolidated `.maw/` repo this wrote `integration_started` to a stray
//! root `.manifold/events/merge.jsonl` while `integration_completed` went to the
//! canonical `.maw/manifold/events/merge.jsonl` — splitting the oplog and
//! littering the repo root with an untracked, non-gitignored `.manifold/`.
//!
//! These tests drive the real `maw` binary through a greenfield consolidated
//! init + a real `maw ws merge --destroy`, then assert:
//!   1. NO root `.manifold/` directory is created.
//!   2. The canonical `.maw/manifold/events/merge.jsonl` carries BOTH the
//!      `integration_started` AND `integration_completed` events.
//!   3. `maw merge events` (which reads the same dir) surfaces both events.

mod manifold_common;

use manifold_common::maw_bin;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Run the `maw` binary in `dir` and panic with stdout+stderr on failure.
fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(maw_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to execute maw");
    assert!(
        out.status.success(),
        "maw {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Greenfield-init a fresh consolidated `.maw/` repo in a temp dir.
///
/// `maw init` only produces the consolidated `.maw/` layout for a *greenfield*
/// init (an empty directory with no pre-existing git repo); brownfield init on
/// an existing git repo preserves/produces the legacy v2 layout. So we init
/// into a bare empty dir and let `maw init` create the git repo too.
fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();

    let out = Command::new(maw_bin())
        .args(["init"])
        .current_dir(root)
        // Make commits created by init / merges deterministic in CI.
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .expect("failed to execute maw init");
    assert!(
        out.status.success(),
        "maw init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Sanity: the consolidated marker must exist and there must be NO root
    // `.manifold/` yet (a stray one would defeat the whole test).
    assert!(
        root.join(".maw").join("manifold").is_dir(),
        "expected consolidated layout (.maw/manifold/) after init; got:\n{}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        !root.join(".manifold").exists(),
        "init unexpectedly created a root .manifold/ dir"
    );

    dir
}

#[test]
fn merge_oplog_is_unified_in_consolidated_layout() {
    let dir = init_consolidated();
    let root = dir.path();

    // Create a workspace, make a tracked change, commit it.
    maw_ok(root, &["ws", "create", "feat", "--from", "main"]);
    maw_ok(
        root,
        &[
            "exec",
            "feat",
            "--",
            "sh",
            "-c",
            "echo feature > feat.txt && git add -A && git commit -m 'add feat'",
        ],
    );

    // Real merge with destroy — the path that emits integration_started (real)
    // followed by integration_completed.
    maw_ok(
        root,
        &[
            "ws",
            "merge",
            "feat",
            "--into",
            "default",
            "--destroy",
            "--message",
            "feat: add feat",
        ],
    );

    // (1) No stray root `.manifold/` must exist.
    assert!(
        !root.join(".manifold").exists(),
        "merge created a stray root .manifold/ dir — oplog mis-routed (bn-1lj2 regression)"
    );

    // (2) The canonical oplog must carry BOTH events.
    let oplog = root
        .join(".maw")
        .join("manifold")
        .join("events")
        .join("merge.jsonl");
    assert!(
        oplog.exists(),
        "canonical oplog missing at {}",
        oplog.display()
    );
    let body = std::fs::read_to_string(&oplog).expect("read canonical oplog");
    assert!(
        body.contains("\"integration_started\""),
        "canonical oplog missing integration_started:\n{body}"
    );
    assert!(
        body.contains("\"integration_completed\""),
        "canonical oplog missing integration_completed:\n{body}"
    );

    // (3) `maw merge events` reads the consolidated dir and surfaces both.
    let events = maw_ok(root, &["merge", "events"]);
    assert!(
        events.contains("integration_started") && events.contains("integration_completed"),
        "`maw merge events` did not surface a unified oplog:\n{events}"
    );
}
