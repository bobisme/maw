//! bn-1du0: `regenerate` merge drivers must be last-resort only.
//!
//! Field report (continuum, 2026-08-11): maw's built-in `Cargo.lock` driver
//! (`kind = "regenerate"`, `command = "cargo generate-lockfile"`) fired for
//! *every* merge that touched the path — unique paths included. Regeneration
//! re-resolves every dependency to the newest compatible version, so merges
//! published lockfiles that neither side had written, silently reverting
//! deliberate pins both sides agreed on.
//!
//! The rule these tests pin down:
//!
//! 1. unique path (one workspace touched it) → driver never fires,
//! 2. shared path that diff3 merged cleanly → driver never fires,
//! 3. shared path with a real textual conflict → driver fires, and the
//!    rewrite is announced with a `NOTE:` line.
//!
//! The driver command is a sentinel (`printf 'REGENERATED' > Cargo.lock`), so
//! the tests never need cargo or the network. `REGENERATED` appearing in the
//! merged file means the driver fired.

mod manifold_common;

use manifold_common::TestRepo;

/// Sentinel driver: any firing overwrites `Cargo.lock` with `REGENERATED`.
const SENTINEL_DRIVER_CONFIG: &str = r#"[repo]
branch = "main"

[[merge.drivers]]
match = "Cargo.lock"
kind = "regenerate"
command = "printf 'REGENERATED\n' > Cargo.lock"
"#;

/// Install the sentinel `regenerate` driver in `.manifold/config.toml`.
fn install_sentinel_driver(repo: &TestRepo) {
    install_driver_config(repo, SENTINEL_DRIVER_CONFIG);
}

/// Overwrite `.manifold/config.toml` with `body`.
fn install_driver_config(repo: &TestRepo, body: &str) {
    let cfg_path = repo.root().join(".manifold").join("config.toml");
    std::fs::write(&cfg_path, body).expect("write config.toml");
}

/// Commit the current working-copy state of a workspace.
fn commit_workspace(repo: &TestRepo, ws: &str, message: &str) {
    repo.git_in_workspace(ws, &["add", "-A"]);
    repo.git_in_workspace(ws, &["commit", "-m", message]);
}

/// Run `maw ws merge` and assert it succeeded, returning `(stdout, stderr)`.
fn merge_ok(repo: &TestRepo, workspaces: &[&str], message: &str) -> (String, String) {
    let mut args = vec!["ws", "merge"];
    args.extend_from_slice(workspaces);
    args.extend_from_slice(&["--destroy", "--message", message]);

    let out = repo.maw_raw(&args);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

// ---------------------------------------------------------------------------
// 1. Unique path — regeneration is pure data loss, so it must not happen.
// ---------------------------------------------------------------------------

#[test]
fn regenerate_driver_never_fires_on_unique_path() {
    let repo = TestRepo::new();
    repo.seed_files(&[(
        "Cargo.lock",
        "# base lock\nname = \"blake3\"\nversion = \"1.8.4\"\n",
    )]);
    install_sentinel_driver(&repo);

    // Only alice touches Cargo.lock — a deliberate pin bump.
    let alice_lock = "# base lock\nname = \"blake3\"\nversion = \"1.8.5\"\n";
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "Cargo.lock", alice_lock);
    commit_workspace(&repo, "alice", "chore: pin blake3 1.8.5");

    let (stdout, stderr) = merge_ok(&repo, &["alice"], "merge alice");

    let merged = repo
        .read_file("default", "Cargo.lock")
        .expect("Cargo.lock should exist after merge");
    assert_eq!(
        merged, alice_lock,
        "unique-path content must survive byte-identical"
    );
    assert!(
        !merged.contains("REGENERATED"),
        "regenerate driver fired on a unique path:\n{merged}"
    );
    assert!(
        !stdout.contains("NOTE: merge driver") && !stderr.contains("NOTE: merge driver"),
        "no driver rewrite should be reported\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// 2. Shared path that diff3 merges cleanly — keep the merged content.
// ---------------------------------------------------------------------------

#[test]
fn regenerate_driver_never_fires_on_clean_shared_merge() {
    let repo = TestRepo::new();
    let base = "[[package]]\nname = \"alpha\"\nversion = \"1.0.0\"\n\
                \n\
                filler1\nfiller2\nfiller3\nfiller4\nfiller5\nfiller6\n\
                \n\
                [[package]]\nname = \"omega\"\nversion = \"1.0.0\"\n";
    repo.seed_files(&[("Cargo.lock", base)]);
    install_sentinel_driver(&repo);

    // alice bumps the first package, bob bumps the last — disjoint regions.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file(
        "alice",
        "Cargo.lock",
        &base.replace(
            "name = \"alpha\"\nversion = \"1.0.0\"",
            "name = \"alpha\"\nversion = \"1.1.0\"",
        ),
    );
    commit_workspace(&repo, "alice", "chore: bump alpha");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file(
        "bob",
        "Cargo.lock",
        &base.replace(
            "name = \"omega\"\nversion = \"1.0.0\"",
            "name = \"omega\"\nversion = \"2.0.0\"",
        ),
    );
    commit_workspace(&repo, "bob", "chore: bump omega");

    let (stdout, stderr) = merge_ok(&repo, &["alice", "bob"], "merge alice + bob");

    let merged = repo
        .read_file("default", "Cargo.lock")
        .expect("Cargo.lock should exist after merge");
    assert!(
        !merged.contains("REGENERATED"),
        "regenerate driver fired on a cleanly merged path:\n{merged}"
    );
    assert!(
        merged.contains("name = \"alpha\"\nversion = \"1.1.0\""),
        "lost alice's clean edit:\n{merged}"
    );
    assert!(
        merged.contains("name = \"omega\"\nversion = \"2.0.0\""),
        "lost bob's clean edit:\n{merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "clean diff3 result must have no markers:\n{merged}"
    );
    assert!(
        !stdout.contains("NOTE: merge driver") && !stderr.contains("NOTE: merge driver"),
        "no driver rewrite should be reported\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// 3. Real textual conflict — the driver fires and says so.
// ---------------------------------------------------------------------------

#[test]
fn regenerate_driver_fires_on_true_conflict_and_reports_note() {
    let repo = TestRepo::new();
    repo.seed_files(&[(
        "Cargo.lock",
        "[[package]]\nname = \"blake3\"\nversion = \"1.8.4\"\n",
    )]);
    install_sentinel_driver(&repo);

    // Both workspaces rewrite the same line — diff3 cannot resolve this.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file(
        "alice",
        "Cargo.lock",
        "[[package]]\nname = \"blake3\"\nversion = \"1.8.5\"\n",
    );
    commit_workspace(&repo, "alice", "chore: blake3 1.8.5");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file(
        "bob",
        "Cargo.lock",
        "[[package]]\nname = \"blake3\"\nversion = \"1.8.6\"\n",
    );
    commit_workspace(&repo, "bob", "chore: blake3 1.8.6");

    let (stdout, stderr) = merge_ok(&repo, &["alice", "bob"], "merge alice + bob");

    let merged = repo
        .read_file("default", "Cargo.lock")
        .expect("Cargo.lock should exist after merge");
    assert!(
        merged.contains("REGENERATED"),
        "regenerate driver should resolve a true conflict:\n{merged}"
    );

    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("NOTE: merge driver"),
        "driver rewrite must be announced\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        combined.contains("regenerated Cargo.lock"),
        "NOTE must name the rewritten path\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        combined.contains("textual conflict"),
        "NOTE must name why the driver fired\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// 4. JSON output carries the same rewrite record as the text NOTE.
// ---------------------------------------------------------------------------

#[test]
fn regenerate_driver_rewrite_appears_in_merge_json() {
    let repo = TestRepo::new();
    repo.seed_files(&[("Cargo.lock", "version = \"1.0.0\"\n")]);
    install_sentinel_driver(&repo);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "Cargo.lock", "version = \"1.1.0\"\n");
    commit_workspace(&repo, "alice", "chore: alice bump");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file("bob", "Cargo.lock", "version = \"1.2.0\"\n");
    commit_workspace(&repo, "bob", "chore: bob bump");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--destroy",
        "--message",
        "merge alice + bob",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("merge JSON must parse: {e}\n{stdout}"));
    let rewrites = parsed["driver_rewrites"]
        .as_array()
        .unwrap_or_else(|| panic!("driver_rewrites missing from merge JSON:\n{stdout}"));
    assert_eq!(rewrites.len(), 1, "expected one rewrite:\n{stdout}");
    assert_eq!(rewrites[0]["path"], "Cargo.lock");
    assert_eq!(rewrites[0]["kind"], "regenerate");
    assert_eq!(rewrites[0]["reason"], "textual conflict");
}

// ---------------------------------------------------------------------------
// 5. `ours` on a unique path is the same data-loss class — it must not fire.
// ---------------------------------------------------------------------------

/// An `ours` driver keeps the epoch version. On a *unique* path that means
/// reverting the only workspace that touched the file, which is the same
/// silent revert the `regenerate` gate exists to prevent.
#[test]
fn ours_driver_never_reverts_a_unique_path() {
    let repo = TestRepo::new();
    repo.seed_files(&[("NOTES.md", "epoch notes\n")]);
    install_driver_config(
        &repo,
        "[repo]\nbranch = \"main\"\n\n[[merge.drivers]]\nmatch = \"NOTES.md\"\nkind = \"ours\"\n",
    );

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "NOTES.md", "alice notes\n");
    commit_workspace(&repo, "alice", "docs: alice notes");

    let (stdout, stderr) = merge_ok(&repo, &["alice"], "merge alice");

    let merged = repo
        .read_file("default", "NOTES.md")
        .expect("NOTES.md should exist after merge");
    assert_eq!(
        merged, "alice notes\n",
        "`ours` reverted the only side that touched the path\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// The same `ours` driver still overrides a *shared* path: two workspaces
/// disagreed, so picking the epoch side is a real, user-declared resolution.
#[test]
fn ours_driver_still_overrides_a_shared_path() {
    let repo = TestRepo::new();
    repo.seed_files(&[("NOTES.md", "epoch notes\n")]);
    install_driver_config(
        &repo,
        "[repo]\nbranch = \"main\"\n\n[[merge.drivers]]\nmatch = \"NOTES.md\"\nkind = \"ours\"\n",
    );

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "NOTES.md", "alice notes\n");
    commit_workspace(&repo, "alice", "docs: alice notes");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file("bob", "NOTES.md", "bob notes\n");
    commit_workspace(&repo, "bob", "docs: bob notes");

    let (stdout, stderr) = merge_ok(&repo, &["alice", "bob"], "merge alice + bob");

    let merged = repo
        .read_file("default", "NOTES.md")
        .expect("NOTES.md should exist after merge");
    assert_eq!(merged, "epoch notes\n", "`ours` should keep the epoch side");

    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("NOTE: merge driver"),
        "the override must be announced\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// 6. A failed non-required driver must hand the conflict back, not swallow it.
// ---------------------------------------------------------------------------

/// With `required = false` a failing regenerate command falls back to the
/// normal merge. Because the driver now fires *only* on conflicts, that
/// fallback has to restore the conflict record — dropping it would publish the
/// epoch content and lose both sides silently.
#[test]
fn failed_optional_regenerate_restores_the_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("Cargo.lock", "version = \"1.0.0\"\n")]);
    install_driver_config(
        &repo,
        "[repo]\nbranch = \"main\"\n\n[[merge.drivers]]\nmatch = \"Cargo.lock\"\n\
         kind = \"regenerate\"\ncommand = \"exit 19\"\nrequired = false\n",
    );

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "Cargo.lock", "version = \"1.1.0\"\n");
    commit_workspace(&repo, "alice", "chore: alice bump");

    repo.maw_ok(&["ws", "create", "bob"]);
    repo.modify_file("bob", "Cargo.lock", "version = \"1.2.0\"\n");
    commit_workspace(&repo, "bob", "chore: bob bump");

    let out = repo.maw_raw(&["ws", "merge", "alice", "bob", "--message", "merge"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        !out.status.success(),
        "a swallowed conflict would let the merge succeed on epoch content\n{combined}"
    );
    assert!(
        combined.contains("conflict"),
        "the restored conflict must be reported\n{combined}"
    );

    let merged = repo
        .read_file("default", "Cargo.lock")
        .expect("Cargo.lock should exist");
    assert_eq!(
        merged, "version = \"1.0.0\"\n",
        "aborted merge must leave the default workspace untouched\n{combined}"
    );
}
