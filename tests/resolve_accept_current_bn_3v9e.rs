//! Regression coverage for bn-3v9e: recorded-side resolution must never
//! overwrite a hand-resolved worktree, and accepting current bytes must be an
//! explicit, marker-free operation.

mod manifold_common;

use manifold_common::TestRepo;

fn setup_committed_conflict(repo: &TestRepo) {
    repo.seed_files(&[("shared.txt", "line1\nshared\nline3\n")]);
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);

    repo.add_file("a", "shared.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "shared.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);
    assert!(
        repo.read_conflict_tree_sidecar("b").is_some(),
        "auto-rebase should leave b in structured conflict state"
    );
}

fn setup_two_committed_conflicts(repo: &TestRepo) {
    repo.seed_files(&[("one.txt", "base one\n"), ("two.txt", "base two\n")]);
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "one.txt", "a one\n");
    repo.add_file("a", "two.txt", "a two\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-qm", "a changes"]);
    repo.add_file("b", "one.txt", "b one\n");
    repo.add_file("b", "two.txt", "b two\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "b changes"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);
}

fn setup_named_committed_conflict(repo: &TestRepo, path: &str) {
    repo.seed_files(&[(path, "base\n")]);
    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", path, "from a\n");
    repo.git_in_workspace("a", &["add", "--", path]);
    repo.git_in_workspace("a", &["commit", "-qm", "a change"]);
    repo.add_file("b", path, "from b\n");
    repo.git_in_workspace("b", &["add", "--", path]);
    repo.git_in_workspace("b", &["commit", "-qm", "b change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);
}

fn placeholder_header(content: &str) -> &str {
    let end = content
        .find("\n\n")
        .expect("structured placeholder should contain a header separator")
        + 2;
    &content[..end]
}

#[test]
fn keep_refuses_hand_resolution_and_accept_current_preserves_it() {
    let repo = TestRepo::new();
    setup_committed_conflict(&repo);

    let path = repo.workspace_path("b").join("shared.txt");
    let placeholder = std::fs::read_to_string(&path).expect("read placeholder");
    let manual_body = "line1\nMANUAL_WITH_INSTANT\nline3\n";
    let hand_resolved = format!("{}{manual_body}", placeholder_header(&placeholder));
    std::fs::write(&path, &hand_resolved).expect("write hand resolution");

    // An unrelated staged change is deliberately present: the resolution
    // auto-commit must not sweep it into HEAD.
    repo.add_file("b", "unrelated.txt", "keep staged\n");
    repo.git_in_workspace("b", &["add", "unrelated.txt"]);

    let keep = repo.maw_raw(&["ws", "resolve", "b", "--keep", "b"]);
    assert!(
        !keep.status.success(),
        "--keep must refuse a hand-edited selected path"
    );
    let keep_stderr = String::from_utf8_lossy(&keep.stderr);
    assert!(
        keep_stderr.contains("Refusing --keep") && keep_stderr.contains("--accept-current"),
        "refusal must name the safe next command; got:\n{keep_stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read after refusal"),
        hand_resolved,
        "the refusal must be atomic and preserve every byte"
    );

    let list = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        list.contains("awaits explicit acceptance") && list.contains("--accept-current"),
        "list output must identify the manual-resolution state; got:\n{list}"
    );

    let accept = repo.maw_raw(&["ws", "resolve", "b", "--accept-current", "--", "shared.txt"]);
    assert!(
        accept.status.success(),
        "accept-current should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&accept.stdout),
        String::from_utf8_lossy(&accept.stderr)
    );
    let stdout = String::from_utf8_lossy(&accept.stdout);
    assert!(
        stdout.contains("does not verify the build or tests"),
        "success must retain the verification gate; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("ready for merge"),
        "resolve must not overclaim merge readiness; got:\n{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read accepted content"),
        manual_body,
        "accept-current must strip only the validated maw header"
    );
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "accepted conflict metadata should be cleared"
    );
    let staged = repo.git_in_workspace("b", &["diff", "--cached", "--name-only"]);
    assert_eq!(
        staged.trim(),
        "unrelated.txt",
        "path-limited auto-commit must leave unrelated staged work untouched"
    );
}

#[test]
fn accept_current_preserves_header_free_manual_bytes_exactly() {
    let repo = TestRepo::new();
    setup_committed_conflict(&repo);

    let path = repo.workspace_path("b").join("shared.txt");
    let manual = "line1\nHEADER_FREE_MANUAL\nline3\n";
    std::fs::write(&path, manual).expect("write manual content");

    let keep = repo.maw_raw(&["ws", "resolve", "b", "--keep", "b"]);
    assert!(!keep.status.success(), "--keep must refuse manual bytes");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), manual);

    let accepted_json = repo.maw_ok(&[
        "ws",
        "resolve",
        "b",
        "shared.txt",
        "--accept-current",
        "--format",
        "json",
    ]);
    let accepted: serde_json::Value =
        serde_json::from_str(&accepted_json).expect("accept-current JSON must be valid");
    assert_eq!(accepted["conflicts_remaining"].as_u64(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&path).expect("read accepted content"),
        manual,
        "header-free manual bytes must remain byte-exact"
    );
}

#[test]
fn accept_current_refuses_untouched_conflict_markers() {
    let repo = TestRepo::new();
    setup_committed_conflict(&repo);

    let path = repo.workspace_path("b").join("shared.txt");
    let before = std::fs::read(&path).expect("read placeholder");
    let accept = repo.maw_raw(&["ws", "resolve", "b", "shared.txt", "--accept-current"]);
    assert!(!accept.status.success(), "markers must block acceptance");
    let stderr = String::from_utf8_lossy(&accept.stderr);
    assert!(
        stderr.contains("still contains conflict-marker lines"),
        "refusal should explain the marker gate; got:\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read after refusal"),
        before,
        "marker refusal must not mutate the file"
    );
    assert!(
        repo.read_conflict_tree_sidecar("b").is_some(),
        "marker refusal must retain conflict metadata"
    );
}

#[test]
fn committed_manual_resolution_prunes_only_its_sidecar_entry() {
    let repo = TestRepo::new();
    setup_two_committed_conflicts(&repo);

    let one = repo.workspace_path("b").join("one.txt");
    std::fs::write(&one, "resolved one\n").expect("write one resolution");
    repo.git_in_workspace("b", &["add", "one.txt"]);
    repo.git_in_workspace("b", &["commit", "-qm", "resolve one only"]);

    let listed = repo.maw_ok(&["ws", "resolve", "b", "--list", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_str(&listed).expect("parse resolve json");
    assert_eq!(
        json["conflict_count"].as_u64(),
        Some(1),
        "only two.txt should remain unresolved: {json}"
    );
    let sidecar = repo
        .read_conflict_tree_sidecar("b")
        .expect("partial conflict sidecar should survive");
    let conflicts = sidecar["conflicts"]
        .as_object()
        .expect("conflicts should be an object");
    assert!(
        !conflicts.contains_key("one.txt") && conflicts.contains_key("two.txt"),
        "stale entries must be pruned per path: {sidecar}"
    );

    let conflict_view = repo.maw_raw(&["ws", "conflicts", "b", "--format", "json"]);
    assert!(
        !conflict_view.status.success(),
        "unresolved conflict state should remain blocking"
    );
    let conflict_json: serde_json::Value = serde_json::from_slice(&conflict_view.stdout)
        .expect("parse conflicts json from blocking result");
    assert_eq!(
        conflict_json["conflict_count"].as_u64(),
        Some(1),
        "ws conflicts must agree with resolve and merge state: {conflict_json}"
    );
}

#[test]
fn accept_current_repairs_header_only_placeholder_without_sidecar() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "feat"]);
    let content = "# structured conflict at orphan.txt\n\
                   # base blob: 0000000000000000000000000000000000000000\n\
                   # side epoch blob: 1111111111111111111111111111111111111111\n\
                   # side feat blob: 2222222222222222222222222222222222222222\n\
                   \n\
                   manually resolved\n";
    repo.add_file("feat", "orphan.txt", content);
    repo.git_in_workspace("feat", &["add", "orphan.txt"]);
    repo.git_in_workspace("feat", &["commit", "-qm", "orphan placeholder"]);

    let accepted = repo.maw_raw(&["ws", "resolve", "feat", "orphan.txt", "--accept-current"]);
    assert!(
        accepted.status.success(),
        "accept-current should repair an orphan header-only placeholder\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&accepted.stdout),
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.workspace_path("feat").join("orphan.txt"))
            .expect("read repaired file"),
        "manually resolved\n"
    );
}

#[test]
fn accept_current_refuses_placeholder_header_for_a_different_path() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "feat"]);
    let content = "# structured conflict at another.txt\n\nmanual\n";
    repo.add_file("feat", "orphan.txt", content);
    repo.git_in_workspace("feat", &["add", "orphan.txt"]);
    repo.git_in_workspace("feat", &["commit", "-qm", "malformed orphan"]);

    let accepted = repo.maw_raw(&[
        "ws",
        "resolve",
        "feat",
        "--accept-current",
        "--",
        "orphan.txt",
    ]);
    assert!(
        !accepted.status.success(),
        "wrong-path header must be refused"
    );
    assert_eq!(
        std::fs::read_to_string(repo.workspace_path("feat").join("orphan.txt"))
            .expect("read refused file"),
        content,
        "header validation failure must not mutate the file"
    );
}

#[test]
fn structured_json_escapes_conflict_paths() {
    let repo = TestRepo::new();
    let path = "quoted\"name.txt";
    setup_named_committed_conflict(&repo, path);

    let listed = repo.maw_ok(&["ws", "resolve", "b", "--list", "--format", "json"]);
    let listed: serde_json::Value =
        serde_json::from_str(&listed).expect("list output must escape special path characters");
    assert_eq!(listed["conflicts"][0]["path"].as_str(), Some(path));

    std::fs::write(repo.workspace_path("b").join(path), "manual\n")
        .expect("write manual resolution");
    let accepted = repo.maw_ok(&[
        "ws",
        "resolve",
        "b",
        "--accept-current",
        "--format",
        "json",
        "--",
        path,
    ]);
    let accepted: serde_json::Value = serde_json::from_str(&accepted)
        .expect("accept-current output must escape special path characters");
    assert_eq!(accepted["accepted_current"][0].as_str(), Some(path));
}
