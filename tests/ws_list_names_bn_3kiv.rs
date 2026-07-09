//! bn-3kiv: `maw ws list --names` (and the `ws ls` / top-level `maw ls`
//! aliases) print a bare, pipeable list of workspace names — one per line,
//! no decoration — so agents/scripts can do
//! `maw ws list --names | xargs -n1 maw ws sync` without parsing JSON.
//!
//! Covers:
//! * exact output for a multi-workspace repo
//! * the "only default" (no extra workspaces created) case
//! * name-set/order consistency with `--format json`'s `workspaces[].name`
//! * the `ws ls` and top-level `maw ls` aliases behave the same way
//! * `--names` conflicts with `--format`/`--json` (mutually exclusive
//!   rendering modes, same pattern as `ws diff --name-only` vs `--json`)

mod manifold_common;

use manifold_common::TestRepo;

/// `default` always exists and is always the first entry in every other
/// list view (bn-2jez); `--names` must include it for the same reason —
/// consistency with `--format json`, not a special-cased omission. Commands
/// that can't act on `default` (e.g. `ws destroy`) already refuse it
/// themselves, so it's safe for a downstream `xargs` pipe to see it.
#[test]
fn names_output_only_default_when_no_extra_workspaces() {
    let repo = TestRepo::new();

    let stdout = repo.maw_ok(&["ws", "list", "--names"]);
    assert_eq!(stdout, "default\n");
}

#[test]
fn names_output_is_exact_bare_lines_no_decoration() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    let stdout = repo.maw_ok(&["ws", "list", "--names"]);

    // Exact: one name per line, default first, then alphabetical — nothing
    // else (no headers, no paths, no annotations, no color).
    assert_eq!(stdout, "default\nalice\nbob\n");
}

#[test]
fn names_output_matches_json_name_set_and_order() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "zeta"]);
    repo.maw_ok(&["ws", "create", "alice"]);

    let names_stdout = repo.maw_ok(&["ws", "list", "--names"]);
    let names: Vec<&str> = names_stdout.lines().collect();

    let json_stdout = repo.maw_ok(&["ws", "list", "--format", "json"]);
    let json: serde_json::Value =
        serde_json::from_str(&json_stdout).expect("ws list --format json should be valid JSON");
    let json_names: Vec<&str> = json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .map(|w| w["name"].as_str().expect("name should be a string"))
        .collect();

    assert_eq!(
        names, json_names,
        "--names output must match --format json's workspaces[].name set and order"
    );
}

#[test]
fn ws_ls_alias_supports_names() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "alice"]);

    let via_list = repo.maw_ok(&["ws", "list", "--names"]);
    let via_ls = repo.maw_ok(&["ws", "ls", "--names"]);

    assert_eq!(via_list, via_ls);
    assert_eq!(via_ls, "default\nalice\n");
}

#[test]
fn top_level_ls_alias_supports_names() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "alice"]);

    let via_ws_list = repo.maw_ok(&["ws", "list", "--names"]);
    let via_top_level_ls = repo.maw_ok(&["ls", "--names"]);

    assert_eq!(via_ws_list, via_top_level_ls);
}

#[test]
fn names_conflicts_with_format() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "list", "--names", "--format", "json"]);
    assert!(
        stderr.contains("--names") && stderr.contains("--format"),
        "expected a clap conflict error mentioning both flags, got: {stderr}"
    );
}

#[test]
fn names_conflicts_with_json_shorthand() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "list", "--names", "--json"]);
    assert!(
        stderr.contains("--names"),
        "expected a clap conflict error mentioning --names, got: {stderr}"
    );
}
