//! bn-2jml: D/F clash detection must cover shared-vs-shared paths.
//!
//! Before the fix, `partition_by_path` only scanned `unique` entries for D/F
//! clashes. With FILE `a` added by w1+w2 (shared) and `a/b` added by w3+w4
//! (shared), no clash was recorded, both paths resolved cleanly, and the
//! build step wrote a root tree holding a duplicate entry `a` (blob AND
//! tree). `maw ws merge` exited 0 and advanced `main` to that corrupt commit
//! (`git fsck`: "duplicateEntries"), and the default workspace checkout then
//! failed with an IO error.
//!
//! After the fix the merge must refuse with a D/F conflict naming all four
//! workspaces, and the branch must not move.

mod manifold_common;

use manifold_common::TestRepo;

#[test]
fn merge_refuses_shared_file_vs_shared_dir_child_clash() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README", "base\n")]);
    let before = repo.git(&["rev-parse", "main"]);

    for ws in ["w1", "w2", "w3", "w4"] {
        repo.maw_ok(&["ws", "create", ws]);
    }
    repo.add_file("w1", "a", "file\n");
    repo.add_file("w2", "a", "file\n");
    repo.add_file("w3", "a/b", "child\n");
    repo.add_file("w4", "a/b", "child\n");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "w1",
        "w2",
        "w3",
        "w4",
        "--into",
        "default",
        "--message",
        "merge",
    ]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "merge must refuse a D/F clash between shared paths:\n{text}"
    );
    assert!(
        text.contains("D/F clash"),
        "expected a D/F conflict:\n{text}"
    );
    let conflict_line = text
        .lines()
        .find(|l| l.contains("Workspaces:"))
        .unwrap_or_else(|| panic!("no Workspaces: line in conflict report:\n{text}"));
    for ws in ["w1", "w2", "w3", "w4"] {
        assert!(
            conflict_line.contains(ws),
            "D/F conflict must name {ws}: {conflict_line}"
        );
    }

    assert_eq!(
        repo.git(&["rev-parse", "main"]),
        before,
        "main must not advance on an unresolved D/F clash"
    );
    let fsck = manifold_common::git_raw(repo.root(), &["fsck", "--strict", "--no-dangling"]);
    let fsck_text =
        String::from_utf8_lossy(&fsck.stderr).to_string() + &String::from_utf8_lossy(&fsck.stdout);
    assert!(
        !fsck_text.contains("duplicateEntries"),
        "no corrupt tree may be written:\n{fsck_text}"
    );
}

/// Resolving a D/F conflict with a DIRECTORY-side workspace must explain the
/// D/F options and suggest only sides that carry content (bn-2jml follow-up:
/// the old message offered content-less w4 as an alternative to w3).
#[test]
fn resolve_with_directory_side_explains_and_offers_only_content_sides() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README", "base\n")]);
    for ws in ["w1", "w2", "w3", "w4"] {
        repo.maw_ok(&["ws", "create", ws]);
    }
    repo.add_file("w1", "a", "file\n");
    repo.add_file("w2", "a", "file\n");
    repo.add_file("w3", "a/b", "child\n");
    repo.add_file("w4", "a/b", "child\n");
    let merge = |extra: &str| {
        let out = repo.maw_raw(&[
            "ws",
            "merge",
            "w1",
            "w2",
            "w3",
            "w4",
            "--into",
            "default",
            "--message",
            "merge",
            extra,
        ]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    };

    let (ok, text) = merge("--resolve-all=w3");
    assert!(!ok, "directory side has no file content to keep:\n{text}");
    assert!(text.contains("DIRECTORY side"), "must explain D/F:\n{text}");
    let try_line = text
        .lines()
        .find(|l| l.trim_start().starts_with("Try:"))
        .unwrap_or_else(|| panic!("no Try: line:\n{text}"));
    assert!(
        try_line.contains("w1") && try_line.contains("w2"),
        "{try_line}"
    );
    assert!(
        !try_line.contains("w3") && !try_line.contains("w4"),
        "content-less sides must not be suggested: {try_line}"
    );

    let (ok, text) = merge("--resolve-all=w1");
    assert!(ok, "file-side resolution must succeed:\n{text}");
    let tree = repo.git(&["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.lines().any(|l| l == "a"), "{tree}");
    assert!(!tree.lines().any(|l| l.starts_with("a/")), "{tree}");
}
