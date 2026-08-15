//! Fault-injection acceptance for post-materialization verify + repair (bn-3gba).
//!
//! Reproduces the **bn-p3m9** corruption signature deterministically: a fresh
//! workspace whose HEAD and index are correct at the base epoch, but whose
//! working tree carries foreign bytes on a tracked path.
//!
//! The `FP_CREATE_AFTER_MATERIALIZE` failpoint fires inside `maw ws create`
//! AFTER the worktree checkout and BEFORE the post-materialization verify. Armed
//! with `MAW_FP=FP_CREATE_AFTER_MATERIALIZE=corrupt:<abs-path>` it overwrites
//! one file in the fresh workspace with `CORRUPT_BYTES` — exactly the "working
//! tree silently disagrees with HEAD" state the field report described.
//!
//! Acceptance (from the bone):
//!
//! 1. the verify DETECTS it,
//! 2. REPAIRS it (the workspace ends byte-identical to HEAD),
//! 3. prints the loud WARNING,
//! 4. records the oplog + artifact event.
//!
//! These tests only compile with `--features failpoints` (the `fp!()` site
//! compiles to nothing otherwise, so the shipped binary stays zero-overhead).
//! Run them via `just sg1-materialize-check`.

#![cfg(feature = "failpoints")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Built with `--features failpoints` because this test target only exists
/// under that feature, so the `fp!()` sites in the binary are live.
const MAW: &str = env!("CARGO_BIN_EXE_maw");

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn maw(dir: &Path) -> Command {
    let mut cmd = Command::new(MAW);
    cmd.current_dir(dir);
    cmd
}

/// The tracked file the fault corrupts. Multi-line so a partial write is
/// obviously different from the real content.
const VICTIM: &str = "src/victim.rs";
const VICTIM_CONTENT: &str = "pub fn answer() -> u32 {\n    42\n}\n";

/// `git init` + seed content + `maw init`. Returns the repo root.
fn setup_repo(dir: &Path) -> PathBuf {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
    std::fs::write(dir.join(VICTIM), VICTIM_CONTENT).expect("write victim");
    std::fs::write(dir.join("README.md"), "hi\n").expect("write readme");
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);

    let out = maw(dir).arg("init").output().expect("maw init");
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.to_path_buf()
}

/// `.maw/workspaces/<name>` (consolidated layout — what `maw init` produces on
/// a brownfield repo here).
fn workspace_path(root: &Path, name: &str) -> PathBuf {
    root.join(".maw").join("workspaces").join(name)
}

/// Porcelain status of the workspace, via the `git` CLI (an independent
/// verifier — this test must not trust the gix code path under test).
fn porcelain(ws: &Path) -> String {
    let out = Command::new("git")
        .current_dir(ws)
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn artifact_files(root: &Path, ws_name: &str) -> Vec<PathBuf> {
    let dir = root
        .join(".maw")
        .join("manifold")
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("materialize-repair");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

/// The headline acceptance test: a fault-injected partial materialization is
/// detected, repaired, and reported.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end acceptance scenario: the four contract clauses (detect / preserve / repair / record) are asserted against a SINGLE injected fault, so splitting them would need the fault re-injected per clause"
)]
fn corrupted_create_is_detected_repaired_and_reported() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    let ws_name = "bn-3gba-victim";
    let ws = workspace_path(&root, ws_name);
    // The failpoint needs an absolute path; the caller always knows the
    // workspace path before `create` runs, which is what makes the `corrupt:`
    // action env-expressible for the faithful (subprocess) tier.
    let victim_abs = ws.join(VICTIM);

    let out = maw(&root)
        .args(["ws", "create", "--from", "main", ws_name])
        .env(
            "MAW_FP",
            format!(
                "FP_CREATE_AFTER_MATERIALIZE=corrupt:{}",
                victim_abs.display()
            ),
        )
        .output()
        .expect("maw ws create");

    assert!(
        out.status.success(),
        "create must still SUCCEED — the verify repairs, it does not abort.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);

    // (3) The loud WARNING, naming the divergent path.
    assert!(
        stderr.contains("WARNING: workspace 'bn-3gba-victim' did not materialize cleanly"),
        "expected the loud post-materialization WARNING on stderr, got:\n{stderr}"
    );
    assert!(
        stderr.contains(VICTIM),
        "the WARNING must NAME the divergent path, got:\n{stderr}"
    );
    assert!(
        stderr.contains("bn-p3m9"),
        "the WARNING must name the bug class so a field report is actionable, got:\n{stderr}"
    );
    assert!(
        stderr.contains("Repaired 1 of 1 path(s) from HEAD."),
        "expected the repair count, got:\n{stderr}"
    );
    assert!(
        stderr.contains("Workspace now matches HEAD."),
        "expected the post-repair confirmation, got:\n{stderr}"
    );

    // (2) The workspace ends byte-identical to HEAD.
    let on_disk = std::fs::read_to_string(&victim_abs).expect("read repaired victim");
    assert_eq!(
        on_disk, VICTIM_CONTENT,
        "the corrupted file must be re-materialized from HEAD byte-for-byte"
    );
    let status = porcelain(&ws);
    assert!(
        status.trim().is_empty(),
        "the repaired workspace must be clean per `git status --porcelain`, got:\n{status}"
    );

    // (4a) The artifact carries the evidence.
    let artifacts = artifact_files(&root, ws_name);
    assert_eq!(
        artifacts.len(),
        1,
        "expected exactly one materialize-repair artifact, got {artifacts:?}"
    );
    let body = std::fs::read_to_string(&artifacts[0]).expect("read artifact");
    let json: serde_json::Value = serde_json::from_str(&body).expect("artifact is JSON");
    assert_eq!(json["schema_version"], 1, "{body}");
    assert_eq!(json["workspace"], ws_name, "{body}");
    assert_eq!(json["operation"], "create", "{body}");
    assert_eq!(json["repaired_count"], 1, "{body}");
    assert_eq!(json["paths"][0]["path"], VICTIM, "{body}");
    assert_eq!(json["paths"][0]["status"], "M", "{body}");
    assert_eq!(json["paths"][0]["repaired"], true, "{body}");
    assert_eq!(
        json["residual_paths"].as_array().map(Vec::len),
        Some(0),
        "a fully repaired workspace has no residual divergence: {body}"
    );

    // (2b) Prime Invariant: the pre-repair bytes were pinned BEFORE being
    // overwritten, and the pin holds the corrupted content verbatim (the only
    // forensic evidence of the mechanism in a real field report).
    let pinned = json["preserved_ref"]
        .as_str()
        .unwrap_or_else(|| panic!("artifact must carry preserved_ref: {body}"))
        .to_owned();
    assert!(
        pinned.starts_with("refs/manifold/recovery/"),
        "pin must live under the recovery namespace: {pinned}"
    );
    assert!(
        stderr.contains(&pinned),
        "the WARNING must name the pin so an operator can inspect it, got:\n{stderr}"
    );
    let show = Command::new("git")
        .current_dir(&root)
        .args(["show", &format!("{pinned}:{VICTIM}")])
        .output()
        .expect("git show pinned blob");
    assert!(
        show.status.success(),
        "the pinned ref must be readable: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&show.stdout),
        String::from_utf8_lossy(maw_core::failpoints::CORRUPT_BYTES),
        "the pin must hold the PRE-repair (corrupted) bytes, not the repaired ones"
    );

    // (4b) The oplog carries the annotation, visible in `maw ws history`.
    let hist = maw(&root)
        .args(["ws", "history", ws_name])
        .output()
        .expect("maw ws history");
    let hist_out = String::from_utf8_lossy(&hist.stdout);
    assert!(
        hist_out.contains("materialize-repair"),
        "the repair must be visible in `maw ws history` (oplog annotation), got:\n{hist_out}"
    );
}

/// Control: without the fault, `ws create` is silent — no WARNING, no
/// artifact, no oplog annotation. Guards against the verifier itself becoming
/// a false-positive generator (which would be worse than the bug it catches).
#[test]
fn clean_create_produces_no_warning_and_no_artifact() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    let ws_name = "bn-3gba-clean";
    let out = maw(&root)
        .args(["ws", "create", "--from", "main", ws_name])
        .output()
        .expect("maw ws create");
    assert!(
        out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("did not materialize cleanly"),
        "a clean create must NOT warn, got:\n{stderr}"
    );
    assert!(
        artifact_files(&root, ws_name).is_empty(),
        "a clean create must not write a materialize-repair artifact"
    );

    let hist = maw(&root)
        .args(["ws", "history", ws_name])
        .output()
        .expect("maw ws history");
    let hist_out = String::from_utf8_lossy(&hist.stdout);
    assert!(
        !hist_out.contains("materialize-repair"),
        "a clean create must not annotate the oplog, got:\n{hist_out}"
    );
}

/// Untracked scratch left in a workspace is NOT divergence: the repair would
/// have to delete it, which the Prime Invariant forbids. A follow-up
/// `maw ws sync` must therefore neither warn nor remove it.
///
/// (The sync itself refuses a dirty workspace, so this asserts the *scoping*
/// through the create path: an untracked file present when the next
/// clean-at-commit op runs must never be reported or touched.)
#[test]
fn untracked_scratch_is_never_treated_as_divergence() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = setup_repo(td.path());

    let ws_name = "bn-3gba-scratch";
    let ws = workspace_path(&root, ws_name);
    let scratch_abs = ws.join("scratch-notes.txt");

    // Arm the failpoint on a path that does NOT exist in HEAD: the injected
    // write creates an untracked file mid-create.
    let out = maw(&root)
        .args(["ws", "create", "--from", "main", ws_name])
        .env(
            "MAW_FP",
            format!(
                "FP_CREATE_AFTER_MATERIALIZE=corrupt:{}",
                scratch_abs.display()
            ),
        )
        .output()
        .expect("maw ws create");
    assert!(
        out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("did not materialize cleanly"),
        "an untracked path must NOT be reported as divergence, got:\n{stderr}"
    );
    assert!(
        scratch_abs.exists(),
        "the untracked file must survive — repairing it would mean DELETING it"
    );
    assert!(
        artifact_files(&root, ws_name).is_empty(),
        "no artifact for untracked-only state"
    );
}
