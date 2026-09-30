//! bn-asqh7: `undo` is a reserved workspace name.
//!
//! A workspace's destroy pins live under `refs/manifold/recovery/<ws>/`, and
//! `maw undo` pins undone merge results under `refs/manifold/recovery/undo/`.
//! A workspace named `undo` would share that namespace, so:
//!
//! 1. `maw ws create undo` and `maw ws recover <x> --to undo` refuse, with a
//!    message that names another name to use.
//! 2. Repos that already have an `undo` workspace (live, or destroyed with
//!    destroy records) keep working: gc errs toward keeping every pin in the
//!    shared namespace, and `maw doctor` warns with a rename hint.
//!
//! Drives the built `maw` binary (rebuild `maw-cli` before trusting a run).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MAW: &str = env!("CARGO_BIN_EXE_maw");
const OLD_LEAF: &str = "2020-01-01T00-00-00.000000000Z";

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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn maw(dir: &Path, args: &[&str]) -> Output {
    Command::new(MAW)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run maw")
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
        "maw {args:?} failed:\n{}",
        combined(&out)
    );
    combined(&out)
}

fn setup_repo(dir: &Path) -> PathBuf {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("file.txt"), "base\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "init"]);
    maw_ok(dir, &["init"]);
    dir.to_path_buf()
}

/// Create `name`, commit in it, merge it into default with `--destroy`.
fn merge_destroy(root: &Path, name: &str) {
    maw_ok(root, &["ws", "create", name, "--from", "main"]);
    let script = format!("printf '{name}\\n' >> file.txt && git add -A && git commit -m '{name}'");
    maw_ok(root, &["exec", name, "--", "sh", "-c", &script]);
    maw_ok(
        root,
        &[
            "ws",
            "merge",
            name,
            "--into",
            "default",
            "--destroy",
            "--message",
            "feat: merge",
        ],
    );
}

/// A pre-existing `undo` workspace, as an older maw (which accepted the name)
/// would have left it: a detached worktree at the current epoch.
fn legacy_undo_workspace(root: &Path) -> PathBuf {
    git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            ".maw/workspaces/undo",
            "refs/manifold/epoch/current",
        ],
    );
    let path = root.join(".maw/workspaces/undo");
    let list = maw_ok(root, &["ws", "list"]);
    assert!(list.contains("undo"), "legacy undo ws is listed:\n{list}");
    path
}

/// `(ref, oid)` of every pin under `refs/manifold/recovery/undo/`.
fn undo_ns_pins(root: &Path) -> Vec<(String, String)> {
    git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/manifold/recovery/undo/",
        ],
    )
    .lines()
    .filter_map(|l| l.split_once(' '))
    .map(|(r, o)| (r.to_owned(), o.to_owned()))
    .collect()
}

/// Move pin `name` to an old timestamp so it is past any `--older-than`.
fn backdate(root: &Path, name: &str, oid: &str, leaf: &str) -> String {
    let old = format!("refs/manifold/recovery/undo/{leaf}");
    git(root, &["update-ref", &old, oid]);
    git(root, &["update-ref", "-d", name]);
    old
}

fn ref_exists(root: &Path, name: &str) -> bool {
    Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "--quiet", name])
        .output()
        .expect("rev-parse")
        .status
        .success()
}

fn doctor_json(root: &Path) -> serde_json::Value {
    let out = maw(root, &["doctor", "--format", "json"]);
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("doctor json: {e}\n{}", combined(&out));
    })
}

fn reserved_check(root: &Path) -> serde_json::Value {
    let doc = doctor_json(root);
    doc["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "reserved workspace names")
        .cloned()
        .unwrap_or_else(|| panic!("no reserved-names check:\n{doc:#}"))
}

#[test]
fn create_undo_is_refused_with_a_suggestion() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());

    let out = maw(&root, &["ws", "create", "undo", "--from", "main"]);
    let text = combined(&out);
    assert!(!out.status.success(), "create undo must fail:\n{text}");
    assert!(text.contains("reserved"), "{text}");
    assert!(text.contains("maw undo"), "names why:\n{text}");
    assert!(text.contains("undo-work"), "suggests another name:\n{text}");
    assert!(
        !root.join(".maw/workspaces/undo").exists(),
        "nothing is created"
    );

    // Near names are fine.
    maw_ok(&root, &["ws", "create", "undo-work", "--from", "main"]);
}

#[test]
fn recover_to_undo_is_refused_before_anything_is_created() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    maw_ok(&root, &["ws", "create", "alice", "--from", "main"]);
    std::fs::write(root.join(".maw/workspaces/alice/draft.txt"), "draft\n").expect("write");
    maw_ok(&root, &["ws", "destroy", "alice", "--force"]);

    let out = maw(&root, &["ws", "recover", "alice", "--to", "undo"]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "recover --to undo must fail:\n{text}"
    );
    assert!(text.contains("reserved"), "{text}");
    assert!(text.contains("undo-work"), "suggests another name:\n{text}");
    assert!(!root.join(".maw/workspaces/undo").exists());
    assert!(undo_ns_pins(&root).is_empty(), "no pin written");

    let pin = git(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/manifold/recovery/alice/",
        ],
    );
    let out = maw(&root, &["ws", "recover", "--ref", &pin, "--to", "undo"]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "recover --ref --to undo must fail:\n{text}"
    );
    assert!(text.contains("undo-work"), "{text}");
    assert!(!root.join(".maw/workspaces/undo").exists());
}

/// Existing repo with a live `undo` workspace: every pin in the shared
/// namespace is treated as the live workspace's and kept by a routine sweep,
/// even an old `maw undo` pin no redo references (which is swept when no
/// `undo` workspace exists, see `gc_undo_pins_bn_43x5k`). Erring toward keeping
/// is the intended behaviour; doctor points at the rename.
#[test]
fn live_legacy_undo_workspace_keeps_shared_pins_and_doctor_warns() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    assert_eq!(reserved_check(&root)["status"], "ok");

    merge_destroy(&root, "alice");
    maw_ok(&root, &["undo"]);
    maw_ok(&root, &["undo"]); // redo: no undo pin is needed any more
    let pins = undo_ns_pins(&root);
    assert!(!pins.is_empty(), "maw undo wrote pins");
    let old: Vec<String> = pins
        .iter()
        .enumerate()
        .map(|(i, (r, o))| {
            backdate(
                &root,
                r,
                o,
                &format!("2020-01-0{}T00-00-00.000000000Z", i + 1),
            )
        })
        .collect();

    legacy_undo_workspace(&root);

    let out = maw_ok(&root, &["gc", "--recovery-snapshots"]);
    for pin in &old {
        assert!(
            ref_exists(&root, pin),
            "pin in the namespace a live `undo` workspace shares must be kept:\n{out}"
        );
    }
    assert!(
        out.contains("LIVE"),
        "kept as a live workspace's pin:\n{out}"
    );

    let check = reserved_check(&root);
    assert_eq!(check["status"], "warn", "{check:#}");
    let msg = check["message"].as_str().unwrap_or_default();
    let fix = check["fix"].as_str().unwrap_or_default();
    assert!(msg.contains("'undo'"), "{check:#}");
    assert!(msg.contains("maw undo"), "{check:#}");
    assert!(
        fix.contains("maw ws recover undo --to undo-work"),
        "rename hint:\n{check:#}"
    );

    // The hint works: after the rename the warning is gone.
    std::fs::write(root.join(".maw/workspaces/undo/draft.txt"), "draft\n").expect("write");
    maw_ok(&root, &["ws", "destroy", "undo", "--force"]);
    maw_ok(&root, &["ws", "recover", "undo", "--to", "undo-work"]);
    assert_eq!(
        std::fs::read_to_string(root.join(".maw/workspaces/undo-work/draft.txt")).expect("read"),
        "draft\n"
    );
    // Destroy records of `undo` remain, so doctor still says so (warn), but
    // no longer about a live workspace.
    let check = reserved_check(&root);
    let msg = check["message"].as_str().unwrap_or_default();
    assert!(msg.contains("destroy record"), "{check:#}");
}

/// Existing repo where an `undo` workspace was destroyed (destroy record +
/// pin in the shared namespace): a `maw undo` pin a pending redo needs is
/// still classified as UNDO and kept; the legacy destroy pin is kept too.
#[test]
fn destroyed_legacy_undo_workspace_does_not_misclassify_redo_pin() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());

    let ws = legacy_undo_workspace(&root);
    std::fs::write(ws.join("draft.txt"), "draft\n").expect("write");
    maw_ok(&root, &["ws", "destroy", "undo", "--force"]);
    let legacy = undo_ns_pins(&root);
    assert_eq!(legacy.len(), 1, "legacy destroy pin: {legacy:?}");
    let legacy_pin = legacy[0].0.clone();

    let check = reserved_check(&root);
    assert_eq!(check["status"], "warn", "{check:#}");
    assert!(
        check["message"]
            .as_str()
            .unwrap_or_default()
            .contains("destroy record"),
        "{check:#}"
    );
    assert!(
        check["fix"]
            .as_str()
            .unwrap_or_default()
            .contains("maw ws recover undo --to undo-work"),
        "{check:#}"
    );

    merge_destroy(&root, "bob");
    maw_ok(&root, &["undo"]);
    let redo_pin = undo_ns_pins(&root)
        .into_iter()
        .find(|(r, _)| *r != legacy_pin)
        .expect("maw undo pin");
    let redo_pin = backdate(&root, &redo_pin.0, &redo_pin.1, OLD_LEAF);

    let out = maw_ok(&root, &["gc", "--recovery-snapshots"]);
    assert!(ref_exists(&root, &redo_pin), "redoable pin kept:\n{out}");
    assert!(out.contains("UNDO"), "and marked UNDO:\n{out}");
    assert!(
        ref_exists(&root, &legacy_pin),
        "young destroy pin kept:\n{out}"
    );

    let redo = maw_ok(&root, &["undo"]);
    assert!(redo.contains("Redid merge"), "{redo}");
}

/// bn-1axaz: every command in the doctor hint runs as printed, before AND
/// after following it. After the rename the `undo` destroy records remain (so
/// doctor still reports them), but the hint used to repeat `maw ws recover
/// undo --to undo-work`, which then fails ("already exists"). And for a
/// destroyed `undo` it claimed "gc keeps them all", which only holds while a
/// live `undo` workspace exists.
#[test]
fn doctor_hint_commands_run_as_printed_after_the_rename() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    let ws = legacy_undo_workspace(&root);
    std::fs::write(ws.join("draft.txt"), "draft\n").expect("write");

    let run_fix = |check: &serde_json::Value| {
        let fix = check["fix"].as_str().unwrap_or_default().to_owned();
        let fix = fix.strip_prefix("Rename: ").unwrap_or(&fix).to_owned();
        // Hint alternatives are `  |  `-separated; a command alternative is
        // `maw ...[ && maw ...]`, anything else is prose.
        for cmd in fix
            .split("  |  ")
            .filter(|alt| alt.starts_with("maw "))
            .flat_map(|alt| alt.split(" && ").map(str::to_owned).collect::<Vec<_>>())
        {
            let cmd = cmd.trim().trim_start_matches("maw ").to_owned();
            let words: Vec<&str> = cmd.split_whitespace().collect();
            let out = maw(&root, &words);
            assert!(
                out.status.success(),
                "doctor hint `maw {cmd}` failed as printed:\n{}\nfull check: {check:#}",
                combined(&out)
            );
        }
    };
    let check = reserved_check(&root);
    assert_eq!(check["status"], "warn", "{check:#}");
    run_fix(&check);
    assert_eq!(
        std::fs::read_to_string(root.join(".maw/workspaces/undo-work/draft.txt")).expect("read"),
        "draft\n"
    );
    let check = reserved_check(&root);
    let msg = check["message"].as_str().unwrap_or_default();
    assert!(msg.contains("destroy record"), "{check:#}");
    assert!(
        !msg.contains("gc keeps them all"),
        "no live `undo` workspace any more:\n{check:#}"
    );
    run_fix(&check);
}
