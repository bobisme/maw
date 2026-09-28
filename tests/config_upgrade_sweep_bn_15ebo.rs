//! bn-15ebo (pre.17 sweep B): config fail-closed policy on recovery and
//! upgrade paths.
//!
//! bn-qi5br made mutating commands refuse on an invalid `.maw.toml`. That is
//! right when the configured branch / default workspace decides what the
//! command does, and wrong when it does not:
//!
//! 1. `maw ws recover <ws> --restore-file <path>` in the consolidated layout
//!    writes into the repo root whatever `.maw.toml` says — the config is
//!    irrelevant, so a typo there must not block recovery.
//! 2. `maw ws recover <ws> --to <new>` creates a workspace (whose base epoch
//!    resync reads the configured branch), so it still refuses — but the
//!    refusal must name the command the user ran, not `maw ws create`, and
//!    must come before the audit log claims a restore happened.
//! 3. `maw migrate` must refuse an invalid `.maw.toml` before Phase B/C
//!    touch anything, not halfway through (Phase D).
//! 4. A pre-bn-2dyz consolidated repo with `[workspace]` in the deprecated
//!    `.maw/config.toml` gets a deprecation warning that says the key IS
//!    read — the `.maw.toml` unknown-key check must not also claim it is
//!    "ignored".

mod manifold_common;

use manifold_common::{TestRepo, maw_bin};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const BROKEN_MAW_TOML: &str = "[repo]\nbranch = trunk\n";

fn maw(dir: &Path, args: &[&str]) -> Output {
    Command::new(maw_bin())
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .env_remove("MAW_LAYOUT")
        .output()
        .expect("failed to execute maw")
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = maw(dir, args);
    assert!(
        out.status.success(),
        "maw {} failed:\n{}",
        args.join(" "),
        combined(&out)
    );
    combined(&out)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Consolidated repo with a destroyed workspace `w` whose snapshot holds
/// `new.txt`, then a broken `.maw.toml`.
fn consolidated_with_destroyed_snapshot() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);
    maw_ok(root, &["init"]);
    maw_ok(root, &["ws", "create", "--from", "main", "w"]);
    std::fs::write(root.join(".maw/workspaces/w/new.txt"), "hello\n").expect("write");
    maw_ok(root, &["ws", "destroy", "w", "--force"]);
    std::fs::write(root.join(".maw.toml"), BROKEN_MAW_TOML).expect("write .maw.toml");
    dir
}

#[test]
fn restore_file_ignores_invalid_maw_toml_in_consolidated_layout() {
    let dir = consolidated_with_destroyed_snapshot();
    let root = dir.path();
    let out = maw(root, &["ws", "recover", "w", "--restore-file", "new.txt"]);
    assert!(
        out.status.success(),
        "--restore-file must not refuse over a config that cannot change \
         where it writes (the repo root):\n{}",
        combined(&out)
    );
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).expect("restored"),
        "hello\n"
    );
}

#[test]
fn recover_to_refusal_names_recover_and_logs_no_restore() {
    let dir = consolidated_with_destroyed_snapshot();
    let root = dir.path();
    let out = maw(root, &["ws", "recover", "w", "--to", "w2"]);
    let text = combined(&out);
    assert!(!out.status.success(), "--to must refuse:\n{text}");
    assert!(
        text.contains("`maw ws recover --to` refuses"),
        "refusal must name the command the user ran:\n{text}"
    );
    assert!(
        !text.contains("`maw ws create` refuses"),
        "refusal must not blame a command the user did not run:\n{text}"
    );
    assert!(
        !text.contains("\"event_type\":\"restore\""),
        "no restore audit event for a refused restore:\n{text}"
    );
    assert!(
        text.contains("--show"),
        "refusal must point at the recovery that still works:\n{text}"
    );
    assert!(!root.join(".maw/workspaces/w2").exists());
}

#[test]
fn migrate_refuses_invalid_maw_toml_before_touching_anything() {
    let repo = TestRepo::new();
    repo.create_workspace("alpha");
    repo.add_file("alpha", "src/lib.rs", "fn one() {}\n");
    std::fs::write(repo.root().join(".maw.toml"), BROKEN_MAW_TOML).expect("write");

    let out = Command::new(maw_bin())
        .arg("migrate")
        .current_dir(repo.root())
        .output()
        .expect("maw migrate");
    let text = combined(&out);
    assert!(!out.status.success(), "migrate must refuse:\n{text}");
    assert!(text.contains(".maw.toml"), "{text}");
    assert!(
        repo.root().join("ws").join("alpha").is_dir(),
        "Phase C must not have relocated workspaces:\n{text}"
    );
    assert!(
        !repo.root().join(".maw").exists(),
        "no consolidated layout may be started:\n{text}"
    );
    let recovery = repo.git(&["for-each-ref", "refs/manifold/recovery/"]);
    assert!(
        recovery.trim().is_empty(),
        "Phase B must not have pinned snapshots:\n{recovery}"
    );
}

#[test]
fn legacy_workspace_section_is_not_reported_as_ignored() {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();
    maw_ok(root, &["init"]);
    let cfg = root.join(".maw/config.toml");
    let mut body = std::fs::read_to_string(&cfg).expect("bootstrap config");
    body.push_str("\n[workspace]\nbackend = \"git-worktree\"\n");
    std::fs::write(&cfg, body).expect("write");

    let text = maw_ok(root, &["ws", "status"]);
    assert!(
        text.contains("deprecated location"),
        "the legacy key is adopted with a deprecation warning:\n{text}"
    );
    assert!(
        !text.contains("unknown key `workspace`"),
        "a key maw reads must not also be reported as ignored:\n{text}"
    );

    // A genuinely unknown section there still warns.
    let mut body = std::fs::read_to_string(&cfg).expect("bootstrap config");
    body.push_str("\n[bogus]\nx = 1\n");
    std::fs::write(&cfg, body).expect("write");
    let text = maw_ok(root, &["ws", "status"]);
    assert!(text.contains("unknown key `bogus`"), "{text}");
}
