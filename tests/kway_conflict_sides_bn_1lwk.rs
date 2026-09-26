//! bn-1lwk: a k-way conflict must list EVERY workspace that edited the
//! conflicted region, not just the pair at the first conflicting fold step.
//!
//! Before the fix, `maw ws merge wa wb wc` with all three rewriting the same
//! line reported `Workspaces: wa, wb`; `wc` was invisible in the conflict
//! record, the last-conflict snapshot and `maw ws conflicts` JSON, and
//! `--resolve cf-X=wc` failed with "Workspace 'wc' is not a side in this
//! conflict" — every available resolution silently discarded wc's committed
//! edit (Prime Invariant class).

mod manifold_common;

use manifold_common::TestRepo;
use serde_json::Value;

fn setup_three_way_same_line() -> TestRepo {
    let repo = TestRepo::new();
    repo.seed_files(&[("x.rs", "line one\n")]);
    for ws in ["wa", "wb", "wc"] {
        repo.maw_ok(&["ws", "create", ws]);
        repo.modify_file(ws, "x.rs", &format!("line {ws}\n"));
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
        "m3",
    ]);
    assert!(
        !out.status.success(),
        "three-way same-line merge must conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    repo
}

fn last_conflict(repo: &TestRepo) -> Value {
    let out = repo.maw_ok(&["merge", "last-conflict", "--format", "json"]);
    serde_json::from_str(&out).expect("last-conflict json")
}

#[test]
fn three_way_same_line_lists_and_resolves_every_workspace() {
    let repo = setup_three_way_same_line();

    // 1. Persisted snapshot lists all three sides.
    let snap = last_conflict(&repo);
    let conflicts = snap["snapshot"]["conflicts"]
        .as_array()
        .expect("conflicts array");
    assert_eq!(conflicts.len(), 1, "one conflict expected: {snap}");
    let cf = &conflicts[0];
    let id = cf["id"].as_str().expect("conflict id").to_owned();
    let sides: Vec<&str> = cf["sides"]
        .as_array()
        .expect("sides")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(sides, vec!["wa", "wb", "wc"], "snapshot sides: {snap}");

    // 2. Engine-derived conflicts JSON: sides and every atom name wc.
    let out = repo.maw_raw(&["ws", "conflicts", "wa", "wb", "wc", "--format", "json"]);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "conflicts json: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let c = &json["conflicts"][0];
    let side_names: Vec<&str> = c["sides"]
        .as_array()
        .expect("sides")
        .iter()
        .filter_map(|s| s["workspace"].as_str())
        .collect();
    assert_eq!(side_names, vec!["wa", "wb", "wc"], "conflicts json: {json}");
    let atoms = c["atoms"].as_array().expect("atoms");
    assert!(!atoms.is_empty(), "atoms expected: {json}");
    for atom in atoms {
        assert!(
            atom["edits"]
                .as_array()
                .expect("edits")
                .iter()
                .any(|e| e["workspace"] == "wc"),
            "every atom must carry wc's edit: {atom}"
        );
    }

    // 3. wc is selectable and its content lands in default.
    repo.maw_ok(&[
        "ws",
        "merge",
        "wa",
        "wb",
        "wc",
        "--into",
        "default",
        "--message",
        "m3",
        "--resolve",
        &format!("{id}=wc"),
    ]);
    assert_eq!(
        repo.read_file("default", "x.rs").as_deref(),
        Some("line wc\n"),
        "resolving to wc must publish wc's committed edit"
    );
}
