//! The recovery front door in the CONSOLIDATED layout (bn-2nhl).
//!
//! Black-box validation found the advertised recovery path broken end-to-end:
//!
//! 1. `maw ws recover --ref <ref> --restore-file <path>` — the command maw
//!    itself prints in the bn-1xmk dirty-trunk replay warning — failed with
//!    "Default workspace not found at <root>/.maw/workspaces/default". In the
//!    consolidated layout the default workspace IS the repo root.
//! 2. `maw ws recover default --show/--restore-file <path>` failed with "No
//!    destroy records found for workspace 'default'" even though the listing
//!    showed a `SOURCE=pinned` row and its footer suggested exactly those
//!    commands.
//! 3. The listing reported `DIRTY_FILES 0` for a snapshot that preserved
//!    several dirty files.
//!
//! These tests drive the built binary through a real dirty-trunk merge in a
//! real consolidated repo and then run the command strings **maw printed**,
//! parsed out of its own output, rather than hand-written equivalents.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

/// The dirty tracked trunk file whose uncommitted bytes must survive and be
/// recoverable.
const VICTIM: &str = "src.txt";
const VICTIM_COMMITTED: &str = "trunk line 1\n";
const VICTIM_DIRTY: &str = "UNCOMMITTED trunk edit\n";

/// A second dirty tracked trunk file, so `DIRTY_FILES` has a count > 1 to get
/// right.
const VICTIM2: &str = "notes.txt";
const VICTIM2_COMMITTED: &str = "notes\n";
const VICTIM2_DIRTY: &str = "UNCOMMITTED notes edit\n";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn run_git(dir: &Path, args: &[&str]) {
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
}

fn maw(dir: &Path, args: &[&str]) -> Output {
    Command::new(MAW)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run maw")
}

fn maw_ok(dir: &Path, args: &[&str]) -> Output {
    let out = maw(dir, args);
    assert!(
        out.status.success(),
        "maw {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `git init` + seed + `maw init`, then commit maw's `.gitignore` so the trunk
/// starts perfectly clean (the dirty set under test is then exactly ours).
fn setup_repo(dir: &Path) -> PathBuf {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(VICTIM), VICTIM_COMMITTED).expect("write victim");
    std::fs::write(dir.join(VICTIM2), VICTIM2_COMMITTED).expect("write victim2");
    std::fs::write(dir.join("other.txt"), "other\n").expect("write other");
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);

    let out = maw(dir, &["init"]);
    assert!(out.status.success(), "maw init failed: {}", stderr_of(&out));

    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "maw init"]);
    let status = Command::new("git")
        .current_dir(dir)
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    assert!(
        status.stdout.is_empty(),
        "trunk should start clean, got: {}",
        String::from_utf8_lossy(&status.stdout)
    );

    // Consolidated layout: the default workspace is the repo root, so there is
    // no `.maw/workspaces/default` directory. This is the premise of failure 1.
    assert!(
        !dir.join(".maw").join("workspaces").join("default").exists(),
        "consolidated layout must not have a .maw/workspaces/default directory"
    );

    dir.to_path_buf()
}

/// Dirty two tracked trunk files and merge a workspace into default, so the
/// preserve/replay path pins a recovery snapshot for `default`.
///
/// Returns the merge's combined output.
fn dirty_trunk_and_merge(root: &Path) -> String {
    maw_ok(root, &["ws", "create", "feat", "--from", "main"]);
    std::fs::write(
        root.join(".maw")
            .join("workspaces")
            .join("feat")
            .join("other.txt"),
        "feature\n",
    )
    .expect("write workspace change");

    std::fs::write(root.join(VICTIM), VICTIM_DIRTY).expect("dirty victim");
    std::fs::write(root.join(VICTIM2), VICTIM2_DIRTY).expect("dirty victim2");

    let out = maw_ok(
        root,
        &[
            "ws",
            "merge",
            "feat",
            "--into",
            "default",
            "--message",
            "merge: feat",
        ],
    );
    format!("{}{}", stdout_of(&out), stderr_of(&out))
}

/// Restore `path` in the repo root to its committed content, so a
/// `--restore-file` without `--force` is not blocked by the safety gate.
fn reset_path_to_head(root: &Path, path: &str) {
    run_git(root, &["checkout", "--", path]);
}

/// Split a printed `maw ...` command string into the argv to pass to the
/// binary — i.e. run exactly what maw told the user to run.
fn printed_args(command: &str) -> Vec<String> {
    let rest = command
        .trim()
        .strip_prefix("maw ")
        .unwrap_or_else(|| panic!("printed command should start with `maw `: {command:?}"));
    rest.split_whitespace().map(ToOwned::to_owned).collect()
}

fn run_printed(root: &Path, command: &str) -> Output {
    let owned = printed_args(command);
    let argv: Vec<&str> = owned.iter().map(String::as_str).collect();
    maw(root, &argv)
}

/// The `recovery_ref` of the pinned `default` row, straight from the listing
/// the user is told to consult.
fn pinned_default_ref(root: &Path) -> String {
    let json = stdout_of(&maw_ok(root, &["ws", "recover", "--format", "json"]));
    let value: serde_json::Value = serde_json::from_str(&json).expect("recover listing is JSON");
    let rows = value["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces array");
    let row = rows
        .iter()
        .find(|r| r["name"] == "default")
        .unwrap_or_else(|| panic!("no `default` row in recover listing: {json}"));
    assert_eq!(row["source"], "pinned_ref", "row should be a pinned row");
    row["recovery_ref"]
        .as_str()
        .expect("pinned row carries its recovery ref")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Failure 1 — the bn-1xmk printed command, run verbatim
// ---------------------------------------------------------------------------

/// The exact `Restore:` command the bn-1xmk warning prints must work verbatim
/// and land the bytes in the REPO ROOT (the consolidated default workspace).
///
/// The warning is triggered deterministically: an embedded git dir with no
/// commit checked out makes `git add -A` (and so `snapshot_working_copy`) fail,
/// which sends the merge down the force-checkout fallback. Since bn-2ds48 the
/// fallback replays the in-memory pin, so the replay is failed too
/// (`FP_CLEANUP_REPLAY_BEFORE_APPLY`) to reach the repair that prints the
/// recovery pointer.
#[cfg(feature = "failpoints")]
#[test]
fn bn_1xmk_printed_restore_command_works_verbatim() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    maw_ok(&root, &["ws", "create", "feat", "--from", "main"]);
    std::fs::write(
        root.join(".maw")
            .join("workspaces")
            .join("feat")
            .join("other.txt"),
        "feature\n",
    )
    .expect("write workspace change");
    std::fs::write(root.join(VICTIM), VICTIM_DIRTY).expect("dirty victim");

    // Embedded repo with no commit → snapshot_working_copy fails → the merge
    // falls back to force checkout and prints the bn-1xmk recovery pointer.
    std::fs::create_dir_all(root.join("embedded")).expect("mkdir embedded");
    run_git(&root.join("embedded"), &["init", "-b", "main"]);
    std::fs::write(root.join("embedded").join("x.txt"), "x\n").expect("write embedded file");

    let merge = Command::new(MAW)
        .current_dir(&root)
        .args([
            "ws",
            "merge",
            "feat",
            "--into",
            "default",
            "--message",
            "merge: feat",
        ])
        .env("MAW_FP", "FP_CLEANUP_REPLAY_BEFORE_APPLY=error:injected")
        .output()
        .expect("run maw");
    assert!(merge.status.success(), "{}", stderr_of(&merge));
    let err = stderr_of(&merge);
    assert!(
        err.contains("WARNING (bn-1xmk)"),
        "expected the bn-1xmk warning, got:\n{err}"
    );

    // Capture the two commands the warning printed, verbatim.
    let restore_cmd = err
        .lines()
        .find_map(|l| l.trim().strip_prefix("Restore:"))
        .expect("warning prints a Restore: command")
        .trim()
        .to_owned();
    let inspect_cmd = err
        .lines()
        .find_map(|l| l.trim().strip_prefix("Inspect:"))
        .expect("warning prints an Inspect: command")
        .trim()
        .to_owned();

    assert!(
        restore_cmd.contains("--restore-file") && restore_cmd.contains(VICTIM),
        "unexpected printed restore command: {restore_cmd}"
    );

    // `Inspect:` verbatim.
    let inspect = run_printed(&root, &inspect_cmd);
    assert!(
        inspect.status.success(),
        "printed inspect command `{inspect_cmd}` failed: {}",
        stderr_of(&inspect)
    );

    // The repair already put the user's bytes back; drop them so the restore
    // has something to prove (this is the "automatic repair FAILED" shape,
    // where the printed command is the user's only route back).
    reset_path_to_head(&root, VICTIM);
    assert_eq!(
        std::fs::read_to_string(root.join(VICTIM)).expect("read victim"),
        VICTIM_COMMITTED
    );

    // `Restore:` verbatim — no added flags, no rewriting.
    let restored = run_printed(&root, &restore_cmd);
    assert!(
        restored.status.success(),
        "printed restore command `{restore_cmd}` failed:\nstdout: {}\nstderr: {}",
        stdout_of(&restored),
        stderr_of(&restored)
    );

    // Bytes are back — in the REPO ROOT, the consolidated default workspace.
    assert_eq!(
        std::fs::read_to_string(root.join(VICTIM)).expect("read victim"),
        VICTIM_DIRTY,
        "the printed recovery command must restore the uncommitted bytes to the repo root"
    );
    assert!(
        !root
            .join(".maw")
            .join("workspaces")
            .join("default")
            .exists(),
        "recovery must not invent a .maw/workspaces/default directory"
    );
}

/// `--ref <ref> --restore-file <path>` writes into the repo root in the
/// consolidated layout (the direct regression for failure 1).
#[test]
fn recover_ref_restore_file_targets_the_repo_root() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());
    dirty_trunk_and_merge(&root);

    let recovery_ref = pinned_default_ref(&root);
    reset_path_to_head(&root, VICTIM);

    let out = maw(
        &root,
        &[
            "ws",
            "recover",
            "--ref",
            &recovery_ref,
            "--restore-file",
            VICTIM,
        ],
    );
    assert!(
        out.status.success(),
        "recover --ref --restore-file failed:\nstdout: {}\nstderr: {}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        !stderr_of(&out).contains("Default workspace not found"),
        "the consolidated default must resolve to the repo root"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(VICTIM)).expect("read victim"),
        VICTIM_DIRTY
    );
}

// ---------------------------------------------------------------------------
// Failure 2 — pinned rows support --show / --restore-file / --to
// ---------------------------------------------------------------------------

/// `maw ws recover default --show <path>` works for a pinned row (no destroy
/// record) and streams the snapshot bytes.
#[test]
fn recover_pinned_row_show_file_works() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());
    dirty_trunk_and_merge(&root);

    let out = maw(&root, &["ws", "recover", "default", "--show", VICTIM]);
    assert!(
        out.status.success(),
        "recover default --show failed:\nstdout: {}\nstderr: {}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        !stderr_of(&out).contains("No destroy records found"),
        "pinned rows must not be rejected for lacking a destroy record"
    );
    assert_eq!(stdout_of(&out), VICTIM_DIRTY);
}

/// `maw ws recover default --restore-file <path>` works for a pinned row.
#[test]
fn recover_pinned_row_restore_file_works() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());
    dirty_trunk_and_merge(&root);

    reset_path_to_head(&root, VICTIM);
    let out = maw(
        &root,
        &["ws", "recover", "default", "--restore-file", VICTIM],
    );
    assert!(
        out.status.success(),
        "recover default --restore-file failed:\nstdout: {}\nstderr: {}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert_eq!(
        std::fs::read_to_string(root.join(VICTIM)).expect("read victim"),
        VICTIM_DIRTY
    );
}

/// The `--restore-file` safety gate still holds for pinned rows: a destination
/// with uncommitted changes is refused without `--force`, and honored with it.
#[test]
fn recover_pinned_row_restore_file_respects_force_gate() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());
    dirty_trunk_and_merge(&root);

    // The trunk still holds the (replayed) uncommitted edits.
    std::fs::write(root.join(VICTIM), "local work in progress\n").expect("write local work");
    let refused = maw(
        &root,
        &["ws", "recover", "default", "--restore-file", VICTIM],
    );
    assert!(
        !refused.status.success(),
        "dirty destination must be refused"
    );
    assert!(
        stderr_of(&refused).contains("Re-run with --force"),
        "refusal should point at --force, got: {}",
        stderr_of(&refused)
    );
    assert_eq!(
        std::fs::read_to_string(root.join(VICTIM)).expect("read victim"),
        "local work in progress\n",
        "a refused restore must not touch the file"
    );

    let forced = maw(
        &root,
        &[
            "ws",
            "recover",
            "default",
            "--restore-file",
            VICTIM,
            "--force",
        ],
    );
    assert!(
        forced.status.success(),
        "forced restore failed: {}",
        stderr_of(&forced)
    );
    assert_eq!(
        std::fs::read_to_string(root.join(VICTIM)).expect("read victim"),
        VICTIM_DIRTY
    );
}

// ---------------------------------------------------------------------------
// Failure 3 — DIRTY_FILES count for pinned rows
// ---------------------------------------------------------------------------

/// The listing must report how many files the pinned snapshot actually
/// preserves — two here, not zero.
#[test]
fn recover_listing_counts_pinned_dirty_files() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());
    let merge_output = dirty_trunk_and_merge(&root);
    assert!(
        merge_output.contains("preserving 2 uncommitted trunk file(s)"),
        "merge should preserve exactly our 2 dirty files, got:\n{merge_output}"
    );

    let text = stdout_of(&maw_ok(&root, &["ws", "recover", "--format", "text"]));
    let row = text
        .lines()
        .find(|l| l.starts_with("default\t"))
        .unwrap_or_else(|| panic!("no default row in listing:\n{text}"));
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols.len(), 6, "unexpected row shape: {row:?}");
    assert_eq!(cols[4], "2", "DIRTY_FILES column should be 2, row: {row:?}");
    assert_eq!(cols[5], "pinned");

    let json = stdout_of(&maw_ok(&root, &["ws", "recover", "--format", "json"]));
    let value: serde_json::Value = serde_json::from_str(&json).expect("JSON listing");
    let row = value["destroyed_workspaces"]
        .as_array()
        .expect("array")
        .iter()
        .find(|r| r["name"] == "default")
        .expect("default row");
    assert_eq!(row["dirty_file_count"], 2);

    // Both preserved paths are really in the snapshot.
    for (path, want) in [(VICTIM, VICTIM_DIRTY), (VICTIM2, VICTIM2_DIRTY)] {
        let shown = stdout_of(&maw_ok(
            &root,
            &["ws", "recover", "default", "--show", path],
        ));
        assert_eq!(shown, want, "snapshot content for {path}");
    }
}

// ---------------------------------------------------------------------------
// The listing footer, run verbatim
// ---------------------------------------------------------------------------

/// Every command the pinned-row footer advertises must actually run. The
/// commands are lifted out of maw's own output and executed with only the
/// documented placeholders substituted.
#[test]
fn recover_listing_footer_commands_work_verbatim() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());
    dirty_trunk_and_merge(&root);

    let listing = stdout_of(&maw_ok(&root, &["ws", "recover"]));
    let suggestions: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("maw ws recover <name>"))
        .map(|l| {
            l.split('#')
                .next()
                .unwrap_or(l)
                .trim()
                .replace("<name>", "default")
                .replace("<path>", VICTIM)
                .replace("<new-workspace>", "recovered")
                .replace("<new-name>", "recovered")
        })
        .collect();
    assert!(
        suggestions.iter().any(|s| s.contains("--restore-file"))
            && suggestions.iter().any(|s| s.contains("--show")),
        "footer should advertise --show and --restore-file for pinned rows:\n{listing}"
    );

    for cmd in suggestions {
        // The restore suggestion carries no --force, so give it a clean
        // destination exactly as a user following the advice would need to.
        if cmd.contains("--restore-file") {
            reset_path_to_head(&root, VICTIM);
        }
        let out = run_printed(&root, &cmd);
        assert!(
            out.status.success(),
            "advertised command `{cmd}` failed:\nstdout: {}\nstderr: {}",
            stdout_of(&out),
            stderr_of(&out)
        );
    }

    // The `--to` suggestion actually materialized a workspace from the pinned
    // snapshot, carrying the preserved bytes.
    let recovered = root.join(".maw").join("workspaces").join("recovered");
    assert!(
        recovered.is_dir(),
        "the advertised --to command should have created the workspace"
    );
    assert_eq!(
        std::fs::read_to_string(recovered.join(VICTIM)).expect("read recovered victim"),
        VICTIM_DIRTY
    );
}
