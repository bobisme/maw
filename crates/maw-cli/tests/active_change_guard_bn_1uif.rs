//! Regression coverage for bn-1uif.
//!
//! The active-change ancestry guard must open source workspaces through the
//! selected backend. A consolidated repository stores them under
//! `.maw/workspaces`, not the legacy `ws` directory.

use std::path::Path;
use std::process::{Command, Output};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    command.current_dir(dir).args(args);
    if program == MAW {
        command.env("MAW_LAYOUT", "consolidated");
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program} {args:?}: {error}"))
}

fn succeed(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn maw(dir: &Path, args: &[&str]) -> String {
    succeed(dir, MAW, args)
}

fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("create temp dir");
    let root = temp.path();
    succeed(root, "git", &["init", "-b", "main"]);
    succeed(root, "git", &["config", "user.email", "test@example.com"]);
    succeed(root, "git", &["config", "user.name", "Test"]);
    std::fs::write(root.join("base.txt"), "base\n").expect("write base file");
    succeed(root, "git", &["add", "-A"]);
    succeed(root, "git", &["commit", "-m", "base"]);
    maw(root, &["init"]);
    succeed(root, "git", &["add", "-A"]);
    if !succeed(root, "git", &["status", "--porcelain"]).is_empty() {
        succeed(root, "git", &["commit", "-m", "maw init artifacts"]);
        maw(root, &["epoch", "sync"]);
    }
    temp
}

fn create_active_change(root: &Path) -> String {
    maw(
        root,
        &[
            "changes",
            "create",
            "active change",
            "--from",
            "main",
            "--id",
            "ch-active",
            "--workspace",
            "active",
        ],
    );
    let active = root.join(".maw/workspaces/active");
    std::fs::write(active.join("active.txt"), "active only\n").expect("write active file");
    maw(root, &["exec", "active", "--", "git", "add", "-A"]);
    maw(
        root,
        &[
            "exec",
            "active",
            "--",
            "git",
            "commit",
            "-m",
            "active change commit",
        ],
    );
    let branch = "feat/ch-active-active-change";
    succeed(
        &active,
        "git",
        &["update-ref", &format!("refs/heads/{branch}"), "HEAD"],
    );
    branch.to_owned()
}

fn commit_source(root: &Path, name: &str) {
    let source = root.join(".maw/workspaces").join(name);
    std::fs::write(source.join(format!("{name}.txt")), "source work\n").expect("write source file");
    maw(root, &["exec", name, "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", name, "--", "git", "commit", "-m", "source commit"],
    );
}

#[test]
fn consolidated_unbound_source_passes_check_plan_and_real_merge_with_active_change() {
    let temp = setup();
    let root = temp.path();
    maw(root, &["ws", "create", "source", "--from", "main"]);
    commit_source(root, "source");
    create_active_change(root);

    assert!(root.join(".maw/workspaces/source").is_dir());
    assert!(!root.join("ws/source").exists());
    maw(
        root,
        &[
            "ws", "merge", "source", "--into", "default", "--check", "--format", "json",
        ],
    );
    maw(
        root,
        &["ws", "merge", "source", "--into", "default", "--plan"],
    );
    maw(
        root,
        &[
            "ws",
            "merge",
            "source",
            "--into",
            "default",
            "--message",
            "merge source",
            "--no-auto-rebase",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).expect("read merged file"),
        "source work\n"
    );
    assert!(
        !root.join("active.txt").exists(),
        "an unrelated active change must not land on trunk"
    );
}

#[test]
fn consolidated_unbound_source_with_active_change_ancestry_is_still_refused() {
    let temp = setup();
    let root = temp.path();
    let active_branch = create_active_change(root);
    maw(root, &["ws", "create", "risky", "--from", &active_branch]);

    let trunk_before = succeed(root, "git", &["rev-parse", "HEAD"]);
    for mode in [&["--check"][..], &["--plan"][..], &[][..]] {
        let mut args = vec![
            "ws",
            "merge",
            "risky",
            "--into",
            "default",
            "--message",
            "unsafe merge",
        ];
        args.extend_from_slice(mode);
        let output = run(root, MAW, &args);
        assert!(!output.status.success(), "risky merge {mode:?} must fail");
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("HEAD includes active change 'ch-active'")
                && diagnostic.contains("Refusing merge into 'main'"),
            "unexpected refusal diagnostic:\n{diagnostic}"
        );
        assert!(
            !diagnostic.contains("failed to open repo at"),
            "guard must open the authoritative consolidated path:\n{diagnostic}"
        );
        assert_eq!(
            succeed(root, "git", &["rev-parse", "HEAD"]),
            trunk_before,
            "refusal must not move trunk"
        );
        assert!(!root.join("active.txt").exists());
        assert!(root.join(".maw/workspaces/risky").is_dir());
    }
}

#[test]
fn guard_open_failure_names_authoritative_consolidated_path() {
    let temp = setup();
    let root = temp.path();
    maw(root, &["ws", "create", "source", "--from", "main"]);
    create_active_change(root);

    let source = root.join(".maw/workspaces/source");
    std::fs::rename(source.join(".git"), source.join(".git-disabled"))
        .expect("make source repository unreadable");
    let output = run(
        root,
        MAW,
        &["ws", "merge", "source", "--into", "default", "--check"],
    );
    assert!(
        !output.status.success(),
        "unreadable source must fail closed"
    );
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains(&format!("failed to open repo at {}", source.display())),
        "diagnostic must name the backend-resolved path:\n{diagnostic}"
    );
    assert!(
        !diagnostic.contains(&root.join("ws/source").display().to_string()),
        "diagnostic must not name the legacy path:\n{diagnostic}"
    );
}
