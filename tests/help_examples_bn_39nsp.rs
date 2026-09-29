//! bn-39nsp: the example commands in maw's own help surfaces must work as
//! written when an agent pastes them non-interactively.
//!
//! Surfaces covered: `maw --help` (which embeds `maw tldr`),
//! `maw ws merge --help`, `maw tldr`, and `maw agents show`.
//!
//! Every `maw ...` command is extracted from each surface. Static checks:
//! - every merge example that lands work (has `--into`, is not `--check`,
//!   `--plan` or `--dry-run`) carries `--message` / `-m`, because a merge
//!   without a message fails when stdin is not a terminal;
//! - each surface says `--message` is required non-interactively;
//! - no stale guidance (`--from origin/main`, `maw exec default -- bn`,
//!   hand-editing conflict markers, the nonexistent `--keep-all`);
//! - conflict guidance uses `maw ws resolve <ws> --list` / `--keep`, an
//!   orchestrator is mentioned generically, and pushing is the lead's job.
//!
//! Dynamic check: in a fresh repo, the create / commit / check / merge
//! examples each surface shows are run exactly as written (placeholders
//! substituted) with stdin closed, and the merge must land the work.

mod manifold_common;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use manifold_common::{TestRepo, maw_bin};

/// The help surfaces under test: (label, argv).
const SURFACES: &[(&str, &[&str])] = &[
    ("maw --help", &["--help"]),
    ("maw ws merge --help", &["ws", "merge", "--help"]),
    ("maw tldr", &["tldr"]),
    ("maw agents show", &["agents", "show"]),
];

fn run_maw(cwd: &Path, args: &[&str]) -> Output {
    Command::new(maw_bin())
        .args(args)
        .current_dir(cwd)
        // Agents run non-interactively: no TTY on stdin.
        .stdin(Stdio::null())
        .output()
        .expect("failed to execute maw")
}

fn surface_text(label: &str, args: &[&str]) -> String {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let out = run_maw(dir.path(), args);
    assert!(
        out.status.success(),
        "`{label}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!text.trim().is_empty(), "`{label}` printed nothing");
    text
}

fn all_surfaces() -> Vec<(&'static str, String)> {
    SURFACES
        .iter()
        .map(|(label, args)| (*label, surface_text(label, args)))
        .collect()
}

/// Extract every `maw ...` command mentioned in `text`.
///
/// A command starts at `maw ` (at line start, after whitespace, a backtick,
/// `(` or `:`) and ends at a closing backtick, an inline comment (` #` or a
/// two-space gap), a ` | ` alternative, ` && ` (the next command is then
/// extracted on its own), or end of line.
fn extract_maw_commands(text: &str) -> Vec<String> {
    let mut cmds = Vec::new();
    for line in text.lines() {
        let bytes = line.as_bytes();
        let mut from = 0;
        while let Some(rel) = line[from..].find("maw ") {
            let start = from + rel;
            let boundary_ok =
                start == 0 || matches!(bytes[start - 1], b' ' | b'\t' | b'`' | b'(' | b':');
            if !boundary_ok {
                from = start + 4;
                continue;
            }
            let rest = &line[start..];
            let end = ["`", " #", "  ", " | ", " && "]
                .iter()
                .filter_map(|t| rest.find(t))
                .min()
                .unwrap_or(rest.len());
            let cmd = rest[..end].trim().trim_end_matches(['.', ',', ';']);
            cmds.push(cmd.to_string());
            from = start + end.max(4);
        }
    }
    cmds
}

/// Minimal shell-word splitter (double and single quotes, no escapes).
fn shell_words(cmd: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn is_merge(words: &[String]) -> bool {
    words.len() >= 3
        && words[0] == "maw"
        && (words[1] == "ws" || words[1] == "workspace")
        && words[2] == "merge"
}

fn has_flag(words: &[String], flags: &[&str]) -> bool {
    words.iter().any(|w| {
        flags
            .iter()
            .any(|f| w == f || w.starts_with(&format!("{f}=")))
    })
}

/// A merge example that actually lands work (needs a commit message).
fn is_landing_merge(words: &[String]) -> bool {
    is_merge(words)
        && has_flag(words, &["--into"])
        && !has_flag(
            words,
            &["--check", "--plan", "--dry-run", "--abort", "--recover"],
        )
}

fn has_message(words: &[String]) -> bool {
    has_flag(words, &["--message", "-m"])
}

/// Replace every `<placeholder>` with `name`.
fn fill_placeholders(cmd: &str, name: &str) -> String {
    let mut out = String::new();
    let mut rest = cmd;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(name);
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

#[test]
fn extractor_sanity() {
    let text = "  5. Merge: maw ws merge <name> --into default --message \"feat: x\"  # note\n\
                | Merge | `maw ws merge a --into default` |\n\
                maw exec a -- git add -A && maw exec a -- git commit -m \"m\"\n\
                see .maw/workspaces and maw-agent";
    let cmds = extract_maw_commands(text);
    assert_eq!(
        cmds,
        vec![
            "maw ws merge <name> --into default --message \"feat: x\"",
            "maw ws merge a --into default",
            "maw exec a -- git add -A",
            "maw exec a -- git commit -m \"m\"",
        ]
    );
    let w = shell_words(&cmds[0]);
    assert!(is_landing_merge(&w) && has_message(&w));
    let w = shell_words(&cmds[1]);
    assert!(is_landing_merge(&w) && !has_message(&w));
}

/// Every merge example that lands work carries `--message`, and every
/// surface states that `--message` is required non-interactively.
#[test]
fn every_landing_merge_example_carries_message() {
    for (label, text) in all_surfaces() {
        let merges: Vec<String> = extract_maw_commands(&text)
            .into_iter()
            .filter(|c| is_landing_merge(&shell_words(c)))
            .collect();
        assert!(
            !merges.is_empty(),
            "`{label}` shows no landing merge example (extractor vacuous?):\n{text}"
        );
        for cmd in &merges {
            assert!(
                has_message(&shell_words(cmd)),
                "`{label}` merge example lacks --message; it fails when stdin is not a \
                 terminal: {cmd}"
            );
        }
        let lower = text.to_lowercase().replace('`', "");
        assert!(
            lower.contains("--message is required"),
            "`{label}` must state that --message is required non-interactively"
        );
    }
}

#[test]
fn no_stale_guidance_in_help_surfaces() {
    for (label, text) in all_surfaces() {
        for stale in [
            "origin/main",
            "exec default -- bn",
            "Edit files to remove",
            "--keep-all",
            "<name1> <name2> --into default\n",
        ] {
            assert!(
                !text.contains(stale),
                "`{label}` still carries stale guidance {stale:?}:\n{text}"
            );
        }
    }
}

#[test]
fn help_surfaces_point_at_resolve_orchestrator_and_lead_push() {
    let surfaces = all_surfaces();
    for (label, text) in &surfaces {
        assert!(
            text.contains("orchestrator (e.g. edict)"),
            "`{label}` must say an orchestrator (e.g. edict) may run the merge"
        );
    }
    for (label, text) in &surfaces {
        if *label == "maw ws merge --help" {
            continue;
        }
        assert!(
            text.contains("maw ws resolve <name> --list") && text.contains("--keep"),
            "`{label}` must resolve conflicts with `maw ws resolve <name> --list` / --keep"
        );
        assert!(
            text.contains("lead or orchestrator"),
            "`{label}` must say the lead or orchestrator pushes"
        );
    }
}

/// Run a surface's create / commit / check / merge examples as written in a
/// fresh consolidated repo with stdin closed.
fn run_surface_workflow(label: &str, text: &str) {
    let repo = TestRepo::new_consolidated();
    let root = repo.root().to_path_buf();
    let cmds = extract_maw_commands(text);
    let name = "alice";

    let run = |cmd: &str| {
        let filled = fill_placeholders(cmd, name);
        let words = shell_words(&filled);
        assert_eq!(words[0], "maw", "not a maw command: {filled}");
        let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
        let out = run_maw(&root, &args);
        assert!(
            out.status.success(),
            "`{label}` example failed non-interactively: {filled}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        filled
    };

    // Create: the surface's own `maw ws create ... --from ...` example, if any.
    let create = cmds.iter().find(|c| {
        let w = shell_words(c);
        w.len() >= 3 && w[1] == "ws" && w[2] == "create" && has_flag(&w, &["--from"])
    });
    match create {
        Some(c) => {
            run(c);
        }
        None => {
            repo.maw_ok(&["ws", "create", name, "--from", "main"]);
        }
    }

    repo.add_file(name, "from-help.txt", &format!("written for {label}\n"));

    // Commit: the surface's `git add -A` / `git commit -m` exec examples.
    let add = cmds
        .iter()
        .find(|c| c.starts_with("maw exec <") && c.ends_with("git add -A"));
    let commit = cmds
        .iter()
        .find(|c| c.starts_with("maw exec <") && c.contains("git commit -m"));
    if let (Some(add), Some(commit)) = (add, commit) {
        run(add);
        run(commit);
    } else {
        repo.git_in_workspace(name, &["add", "-A"]);
        repo.git_in_workspace(name, &["commit", "-m", "feat: help example"]);
    }

    // Check (dry-run) then land, using the surface's own examples: the
    // first single-workspace `--into default` merge of each kind.
    let single_into_default = |c: &&String| {
        let w = shell_words(c);
        is_merge(&w)
            && w.len() >= 4
            && !w[3].starts_with('-')
            && (w.len() < 5 || w[4].starts_with('-'))
            && w.windows(2).any(|p| p[0] == "--into" && p[1] == "default")
    };
    if let Some(check) = cmds
        .iter()
        .filter(single_into_default)
        .find(|c| shell_words(c).iter().any(|w| w == "--check") && !c.contains("json"))
    {
        run(check);
    }
    let land = cmds
        .iter()
        .filter(single_into_default)
        .find(|c| {
            let w = shell_words(c);
            is_landing_merge(&w) && has_flag(&w, &["--destroy"]) && !c.contains("json")
        })
        .unwrap_or_else(|| panic!("`{label}` shows no `--into default --destroy` merge example"));
    let ran = run(land);

    assert!(
        root.join("from-help.txt").is_file(),
        "`{label}` merge example `{ran}` did not land the work in default"
    );
}

#[test]
fn help_examples_run_non_interactively_as_written() {
    for (label, text) in all_surfaces() {
        run_surface_workflow(label, &text);
    }
}
