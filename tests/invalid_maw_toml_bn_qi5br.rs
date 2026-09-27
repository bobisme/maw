//! bn-qi5br: an invalid (unparseable) `.maw.toml` must never make maw silently
//! fall back to defaults.
//!
//! Before the fix, ~15 call sites did `MawConfig::load(root).unwrap_or_default()`
//! (or `.ok()` / `map_or_else(|_| "main")`), so a typo in `.maw.toml` silently
//! meant: branch `main`, default workspace `default`, default epoch-lock wait
//! settings, invariant audit on, no post-sync hooks. `maw epoch sync` would
//! re-point the epoch at `main` even when the user configured another branch,
//! and `maw ws create` would base a workspace on the wrong branch.
//!
//! Same policy as bn-hcbc8 (manifold `config.toml`):
//! - state-mutating commands refuse, naming the file, the parse error (with its
//!   line) and how to fix it;
//! - read-only commands keep working but warn once.

mod manifold_common;

use manifold_common::{TestRepo, git_ok};

/// The user meant `branch = "trunk"` but forgot the quotes: a TOML parse error
/// on line 2.
const BROKEN_MAW_TOML: &str = "[repo]\nbranch = trunk\n";

fn write_maw_toml(repo: &TestRepo, toml: &str) {
    std::fs::write(repo.root().join(".maw.toml"), toml).expect("write .maw.toml");
}

/// Advance `refs/heads/main` past the epoch with a direct commit, leaving the
/// epoch ref untouched (so `epoch sync` has something to do).
fn push_main_ahead(repo: &TestRepo) -> String {
    let ws_default = repo.default_workspace();
    let epoch_before = repo.current_epoch();
    std::fs::write(ws_default.join("docs.md"), "upstream\n").expect("write");
    git_ok(&ws_default, &["add", "-A"]);
    git_ok(&ws_default, &["commit", "-m", "docs: upstream"]);
    let new_oid = git_ok(&ws_default, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &new_oid]);
    git_ok(&ws_default, &["reset", "--hard", &epoch_before]);
    new_oid
}

/// The refusal/warning must name the file, the parse error with its line, and
/// the fix.
fn assert_actionable(stderr: &str) {
    assert!(
        stderr.contains(".maw.toml"),
        "message must name the file:\n{stderr}"
    );
    assert!(
        stderr.contains("line 2"),
        "message must carry the parse error with its line:\n{stderr}"
    );
    assert!(
        stderr.contains("To fix"),
        "message must say how to fix it:\n{stderr}"
    );
}

#[test]
fn epoch_sync_refuses_invalid_maw_toml() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    push_main_ahead(&repo);
    write_maw_toml(&repo, BROKEN_MAW_TOML);
    let epoch_before = repo.current_epoch();

    let stderr = repo.maw_fails(&["epoch", "sync"]);
    assert_actionable(&stderr);
    assert!(
        stderr.contains("refuses"),
        "mutating command must say it refuses:\n{stderr}"
    );
    assert_eq!(
        repo.current_epoch(),
        epoch_before,
        "epoch sync must not move the epoch to the default branch `main` \
         when .maw.toml is invalid"
    );
}

#[test]
fn ws_create_refuses_invalid_maw_toml() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    write_maw_toml(&repo, BROKEN_MAW_TOML);

    let stderr = repo.maw_fails(&["ws", "create", "bob"]);
    assert_actionable(&stderr);
    assert!(
        !repo.workspace_exists("bob"),
        "ws create must not create a workspace on default settings"
    );
}

#[test]
fn ws_merge_refuses_invalid_maw_toml_with_line_and_fix() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice\n");
    write_maw_toml(&repo, BROKEN_MAW_TOML);
    let epoch_before = repo.current_epoch();

    let stderr = repo.maw_fails(&["ws", "merge", "alice", "--message", "merge alice"]);
    assert_actionable(&stderr);
    assert_eq!(repo.current_epoch(), epoch_before);
    assert!(repo.workspace_exists("alice"));
}

#[test]
fn ws_sync_refuses_invalid_maw_toml() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    push_main_ahead(&repo);
    repo.maw_ok(&["epoch", "sync"]);
    let head_before = repo.workspace_head("alice");

    write_maw_toml(&repo, BROKEN_MAW_TOML);
    let stderr = repo.maw_fails(&["ws", "sync", "alice"]);
    assert_actionable(&stderr);
    assert_eq!(repo.workspace_head("alice"), head_before);
}

#[test]
fn ws_status_warns_once_on_invalid_maw_toml() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    write_maw_toml(&repo, BROKEN_MAW_TOML);

    let out = repo.maw_raw(&["ws", "status"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "`maw ws status` must still work on an invalid .maw.toml:\n{stderr}"
    );
    assert!(stderr.contains("WARNING"), "must warn loudly:\n{stderr}");
    assert_actionable(&stderr);
    assert_eq!(
        stderr.matches("invalid maw config").count(),
        1,
        "warning should be printed once per process:\n{stderr}"
    );
}

#[test]
fn doctor_reports_invalid_maw_toml() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    write_maw_toml(&repo, BROKEN_MAW_TOML);

    let out = repo.maw_raw(&["doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let all = format!("{stdout}\n{stderr}");
    assert!(
        !all.contains("could not check (config unreadable)"),
        "doctor must not hide the config error behind an ok check:\n{all}"
    );
    assert_actionable(&all);
}
