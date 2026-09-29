//! bn-43x5k: `maw gc --recovery-snapshots` and the pins it must not strand.
//!
//! 1. `refs/manifold/recovery/undo/*` pins are written by `maw undo`; the one
//!    a pending redo would re-apply is the only thing keeping the undone merge
//!    result reachable. gc used to treat "undo" as a destroyed workspace and
//!    dropped such a pin after `--older-than` days with no `--force`.
//! 2. `--include-live --force` on a reused workspace name dropped the old
//!    workspace's pin but left its destroy record claiming the missing ref,
//!    so `maw ws recover` and `maw doctor`/fsck pointed at nothing.
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

fn commit_in(root: &Path, ws: &str, line: &str) {
    let script = format!("printf '{line}\\n' >> file.txt && git add -A && git commit -m '{line}'");
    maw_ok(root, &["exec", ws, "--", "sh", "-c", &script]);
}

/// Create `name`, commit in it, merge it into default with `--destroy`.
/// Returns the merge result (`epoch_after`).
fn merge_destroy(root: &Path, name: &str) -> String {
    maw_ok(root, &["ws", "create", name, "--from", "main"]);
    commit_in(root, name, name);
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
    git(root, &["rev-parse", "refs/manifold/epoch/current"])
}

/// `(ref, oid)` of every pin under `refs/manifold/recovery/<ns>/`.
fn pins(root: &Path, ns: &str) -> Vec<(String, String)> {
    let out = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &format!("refs/manifold/recovery/{ns}/"),
        ],
    );
    out.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(r, o)| (r.to_owned(), o.to_owned()))
        .collect()
}

/// Rename every undo pin to an old timestamp so it is past any `--older-than`.
fn backdate_undo_pins(root: &Path) -> String {
    let current = pins(root, "undo");
    assert_eq!(current.len(), 1, "exactly one undo pin: {current:?}");
    let (name, oid) = &current[0];
    let old = format!("refs/manifold/recovery/undo/{OLD_LEAF}");
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

#[test]
fn redoable_undo_pin_survives_age_sweep_until_redo_is_no_longer_possible() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    let after = merge_destroy(&root, "alice");
    maw_ok(&root, &["undo"]);
    let pin = backdate_undo_pins(&root);
    assert_eq!(git(&root, &["rev-parse", &pin]), after);

    // The redo still needs the pin: a routine age sweep keeps it and says why.
    let out = maw_ok(&root, &["gc", "--recovery-snapshots"]);
    assert!(
        ref_exists(&root, &pin),
        "a still-redoable undo pin must survive a plain age sweep:\n{out}"
    );
    assert!(out.contains(&pin), "the kept pin is listed:\n{out}");
    assert!(out.contains("UNDO"), "the kept pin is marked UNDO:\n{out}");

    // The redo still works (the pin kept the merge result reachable).
    let redo = maw_ok(&root, &["undo"]);
    assert!(redo.contains("Redid merge"), "{redo}");

    // After the redo nothing references the old pin: it ages out like any
    // other pin, with no --force (it is old and not live).
    let out = maw_ok(&root, &["gc", "--recovery-snapshots"]);
    assert!(
        !ref_exists(&root, &pin),
        "an undo pin no redo references is swept like any other old pin:\n{out}"
    );
}

#[test]
fn dropping_a_redoable_undo_pin_needs_force_and_is_listed_as_undo() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    merge_destroy(&root, "bob");
    maw_ok(&root, &["undo"]);
    let pin = backdate_undo_pins(&root);

    let out = maw(&root, &["gc", "--recovery-snapshots", "--include-live"]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "must refuse without --force:\n{text}"
    );
    assert!(text.contains(&pin), "refusal lists the pin:\n{text}");
    assert!(
        text.contains("UNDO") && !text.contains("workspace undo, LIVE"),
        "the pin is marked UNDO, not LIVE:\n{text}"
    );
    assert!(ref_exists(&root, &pin), "refused gc deletes nothing");

    maw_ok(
        &root,
        &["gc", "--recovery-snapshots", "--include-live", "--force"],
    );
    assert!(!ref_exists(&root, &pin), "--force drops it");
}

#[test]
fn include_live_force_on_reused_name_drops_the_old_destroy_record_too() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());

    // carol v1: dirty work, destroyed with --force → pin + destroy record.
    maw_ok(&root, &["ws", "create", "carol", "--from", "main"]);
    std::fs::write(
        root.join(".maw/workspaces/carol/draft.txt"),
        "unmerged draft\n",
    )
    .expect("write draft");
    maw_ok(&root, &["ws", "destroy", "carol", "--force"]);
    let old_pins = pins(&root, "carol");
    assert_eq!(old_pins.len(), 1, "{old_pins:?}");
    let old_pin = old_pins[0].0.clone();
    let records_dir = root.join(".maw/manifold/artifacts/ws/carol/destroy");
    let records = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        Path::new(n).extension().is_some_and(|x| x == "json") && n != "latest.json"
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(records(&records_dir).len(), 1, "one destroy record");

    // carol v2: the name is reused.
    maw_ok(&root, &["ws", "create", "carol", "--from", "main"]);

    maw_ok(
        &root,
        &[
            "gc",
            "--recovery-snapshots",
            "--older-than",
            "0",
            "--include-live",
            "--force",
        ],
    );
    assert!(!ref_exists(&root, &old_pin), "old carol pin dropped");
    assert!(
        records(&records_dir).is_empty(),
        "the dropped pin's destroy record goes in the same pass: {:?}",
        records(&records_dir)
    );

    // Once carol v2 is gone too, nothing may point at the missing ref.
    maw_ok(&root, &["ws", "destroy", "carol"]);
    let fsck = combined(&maw(&root, &["fsck"]));
    assert!(
        !fsck.contains(&old_pin),
        "fsck must not report a record claiming the swept pin:\n{fsck}"
    );
    let recover = combined(&maw(&root, &["ws", "recover", "carol"]));
    assert!(
        !recover.contains(&old_pin),
        "`maw ws recover` must not point at the swept pin:\n{recover}"
    );
}
