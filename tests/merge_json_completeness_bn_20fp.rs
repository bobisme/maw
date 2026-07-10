//! Integration tests for bn-20fp: merge-path JSON completeness, the
//! `--format json` discoverability hint, and the tail-visible destroy-cwd
//! warning.
//!
//! Motivation (mess field report 2, bn-1m4d items 3+4): an orchestrator
//! grepped the text merge output for the `[OK]` sentinel all session and lost
//! every NOTE/warning — conflicted-sibling lines and, twice, the destroy-cwd
//! warning. These tests lock in:
//!   (a) one `--format json` object carrying per-sibling outcomes, conflict
//!       flags, overlap, warnings, cwd_destroyed, invariant summary;
//!   (b) `cwd_destroyed` in both merge-destroy and standalone destroy JSON;
//!   (c) the text path: byte-stable sentinel, a json-hint line, and the
//!       destroy-cwd warning as the true final line;
//!   (d) field-stability — pre-existing JSON keys keep their names;
//!   (e) sync JSON schema consistency (overlap_hint + conflicted_paths).

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod manifold_common;

use manifold_common::{TestRepo, maw_bin};
use std::process::Command;

fn make_commit(repo: &TestRepo, ws: &str, file: &str, content: &str, msg: &str) {
    repo.add_file(ws, file, content);
    repo.git_in_workspace(ws, &["add", "-A"]);
    repo.git_in_workspace(ws, &["commit", "-m", msg]);
}

fn parse_json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}):\n{stdout}"))
}

// ---------------------------------------------------------------------------
// (a) Full-shape merge JSON: replayed + skipped_dirty + conflicted siblings.
// ---------------------------------------------------------------------------

#[test]
fn merge_json_carries_full_per_sibling_and_top_level_shape() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "base\n")]);

    // Source workspace whose merge advances the epoch and touches shared.txt.
    repo.maw_ok(&["ws", "create", "merger"]);
    make_commit(
        &repo,
        "merger",
        "shared.txt",
        "merger update\n",
        "merger: edit",
    );

    // Sibling: committed-ahead, non-overlapping → replayed clean.
    repo.maw_ok(&["ws", "create", "sib-clean"]);
    make_commit(&repo, "sib-clean", "clean.txt", "clean\n", "sib-clean: add");

    // Sibling: committed-ahead on shared.txt → conflicted replay.
    repo.maw_ok(&["ws", "create", "sib-conflict"]);
    make_commit(
        &repo,
        "sib-conflict",
        "shared.txt",
        "sib-conflict update\n",
        "sib-conflict: edit",
    );

    // Sibling: dirty (uncommitted) → skipped_dirty.
    repo.maw_ok(&["ws", "create", "sib-dirty"]);
    repo.add_file("sib-dirty", "dirty.txt", "uncommitted\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "merger",
        "--message",
        "feat: merge merger",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);

    // Top-level gap-fill fields.
    assert_eq!(v["status"], "success");
    assert!(
        v["merged_sha"].as_str().is_some(),
        "merged_sha missing:\n{stdout}"
    );
    assert_eq!(v["merged_sha"], v["epoch"], "merged_sha must equal epoch");
    assert_eq!(v["epoch_after"], v["epoch"], "epoch_after must equal epoch");
    assert!(v["epoch_before"].as_str().is_some(), "epoch_before missing");
    assert_ne!(
        v["epoch_before"], v["epoch_after"],
        "epoch must have advanced"
    );
    assert_eq!(v["sources"][0], "merger");
    assert_eq!(v["cwd_destroyed"], false);
    assert!(v["recovery"].is_object(), "recovery object missing");

    // Field-stability: the pre-existing keys are still present + shaped.
    assert_eq!(v["workspaces"][0], "merger");
    assert_eq!(v["conflict_count"], 0);
    assert!(v["invariant"].is_object(), "invariant summary missing");
    assert!(v["invariant"]["siblings_checked"].as_u64().is_some());
    assert!(v["invariant"]["orphaned"].is_array());

    // Per-sibling rows: collect by name and assert each action + flag.
    let siblings = v["siblings"]
        .as_array()
        .expect("siblings[] must be present");
    let by_name = |name: &str| -> serde_json::Value {
        siblings
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("sibling '{name}' missing from siblings[]:\n{stdout}"))
            .clone()
    };

    let clean = by_name("sib-clean");
    assert_eq!(clean["action"], "replayed");
    assert_eq!(clean["conflicted"], false);
    assert!(clean["replayed_commits"].as_u64().unwrap() >= 1);

    let conflict = by_name("sib-conflict");
    assert_eq!(conflict["action"], "conflicted");
    assert_eq!(conflict["conflicted"], true);
    // conflict_files should name the overlapping file (best-effort derivation).
    let files = conflict["conflict_files"]
        .as_array()
        .expect("conflict_files[]");
    assert!(
        files.iter().any(|f| f == "shared.txt"),
        "conflict_files should list shared.txt:\n{conflict:#}"
    );
    // Overlap hint: the replay rode over shared.txt which the sibling touches.
    assert_eq!(conflict["overlap_hint"]["count"].as_u64().unwrap(), 1);

    let dirty = by_name("sib-dirty");
    assert_eq!(dirty["action"], "skipped_dirty");
    assert_eq!(dirty["conflicted"], false);

    // warnings[] must surface the conflicted-sibling NOTE the text path prints.
    let warnings = v["warnings"]
        .as_array()
        .expect("warnings[] must be present");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("sib-conflict")),
        "warnings[] must mention the conflicted sibling:\n{warnings:#?}"
    );

    // sibling_conflicts (pre-existing bn-mq6j field) still present + correct.
    assert_eq!(v["sibling_conflicts"][0], "sib-conflict");
}

// ---------------------------------------------------------------------------
// (b/c) Text path: sentinel byte-stability + json hint line ordering.
// ---------------------------------------------------------------------------

#[test]
fn merge_text_output_has_sentinel_then_json_hint_line() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# proj\n")]);
    repo.maw_ok(&["ws", "create", "feat"]);
    make_commit(&repo, "feat", "feature.txt", "f\n", "feat: add");

    let stdout = repo.maw_ok(&["ws", "merge", "feat", "--message", "feat: merge feat"]);

    // Sentinel is byte-stable (bn-1kop / bn-20fp contract).
    let sentinel_line = stdout
        .lines()
        .find(|l| l.starts_with("[OK] merged "))
        .expect("sentinel line missing");
    assert!(
        sentinel_line.starts_with("[OK] merged feat into ") && sentinel_line.contains(" @ "),
        "sentinel shape changed: {sentinel_line}"
    );

    // The json-hint line follows the sentinel.
    let lines: Vec<&str> = stdout.lines().collect();
    let sentinel_idx = lines
        .iter()
        .position(|l| l.starts_with("[OK] merged "))
        .unwrap();
    assert!(
        lines[sentinel_idx + 1].contains("machine-readable: maw ws merge --format json"),
        "json-hint line must immediately follow the sentinel:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// (b) cwd_destroyed — merge --destroy JSON + tail-visible text warning.
// ---------------------------------------------------------------------------

#[test]
fn merge_destroy_json_reports_cwd_destroyed_true_from_inside() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# proj\n")]);
    repo.maw_ok(&["ws", "create", "victim"]);
    repo.add_file("victim", "feature.txt", "feature\n");
    let ws_path = repo.workspace_path("victim");

    let out = Command::new(maw_bin())
        .args([
            "ws",
            "merge",
            "victim",
            "--into",
            "default",
            "--destroy",
            "--message",
            "test",
            "--format",
            "json",
        ])
        .current_dir(&ws_path)
        .output()
        .expect("run maw");
    assert!(
        out.status.success(),
        "merge --destroy --format json must succeed"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let v = parse_json(&stdout);
    assert_eq!(
        v["cwd_destroyed"], true,
        "cwd_destroyed must be true:\n{stdout}"
    );
    assert_eq!(v["destroyed"][0], "victim");
}

#[test]
fn merge_destroy_text_warning_is_the_final_line_from_inside() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# proj\n")]);
    repo.maw_ok(&["ws", "create", "victim2"]);
    repo.add_file("victim2", "feature.txt", "feature\n");
    let ws_path = repo.workspace_path("victim2");

    let out = Command::new(maw_bin())
        .args([
            "ws",
            "merge",
            "victim2",
            "--into",
            "default",
            "--destroy",
            "--message",
            "test",
        ])
        .current_dir(&ws_path)
        .output()
        .expect("run maw");
    assert!(out.status.success());

    // Combine stdout+stderr in call order; the destroy-cwd note must be last.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("[OK] merged victim2 into ") && stdout.contains(" @ "),
        "sentinel must be present on stdout:\n{stdout}"
    );
    let last_note = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .expect("stderr should carry the final note");
    assert!(
        last_note.contains("was just destroyed") && last_note.contains("victim2"),
        "destroy-cwd warning must be the final non-empty stderr line:\n{stderr}"
    );
}

#[test]
fn merge_destroy_json_cwd_destroyed_false_from_root() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# proj\n")]);
    repo.maw_ok(&["ws", "create", "bystander"]);
    repo.add_file("bystander", "feature.txt", "feature\n");

    let stdout = repo.maw_ok(&[
        "ws",
        "merge",
        "bystander",
        "--into",
        "default",
        "--destroy",
        "--message",
        "test",
        "--format",
        "json",
    ]);
    let v = parse_json(&stdout);
    assert_eq!(
        v["cwd_destroyed"], false,
        "cwd not inside → false:\n{stdout}"
    );
    assert_eq!(v["destroyed"][0], "bystander");
}

// ---------------------------------------------------------------------------
// (b) Standalone destroy JSON — cwd_destroyed true/false.
// ---------------------------------------------------------------------------

#[test]
fn standalone_destroy_json_reports_cwd_destroyed() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "solo-inside"]);
    let ws_path = repo.workspace_path("solo-inside");

    let out = Command::new(maw_bin())
        .args(["ws", "destroy", "solo-inside", "--format", "json"])
        .current_dir(&ws_path)
        .output()
        .expect("run maw");
    assert!(
        out.status.success(),
        "standalone destroy --format json must succeed"
    );
    let v = parse_json(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(v["status"], "destroyed");
    assert_eq!(v["workspace"], "solo-inside");
    assert_eq!(v["cwd_destroyed"], true);
    assert!(v["recovery"].is_object());

    // From outside → false.
    repo.maw_ok(&["ws", "create", "solo-outside"]);
    let stdout = repo.maw_ok(&["ws", "destroy", "solo-outside", "--format", "json"]);
    let v2 = parse_json(&stdout);
    assert_eq!(v2["cwd_destroyed"], false);
}

// ---------------------------------------------------------------------------
// (e) Sync JSON schema consistency: overlap_hint + conflicted_paths present.
// ---------------------------------------------------------------------------

#[test]
fn sync_json_includes_overlap_hint_for_overlapping_rebase() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "base\n")]);

    // Stale sibling with a committed change on shared.txt.
    repo.maw_ok(&["ws", "create", "stale"]);
    make_commit(&repo, "stale", "shared.txt", "stale edit\n", "stale: edit");

    // Advance the epoch via a merge that also touches shared.txt, WITHOUT
    // auto-rebasing the stale sibling (so we can sync it explicitly next).
    repo.maw_ok(&["ws", "create", "advancer"]);
    make_commit(
        &repo,
        "advancer",
        "shared.txt",
        "advancer edit\n",
        "advancer: edit",
    );
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--into",
        "default",
        "--message",
        "feat: advance",
        "--no-auto-rebase",
    ]);

    let stdout = repo.maw_ok(&["ws", "sync", "stale", "--format", "json"]);
    let v = parse_json(&stdout);
    assert_eq!(
        v["action"], "rebased",
        "stale sibling should have been rebased:\n{stdout}"
    );
    // conflicted_paths key exists (pre-existing bn-mq6j field).
    assert!(
        v.get("conflicted_paths").is_some(),
        "conflicted_paths must be present"
    );
    // overlap_hint added by bn-20fp for schema consistency with merge siblings.
    assert!(
        v["overlap_hint"]["count"].as_u64().unwrap_or(0) >= 1,
        "overlap_hint must report the shared.txt overlap:\n{stdout}"
    );
    assert!(
        v["overlap_hint"]["sample_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "shared.txt")
    );
}
