use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use glob::Pattern;
use serde::Serialize;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceId;
use maw_core::refs as manifold_refs;

use super::{DEFAULT_WORKSPACE, get_backend, repo_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFormat {
    Patch,
    Stat,
    NameOnly,
    NameStatus,
    Json,
}

#[derive(Debug)]
enum AgainstMode {
    Default,
    Epoch,
    Branch(String),
    Oid(String),
}

#[derive(Debug)]
struct ResolvedRev {
    label: String,
    rev: String,
    oid: String,
}

#[derive(Debug, Clone)]
struct DiffEntry {
    path: String,
    old_path: Option<String>,
    status: String,
    additions: u32,
    deletions: u32,
    binary: bool,
}

#[derive(Debug, Serialize)]
struct DiffJsonOutput {
    workspace: String,
    against: DiffRevisionJson,
    head: DiffRevisionJson,
    stats: DiffStatsJson,
    files: Vec<DiffFileJson>,
}

#[derive(Debug, Serialize)]
struct DiffRevisionJson {
    label: String,
    rev: String,
    oid: String,
}

#[derive(Debug, Serialize)]
struct DiffStatsJson {
    files_changed: usize,
    added: usize,
    modified: usize,
    deleted: usize,
    renamed: usize,
    copied: usize,
    others: usize,
    additions: u32,
    deletions: u32,
}

#[derive(Debug, Serialize)]
struct DiffFileJson {
    path: String,
    old_path: Option<String>,
    status: String,
    additions: u32,
    deletions: u32,
    binary: bool,
}

pub fn diff(
    workspace: &str,
    against: Option<&str>,
    format: DiffFormat,
    paths: &[String],
) -> Result<()> {
    let ws_id = WorkspaceId::new(workspace)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{workspace}': {e}"))?;

    let root = repo_root()?;
    let backend = get_backend()?;

    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{workspace}' does not exist\n  Check: maw ws list\n  Fix: maw ws create {workspace}"
        );
    }

    let pathspecs = resolve_pathspecs(paths)?;

    let head = materialize_workspace_state(&backend, &root, &ws_id)?;
    let base = resolve_against(&backend, &root, &ws_id, against)?;

    match format {
        DiffFormat::Patch => print_patch(&root, &base.rev, &head.rev, &pathspecs)?,
        DiffFormat::Stat => print_stat(&root, &base.rev, &head.rev, &pathspecs)?,
        DiffFormat::NameOnly | DiffFormat::NameStatus => {
            let mut entries = collect_diff_entries(&root, &base.rev, &head.rev, &pathspecs)?;
            entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.status.cmp(&b.status)));
            if matches!(format, DiffFormat::NameOnly) {
                let mut names = BTreeSet::new();
                for e in &entries {
                    names.insert(e.path.clone());
                }
                for name in names {
                    println!("{name}");
                }
            } else {
                for e in &entries {
                    if let Some(old) = &e.old_path {
                        println!("{}\t{} -> {}", e.status, old, e.path);
                    } else {
                        println!("{}\t{}", e.status, e.path);
                    }
                }
            }
        }
        DiffFormat::Json => {
            let mut entries = collect_diff_entries(&root, &base.rev, &head.rev, &pathspecs)?;
            entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.status.cmp(&b.status)));
            print_json(&ws_id, &base, &head, &entries)?;
        }
    }

    Ok(())
}

fn print_stat(root: &Path, base_rev: &str, head_rev: &str, pathspecs: &[String]) -> Result<()> {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let mut args = vec![
        "diff".to_string(),
        "--stat".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
        head_rev.to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }

    if is_tty {
        let status = Command::new("git")
            .args(&args)
            .current_dir(root)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Failed to run git diff --stat")?;
        if !status.success() {
            bail!("git diff --stat exited with status {}", status);
        }
    } else {
        args.insert(1, "--color=never".to_string());
        let out = git_stdout(root, &args)?;
        print!("{out}");
    }

    Ok(())
}

fn print_patch(root: &Path, base_rev: &str, head_rev: &str, pathspecs: &[String]) -> Result<()> {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let mut diff_args = vec![
        "diff".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
        head_rev.to_string(),
    ];
    if !pathspecs.is_empty() {
        diff_args.push("--".to_string());
        diff_args.extend(pathspecs.iter().cloned());
    }

    if is_tty {
        // Spawn git with inherited stdio so it uses its configured pager
        // (core.pager / GIT_PAGER, e.g. delta).
        let status = Command::new("git")
            .args(&diff_args)
            .current_dir(root)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Failed to run git diff")?;
        if !status.success() {
            bail!("git diff exited with status {}", status);
        }
    } else {
        // No tty — capture output, no pager, no color.
        diff_args.insert(1, "--color=never".to_string());
        let patch = git_stdout(root, &diff_args)?;
        print!("{patch}");
    }

    Ok(())
}

fn print_json(
    ws_id: &WorkspaceId,
    base: &ResolvedRev,
    head: &ResolvedRev,
    entries: &[DiffEntry],
) -> Result<()> {
    let stats = summarize(entries);
    let out = DiffJsonOutput {
        workspace: ws_id.as_str().to_owned(),
        against: DiffRevisionJson {
            label: base.label.clone(),
            rev: base.rev.clone(),
            oid: base.oid.clone(),
        },
        head: DiffRevisionJson {
            label: head.label.clone(),
            rev: head.rev.clone(),
            oid: head.oid.clone(),
        },
        stats,
        files: entries
            .iter()
            .map(|e| DiffFileJson {
                path: e.path.clone(),
                old_path: e.old_path.clone(),
                status: e.status.clone(),
                additions: e.additions,
                deletions: e.deletions,
                binary: e.binary,
            })
            .collect(),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn summarize(entries: &[DiffEntry]) -> DiffStatsJson {
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut copied = 0usize;
    let mut others = 0usize;
    let mut additions = 0u32;
    let mut deletions = 0u32;

    for e in entries {
        match e.status.as_str() {
            "A" => added += 1,
            "M" => modified += 1,
            "D" => deleted += 1,
            "R" => renamed += 1,
            "C" => copied += 1,
            _ => others += 1,
        }
        additions = additions.saturating_add(e.additions);
        deletions = deletions.saturating_add(e.deletions);
    }

    DiffStatsJson {
        files_changed: entries.len(),
        added,
        modified,
        deleted,
        renamed,
        copied,
        others,
        additions,
        deletions,
    }
}

fn resolve_against<B: WorkspaceBackend>(
    backend: &B,
    root: &Path,
    workspace: &WorkspaceId,
    against_raw: Option<&str>,
) -> Result<ResolvedRev>
where
    B::Error: std::fmt::Display,
{
    match parse_against(against_raw) {
        AgainstMode::Default => {
            if workspace.as_str() == DEFAULT_WORKSPACE {
                return materialize_workspace_state(backend, root, workspace);
            }

            let default_id = WorkspaceId::new(DEFAULT_WORKSPACE)
                .expect("default workspace id should always be valid");
            if !backend.exists(&default_id) {
                bail!(
                    "Default workspace '{DEFAULT_WORKSPACE}' is missing\n  Fix: maw init\n  Check: maw doctor"
                );
            }
            materialize_workspace_state(backend, root, &default_id)
        }
        AgainstMode::Epoch => {
            let rev = manifold_refs::EPOCH_CURRENT.to_string();
            let oid = resolve_rev_oid(root, &rev).with_context(
                || "Current epoch ref is missing\n  Fix: maw init\n  Check: maw doctor",
            )?;
            Ok(ResolvedRev {
                label: "epoch".to_string(),
                rev,
                oid,
            })
        }
        AgainstMode::Branch(branch) => {
            let rev = if branch.starts_with("refs/") {
                branch
            } else {
                format!("refs/heads/{branch}")
            };
            let oid = resolve_rev_oid(root, &rev).with_context(|| {
                format!(
                    "Branch '{rev}' was not found\n  Check: git -C {} branch --list\n  Fix: use --against branch:<name>",
                    root.display()
                )
            })?;
            Ok(ResolvedRev {
                label: "branch".to_string(),
                rev,
                oid,
            })
        }
        AgainstMode::Oid(oid) => {
            let resolved = resolve_rev_oid(root, &oid).with_context(|| {
                format!(
                    "Revision '{oid}' is invalid\n  Check: git -C {} rev-parse {oid}",
                    root.display()
                )
            })?;
            Ok(ResolvedRev {
                label: "oid".to_string(),
                rev: oid,
                oid: resolved,
            })
        }
    }
}

fn materialize_workspace_state<B: WorkspaceBackend>(
    backend: &B,
    root: &Path,
    ws_id: &WorkspaceId,
) -> Result<ResolvedRev>
where
    B::Error: std::fmt::Display,
{
    backend
        .status(ws_id)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("Failed to inspect workspace '{}'.", ws_id.as_str()))?;

    let ws_ref = manifold_refs::workspace_state_ref(ws_id.as_str());
    if let Ok(oid) = resolve_rev_oid(root, &ws_ref) {
        return Ok(ResolvedRev {
            label: ws_id.as_str().to_owned(),
            rev: ws_ref,
            oid,
        });
    }

    let ws_path = backend.workspace_path(ws_id);
    let oid = git_stdout_simple(&ws_path, &["rev-parse", "HEAD"])
        .with_context(|| format!("Failed to resolve HEAD for workspace '{}'.", ws_id.as_str()))?;

    Ok(ResolvedRev {
        label: ws_id.as_str().to_owned(),
        rev: oid.clone(),
        oid,
    })
}

fn parse_against(raw: Option<&str>) -> AgainstMode {
    let Some(raw) = raw else {
        return AgainstMode::Default;
    };

    let value = raw.trim();
    if value.is_empty() || value == "default" {
        return AgainstMode::Default;
    }
    if value == "epoch" {
        return AgainstMode::Epoch;
    }
    if let Some(branch) = value.strip_prefix("branch:") {
        return AgainstMode::Branch(branch.to_string());
    }
    if let Some(oid) = value.strip_prefix("oid:") {
        return AgainstMode::Oid(oid.to_string());
    }
    if looks_like_oid(value) {
        return AgainstMode::Oid(value.to_string());
    }
    AgainstMode::Branch(value.to_string())
}

fn looks_like_oid(value: &str) -> bool {
    let len = value.len();
    len >= 7 && len <= 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_pathspecs(paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in paths {
        let pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        Pattern::new(pattern)
            .with_context(|| format!("Invalid path glob '{pattern}' in --paths"))?;
        out.push(format!(":(glob){pattern}"));
    }
    Ok(out)
}

fn collect_diff_entries(
    root: &Path,
    base_rev: &str,
    head_rev: &str,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    let mut args = vec![
        "diff".to_string(),
        "--name-status".to_string(),
        "-z".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
        head_rev.to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let raw = git_stdout_bytes(root, &args)?;
    let mut entries = parse_name_status_z(&raw)?;

    for entry in &mut entries {
        let stats = collect_numstat_for_entry(root, base_rev, head_rev, entry)?;
        entry.additions = stats.0;
        entry.deletions = stats.1;
        entry.binary = stats.2;
    }

    Ok(entries)
}

fn collect_numstat_for_entry(
    root: &Path,
    base_rev: &str,
    head_rev: &str,
    entry: &DiffEntry,
) -> Result<(u32, u32, bool)> {
    let target_path = entry.path.as_str();
    let args = vec![
        "diff".to_string(),
        "--numstat".to_string(),
        "--find-renames".to_string(),
        base_rev.to_string(),
        head_rev.to_string(),
        "--".to_string(),
        target_path.to_string(),
    ];
    let out = git_stdout(root, &args)?;
    let Some(line) = out.lines().next() else {
        return Ok((0, 0, false));
    };

    let mut parts = line.split('\t');
    let add = parts.next().unwrap_or("0");
    let del = parts.next().unwrap_or("0");
    if add == "-" || del == "-" {
        return Ok((0, 0, true));
    }
    let additions = add.parse::<u32>().unwrap_or(0);
    let deletions = del.parse::<u32>().unwrap_or(0);
    Ok((additions, deletions, false))
}

fn parse_name_status_z(raw: &[u8]) -> Result<Vec<DiffEntry>> {
    let mut entries = Vec::new();
    let tokens: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|t| !t.is_empty()).collect();
    let mut idx = 0usize;

    while idx < tokens.len() {
        let status_raw = std::str::from_utf8(tokens[idx])
            .context("invalid UTF-8 in git diff --name-status output")?
            .to_string();
        idx += 1;

        let status_code = status_raw
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());

        if status_code == "R" || status_code == "C" {
            if idx + 1 >= tokens.len() {
                bail!("malformed rename/copy record in git diff output");
            }
            let old_path = std::str::from_utf8(tokens[idx])
                .context("invalid UTF-8 path in diff output")?
                .to_string();
            let path = std::str::from_utf8(tokens[idx + 1])
                .context("invalid UTF-8 path in diff output")?
                .to_string();
            idx += 2;
            entries.push(DiffEntry {
                path,
                old_path: Some(old_path),
                status: status_code,
                additions: 0,
                deletions: 0,
                binary: false,
            });
            continue;
        }

        if idx >= tokens.len() {
            bail!("malformed record in git diff output");
        }
        let path = std::str::from_utf8(tokens[idx])
            .context("invalid UTF-8 path in diff output")?
            .to_string();
        idx += 1;
        entries.push(DiffEntry {
            path,
            old_path: None,
            status: status_code,
            additions: 0,
            deletions: 0,
            binary: false,
        });
    }

    Ok(entries)
}

fn resolve_rev_oid(root: &Path, rev: &str) -> Result<String> {
    git_stdout_simple(root, &["rev-parse", rev])
}

fn git_stdout_simple(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_stdout(dir: &Path, args: &[String]) -> Result<String> {
    let out = git_stdout_bytes(dir, args)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn git_stdout_bytes(dir: &Path, args: &[String]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_against_defaults_to_default() {
        assert!(matches!(parse_against(None), AgainstMode::Default));
        assert!(matches!(
            parse_against(Some("default")),
            AgainstMode::Default
        ));
    }

    #[test]
    fn parse_against_handles_epoch_branch_and_oid() {
        assert!(matches!(parse_against(Some("epoch")), AgainstMode::Epoch));
        assert!(matches!(
            parse_against(Some("branch:main")),
            AgainstMode::Branch(b) if b == "main"
        ));
        assert!(matches!(
            parse_against(Some("oid:abc1234")),
            AgainstMode::Oid(o) if o == "abc1234"
        ));
        assert!(matches!(
            parse_against(Some("a1b2c3d")),
            AgainstMode::Oid(o) if o == "a1b2c3d"
        ));
        assert!(matches!(
            parse_against(Some("feature/my-branch")),
            AgainstMode::Branch(b) if b == "feature/my-branch"
        ));
    }

    #[test]
    fn parse_name_status_z_parses_rename_and_modify() {
        let raw = b"R100\0old.rs\0new.rs\0M\0src/lib.rs\0";
        let parsed = parse_name_status_z(raw).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].status, "R");
        assert_eq!(parsed[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(parsed[0].path, "new.rs");
        assert_eq!(parsed[1].status, "M");
        assert_eq!(parsed[1].path, "src/lib.rs");
    }

    #[test]
    fn resolve_pathspecs_validates_globs() {
        let specs = resolve_pathspecs(&["src/**/*.rs".to_string(), "README*".to_string()]).unwrap();
        assert_eq!(specs, vec![":(glob)src/**/*.rs", ":(glob)README*"]);
    }
}
