//! bn-34zv: atom-level `--resolve cf-X.N=<ws>` must splice the chosen side's
//! region text byte-exactly and must keep every clean (non-conflicting) edit
//! made elsewhere in the file.
//!
//! Before the fix the splice rebuilt the file from the BASE, substituting
//! atom text that `parse_diff3_atoms` had stored without its trailing
//! newline: `l1\nwal2\nl3\n` vs `l1\nwbl2\nl3\n` resolved `.0=wb` to
//! `l1\nwbl2l3\n` (lines joined), and any clean edit a participant made
//! outside the conflicted region was silently reverted to base.

mod manifold_common;

use manifold_common::TestRepo;
use serde_json::Value;

/// Commit `content` to `path` in a fresh workspace `ws`.
fn ws_with(repo: &TestRepo, ws: &str, path: &str, content: &str) {
    repo.maw_ok(&["ws", "create", ws]);
    repo.modify_file(ws, path, content);
    repo.git_in_workspace(ws, &["add", "-A"]);
    repo.git_in_workspace(ws, &["commit", "-m", ws]);
}

/// Run the merge expecting a conflict; return (file id, atom ids) of the
/// single reported conflict.
fn conflict_ids(repo: &TestRepo, workspaces: &[&str]) -> (String, Vec<String>) {
    let mut args = vec!["ws", "merge"];
    args.extend_from_slice(workspaces);
    args.extend_from_slice(&["--into", "default", "--message", "m", "--format", "json"]);
    let out = repo.maw_raw(&args);
    assert!(
        !out.status.success(),
        "merge must conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "merge json: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let conflicts = json["conflicts"].as_array().expect("conflicts array");
    assert_eq!(conflicts.len(), 1, "one conflict expected: {json}");
    let id = conflicts[0]["id"].as_str().expect("id").to_owned();
    let atom_ids = conflicts[0]["atom_ids"]
        .as_array()
        .expect("atom_ids")
        .iter()
        .map(|v| v.as_str().expect("atom id").to_owned())
        .collect();
    (id, atom_ids)
}

fn resolve(repo: &TestRepo, workspaces: &[&str], resolutions: &[String]) {
    let mut args: Vec<String> = vec!["ws".into(), "merge".into()];
    args.extend(workspaces.iter().map(|s| (*s).to_owned()));
    args.extend(["--into", "default", "--message", "m"].map(str::to_owned));
    for r in resolutions {
        args.push("--resolve".into());
        args.push(r.clone());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.maw_ok(&refs);
}

#[test]
fn atom_resolution_keeps_region_trailing_newline() {
    for (pick, expected) in [("wb", "l1\nwbl2\nl3\n"), ("wa", "l1\nwal2\nl3\n")] {
        let repo = TestRepo::new();
        repo.seed_files(&[("f.txt", "l1\nl2\nl3\n")]);
        ws_with(&repo, "wa", "f.txt", "l1\nwal2\nl3\n");
        ws_with(&repo, "wb", "f.txt", "l1\nwbl2\nl3\n");
        let (_, atoms) = conflict_ids(&repo, &["wa", "wb"]);
        assert_eq!(atoms.len(), 1, "one atom expected: {atoms:?}");
        resolve(&repo, &["wa", "wb"], &[format!("{}={pick}", atoms[0])]);
        assert_eq!(
            repo.read_file("default", "f.txt").as_deref(),
            Some(expected),
            "atom resolution to {pick} must equal {pick}'s file byte-for-byte"
        );
    }
}

#[test]
fn atom_resolution_keeps_participants_clean_edits_outside_the_region() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")]);
    // wa: inserts two lines at the top (shifts line numbers), conflicts on
    // l5, and cleanly edits l8. wb: conflicts on l5 and cleanly edits l3.
    ws_with(
        &repo,
        "wa",
        "f.txt",
        "new0\nnew1\nl1\nl2\nl3\nl4\nA5\nl6\nl7\nA8\n",
    );
    ws_with(&repo, "wb", "f.txt", "l1\nl2\nB3\nl4\nB5\nl6\nl7\nl8\n");
    let (_, atoms) = conflict_ids(&repo, &["wa", "wb"]);
    assert_eq!(atoms.len(), 1, "one atom expected: {atoms:?}");
    resolve(&repo, &["wa", "wb"], &[format!("{}=wb", atoms[0])]);
    assert_eq!(
        repo.read_file("default", "f.txt").as_deref(),
        Some("new0\nnew1\nl1\nl2\nB3\nl4\nB5\nl6\nl7\nA8\n"),
        "resolving the l5 atom to wb must keep wa's insert + l8 edit and wb's l3 edit"
    );
}

#[test]
fn atom_resolution_mixed_choices_across_two_atoms() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "l1\nl2\nl3\nl4\nl5\nl6\nl7\n")]);
    ws_with(&repo, "wa", "f.txt", "A1\nl2\nl3\nl4\nl5\nl6\nA7");
    ws_with(&repo, "wb", "f.txt", "B1\nl2\nl3\nl4\nl5\nl6\nB7\n");
    let (_, atoms) = conflict_ids(&repo, &["wa", "wb"]);
    assert_eq!(atoms.len(), 2, "two atoms expected: {atoms:?}");
    resolve(
        &repo,
        &["wa", "wb"],
        &[format!("{}=wb", atoms[0]), format!("{}=wa", atoms[1])],
    );
    // wa's last line has no trailing newline; choosing wa for that atom
    // must reproduce that exactly.
    assert_eq!(
        repo.read_file("default", "f.txt").as_deref(),
        Some("B1\nl2\nl3\nl4\nl5\nl6\nA7"),
    );
}
