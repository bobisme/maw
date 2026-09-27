//! bn-28os: resolving a conflict to one side must keep a non-participant
//! ("bystander") workspace's disjoint edit to the same file.
//!
//! Before the fix, with wa/wb both rewriting line 1 and wc rewriting line 7,
//! the conflict correctly listed only wa and wb — but `--resolve cf-X=wa`
//! replaced the whole file with wa's version, so wc's committed edit was
//! silently absent while the output claimed wc had been merged. wc was never
//! shown as involved, so the user never chose to drop it (Prime Invariant).

mod manifold_common;

use manifold_common::TestRepo;
use serde_json::Value;

const BASE: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7\n";
const WS: [&str; 3] = ["wa", "wb", "wc"];

fn setup() -> (TestRepo, String, Vec<String>) {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", BASE)]);
    for (ws, content) in [
        ("wa", "A1\nl2\nl3\nl4\nl5\nl6\nl7\n"),
        ("wb", "B1\nl2\nl3\nl4\nl5\nl6\nl7\n"),
        ("wc", "l1\nl2\nl3\nl4\nl5\nl6\nC7\n"),
    ] {
        repo.maw_ok(&["ws", "create", ws]);
        repo.modify_file(ws, "f.txt", content);
        repo.git_in_workspace(ws, &["add", "-A"]);
        repo.git_in_workspace(ws, &["commit", "-m", ws]);
    }
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "wa",
        "wb",
        "wc",
        "--into",
        "default",
        "--message",
        "m",
        "--format",
        "json",
    ]);
    assert!(
        !out.status.success(),
        "merge must conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).expect("merge json");
    let conflicts = json["conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1, "{json}");
    let cf = &conflicts[0];
    let workspaces: Vec<&str> = cf["workspaces"]
        .as_array()
        .expect("workspaces")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        workspaces,
        vec!["wa", "wb"],
        "wc must not be a participant: {json}"
    );
    let atoms = cf["atom_ids"]
        .as_array()
        .expect("atom_ids")
        .iter()
        .map(|v| v.as_str().expect("atom id").to_owned())
        .collect();
    (repo, cf["id"].as_str().expect("id").to_owned(), atoms)
}

fn merge_with(repo: &TestRepo, extra: &[&str]) {
    let mut args = vec!["ws", "merge"];
    args.extend_from_slice(&WS);
    args.extend_from_slice(&["--into", "default", "--message", "m"]);
    args.extend_from_slice(extra);
    repo.maw_ok(&args);
}

#[test]
fn file_level_resolution_keeps_bystander_edit() {
    for (pick, first) in [("wa", "A1"), ("wb", "B1")] {
        let (repo, id, _) = setup();
        merge_with(&repo, &["--resolve", &format!("{id}={pick}")]);
        assert_eq!(
            repo.read_file("default", "f.txt").as_deref(),
            Some(format!("{first}\nl2\nl3\nl4\nl5\nl6\nC7\n").as_str()),
            "resolving to {pick} must keep wc's disjoint C7 edit"
        );
    }
}

#[test]
fn atom_level_resolution_keeps_bystander_edit() {
    let (repo, _, atoms) = setup();
    assert_eq!(atoms.len(), 1, "{atoms:?}");
    merge_with(&repo, &["--resolve", &format!("{}=wb", atoms[0])]);
    assert_eq!(
        repo.read_file("default", "f.txt").as_deref(),
        Some("B1\nl2\nl3\nl4\nl5\nl6\nC7\n"),
    );
}

#[test]
fn resolve_all_keeps_bystander_edit() {
    let (repo, _, _) = setup();
    merge_with(&repo, &["--resolve-all", "wa"]);
    assert_eq!(
        repo.read_file("default", "f.txt").as_deref(),
        Some("A1\nl2\nl3\nl4\nl5\nl6\nC7\n"),
    );
}

#[test]
fn content_resolution_warns_when_bystander_edit_is_missing() {
    let (repo, id, _) = setup();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("resolved.txt");
    std::fs::write(&path, "R1\nl2\nl3\nl4\nl5\nl6\nl7\n").expect("write");
    let spec = format!("{id}=content:{}", path.display());
    let mut args = vec!["ws", "merge"];
    args.extend_from_slice(&WS);
    args.extend_from_slice(&["--into", "default", "--message", "m", "--resolve", &spec]);
    let out = repo.maw_raw(&args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("WARNING") && stderr.contains("wc"),
        "user content dropping wc's edit must be called out: {stderr}"
    );
}
