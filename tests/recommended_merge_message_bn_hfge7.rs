//! bn-hfge7: every command maw recommends that lands a merge carries
//! `--message`, because `maw ws merge` without it fails when stdin is not a
//! terminal (agents, scripts, CI). bn-39nsp fixed the help surfaces; this
//! covers the machine-readable `recommended_command` / `recommended_action`
//! fields of `maw ws destroy` and every other "next command" hint in the CLI
//! source.
//!
//! Dynamic: the destroy preview / refusal recommendations are run exactly as
//! written with stdin closed and must land the work.
//! Static: a scan of `crates/maw-cli/src` finds no landing-merge suggestion
//! without `--message`.

mod manifold_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use manifold_common::{TestRepo, maw_bin};

/// Run a recommended shell command as an agent would: non-interactive stdin,
/// `maw` resolved to the binary under test.
fn run_as_agent(repo: &TestRepo, cmd: &str) -> std::process::Output {
    let bin = maw_bin();
    let dir = bin.parent().expect("bin dir").to_path_buf();
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(repo.root())
        .env("PATH", path)
        .stdin(Stdio::null())
        .output()
        .expect("run sh")
}

fn assert_lands(repo: &TestRepo, cmd: &str, ws: &str, file: &str) {
    let out = run_as_agent(repo, cmd);
    assert!(
        out.status.success(),
        "recommended command must work non-interactively as written:\n{cmd}\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!repo.workspace_exists(ws), "--destroy removed {ws}");
    assert!(
        repo.default_workspace().join(file).exists(),
        "{file} landed in default after `{cmd}`"
    );
}

#[test]
fn destroy_preview_recommended_command_carries_message_and_runs() {
    let repo = TestRepo::new();
    repo.create_workspace("bob");
    repo.add_file("bob", "important.txt", "critical data\n");

    let out = repo.maw_ok(&["ws", "destroy", "bob", "--dry-run", "--format", "json"]);
    let preview: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(preview["action"], "would-refuse");
    let rec = preview["recommended_command"].as_str().expect("string");
    assert!(rec.contains("--message"), "{rec}");
    assert_lands(&repo, rec, "bob", "important.txt");
}

#[test]
fn destroy_force_preview_recommended_command_carries_message() {
    let repo = TestRepo::new();
    repo.create_workspace("carol");
    repo.add_file("carol", "draft.txt", "draft\n");

    let out = repo.maw_ok(&[
        "ws",
        "destroy",
        "carol",
        "--dry-run",
        "--force",
        "--format",
        "json",
    ]);
    let preview: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(preview["action"], "would-force-snapshot");
    let rec = preview["recommended_command"].as_str().expect("string");
    assert!(rec.contains("--message"), "{rec}");
}

/// Extract the first JSON object embedded in `text`.
fn first_json_object(text: &str) -> serde_json::Value {
    let start = text
        .find('{')
        .unwrap_or_else(|| panic!("no JSON in:\n{text}"));
    let slice = &text[start..];
    let mut depth = 0i32;
    for (i, c) in slice.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&slice[..=i]).expect("parse json");
                }
            }
            _ => {}
        }
    }
    panic!("unterminated JSON in:\n{text}");
}

#[test]
fn destroy_refusal_json_recommendations_carry_message_and_run() {
    let repo = TestRepo::new();
    repo.create_workspace("dave");
    repo.add_file("dave", "feature.txt", "feature\n");
    repo.git_in_workspace("dave", &["add", "-A"]);
    repo.git_in_workspace("dave", &["commit", "-m", "feat: feature"]);

    let out = repo.maw_raw(&["ws", "destroy", "dave", "--format", "json"]);
    assert!(!out.status.success(), "committed work refuses destroy");
    let v = first_json_object(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["recommended_action_kind"], "merge-and-destroy");
    let alt = v["merge_destroy_alternative"].as_str().expect("alt");
    assert!(alt.contains("--message"), "{v:#}");
    let rec = v["recommended_action"].as_str().expect("rec");
    assert!(rec.contains("--message"), "{v:#}");
    assert_lands(&repo, rec, "dave", "feature.txt");
}

#[test]
fn destroy_refusal_for_dirty_work_commit_then_merge_runs() {
    let repo = TestRepo::new();
    repo.create_workspace("erin");
    repo.add_file("erin", "wip.txt", "wip\n");

    let out = repo.maw_raw(&["ws", "destroy", "erin", "--format", "json"]);
    assert!(!out.status.success(), "dirty work refuses destroy");
    let v = first_json_object(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["recommended_action_kind"], "commit-then-merge");
    let rec = v["recommended_action"].as_str().expect("rec");
    assert!(
        rec.contains("maw ws merge erin") && rec.contains("--message"),
        "{v:#}"
    );
    assert_lands(&repo, rec, "erin", "wip.txt");
}

// ---------------------------------------------------------------------------
// Static scan
// ---------------------------------------------------------------------------

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir").flatten() {
        let p = entry.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// Non-test source lines, with `\`-continued string lines joined.
fn source_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    let all: Vec<&str> = text.lines().collect();
    for (i, raw) in all.iter().enumerate() {
        if raw.trim_start().starts_with("#[cfg(test)]")
            && all[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("#["))
                .is_some_and(|l| l.trim_start().starts_with("mod "))
        {
            break; // trailing unit-test module
        }
        let trimmed = raw.trim();
        let (n, mut acc) = pending.take().unwrap_or_else(|| (i + 1, String::new()));
        // A `\\` continuation drops the next line's leading whitespace.
        if acc.is_empty() {
            acc.push(' ');
        }
        acc.push_str(trimmed.trim_end_matches('\\'));
        if trimmed.ends_with('\\') {
            pending = Some((n, acc));
        } else {
            lines.push((n, acc));
        }
    }
    if let Some(p) = pending {
        lines.push(p);
    }
    lines
}

/// The merge suggestion starting at `maw ws merge ` in `line`, up to the end
/// of the string literal / inline code / alternative.
fn suggestion(rest: &str) -> &str {
    let end = [
        rest.find('`'),
        rest.find("\\n"),
        rest.find("\\t"),
        rest.find("  "),
        rest.find(" or "),
        rest.find(" | "),
        rest.find(')'),
    ]
    .into_iter()
    .flatten()
    .min()
    .unwrap_or(rest.len());
    // A closing quote that is not an escaped `\"` ends the literal too.
    let bytes = rest.as_bytes();
    let quote = (0..end).find(|&i| bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\'));
    &rest[..quote.unwrap_or(end)]
}

fn is_landing(cmd: &str) -> bool {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    // Prefix match: `--check{reset}` (a format arg glued on) is still --check.
    let has = |f: &str| words.iter().any(|w| w.starts_with(f));
    has("--into")
        && !has("--check")
        && !has("--plan")
        && !has("--dry-run")
        && !has("--recover")
        && !has("--abort")
}

fn has_message(cmd: &str) -> bool {
    cmd.split_whitespace()
        .any(|w| w == "--message" || w == "-m" || w.starts_with("--message="))
}

#[test]
fn no_landing_merge_suggestion_without_message_in_cli_source() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/maw-cli/src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    files.sort();
    let mut offenders = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read");
        for (n, line) in source_lines(&text) {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            let mut from = 0;
            while let Some(rel) = line[from..].find("maw ws merge ") {
                let start = from + rel;
                let cmd = suggestion(&line[start..]);
                if is_landing(cmd) && !has_message(cmd) {
                    offenders.push(format!(
                        "{}:{n}: {cmd}",
                        file.strip_prefix(&src).unwrap_or(file).display()
                    ));
                }
                from = start + "maw ws merge ".len();
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "landing-merge suggestions without --message (fail non-interactively):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn scanner_sanity() {
    assert!(is_landing("maw ws merge a --into default --destroy"));
    assert!(!is_landing("maw ws merge a --into default --check"));
    assert!(!is_landing("maw ws merge --recover"));
    assert!(has_message("maw ws merge a --into default --message \"x\""));
    assert_eq!(
        suggestion("maw ws merge {ws} --into default\"),"),
        "maw ws merge {ws} --into default"
    );
    assert_eq!(
        suggestion("maw ws merge a --into d --message \\\"<msg>\\\"\","),
        "maw ws merge a --into d --message \\\"<msg>\\\""
    );
    assert!(!is_landing("maw ws merge {} --into default --check{reset}"));
    assert_eq!(
        suggestion("maw ws merge <n> --into default --check\\t# dry-run"),
        "maw ws merge <n> --into default --check"
    );
    let joined = source_lines(
        "x \\\n  --message y\n#[cfg(test)]\nfn keep() {}\n#[cfg(test)]\nmod tests {\nz",
    );
    assert_eq!(joined.len(), 3, "{joined:?}");
    assert!(joined[2].1.contains("keep"));
    let joined = source_lines("x \\\n  --message y\n#[cfg(test)]\nmod tests {\nz");
    assert_eq!(joined.len(), 1, "{joined:?}");
    assert!(joined[0].1.contains('x') && joined[0].1.contains("--message"));
}
