//! bn-hcbc8: an invalid (unparseable) manifold `config.toml` must never make
//! maw silently fall back to defaults.
//!
//! Before the fix, several call sites did
//! `load_manifold_config(root).unwrap_or_default()`, so a typo in
//! `config.toml` silently re-enabled or disabled safety settings
//! (`merge.auto_absorb_ff`, `merge.auto_rebase_siblings`, the post-rebase
//! sanity check, …). In particular `maw ws merge` FF-absorbed trunk commits
//! into the epoch with the *default* `auto_absorb_ff = true` before a later
//! stage noticed the bad config and refused — an epoch mutation the user's
//! (typo'd) config meant to forbid.
//!
//! Now:
//! - state-mutating commands (`ws merge`, `ws sync`) refuse up front, before
//!   any lock or epoch mutation, naming the file and the parse error;
//! - read-only commands (`ws list`, `ws status`) keep working but warn loudly,
//!   naming the file and the parse error.

mod manifold_common;

use manifold_common::{TestRepo, git_ok};

/// A config whose intent is "never FF-absorb" but with a typo in the key.
const TYPO_CONFIG: &str = "[repo]\nbranch = \"main\"\n\n[merge]\nauto_absorb_f = false\n";

fn write_config(repo: &TestRepo, toml: &str) {
    std::fs::write(repo.root().join(".manifold").join("config.toml"), toml)
        .expect("write .manifold/config.toml");
}

/// Advance `refs/heads/main` past the epoch with a direct commit on an
/// unrelated path (the FF-absorb shape), leaving the epoch ref untouched.
fn push_branch_ahead(repo: &TestRepo) -> String {
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

fn assert_actionable(stderr: &str) {
    assert!(
        stderr.contains("config.toml") && stderr.contains("auto_absorb_f"),
        "message must name the file and the parse error:\n{stderr}"
    );
    assert!(
        stderr.contains("To fix"),
        "message must say how to fix it:\n{stderr}"
    );
}

#[test]
fn merge_refuses_invalid_config_before_any_epoch_mutation() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice\n");
    push_branch_ahead(&repo);

    write_config(&repo, TYPO_CONFIG);
    let epoch_before = repo.current_epoch();

    let stderr = repo.maw_fails(&["ws", "merge", "alice", "--message", "merge alice"]);
    assert_actionable(&stderr);
    assert!(
        !stderr.contains("Absorbed"),
        "merge must not FF-absorb with default settings on an invalid config:\n{stderr}"
    );
    assert_eq!(
        repo.current_epoch(),
        epoch_before,
        "epoch must not move when the config is invalid"
    );
    assert!(
        repo.workspace_exists("alice"),
        "source workspace must be left alone"
    );
}

#[test]
fn sync_refuses_invalid_config() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    let head_before = repo.workspace_head("alice");

    write_config(&repo, TYPO_CONFIG);
    let stderr = repo.maw_fails(&["ws", "sync", "alice"]);
    assert_actionable(&stderr);
    assert_eq!(repo.workspace_head("alice"), head_before);
}

#[test]
fn read_only_commands_warn_but_work_on_invalid_config() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    write_config(&repo, TYPO_CONFIG);

    for args in [&["ws", "list"][..], &["ws", "status"][..]] {
        let out = repo.maw_raw(args);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            out.status.success(),
            "`maw {}` must still work on an invalid config:\n{stderr}",
            args.join(" ")
        );
        assert!(
            stderr.contains("WARNING")
                && stderr.contains("config.toml")
                && stderr.contains("auto_absorb_f"),
            "`maw {}` must warn loudly with the file and the parse error:\n{stderr}",
            args.join(" ")
        );
        assert_eq!(
            stderr.matches("invalid maw config").count(),
            1,
            "warning should be printed once per process:\n{stderr}"
        );
    }
}
