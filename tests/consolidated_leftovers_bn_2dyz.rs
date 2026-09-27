//! bn-2dyz: consolidated-layout leftovers.
//!
//! 1. Manifold config path split: merge read `.maw/manifold/config.toml`
//!    but backend selection, FF-absorb (`reconcile_epoch_with_branch`) and
//!    sibling auto-rebase read the bootstrap `.maw/config.toml`, so e.g.
//!    `merge.auto_absorb_ff = false` in the canonical file was silently
//!    ignored by FF-absorb. All readers now resolve the canonical
//!    `<manifold_dir>/config.toml`, with `.maw/config.toml` as a per-key
//!    fallback that warns it is deprecated.
//! 2. Merge quarantines (`merge-quarantine-<id>`) were listed as "ready to
//!    merge" with a `maw ws merge ... --destroy` suggestion, and a manual
//!    `maw ws sync` (or `maw exec`'s auto-sync) would rebase the candidate.
//!    They are now listed as quarantines with promote/abandon commands, and
//!    `ws sync` / `ws merge` refuse them.
//!
//! These tests drive the real `maw` binary on a greenfield consolidated repo.

mod manifold_common;

use manifold_common::maw_bin;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

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

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn init_consolidated() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    maw_ok(dir.path(), &["init"]);
    assert!(dir.path().join(".maw").join("manifold").is_dir());
    dir
}

fn commit_in_ws(root: &Path, name: &str, script: &str) {
    maw_ok(root, &["ws", "create", name, "--from", "main"]);
    maw_ok(
        root,
        &[
            "exec",
            name,
            "--",
            "sh",
            "-c",
            &format!("{script} && git add -A && git commit -qm {name}"),
        ],
    );
}

fn merge(root: &Path, name: &str) -> Output {
    maw(
        root,
        &[
            "ws",
            "merge",
            name,
            "--into",
            "default",
            "--message",
            &format!("feat: {name}"),
        ],
    )
}

/// Direct trunk commit: advances `main` past the epoch, so the next merge
/// hits the FF-absorb reconcile path.
fn commit_on_trunk(root: &Path) {
    std::fs::write(root.join("trunk.txt"), "t\n").expect("write");
    git(root, &["add", "trunk.txt"]);
    git(root, &["commit", "-qm", "trunk"]);
}

/// Item 1 regression: `auto_absorb_ff = false` in the CANONICAL
/// consolidated manifold config must reach FF-absorb. Before the fix the
/// reconcile read `.maw/config.toml` (defaults) and silently absorbed.
#[test]
fn canonical_manifold_config_auto_absorb_ff_false_is_honored_by_merge() {
    let dir = init_consolidated();
    let root = dir.path();
    std::fs::write(
        root.join(".maw").join("manifold").join("config.toml"),
        "[repo]\nbranch = \"main\"\n\n[merge]\nauto_absorb_ff = false\n",
    )
    .expect("write canonical config");

    commit_on_trunk(root);
    commit_in_ws(root, "alice", "echo a > alice.txt");
    let out = merge(root, "alice");
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "merge must refuse (FF-absorb disabled), got success:\n{text}"
    );
    assert!(
        text.contains("diverged from the current epoch"),
        "expected the legacy diverged error:\n{text}"
    );
    assert!(!text.contains("Absorbed"), "must not FF-absorb:\n{text}");
}

/// Control: with the canonical config at defaults the same shape absorbs,
/// so the test above is not passing for an unrelated reason.
#[test]
fn default_config_ff_absorbs() {
    let dir = init_consolidated();
    let root = dir.path();
    commit_on_trunk(root);
    commit_in_ws(root, "alice", "echo a > alice.txt");
    let out = merge(root, "alice");
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("Absorbed"), "expected an FF-absorb:\n{text}");
    assert!(
        !text.contains("deprecated"),
        "no deprecation noise:\n{text}"
    );
}

/// Back-compat: a setting that only exists in the deprecated bootstrap
/// location is still honored — by EVERY reader, now including merge's own
/// config — and the user is told to move it.
#[test]
fn legacy_bootstrap_setting_is_honored_with_deprecation_warning() {
    let dir = init_consolidated();
    let root = dir.path();
    std::fs::write(
        root.join(".maw").join("config.toml"),
        "[repo]\nbranch = \"main\"\n\n[merge]\nauto_absorb_ff = false\n",
    )
    .expect("write legacy config");

    commit_on_trunk(root);
    commit_in_ws(root, "alice", "echo a > alice.txt");
    let out = merge(root, "alice");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("diverged from the current epoch"), "{text}");
    assert!(
        text.contains("deprecated location") && text.contains("manifold/config.toml"),
        "expected a deprecation warning pointing at the canonical file:\n{text}"
    );
    assert_eq!(
        text.matches("deprecated location").count(),
        1,
        "warning must be printed once per process:\n{text}"
    );
}

/// Merge a workspace whose content fails validation, producing a
/// quarantine. Returns `(merge_id, quarantine workspace path)`.
fn make_quarantine(root: &Path) -> (String, std::path::PathBuf) {
    std::fs::write(
        root.join(".maw").join("manifold").join("config.toml"),
        "[merge.validation]\ncommand = \"test ! -f BROKEN\"\non_failure = \"quarantine\"\n",
    )
    .expect("write manifold config");
    commit_in_ws(root, "worker", "echo hi > hello.txt && echo x > BROKEN");
    let out = merge(root, "worker");
    let text = combined(&out);
    assert!(text.contains("Quarantine workspace created"), "{text}");
    let qdir = root.join(".maw").join("manifold").join("quarantine");
    let mut ids: Vec<String> = std::fs::read_dir(&qdir)
        .expect("read quarantine dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    let id = ids.pop().expect("id");
    let ws = root
        .join(".maw")
        .join("workspaces")
        .join(format!("merge-quarantine-{id}"));
    (id, ws)
}

/// Advance the epoch past the quarantine's recorded epoch (validation off).
fn advance_epoch(root: &Path) {
    std::fs::write(
        root.join(".maw").join("manifold").join("config.toml"),
        "[repo]\nbranch = \"main\"\n",
    )
    .expect("reset config");
    commit_in_ws(root, "other", "echo o > other.txt");
    let out = merge(root, "other");
    assert!(out.status.success(), "{}", combined(&out));
}

#[test]
fn ws_list_shows_quarantine_with_promote_abandon_not_merge_ready() {
    let dir = init_consolidated();
    let root = dir.path();
    let (id, _qws) = make_quarantine(root);
    let qname = format!("merge-quarantine-{id}");

    let text = maw_ok(root, &["ws", "list"]);
    let qline = text
        .lines()
        .find(|l| l.starts_with(&qname))
        .unwrap_or_else(|| panic!("quarantine missing from ws list:\n{text}"));
    assert!(!qline.contains("ready to merge"), "{text}");
    assert!(
        !text.contains(&format!("maw ws merge {qname}")),
        "ws list must not suggest merging a quarantine:\n{text}"
    );
    assert!(
        text.contains(&format!("maw merge promote {id}"))
            && text.contains(&format!("maw merge abandon {id}")),
        "{text}"
    );

    let json = maw_ok(root, &["ws", "list", "--format", "json"]);
    assert!(
        json.contains(&format!("\"fix_command\": \"maw merge promote {id}\"")),
        "{json}"
    );
}

#[test]
fn ws_sync_and_merge_refuse_quarantine_and_leave_it_untouched() {
    let dir = init_consolidated();
    let root = dir.path();
    let (id, qws) = make_quarantine(root);
    let qname = format!("merge-quarantine-{id}");
    let head_before = git(&qws, &["rev-parse", "HEAD"]);
    advance_epoch(root);

    let out = maw(root, &["ws", "sync", &qname]);
    let text = combined(&out);
    assert!(!out.status.success(), "ws sync must refuse:\n{text}");
    assert!(
        text.contains("merge quarantine") && text.contains(&format!("maw merge promote {id}")),
        "{text}"
    );
    assert_eq!(git(&qws, &["rev-parse", "HEAD"]), head_before, "{text}");

    // `--all` skips it (and does not fail because of it).
    let out = maw(root, &["ws", "sync", "--all"]);
    assert!(out.status.success(), "{}", combined(&out));
    assert_eq!(git(&qws, &["rev-parse", "HEAD"]), head_before);

    // `maw exec` auto-sync must not rebase the candidate either.
    maw_ok(root, &["exec", &qname, "--", "true"]);
    assert_eq!(git(&qws, &["rev-parse", "HEAD"]), head_before);

    for extra in [&[][..], &["--check"][..]] {
        let mut args = vec!["ws", "merge", qname.as_str(), "--into", "default"];
        args.extend_from_slice(extra);
        if extra.is_empty() {
            args.extend_from_slice(&["--message", "nope"]);
        }
        let out = maw(root, &args);
        let text = combined(&out);
        assert!(
            !out.status.success(),
            "ws merge {extra:?} must refuse:\n{text}"
        );
        assert!(
            text.contains("merge quarantine") && text.contains(&format!("maw merge abandon {id}")),
            "{text}"
        );
    }
    assert_eq!(git(&qws, &["rev-parse", "HEAD"]), head_before);
    assert!(
        qws.is_dir(),
        "refused merge must not destroy the quarantine"
    );
}
