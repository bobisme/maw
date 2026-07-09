//! Integration tests for bn-1aey: warn when the invoking shell's cwd is
//! inside a workspace that is about to be (or was just) destroyed.
//!
//! From bn-38nz item 6: a `merge --destroy` run from a shell cwd inside the
//! destroyed workspace left every chained command afterwards failing with
//! opaque errors (`ensure_repo_root` -> `std::env::current_dir` "Could not
//! determine current directory"). Both destroy paths now print a one-line
//! stderr note when the process cwd was inside the workspace being
//! destroyed, without blocking the destroy itself.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod manifold_common;

use manifold_common::{TestRepo, maw_bin};
use std::process::Command;

const NOTE_FRAGMENT: &str = "was just destroyed";

/// Standalone `maw ws destroy --force` run with cwd inside the workspace
/// prints the note after destroying successfully.
#[test]
fn standalone_destroy_from_inside_prints_cwd_note() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "cwd-victim"]);
    repo.add_file("cwd-victim", "wip.txt", "scratch\n");

    let ws_path = repo.workspace_path("cwd-victim");

    let out = Command::new(maw_bin())
        .args(["ws", "destroy", "cwd-victim", "--force"])
        .current_dir(&ws_path)
        .output()
        .expect("failed to execute maw");

    assert!(
        out.status.success(),
        "destroy --force from inside the workspace should still succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(NOTE_FRAGMENT) && stderr.contains("cwd-victim"),
        "expected a cwd note naming the destroyed workspace; got stderr:\n{stderr}"
    );

    assert!(
        !repo.workspace_exists("cwd-victim"),
        "workspace should be destroyed despite cwd having been inside it"
    );
}

/// Standalone destroy of a *clean* workspace (no `--force` needed) run with
/// cwd inside it also prints the note.
#[test]
fn standalone_destroy_clean_from_inside_prints_cwd_note() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "cwd-victim-clean"]);
    let ws_path = repo.workspace_path("cwd-victim-clean");

    let out = Command::new(maw_bin())
        .args(["ws", "destroy", "cwd-victim-clean"])
        .current_dir(&ws_path)
        .output()
        .expect("failed to execute maw");

    assert!(
        out.status.success(),
        "destroy of a clean workspace from inside it should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(NOTE_FRAGMENT) && stderr.contains("cwd-victim-clean"),
        "expected a cwd note naming the destroyed workspace; got stderr:\n{stderr}"
    );
}

/// Destroying a workspace from OUTSIDE it (the normal case, e.g. from the
/// repo root) must NOT print the cwd note.
#[test]
fn standalone_destroy_from_outside_does_not_print_cwd_note() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "cwd-bystander"]);

    let stdout = repo.maw_ok(&["ws", "destroy", "cwd-bystander"]);
    // maw_ok only returns stdout; use maw_raw-equivalent to inspect stderr too.
    assert!(
        !stdout.contains(NOTE_FRAGMENT),
        "note should not appear on stdout for an unrelated cwd"
    );

    let out = repo.maw_raw_exact(&["ws", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(NOTE_FRAGMENT),
        "unrelated command output should never carry the destroy-cwd note"
    );
}

/// `maw ws merge <name> --destroy` run with cwd inside the source workspace
/// prints the note for that workspace after the merge+destroy completes.
#[test]
fn merge_destroy_from_inside_source_workspace_prints_cwd_note() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# Project\n")]);
    repo.maw_ok(&["ws", "create", "merge-cwd-victim"]);
    repo.add_file("merge-cwd-victim", "feature.txt", "feature\n");

    let ws_path = repo.workspace_path("merge-cwd-victim");

    let out = Command::new(maw_bin())
        .args([
            "ws",
            "merge",
            "merge-cwd-victim",
            "--into",
            "default",
            "--destroy",
            "--message",
            "test merge",
        ])
        .current_dir(&ws_path)
        .output()
        .expect("failed to execute maw");

    assert!(
        out.status.success(),
        "merge --destroy from inside the source workspace should still succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(NOTE_FRAGMENT) && stderr.contains("merge-cwd-victim"),
        "expected a cwd note naming the destroyed source workspace; got stderr:\n{stderr}"
    );

    assert!(
        !repo.workspace_exists("merge-cwd-victim"),
        "source workspace should be destroyed"
    );
    assert_eq!(
        repo.read_file("default", "feature.txt").as_deref(),
        Some("feature\n"),
        "merged content should land in default"
    );
}
