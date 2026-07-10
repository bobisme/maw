//! Integration tests for `maw release prepare` / `maw release preflight`
//! (bn-1obp).
//!
//! These build a throwaway cargo workspace (root virtual manifest + two member
//! crates with an internal path+version dep) and drive the real `maw` binary
//! against it, mirroring the maw repo's own release-bump shape without needing
//! a maw-initialized manifold.

mod manifold_common;

use std::path::Path;
use std::process::Command;

use manifold_common::maw_bin;
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create dir");
    }
    std::fs::write(path, contents).expect("write file");
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a minimal cargo workspace fixture at version `0.1.0`, committed.
fn fixture() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();

    write(
        &root.join("Cargo.toml"),
        r#"[workspace]
members = ["crate-a", "crate-b"]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
"#,
    );
    write(
        &root.join("crate-a/Cargo.toml"),
        r#"[package]
name = "fixture-a"
version.workspace = true
edition.workspace = true

[dependencies]
"#,
    );
    write(&root.join("crate-a/src/lib.rs"), "pub fn a() {}\n");
    write(
        &root.join("crate-b/Cargo.toml"),
        r#"[package]
name = "fixture-b"
version.workspace = true
edition.workspace = true

[dependencies]
fixture-a = { path = "../crate-a", version = "0.1.0" }
"#,
    );
    write(&root.join("crate-b/src/lib.rs"), "pub fn b() {}\n");
    write(
        &root.join("CHANGELOG.md"),
        "# Changelog\n\nAll notable changes.\n\n## v0.1.0 (2026-01-01)\n\nFirst.\n",
    );
    write(&root.join("README.md"), "# fixture\n\nA test workspace.\n");

    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    dir
}

fn run_maw(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(maw_bin())
        .args(args)
        .current_dir(root)
        .output()
        .expect("run maw")
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).expect("read file")
}

#[test]
fn prepare_bumps_lockstep_and_is_idempotent() {
    let dir = fixture();
    let root = dir.path();

    let out = run_maw(root, &["release", "prepare", "v0.2.0"]);
    assert!(
        out.status.success(),
        "prepare failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Workspace version bumped.
    assert!(
        read(root, "Cargo.toml").contains("version = \"0.2.0\""),
        "root version not bumped"
    );
    // Internal path-dep string bumped in lockstep.
    let b = read(root, "crate-b/Cargo.toml");
    assert!(
        b.contains(r#"version = "0.2.0""#),
        "path-dep not bumped: {b}"
    );
    // Cargo.lock regenerated at the new version.
    let lock = read(root, "Cargo.lock");
    assert!(
        lock.contains("name = \"fixture-a\"") && lock.contains("version = \"0.2.0\""),
        "lock not regenerated: {lock}"
    );
    // CHANGELOG section scaffolded.
    assert!(
        read(root, "CHANGELOG.md").contains("## v0.2.0"),
        "changelog section missing"
    );

    // Snapshot every tracked file, then re-run: must be a byte-for-byte no-op.
    let snapshot: Vec<(String, String)> = [
        "Cargo.toml",
        "crate-b/Cargo.toml",
        "Cargo.lock",
        "CHANGELOG.md",
    ]
    .iter()
    .map(|f| ((*f).to_string(), read(root, f)))
    .collect();

    let out2 = run_maw(root, &["release", "prepare", "v0.2.0"]);
    assert!(out2.status.success(), "second prepare failed");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("already prepared") || stdout2.contains("No changes"),
        "second run not reported as no-op: {stdout2}"
    );
    for (f, before) in &snapshot {
        assert_eq!(&read(root, f), before, "{f} changed on idempotent re-run");
    }
}

#[test]
fn preflight_passes_when_consistent_and_names_file_on_skew() {
    let dir = fixture();
    let root = dir.path();

    // Prepare + commit → clean, consistent tree at 0.2.0.
    assert!(
        run_maw(root, &["release", "prepare", "v0.2.0"])
            .status
            .success()
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "bump 0.2.0"]);

    // Clean tree passes preflight.
    let ok = run_maw(root, &["release", "preflight", "v0.2.0"]);
    assert!(
        ok.status.success(),
        "preflight should pass on consistent tree: {}\n{}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr)
    );

    // Skew one internal path-dep string.
    let b_path = root.join("crate-b/Cargo.toml");
    let skewed = std::fs::read_to_string(&b_path)
        .unwrap()
        .replace(r#"version = "0.2.0""#, r#"version = "0.1.0""#);
    std::fs::write(&b_path, skewed).unwrap();

    // --allow-dirty so the ONLY failure surfaced is the skew, not the dirty tree.
    let bad = run_maw(root, &["release", "preflight", "v0.2.0", "--allow-dirty"]);
    assert!(!bad.status.success(), "skew must fail preflight");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(
        combined.contains("skew") && combined.contains("crate-b/Cargo.toml"),
        "skew failure must name the file: {combined}"
    );
}

#[test]
fn prepare_refuses_dirty_tree_outside_edit_surface() {
    let dir = fixture();
    let root = dir.path();
    // A stray non-edit-surface change.
    write(
        &root.join("crate-a/src/lib.rs"),
        "pub fn a() { /* edit */ }\n",
    );

    let out = run_maw(root, &["release", "prepare", "v0.2.0"]);
    assert!(!out.status.success(), "prepare must refuse a dirty tree");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("outside the release edit surface"),
        "unexpected error: {err}"
    );
}
