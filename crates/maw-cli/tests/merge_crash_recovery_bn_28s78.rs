//! bn-28s78: gaps in the bn-1fcox crashed-merge recovery.
//!
//! 1. Dirty-target replay: the live merge passed the merge engine's
//!    `resolved_paths` to the target checkout's snapshot replay; recovery
//!    (which has no such list in the journal) passed `&[]`. The only paths
//!    that list ever added to the replay's 3-way set were paths whose
//!    committed bytes the merge left UNCHANGED — e.g. a both-sides-edited
//!    path resolved by an `ours` driver back to the epoch's bytes. For
//!    those, the live merge reported a spurious "local-vs-merge conflict"
//!    (a user deletion vs "merged" = base bytes; or any edit under
//!    `merge=binary`) and then the bn-1xmk fidelity check restored the
//!    user's version anyway, while a crash-recovered merge reported a clean
//!    replay. Same bytes, different story. The replay now depends only on
//!    (anchor, merged commit, snapshot), so both agree and neither lies.
//!
//! 2. Post-merge hooks: the live merge runs `[hooks] post_merge` as its very
//!    last step (after clearing the journal), so a journal on disk proves
//!    they never ran. Recovery deliberately does not run them late; it says
//!    so and lists them.
//!
//! The crash tests need `--features failpoints` (`just sg1-faithful-test`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn git_quiet(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn maw_raw(dir: &Path, args: &[&str], fp: Option<&str>) -> Output {
    let mut cmd = Command::new(MAW);
    cmd.current_dir(dir).args(args).env_remove("MAW_FP");
    if let Some(spec) = fp {
        cmd.env("MAW_FP", spec);
    }
    cmd.output().expect("run maw")
}

fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

fn maw(dir: &Path, args: &[&str]) -> String {
    let out = maw_raw(dir, args, None);
    assert!(
        out.status.success(),
        "maw {args:?} failed:\n{}",
        combined(&out)
    );
    combined(&out)
}

fn ws_path(root: &Path, name: &str) -> PathBuf {
    root.join(".maw/workspaces").join(name)
}

#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn journal(root: &Path) -> PathBuf {
    root.join(".maw/manifold/merge-state.json")
}

fn init_repo(root: &Path, files: &[(&str, &str)]) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    for (path, content) in files {
        std::fs::write(root.join(path), content).expect("write seed file");
    }
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
}

fn commit_config_and_sync(root: &Path) {
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw config"]);
        maw(root, &["epoch", "sync"]);
    }
}

fn ws_commit(root: &Path, name: &str, path: &str, content: &str) {
    maw(root, &["ws", "create", name, "--from", "main"]);
    std::fs::write(ws_path(root, name).join(path), content).expect("write ws file");
    maw(root, &["exec", name, "--", "git", "add", "-A"]);
    maw(
        root,
        &[
            "exec",
            name,
            "--",
            "git",
            "commit",
            "-m",
            &format!("{name} work"),
        ],
    );
}

/// How the target's uncommitted edit to `f.txt` looks.
#[derive(Clone, Copy, Debug)]
enum Dirty {
    /// The user deleted `f.txt` (no driver involved).
    Delete,
    /// The user edited `f.txt`, which `.gitattributes` marks `merge=binary`.
    EditBinary,
}

/// `a` and `b` both edit `f.txt`; an `ours` merge driver resolves it back to
/// the epoch's bytes, so `f.txt` is a merge-resolved path whose committed
/// content the merge does NOT change. The target (repo root) then carries an
/// uncommitted edit to it.
fn setup_driver_kept_path(root: &Path, dirty: Dirty) {
    let mut seed = vec![("f.txt", "base\n")];
    if matches!(dirty, Dirty::EditBinary) {
        seed.push((".gitattributes", "f.txt merge=binary\n"));
    }
    init_repo(root, &seed);
    let cfg = root.join(".maw/manifold/config.toml");
    let mut text = std::fs::read_to_string(&cfg).expect("read manifold config");
    text.push_str("\n[[merge.drivers]]\nmatch = \"f.txt\"\nkind = \"ours\"\n");
    std::fs::write(&cfg, text).expect("write manifold config");
    commit_config_and_sync(root);
    ws_commit(root, "a", "f.txt", "from a\n");
    ws_commit(root, "b", "f.txt", "from b\n");
    match dirty {
        Dirty::Delete => std::fs::remove_file(root.join("f.txt")).expect("rm f.txt"),
        Dirty::EditBinary => {
            std::fs::write(root.join("f.txt"), "user edit\n").expect("edit f.txt");
        }
    }
}

fn merge_ab(root: &Path, fp: Option<&str>) -> Output {
    maw_raw(
        root,
        &[
            "ws",
            "merge",
            "a",
            "b",
            "--into",
            "default",
            "--destroy",
            "--message",
            "merge a b",
        ],
        fp,
    )
}

/// Everything observable about the target after the merge.
#[derive(Debug, PartialEq, Eq)]
struct TargetState {
    committed_f: String,
    status: String,
    f_on_disk: Option<String>,
    false_conflict_reported: bool,
    fidelity_alarm: bool,
}

fn target_state(root: &Path, output: &str) -> TargetState {
    TargetState {
        committed_f: git(root, &["show", "HEAD:f.txt"]),
        status: git(root, &["status", "--porcelain", "--untracked-files=no"]),
        f_on_disk: std::fs::read_to_string(root.join("f.txt")).ok(),
        false_conflict_reported: output.contains("local-vs-merge conflict"),
        fidelity_alarm: output.contains("replay did not reproduce"),
    }
}

fn expected_state(dirty: Dirty) -> TargetState {
    match dirty {
        Dirty::Delete => TargetState {
            committed_f: "base".to_owned(),
            status: " D f.txt".to_owned(),
            f_on_disk: None,
            false_conflict_reported: false,
            fidelity_alarm: false,
        },
        Dirty::EditBinary => TargetState {
            committed_f: "base".to_owned(),
            status: " M f.txt".to_owned(),
            f_on_disk: Some("user edit\n".to_owned()),
            false_conflict_reported: false,
            fidelity_alarm: false,
        },
    }
}

fn check_live_merge(dirty: Dirty) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_driver_kept_path(root, dirty);
    let out = merge_ab(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_eq!(
        target_state(root, &text),
        expected_state(dirty),
        "live merge ({dirty:?}): the merge left f.txt's committed bytes \
         unchanged, so the user's uncommitted version must be replayed \
         cleanly — no conflict report, no fidelity alarm:\n{text}"
    );
}

/// The live merge: a path the merge resolved without changing its bytes is
/// not a merge-vs-local conflict.
#[test]
fn live_merge_replays_edit_to_driver_kept_path_without_false_conflict() {
    check_live_merge(Dirty::Delete);
    check_live_merge(Dirty::EditBinary);
}

/// Crash after the CAS and before the target checkout, then
/// `maw ws merge --recover`: the target ends exactly as after the
/// uninterrupted merge (bytes, status, and what was reported).
#[cfg(feature = "failpoints")]
#[test]
fn recovered_merge_replays_target_exactly_like_live_merge() {
    for dirty in [Dirty::Delete, Dirty::EditBinary] {
        let live_dir = tempfile::tempdir().expect("tempdir");
        let live_root = live_dir.path();
        setup_driver_kept_path(live_root, dirty);
        let out = merge_ab(live_root, None);
        let live_text = combined(&out);
        assert!(out.status.success(), "live merge failed:\n{live_text}");
        let live = target_state(live_root, &live_text);

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        setup_driver_kept_path(root, dirty);
        let crash = merge_ab(root, Some("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT=abort"));
        assert!(
            !crash.status.success(),
            "the injected abort must kill the merge:\n{}",
            combined(&crash)
        );
        assert!(journal(root).exists(), "the crash must leave the journal");
        let text = maw(root, &["ws", "merge", "--recover"]);
        assert!(text.contains("already landed"), "{text}");
        assert!(!journal(root).exists());
        assert!(!ws_path(root, "a").exists() && !ws_path(root, "b").exists());
        let recovered = target_state(root, &text);

        assert_eq!(
            recovered, live,
            "{dirty:?}: crash-recovered target differs from the live merge\n\
             live output:\n{live_text}\nrecover output:\n{text}"
        );
        assert_eq!(recovered, expected_state(dirty), "{dirty:?}:\n{text}");
    }
}

/// Post-merge hooks: never run by recovery (a journal proves the merge never
/// reached them), always reported with the commands to run; the live merge
/// still runs them.
#[cfg(feature = "failpoints")]
#[test]
fn recovery_reports_skipped_post_merge_hooks_and_does_not_run_them() {
    let hook_dir = tempfile::tempdir().expect("tempdir");
    let hook_log = hook_dir.path().join("hook.log");
    let hook_cmd = format!("echo ran >> '{}'", hook_log.display());

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("f.txt", "base\n")]);
    std::fs::write(
        root.join(".maw.toml"),
        format!(
            "[hooks]\npost_merge = [\"{}\"]\n",
            hook_cmd.replace('"', "\\\"")
        ),
    )
    .expect("write .maw.toml");
    commit_config_and_sync(root);
    ws_commit(root, "a", "a.txt", "a\n");
    ws_commit(root, "b", "b.txt", "b\n");

    let merge = |ws: &str, fp: Option<&str>| {
        maw_raw(
            root,
            &[
                "ws",
                "merge",
                ws,
                "--into",
                "default",
                "--destroy",
                "--message",
                &format!("merge {ws}"),
            ],
            fp,
        )
    };

    // Explicit --recover (text).
    let crash = merge("a", Some("FP_COMMIT_AFTER_EPOCH_CAS=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("already landed"), "{text}");
    assert!(
        text.contains("Post-merge hooks were NOT run") && text.contains(&hook_cmd),
        "recovery must say the post-merge hooks were skipped and list them:\n{text}"
    );
    assert!(
        !hook_log.exists(),
        "recovery must not run post-merge hooks late:\n{text}"
    );

    // Auto-recovery at the next merge's start (stderr) + JSON via --recover
    // are covered by the same Finalized report; check JSON explicitly.
    let crash = merge("b", Some("FP_COMMIT_AFTER_EPOCH_CAS=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    let out = maw_raw(
        root,
        &["ws", "merge", "--recover", "--format", "json"],
        None,
    );
    assert!(out.status.success(), "{}", combined(&out));
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("recover --format json is one JSON document");
    assert_eq!(json["action"], "finalized", "{json}");
    assert_eq!(
        json["post_merge_hooks_skipped"],
        serde_json::json!([hook_cmd]),
        "{json}"
    );
    assert!(
        !hook_log.exists(),
        "JSON recovery must not run hooks either"
    );

    // The live merge still runs them (the hook config is live).
    ws_commit(root, "c", "c.txt", "c\n");
    let out = merge("c", None);
    assert!(out.status.success(), "{}", combined(&out));
    assert_eq!(
        std::fs::read_to_string(&hook_log).expect("live merge ran the post-merge hook"),
        "ran\n"
    );
}
