//! `maw ws recover` — list, inspect, search, and restore recovery snapshots.
//!
//! Recovery points are pinned under `refs/manifold/recovery/<workspace>/<timestamp>`.
//! Destroyed workspaces additionally write destroy records under
//! `.manifold/artifacts/ws/<workspace>/destroy/` which `maw ws recover <name>` uses.
//!
//! # Modes
//!
//! - `maw ws recover` — list destroyed workspaces with snapshots (destroy records)
//! - `maw ws recover <name>` — show destroy history for a workspace
//! - `maw ws recover --search <pattern>` — content search across pinned recovery snapshots
//! - `maw ws recover <name> --search <pattern>` — search snapshots for one workspace
//! - `maw ws recover --ref <recovery-ref> --show <path>` — show a file from a specific snapshot
//! - `maw ws recover <name> --show <path>` — show a file from latest destroy snapshot
//! - `maw ws recover --ref <recovery-ref> --to <new-name>` — restore a specific snapshot
//! - `maw ws recover <name> --to <new-name>` — restore latest destroy snapshot

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::audit::{self, AuditEvent};
use crate::format::OutputFormat;
use crate::merge_state::MergeStateFile;

use super::capture::RECOVERY_PREFIX;
use super::destroy_record::{self, DestroyRecord, RecordCaptureMode};
use super::{repo_root, validate_workspace_name, workspace_path};

// ---------------------------------------------------------------------------
// List all destroyed workspaces
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DestroyedWorkspaceSummary {
    name: String,
    destroyed_at: String,
    capture_mode: String,
    snapshot_oid: Option<String>,
    dirty_file_count: usize,
}

#[derive(Serialize)]
struct RecoverListEnvelope {
    destroyed_workspaces: Vec<DestroyedWorkspaceSummary>,
    advice: Vec<String>,
}

pub fn list_destroyed(format: OutputFormat) -> Result<()> {
    let root = repo_root()?;
    let names = destroy_record::list_destroyed_workspaces(&root)?;

    if names.is_empty() {
        match format {
            OutputFormat::Json => {
                let envelope = RecoverListEnvelope {
                    destroyed_workspaces: vec![],
                    advice: vec!["No destroyed workspaces with snapshots found.".to_string()],
                };
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            }
            _ => {
                println!("No destroyed workspaces with snapshots found.");
            }
        }
        return Ok(());
    }

    let mut summaries = Vec::new();
    for name in &names {
        if let Ok(Some(record)) = destroy_record::read_latest_record(&root, name) {
            summaries.push(DestroyedWorkspaceSummary {
                name: name.clone(),
                destroyed_at: record.destroyed_at.clone(),
                capture_mode: record.capture_mode.to_string(),
                snapshot_oid: record.snapshot_oid.as_ref().map(|o| o[..12].to_string()),
                dirty_file_count: record.dirty_files.len(),
            });
        }
    }

    match format {
        OutputFormat::Json => {
            let envelope = RecoverListEnvelope {
                destroyed_workspaces: summaries,
                advice: vec![
                    "Inspect: maw ws recover <name>".to_string(),
                    "Show file: maw ws recover <name> --show <path>".to_string(),
                    "Restore: maw ws recover <name> --to <new-name>".to_string(),
                ],
            };
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        OutputFormat::Text => {
            print_list_text(&summaries);
        }
        OutputFormat::Pretty => {
            print_list_pretty(&summaries, format);
        }
    }

    Ok(())
}

fn print_list_text(summaries: &[DestroyedWorkspaceSummary]) {
    println!("NAME\tDESTROYED_AT\tCAPTURE\tSNAPSHOT\tDIRTY_FILES");
    for s in summaries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            s.name,
            s.destroyed_at,
            s.capture_mode,
            s.snapshot_oid.as_deref().unwrap_or("-"),
            s.dirty_file_count,
        );
    }
    println!();
    println!("Next: maw ws recover <name>");
}

fn print_list_pretty(summaries: &[DestroyedWorkspaceSummary], format: OutputFormat) {
    let use_color = format.should_use_color();

    if use_color {
        println!("\x1b[1mDestroyed workspaces with snapshots:\x1b[0m");
    } else {
        println!("Destroyed workspaces with snapshots:");
    }
    println!();

    // Calculate column widths
    let name_width = summaries
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    for s in summaries {
        let snapshot_display = s.snapshot_oid.as_deref().unwrap_or("-");
        let dirty_suffix = if s.dirty_file_count > 0 {
            format!(" ({} dirty files)", s.dirty_file_count)
        } else {
            String::new()
        };

        if use_color {
            println!(
                "  \x1b[1;36m{:<width$}\x1b[0m  {}  {:<14} {}{}",
                s.name,
                s.destroyed_at,
                s.capture_mode,
                snapshot_display,
                dirty_suffix,
                width = name_width,
            );
        } else {
            println!(
                "  {:<width$}  {}  {:<14} {}{}",
                s.name,
                s.destroyed_at,
                s.capture_mode,
                snapshot_display,
                dirty_suffix,
                width = name_width,
            );
        }
    }

    println!();
    if use_color {
        println!("\x1b[90mNext: maw ws recover <name>\x1b[0m");
    } else {
        println!("Next: maw ws recover <name>");
    }
}

// ---------------------------------------------------------------------------
// Search pinned recovery refs (agents)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RecoveryRef {
    ref_name: String,
    workspace: String,
    timestamp: String,
    oid: String,
}

#[derive(Clone, Debug)]
struct GrepHit {
    path: String,
    line: usize,
    line_text: String,
}

#[derive(Serialize)]
struct SnippetLine {
    line: usize,
    text: String,
    is_match: bool,
}

#[derive(Serialize)]
struct SearchHit {
    ref_name: String,
    workspace: String,
    timestamp: String,
    oid: String,
    oid_short: String,
    path: String,
    line: usize,
    snippet: Vec<SnippetLine>,
}

#[derive(Serialize)]
struct RecoverSearchEnvelope {
    pattern: String,
    workspace_filter: Option<String>,
    ref_filter: Option<String>,
    scanned_refs: usize,
    hit_count: usize,
    truncated: bool,
    hits: Vec<SearchHit>,
    advice: Vec<String>,
}

/// Search pinned recovery snapshots by content.
///
/// - If `workspace_filter` is `Some`, only snapshots for that workspace are searched.
/// - If `ref_filter` is `Some`, only that snapshot is searched.
///
/// `context` controls how many surrounding lines are included per match.
/// `max_hits` caps total matches returned (deterministic truncation).
#[allow(clippy::too_many_arguments)]
pub fn search(
    pattern: &str,
    workspace_filter: Option<&str>,
    ref_filter: Option<&str>,
    context: usize,
    max_hits: usize,
    regex: bool,
    ignore_case: bool,
    text: bool,
    format: OutputFormat,
) -> Result<()> {
    if pattern.is_empty() {
        bail!("Search pattern cannot be empty");
    }
    if max_hits == 0 {
        bail!("--max-hits must be >= 1");
    }

    let git_cwd = super::git_cwd()?;
    let mut refs = list_recovery_refs(&git_cwd)?;

    if let Some(ws) = workspace_filter {
        refs.retain(|r| r.workspace == ws);
    }

    if let Some(rf) = ref_filter {
        validate_recovery_ref(rf)?;
        refs.retain(|r| r.ref_name == rf);
        if refs.is_empty() {
            bail!(
                "Recovery ref '{rf}' not found under {RECOVERY_PREFIX}.\n  \
                 List refs: git for-each-ref {RECOVERY_PREFIX}"
            );
        }
    }

    // Deterministic order regardless of filesystem ref ordering.
    refs.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));

    if refs.is_empty() {
        match format {
            OutputFormat::Json => {
                let envelope = RecoverSearchEnvelope {
                    pattern: pattern.to_string(),
                    workspace_filter: workspace_filter.map(|s| s.to_string()),
                    ref_filter: ref_filter.map(|s| s.to_string()),
                    scanned_refs: 0,
                    hit_count: 0,
                    truncated: false,
                    hits: vec![],
                    advice: vec![
                        "No pinned recovery snapshots found to search.".to_string(),
                        format!("List refs: git for-each-ref {RECOVERY_PREFIX}"),
                    ],
                };
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            }
            _ => {
                println!("No pinned recovery snapshots found to search.");
                println!("List refs: git for-each-ref {RECOVERY_PREFIX}");
            }
        }
        return Ok(());
    }

    let mut hits: Vec<SearchHit> = Vec::new();
    let mut truncated = false;
    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();

    crate::fp!("FP_RECOVER_BEFORE_SEARCH")?;
    'scan: for r in &refs {
        let grep_hits = git_grep_hits(&git_cwd, &r.oid, pattern, regex, ignore_case, text)?;
        for gh in grep_hits {
            let snippet = build_snippet(
                &git_cwd,
                &r.oid,
                &gh.path,
                gh.line,
                context,
                &gh.line_text,
                &mut file_cache,
            )?;

            hits.push(SearchHit {
                ref_name: r.ref_name.clone(),
                workspace: r.workspace.clone(),
                timestamp: r.timestamp.clone(),
                oid: r.oid.clone(),
                oid_short: r.oid[..r.oid.len().min(12)].to_string(),
                path: gh.path,
                line: gh.line,
                snippet,
            });

            if hits.len() >= max_hits {
                truncated = true;
                break 'scan;
            }
        }
    }

    let envelope = RecoverSearchEnvelope {
        pattern: pattern.to_string(),
        workspace_filter: workspace_filter.map(|s| s.to_string()),
        ref_filter: ref_filter.map(|s| s.to_string()),
        scanned_refs: refs.len(),
        hit_count: hits.len(),
        truncated,
        hits,
        advice: vec![
            "Show file: maw ws recover --ref <ref> --show <path>".to_string(),
            "Restore:   maw ws recover --ref <ref> --to <new-workspace>".to_string(),
        ],
    };

    audit::log_audit(&AuditEvent::Search {
        pattern_hash: audit::hash_pattern(pattern),
        workspace_filter: workspace_filter.map(|s| s.to_string()),
        ref_filter: ref_filter.map(|s| s.to_string()),
        hit_count: envelope.hit_count,
    });

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        OutputFormat::Text => print_search_text(&envelope),
        OutputFormat::Pretty => print_search_pretty(&envelope, format),
    }

    Ok(())
}

fn validate_recovery_ref(r: &str) -> Result<()> {
    if !r.starts_with(RECOVERY_PREFIX) {
        bail!(
            "Recovery ref must be under {RECOVERY_PREFIX}.\n  \
             Got: {r}"
        );
    }
    // Require a workspace + timestamp suffix (two components after prefix).
    let rest = &r[RECOVERY_PREFIX.len()..];
    if !rest.contains('/') {
        bail!(
            "Recovery ref must be of form {RECOVERY_PREFIX}<workspace>/<timestamp>.\n  \
             Got: {r}"
        );
    }
    Ok(())
}

fn parse_recovery_ref_name(ref_name: &str) -> Option<(String, String)> {
    if !ref_name.starts_with(RECOVERY_PREFIX) {
        return None;
    }
    let rest = &ref_name[RECOVERY_PREFIX.len()..];
    let mut it = rest.splitn(2, '/');
    let ws = it.next()?;
    let ts = it.next()?;
    Some((ws.to_string(), ts.to_string()))
}

fn list_recovery_refs(git_cwd: &Path) -> Result<Vec<RecoveryRef>> {
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            RECOVERY_PREFIX,
        ])
        .current_dir(git_cwd)
        .output()
        .context("failed to run git for-each-ref for recovery refs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git for-each-ref failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out: Vec<RecoveryRef> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let ref_name = match parts.next() {
            Some(v) => v,
            None => continue,
        };
        let oid = match parts.next() {
            Some(v) => v,
            None => continue,
        };
        if let Some((ws, ts)) = parse_recovery_ref_name(ref_name) {
            out.push(RecoveryRef {
                ref_name: ref_name.to_string(),
                workspace: ws,
                timestamp: ts,
                oid: oid.to_string(),
            });
        }
    }

    Ok(out)
}

fn git_grep_hits(
    git_cwd: &Path,
    oid: &str,
    pattern: &str,
    regex: bool,
    ignore_case: bool,
    text: bool,
) -> Result<Vec<GrepHit>> {
    let mut args: Vec<&str> = vec!["grep", "-n", "--no-color"];

    if ignore_case {
        args.push("-i");
    }
    if !regex {
        args.push("-F");
    }
    if text {
        // Search binary blobs as if they were text.
        args.push("-a");
    } else {
        // Default: ignore binary files.
        args.push("-I");
    }

    // Always use -e so patterns beginning with '-' can't be interpreted as flags.
    args.push("-e");
    args.push(pattern);
    args.push(oid);

    let output = Command::new("git")
        .args(args)
        .current_dir(git_cwd)
        .output()
        .context("failed to run git grep")?;

    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(vec![]),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git grep failed: {}", stderr.trim());
        }
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut hits: Vec<GrepHit> = Vec::new();
    let prefix = format!("{oid}:");

    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let rest = line.strip_prefix(&prefix).unwrap_or(line);
        let mut parts = rest.splitn(3, ':');
        let path = match parts.next() {
            Some(v) => v,
            None => continue,
        };
        let line_str = match parts.next() {
            Some(v) => v,
            None => continue,
        };
        let text = parts.next().unwrap_or("");

        let line_no: usize = match line_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        hits.push(GrepHit {
            path: path.to_string(),
            line: line_no,
            line_text: text.to_string(),
        });
    }

    Ok(hits)
}

fn read_file_lines(git_cwd: &Path, oid: &str, path: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["show", &format!("{oid}:{path}")])
        .current_dir(git_cwd)
        .output()
        .context("failed to run git show for snippet")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git show failed while building snippet for {oid_short}:{path}: {}",
            stderr.trim(),
            oid_short = &oid[..oid.len().min(12)]
        );
    }

    let content = String::from_utf8_lossy(&output.stdout);
    Ok(content.lines().map(|l| l.to_string()).collect())
}

fn build_snippet(
    git_cwd: &Path,
    oid: &str,
    path: &str,
    line: usize,
    context: usize,
    fallback_line_text: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<SnippetLine>> {
    if context == 0 {
        return Ok(vec![SnippetLine {
            line,
            text: fallback_line_text.to_string(),
            is_match: true,
        }]);
    }

    let key = format!("{oid}:{path}");
    if !cache.contains_key(&key) {
        let lines = read_file_lines(git_cwd, oid, path)?;
        cache.insert(key.clone(), lines);
    }

    let lines = cache
        .get(&key)
        .context("internal error: file cache missing key")?;

    if lines.is_empty() {
        return Ok(vec![]);
    }

    // git grep line numbers are 1-based.
    let start = line.saturating_sub(context).max(1);
    let mut end = line.saturating_add(context);
    if end > lines.len() {
        end = lines.len();
    }

    let mut out = Vec::new();
    for ln in start..=end {
        if let Some(text) = lines.get(ln - 1) {
            out.push(SnippetLine {
                line: ln,
                text: text.clone(),
                is_match: ln == line,
            });
        }
    }
    Ok(out)
}

fn print_search_text(env: &RecoverSearchEnvelope) {
    println!("PATTERN\t{}", env.pattern);
    if let Some(ref ws) = env.workspace_filter {
        println!("WORKSPACE\t{ws}");
    }
    if let Some(ref rf) = env.ref_filter {
        println!("REF\t{rf}");
    }

    let trunc = if env.truncated { " (truncated)" } else { "" };
    println!(
        "SCANNED_REFS\t{}\nHITS\t{}{}",
        env.scanned_refs, env.hit_count, trunc
    );
    println!();

    for h in &env.hits {
        println!(
            "{}\t{}\t{}\t{}:{}\t{}",
            h.workspace, h.timestamp, h.oid_short, h.path, h.line, h.ref_name
        );
        for sl in &h.snippet {
            let marker = if sl.is_match { ">" } else { " " };
            println!("  {marker} {:>6}\t{}", sl.line, sl.text);
        }
        println!();
    }

    println!("Next: maw ws recover --ref <ref> --show <path>");
    println!("      maw ws recover --ref <ref> --to <new-workspace>");
}

fn print_search_pretty(env: &RecoverSearchEnvelope, format: OutputFormat) {
    let use_color = format.should_use_color();

    if use_color {
        println!("\x1b[1mSearch recovery snapshots\x1b[0m");
    } else {
        println!("Search recovery snapshots");
    }

    println!("  Pattern:  {}", env.pattern);
    if let Some(ref ws) = env.workspace_filter {
        println!("  Workspace: {ws}");
    }
    if let Some(ref rf) = env.ref_filter {
        println!("  Ref:      {rf}");
    }

    let trunc = if env.truncated { " (truncated)" } else { "" };
    println!(
        "  Scanned:  {} ref(s)\n  Hits:     {}{}",
        env.scanned_refs, env.hit_count, trunc
    );
    println!();

    for h in &env.hits {
        if use_color {
            println!(
                "\x1b[33m{}\x1b[0m {} {}... {}:{}",
                h.workspace, h.timestamp, h.oid_short, h.path, h.line
            );
            println!("  \x1b[90mref: {}\x1b[0m", h.ref_name);
        } else {
            println!(
                "{} {} {}... {}:{}",
                h.workspace, h.timestamp, h.oid_short, h.path, h.line
            );
            println!("  ref: {}", h.ref_name);
        }

        for sl in &h.snippet {
            let marker = if sl.is_match { ">" } else { " " };
            if use_color && sl.is_match {
                println!("  {marker} \x1b[1m{:>6}\x1b[0m {}", sl.line, sl.text);
            } else {
                println!("  {marker} {:>6} {}", sl.line, sl.text);
            }
        }
        println!();
    }

    if use_color {
        println!("\x1b[90mNext: maw ws recover --ref <ref> --show <path>\x1b[0m");
        println!("\x1b[90m      maw ws recover --ref <ref> --to <new-workspace>\x1b[0m");
    } else {
        println!("Next: maw ws recover --ref <ref> --show <path>");
        println!("      maw ws recover --ref <ref> --to <new-workspace>");
    }
}

// ---------------------------------------------------------------------------
// Show destroy history for a specific workspace
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RecoverShowEnvelope {
    workspace: String,
    records: Vec<DestroyRecord>,
    advice: Vec<String>,
}

pub fn show_workspace(name: &str, format: OutputFormat) -> Result<()> {
    validate_workspace_name(name)?;
    let root = repo_root()?;

    let record_files = destroy_record::list_record_files(&root, name)?;
    if record_files.is_empty() {
        bail!(
            "No destroy records found for workspace '{name}'.\n  \
             List destroyed workspaces: maw ws recover"
        );
    }

    let mut records = Vec::new();
    for filename in &record_files {
        let record = destroy_record::read_record(&root, name, filename)?;
        records.push(record);
    }

    match format {
        OutputFormat::Json => {
            let envelope = RecoverShowEnvelope {
                workspace: name.to_string(),
                records,
                advice: vec![
                    "Show file: maw ws recover <name> --show <path>".to_string(),
                    "Restore: maw ws recover <name> --to <new-name>".to_string(),
                ],
            };
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        OutputFormat::Text => print_show_text(name, &records),
        OutputFormat::Pretty => print_show_pretty(name, &records, format),
    }

    Ok(())
}

fn print_show_text(name: &str, records: &[DestroyRecord]) {
    println!(
        "Destroy history for workspace '{name}' ({} record(s)):",
        records.len()
    );
    println!();
    for (i, r) in records.iter().enumerate() {
        println!("--- Record {} ---", i + 1);
        println!("destroyed_at:  {}", r.destroyed_at);
        println!("capture_mode:  {}", r.capture_mode);
        println!("final_head:    {}", r.final_head);
        if let Some(ref oid) = r.snapshot_oid {
            println!("snapshot_oid:  {oid}");
        }
        if let Some(ref sref) = r.snapshot_ref {
            println!("snapshot_ref:  {sref}");
        }
        if let Some(ref href) = r.final_head_ref {
            println!("final_head_ref: {href}");
        }
        println!("base_epoch:    {}", r.base_epoch);
        println!("reason:        {:?}", r.destroy_reason);
        println!("tool_version:  {}", r.tool_version);
        if !r.dirty_files.is_empty() {
            println!("dirty_files ({}):", r.dirty_files.len());
            for f in &r.dirty_files {
                println!("  {f}");
            }
        }
        println!();
    }
    println!("Next: maw ws recover {name} --show <path>");
    println!("      maw ws recover {name} --to <new-name>");
}

fn print_show_pretty(name: &str, records: &[DestroyRecord], format: OutputFormat) {
    let use_color = format.should_use_color();

    if use_color {
        println!(
            "\x1b[1mDestroy history for '{}' ({} record(s)):\x1b[0m",
            name,
            records.len()
        );
    } else {
        println!(
            "Destroy history for '{}' ({} record(s)):",
            name,
            records.len()
        );
    }
    println!();

    for (i, r) in records.iter().enumerate() {
        let header = format!("Record {}", i + 1);
        if use_color {
            println!("  \x1b[1;33m{header}\x1b[0m");
        } else {
            println!("  {header}");
        }
        println!("    Destroyed:  {}", r.destroyed_at);
        println!("    Capture:    {}", r.capture_mode);
        println!("    Final HEAD: {}...", &r.final_head[..12]);
        if let Some(ref oid) = r.snapshot_oid {
            println!("    Snapshot:   {}...", &oid[..oid.len().min(12)]);
        }
        if let Some(ref sref) = r.snapshot_ref {
            println!("    Pin ref:    {sref}");
        }
        println!("    Epoch:      {}...", &r.base_epoch[..12]);
        println!("    Reason:     {:?}", r.destroy_reason);
        if !r.dirty_files.is_empty() {
            println!("    Dirty files ({}):", r.dirty_files.len());
            for f in &r.dirty_files {
                println!("      {f}");
            }
        }
        println!();
    }

    if use_color {
        println!("\x1b[90mNext: maw ws recover {name} --show <path>\x1b[0m");
        println!("\x1b[90m      maw ws recover {name} --to <new-name>\x1b[0m");
    } else {
        println!("Next: maw ws recover {name} --show <path>");
        println!("      maw ws recover {name} --to <new-name>");
    }
}

// ---------------------------------------------------------------------------
// Show a specific file from the snapshot
// ---------------------------------------------------------------------------

pub fn show_file_by_ref(recovery_ref: &str, path: &str) -> Result<()> {
    validate_recovery_ref(recovery_ref)?;
    validate_show_path(path)?;

    audit::log_audit(&AuditEvent::Show {
        ref_name: recovery_ref.to_string(),
        path: path.to_string(),
    });

    let git_cwd = super::git_cwd()?;
    let oid = resolve_ref_to_oid(&git_cwd, recovery_ref)?;
    show_file_at_oid(&git_cwd, &oid, path)
}

fn resolve_ref_to_oid(git_cwd: &Path, reference: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", &format!("{reference}^{{commit}}")])
        .current_dir(git_cwd)
        .output()
        .context("failed to resolve recovery ref")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to resolve recovery ref '{reference}': {}",
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn show_file_at_oid(git_cwd: &Path, oid: &str, path: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["show", &format!("{oid}:{path}")])
        .current_dir(git_cwd)
        .output()
        .context("failed to run git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to show file '{path}' from snapshot {oid_short}: {}",
            stderr.trim(),
            oid_short = &oid[..oid.len().min(12)]
        );
    }

    use std::io::Write;
    std::io::stdout().write_all(&output.stdout)?;
    Ok(())
}

pub fn show_file(name: &str, path: &str) -> Result<()> {
    validate_workspace_name(name)?;
    validate_show_path(path)?;
    let root = repo_root()?;

    let record = destroy_record::read_latest_record(&root, name)?
        .with_context(|| format!("No destroy records found for workspace '{name}'"))?;

    // Log the show using the snapshot ref if available, otherwise the workspace name.
    let ref_for_audit = record
        .snapshot_ref
        .clone()
        .unwrap_or_else(|| format!("(workspace:{name})"));
    audit::log_audit(&AuditEvent::Show {
        ref_name: ref_for_audit,
        path: path.to_string(),
    });

    let oid = resolve_recoverable_oid(&record)?;

    // Use git show <oid>:<path> to retrieve the file content.
    // Run from the git common dir (repo root) so the ref resolves.
    let git_cwd = super::git_cwd()?;
    let output = Command::new("git")
        .args(["show", &format!("{oid}:{path}")])
        .current_dir(&git_cwd)
        .output()
        .context("failed to run git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist") || stderr.contains("exists on disk, but not in") {
            bail!(
                "File '{path}' not found in snapshot {oid_short} for workspace '{name}'.\n  \
                 List dirty files: maw ws recover {name}",
                oid_short = &oid[..oid.len().min(12)],
            );
        }
        bail!("git show failed: {}", stderr.trim());
    }

    // Write raw content to stdout (binary-safe isn't needed for text, but let's be correct)
    use std::io::Write;
    std::io::stdout()
        .write_all(&output.stdout)
        .context("write to stdout")?;

    Ok(())
}

/// Validate the `--show` path argument against directory traversal.
fn validate_show_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("Path cannot be empty");
    }
    if path.starts_with('/') {
        bail!("Path must be relative (no leading '/')");
    }
    if path.contains('\0') {
        bail!("Path cannot contain null bytes");
    }
    // Reject path traversal components
    for component in path.split('/') {
        if component == ".." {
            bail!("Path cannot contain '..' components (directory traversal)");
        }
    }
    Ok(())
}

/// Resolve the OID to use for file retrieval from a destroy record.
///
/// For `DirtySnapshot` mode, uses the snapshot OID (the stash commit).
/// For `HeadOnly` mode, uses the final HEAD.
/// For `None` mode, fails — nothing was captured.
fn resolve_recoverable_oid(record: &DestroyRecord) -> Result<String> {
    match record.capture_mode {
        RecordCaptureMode::DirtySnapshot => record
            .snapshot_oid
            .clone()
            .context("destroy record has dirty_snapshot mode but no snapshot_oid"),
        RecordCaptureMode::HeadOnly => Ok(record.final_head.clone()),
        RecordCaptureMode::None => bail!(
            "No snapshot was captured for this workspace (capture_mode=none).\n  \
             The workspace was clean at its epoch when destroyed."
        ),
    }
}

// ---------------------------------------------------------------------------
// Restore snapshot to a new workspace
// ---------------------------------------------------------------------------

pub fn restore_ref_to(recovery_ref: &str, new_name: &str) -> Result<()> {
    validate_recovery_ref(recovery_ref)?;
    validate_workspace_name(new_name)?;

    audit::log_audit(&AuditEvent::Restore {
        ref_name: recovery_ref.to_string(),
        new_workspace: new_name.to_string(),
    });

    let git_cwd = super::git_cwd()?;
    let oid = resolve_ref_to_oid(&git_cwd, recovery_ref)?;

    // Create new workspace (empty) and then populate it from the snapshot tree.
    super::create::create(new_name, None, false, None)?;
    let new_path = workspace_path(new_name)?;

    if let Err(e) = populate_from_snapshot(&new_path, &oid) {
        eprintln!(
            "Populate failed, rolling back workspace '{new_name}': {e:#}"
        );
        if let Err(cleanup_err) = super::create::destroy(new_name, false, true) {
            eprintln!(
                "WARNING: rollback destroy also failed: {cleanup_err:#}\n  \
                 Manual cleanup may be needed: maw ws destroy {new_name} --force"
            );
        }
        return Err(e).context("Failed to populate workspace from snapshot");
    }

    println!(
        "Restored snapshot {oid_short} to workspace '{new_name}'.",
        oid_short = &oid[..oid.len().min(12)]
    );
    println!("Next: maw exec {new_name} -- git status");

    Ok(())
}

pub fn restore_to(name: &str, new_name: &str) -> Result<()> {
    crate::fp!("FP_RECOVER_BEFORE_RESTORE")?;
    validate_workspace_name(name)?;
    validate_workspace_name(new_name)?;

    audit::log_audit(&AuditEvent::Restore {
        ref_name: format!("(workspace:{name})"),
        new_workspace: new_name.to_string(),
    });

    let root = repo_root()?;

    // Check that the destination doesn't already exist
    let dest_path = workspace_path(new_name)?;
    if dest_path.exists() {
        bail!(
            "Workspace '{new_name}' already exists at {}.\n  \
             Choose a different name.",
            dest_path.display()
        );
    }

    // Check that the source name isn't an active workspace
    // (it's fine if a destroyed workspace has the same name as a live one —
    // the user asked to restore it to a *different* name)

    let record = destroy_record::read_latest_record(&root, name)?
        .with_context(|| format!("No destroy records found for workspace '{name}'"))?;

    let oid = resolve_recoverable_oid(&record)?;

    // Step 1: Create the new workspace via the standard create path
    println!("Creating workspace '{new_name}' from snapshot of '{name}'...");
    super::create::create(new_name, None, false, None)?;

    // Step 2: Populate from the snapshot using git read-tree + checkout-index
    let new_ws_path = workspace_path(new_name)?;
    if let Err(e) = populate_from_snapshot(&new_ws_path, &oid) {
        eprintln!(
            "Populate failed, rolling back workspace '{new_name}': {e:#}"
        );
        if let Err(cleanup_err) = super::create::destroy(new_name, false, true) {
            eprintln!(
                "WARNING: rollback destroy also failed: {cleanup_err:#}\n  \
                 Manual cleanup may be needed: maw ws destroy {new_name} --force"
            );
        }
        return Err(e).context("Failed to populate workspace from snapshot");
    }

    println!();
    println!("Restored snapshot of '{name}' into workspace '{new_name}'.");
    println!();
    println!("  Snapshot:  {}...", &oid[..oid.len().min(12)]);
    println!("  Path:      {}/", new_ws_path.display());
    if !record.dirty_files.is_empty() {
        println!("  Recovered: {} dirty file(s)", record.dirty_files.len());
    }
    println!();
    println!("Next: maw exec {new_name} -- git status");
    println!("      maw exec {new_name} -- git diff");

    Ok(())
}

/// Populate a workspace from a snapshot OID using git2-style operations.
///
/// Uses `git read-tree` to load the snapshot tree into the index, then
/// `git checkout-index` to materialize the files.
fn populate_from_snapshot(ws_path: &std::path::Path, oid: &str) -> Result<()> {
    // For stash commits (WorktreeCapture), the worktree state is stored
    // in the third parent's tree. But `git read-tree` on the stash commit
    // itself accesses the top-level tree which includes the index state.
    //
    // The safest approach: use `git checkout <oid> -- .` which overwrites
    // the working tree with the snapshot's content.

    // First, read the snapshot tree into the index
    let read_tree = Command::new("git")
        .args(["read-tree", oid])
        .current_dir(ws_path)
        .output()
        .context("git read-tree failed")?;

    if !read_tree.status.success() {
        let stderr = String::from_utf8_lossy(&read_tree.stderr);
        bail!("git read-tree failed: {}", stderr.trim());
    }

    // Then checkout the index to the working tree (overwrite existing files)
    let checkout = Command::new("git")
        .args(["checkout-index", "--all", "--force"])
        .current_dir(ws_path)
        .output()
        .context("git checkout-index failed")?;

    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        bail!("git checkout-index failed: {}", stderr.trim());
    }

    // Reset the index back to HEAD so the workspace shows snapshot files
    // as unstaged modifications (not staged additions)
    let reset = Command::new("git")
        .args(["reset"])
        .current_dir(ws_path)
        .output()
        .context("git reset failed")?;

    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr);
        tracing::warn!("git reset after populate failed: {}", stderr.trim());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Dangling snapshot detection and cleanup
// ---------------------------------------------------------------------------

/// A snapshot ref that is no longer needed (the workspace was destroyed
/// or the recovery point has been superseded by newer ones).
#[derive(Clone, Debug, Serialize)]
pub struct DanglingSnapshot {
    /// The full ref name (e.g. `refs/manifold/recovery/alice/2025-01-01T00-00-00Z`).
    pub ref_name: String,
    /// The workspace this ref belongs to.
    pub workspace: String,
    /// The timestamp suffix from the ref name.
    pub timestamp: String,
    /// The OID the ref points to.
    pub oid: String,
    /// Why this ref is considered dangling.
    pub reason: DanglingReason,
}

/// Reason a snapshot ref is considered dangling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DanglingReason {
    /// The workspace no longer exists (successfully destroyed).
    WorkspaceDestroyed,
    /// The workspace exists but has newer recovery snapshots that supersede this one.
    SupersededByNewer,
}

/// Detect dangling snapshot refs.
///
/// A snapshot ref is "dangling" if:
/// - It exists under `refs/manifold/recovery/<workspace>/`
/// - The workspace it belongs to no longer exists (destroyed successfully), OR
/// - The workspace has multiple recovery refs and this one is superseded by
///   newer snapshots (not the most recent for that workspace).
///
/// Safety: never marks a ref as dangling if:
/// - The workspace still exists and has uncommitted work
/// - The snapshot is the only recovery point for that workspace
/// - There's an active merge in progress referencing it
pub fn find_dangling_snapshots(root: &Path) -> Result<Vec<DanglingSnapshot>> {
    let default_ws = root.join("ws").join("default");
    let git_cwd = if default_ws.exists() {
        default_ws
    } else {
        root.to_path_buf()
    };
    let refs = list_recovery_refs(&git_cwd)?;

    if refs.is_empty() {
        return Ok(vec![]);
    }

    // Gather set of active workspace names (directories under ws/).
    let active_workspaces = list_active_workspace_names(root);

    // Check for an active merge in progress.
    let merge_workspaces = active_merge_workspaces(root);

    // Group refs by workspace name.
    let mut by_workspace: HashMap<String, Vec<RecoveryRef>> = HashMap::new();
    for r in &refs {
        by_workspace
            .entry(r.workspace.clone())
            .or_default()
            .push(r.clone());
    }

    let mut dangling = Vec::new();

    for (ws_name, mut ws_refs) in by_workspace {
        // Sort by timestamp (ascending) so last element is the most recent.
        ws_refs.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        let ws_exists = active_workspaces.contains(&ws_name);
        let ws_in_merge = merge_workspaces.contains(&ws_name);

        if ws_in_merge {
            // Safety: never GC refs for a workspace involved in an active merge.
            continue;
        }

        if !ws_exists {
            // Workspace was destroyed. All refs for it are dangling UNLESS
            // it's the only recovery point (we always keep at least the
            // most recent one to allow future recovery).
            if ws_refs.len() == 1 {
                // Only one ref — keep it as the sole recovery point.
                continue;
            }
            // Mark all except the most recent as dangling (destroyed + superseded).
            // The most recent one is also dangling (workspace gone), but we
            // keep it for safety unless the user explicitly GCs.
            for r in &ws_refs {
                dangling.push(DanglingSnapshot {
                    ref_name: r.ref_name.clone(),
                    workspace: r.workspace.clone(),
                    timestamp: r.timestamp.clone(),
                    oid: r.oid.clone(),
                    reason: if r.ref_name == ws_refs.last().unwrap().ref_name {
                        DanglingReason::WorkspaceDestroyed
                    } else {
                        DanglingReason::SupersededByNewer
                    },
                });
            }
        } else {
            // Workspace still exists. Only mark older refs as superseded
            // if there are multiple recovery points.
            if ws_refs.len() <= 1 {
                continue;
            }
            // Keep the most recent, mark older ones as superseded.
            for r in &ws_refs[..ws_refs.len() - 1] {
                dangling.push(DanglingSnapshot {
                    ref_name: r.ref_name.clone(),
                    workspace: r.workspace.clone(),
                    timestamp: r.timestamp.clone(),
                    oid: r.oid.clone(),
                    reason: DanglingReason::SupersededByNewer,
                });
            }
        }
    }

    // Stable sort for deterministic output.
    dangling.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));

    Ok(dangling)
}

/// Clean up dangling snapshot refs.
///
/// Deletes refs that `find_dangling_snapshots` identified as safe to remove.
/// If `all` is true, removes all dangling refs (including the most recent
/// ref for destroyed workspaces). If `all` is false, only removes
/// superseded refs (preserving the most recent ref per workspace).
///
/// Returns the list of refs that were deleted.
pub fn cleanup_dangling_snapshots(root: &Path, all: bool) -> Result<Vec<DanglingSnapshot>> {
    let dangling = find_dangling_snapshots(root)?;

    if dangling.is_empty() {
        return Ok(vec![]);
    }

    let to_remove: Vec<&DanglingSnapshot> = if all {
        dangling.iter().collect()
    } else {
        // Conservative: only remove superseded refs, not the last ref
        // for destroyed workspaces.
        dangling
            .iter()
            .filter(|d| d.reason == DanglingReason::SupersededByNewer)
            .collect()
    };

    let mut removed = Vec::new();
    for d in to_remove {
        crate::refs::delete_ref(root, &d.ref_name)
            .map_err(|e| anyhow::anyhow!("failed to delete ref {}: {e}", d.ref_name))?;
        removed.push(d.clone());
    }

    Ok(removed)
}

/// Run `maw ws recover --gc` — list or clean up dangling snapshot refs.
pub fn gc(all: bool, dry_run: bool, format: OutputFormat) -> Result<()> {
    let root = repo_root()?;

    if dry_run {
        let dangling = find_dangling_snapshots(&root)?;
        let to_show: Vec<&DanglingSnapshot> = if all {
            dangling.iter().collect()
        } else {
            dangling
                .iter()
                .filter(|d| d.reason == DanglingReason::SupersededByNewer)
                .collect()
        };

        match format {
            OutputFormat::Json => {
                let envelope = GcEnvelope {
                    dry_run: true,
                    removed: to_show.iter().cloned().cloned().collect(),
                    total_dangling: dangling.len(),
                    advice: vec!["Run without --dry-run to delete.".to_string()],
                };
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            }
            _ => {
                if to_show.is_empty() {
                    println!("No dangling snapshot refs to clean up.");
                } else {
                    println!(
                        "Found {} dangling snapshot ref(s) (dry run):",
                        to_show.len()
                    );
                    println!();
                    for d in &to_show {
                        println!(
                            "  {} ({}, {})",
                            d.ref_name,
                            d.workspace,
                            match &d.reason {
                                DanglingReason::WorkspaceDestroyed => "workspace destroyed",
                                DanglingReason::SupersededByNewer => "superseded by newer",
                            }
                        );
                    }
                    println!();
                    println!("Run without --dry-run to delete.");
                }
            }
        }

        return Ok(());
    }

    let removed = cleanup_dangling_snapshots(&root, all)?;

    match format {
        OutputFormat::Json => {
            let envelope = GcEnvelope {
                dry_run: false,
                removed: removed.clone(),
                total_dangling: removed.len(),
                advice: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        _ => {
            if removed.is_empty() {
                println!("No dangling snapshot refs to clean up.");
            } else {
                println!("Removed {} dangling snapshot ref(s):", removed.len());
                println!();
                for d in &removed {
                    println!(
                        "  {} ({}, {})",
                        d.ref_name,
                        d.workspace,
                        match &d.reason {
                            DanglingReason::WorkspaceDestroyed => "workspace destroyed",
                            DanglingReason::SupersededByNewer => "superseded by newer",
                        }
                    );
                }
            }
        }
    }

    Ok(())
}

#[derive(Serialize)]
struct GcEnvelope {
    dry_run: bool,
    removed: Vec<DanglingSnapshot>,
    total_dangling: usize,
    advice: Vec<String>,
}

/// List active workspace names by scanning the `ws/` directory.
fn list_active_workspace_names(root: &Path) -> HashSet<String> {
    let ws_dir = root.join("ws");
    let mut names = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&ws_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                names.insert(name);
            }
        }
    }
    names
}

/// Get workspace names involved in an active merge (if any).
fn active_merge_workspaces(root: &Path) -> HashSet<String> {
    let state_path = root.join(".manifold").join("merge-state.json");
    let mut names = HashSet::new();
    if let Ok(state) = MergeStateFile::read(&state_path) {
        for ws_id in &state.sources {
            names.insert(ws_id.as_str().to_string());
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn validate_show_path_rejects_traversal() {
        assert!(validate_show_path("../etc/passwd").is_err());
        assert!(validate_show_path("foo/../bar").is_err());
        assert!(validate_show_path("foo/../../bar").is_err());
    }

    #[test]
    fn validate_show_path_rejects_absolute() {
        assert!(validate_show_path("/etc/passwd").is_err());
    }

    #[test]
    fn validate_show_path_rejects_empty() {
        assert!(validate_show_path("").is_err());
    }

    #[test]
    fn validate_show_path_rejects_null() {
        assert!(validate_show_path("foo\0bar").is_err());
    }

    #[test]
    fn validate_show_path_allows_normal_paths() {
        assert!(validate_show_path("src/main.rs").is_ok());
        assert!(validate_show_path("README.md").is_ok());
        assert!(validate_show_path("src/foo/bar.rs").is_ok());
        assert!(validate_show_path(".hidden").is_ok());
        assert!(validate_show_path("foo/./bar").is_ok()); // "." is fine, only ".." is traversal
    }

    #[test]
    fn resolve_oid_dirty_snapshot() {
        let record = DestroyRecord {
            workspace_id: "test".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
            final_head: "a".repeat(40),
            final_head_ref: None,
            snapshot_oid: Some("b".repeat(40)),
            snapshot_ref: Some("refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_string()),
            capture_mode: RecordCaptureMode::DirtySnapshot,
            dirty_files: vec!["foo.rs".to_string()],
            base_epoch: "c".repeat(40),
            destroy_reason: super::destroy_record::DestroyReason::Destroy,
            tool_version: "0.47.0".to_string(),
        };
        let oid = resolve_recoverable_oid(&record).unwrap();
        assert_eq!(oid, "b".repeat(40));
    }

    #[test]
    fn resolve_oid_head_only() {
        let record = DestroyRecord {
            workspace_id: "test".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
            final_head: "a".repeat(40),
            final_head_ref: Some("refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_string()),
            snapshot_oid: None,
            snapshot_ref: None,
            capture_mode: RecordCaptureMode::HeadOnly,
            dirty_files: vec![],
            base_epoch: "c".repeat(40),
            destroy_reason: super::destroy_record::DestroyReason::Destroy,
            tool_version: "0.47.0".to_string(),
        };
        let oid = resolve_recoverable_oid(&record).unwrap();
        assert_eq!(oid, "a".repeat(40));
    }

    #[test]
    fn resolve_oid_none_fails() {
        let record = DestroyRecord {
            workspace_id: "test".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
            final_head: "a".repeat(40),
            final_head_ref: None,
            snapshot_oid: None,
            snapshot_ref: None,
            capture_mode: RecordCaptureMode::None,
            dirty_files: vec![],
            base_epoch: "c".repeat(40),
            destroy_reason: super::destroy_record::DestroyReason::Destroy,
            tool_version: "0.47.0".to_string(),
        };
        assert!(resolve_recoverable_oid(&record).is_err());
    }

    #[test]
    fn validate_recovery_ref_requires_prefix_and_suffix() {
        assert!(validate_recovery_ref("refs/heads/main").is_err());
        assert!(validate_recovery_ref("refs/manifold/recovery").is_err());
        assert!(validate_recovery_ref("refs/manifold/recovery/").is_err());
        assert!(validate_recovery_ref("refs/manifold/recovery/ws-only").is_err());
        assert!(validate_recovery_ref("refs/manifold/recovery/alice/2025-01-01T00-00-00Z").is_ok());
    }

    #[test]
    fn parse_recovery_ref_name_extracts_workspace_and_timestamp() {
        let (ws, ts) =
            parse_recovery_ref_name("refs/manifold/recovery/alice/2025-01-01T00-00-00Z").unwrap();
        assert_eq!(ws, "alice");
        assert_eq!(ts, "2025-01-01T00-00-00Z");
    }

    #[test]
    fn list_and_grep_recovery_refs_in_temp_repo() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // init repo
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(
            root.join("a.txt"),
            "one
needle
three
",
        )
        .unwrap();
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let oid_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(oid_out.status.success());
        let oid = String::from_utf8_lossy(&oid_out.stdout).trim().to_string();

        // pin recovery ref
        let ref_name = "refs/manifold/recovery/alice/2025-01-01T00-00-00Z";
        let upd = Command::new("git")
            .args(["update-ref", ref_name, &oid])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(upd.status.success());

        let refs = list_recovery_refs(root).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_name, ref_name);
        assert_eq!(refs[0].workspace, "alice");
        assert_eq!(refs[0].oid, oid);

        let hits = git_grep_hits(root, &oid, "needle", false, false, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.txt");
        assert_eq!(hits[0].line, 2);

        let mut cache = HashMap::new();
        let snippet =
            build_snippet(root, &oid, "a.txt", 2, 1, &hits[0].line_text, &mut cache).unwrap();
        assert_eq!(snippet.len(), 3);
        assert_eq!(snippet[1].line, 2);
        assert!(snippet[1].is_match);
    }

    // -----------------------------------------------------------------------
    // Dangling snapshot detection
    // -----------------------------------------------------------------------

    /// Helper: create a git repo with a commit and ws/ directory structure.
    /// Returns (tempdir, root, HEAD oid).
    fn setup_dangling_test_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&root)
            .output()
            .unwrap();

        fs::write(root.join("a.txt"), "content\n").unwrap();
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&root)
            .output()
            .unwrap();

        let oid_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let oid = String::from_utf8_lossy(&oid_out.stdout).trim().to_string();

        // Create ws/ directory
        fs::create_dir_all(root.join("ws")).unwrap();

        (dir, root, oid)
    }

    fn pin_ref(root: &std::path::Path, ref_name: &str, oid: &str) {
        let out = Command::new("git")
            .args(["update-ref", ref_name, oid])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "update-ref failed for {ref_name}");
    }

    #[test]
    fn dangling_no_refs_returns_empty() {
        let (_dir, root, _oid) = setup_dangling_test_repo();
        let result = find_dangling_snapshots(&root).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn single_ref_for_destroyed_workspace_is_not_dangling() {
        // A single recovery ref for a workspace that no longer exists
        // should NOT be marked dangling (it's the only recovery point).
        let (_dir, root, oid) = setup_dangling_test_repo();

        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-01T00-00-00Z",
            &oid,
        );

        // alice workspace does not exist under ws/
        let result = find_dangling_snapshots(&root).unwrap();
        assert!(
            result.is_empty(),
            "single ref for destroyed ws should not be dangling"
        );
    }

    #[test]
    fn multiple_refs_for_destroyed_workspace_are_dangling() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-01T00-00-00Z",
            &oid,
        );
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-02T00-00-00Z",
            &oid,
        );

        // alice workspace does not exist under ws/
        let result = find_dangling_snapshots(&root).unwrap();
        assert_eq!(result.len(), 2, "both refs for destroyed ws should be dangling");

        // Check reasons
        let superseded: Vec<_> = result
            .iter()
            .filter(|d| d.reason == DanglingReason::SupersededByNewer)
            .collect();
        let destroyed: Vec<_> = result
            .iter()
            .filter(|d| d.reason == DanglingReason::WorkspaceDestroyed)
            .collect();
        assert_eq!(superseded.len(), 1, "older ref should be superseded");
        assert_eq!(destroyed.len(), 1, "most recent ref should be 'destroyed'");
    }

    #[test]
    fn single_ref_for_active_workspace_is_not_dangling() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Create workspace directory
        fs::create_dir_all(root.join("ws").join("bob")).unwrap();

        pin_ref(
            &root,
            "refs/manifold/recovery/bob/2025-01-01T00-00-00Z",
            &oid,
        );

        let result = find_dangling_snapshots(&root).unwrap();
        assert!(
            result.is_empty(),
            "single ref for active ws should not be dangling"
        );
    }

    #[test]
    fn multiple_refs_for_active_workspace_marks_old_as_superseded() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Create workspace directory
        fs::create_dir_all(root.join("ws").join("bob")).unwrap();

        pin_ref(
            &root,
            "refs/manifold/recovery/bob/2025-01-01T00-00-00Z",
            &oid,
        );
        pin_ref(
            &root,
            "refs/manifold/recovery/bob/2025-01-02T00-00-00Z",
            &oid,
        );

        let result = find_dangling_snapshots(&root).unwrap();
        assert_eq!(result.len(), 1, "only the older ref should be dangling");
        assert_eq!(result[0].reason, DanglingReason::SupersededByNewer);
        assert!(result[0].timestamp.contains("01-01"));
    }

    #[test]
    fn cleanup_removes_superseded_refs() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Two refs for destroyed workspace
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-01T00-00-00Z",
            &oid,
        );
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-02T00-00-00Z",
            &oid,
        );

        // Conservative cleanup (all=false): only superseded
        let removed = cleanup_dangling_snapshots(&root, false).unwrap();
        assert_eq!(removed.len(), 1, "only superseded ref should be removed");
        assert_eq!(removed[0].reason, DanglingReason::SupersededByNewer);

        // Verify the ref is actually gone
        let refs = list_recovery_refs(&root).unwrap();
        assert_eq!(refs.len(), 1, "only one ref should remain after cleanup");
        assert!(refs[0].timestamp.contains("01-02"));
    }

    #[test]
    fn cleanup_all_removes_all_dangling_refs() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Two refs for destroyed workspace
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-01T00-00-00Z",
            &oid,
        );
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-02T00-00-00Z",
            &oid,
        );

        // Aggressive cleanup (all=true)
        let removed = cleanup_dangling_snapshots(&root, true).unwrap();
        assert_eq!(removed.len(), 2, "all refs should be removed");

        // Verify all refs are gone
        let refs = list_recovery_refs(&root).unwrap();
        assert!(refs.is_empty(), "no refs should remain after cleanup --all");
    }

    #[test]
    fn active_merge_protects_workspace_refs() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Pin recovery refs for "carol"
        pin_ref(
            &root,
            "refs/manifold/recovery/carol/2025-01-01T00-00-00Z",
            &oid,
        );
        pin_ref(
            &root,
            "refs/manifold/recovery/carol/2025-01-02T00-00-00Z",
            &oid,
        );

        // Simulate active merge by writing merge-state.json
        let manifold_dir = root.join(".manifold");
        fs::create_dir_all(&manifold_dir).unwrap();
        let merge_state = serde_json::json!({
            "phase": "build",
            "sources": ["carol"],
            "epoch_before": "a".repeat(40),
            "started_at": 1704067200u64,
            "updated_at": 1704067200u64
        });
        fs::write(
            manifold_dir.join("merge-state.json"),
            serde_json::to_string_pretty(&merge_state).unwrap(),
        )
        .unwrap();

        // Should find no dangling refs because carol is in an active merge
        let result = find_dangling_snapshots(&root).unwrap();
        assert!(
            result.is_empty(),
            "refs for workspace in active merge should not be dangling"
        );
    }

    #[test]
    fn list_active_workspace_names_finds_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("ws").join("alice")).unwrap();
        fs::create_dir_all(root.join("ws").join("bob")).unwrap();
        // File, not directory — should be excluded
        fs::write(root.join("ws").join("not-a-ws"), "").unwrap();

        let names = list_active_workspace_names(root);
        assert!(names.contains("alice"));
        assert!(names.contains("bob"));
        assert!(!names.contains("not-a-ws"));
    }

    #[test]
    fn mixed_workspaces_only_dangling_for_destroyed() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Create one active workspace
        fs::create_dir_all(root.join("ws").join("bob")).unwrap();

        // bob has one ref (active, should NOT be dangling)
        pin_ref(
            &root,
            "refs/manifold/recovery/bob/2025-01-01T00-00-00Z",
            &oid,
        );

        // alice has two refs (destroyed, both dangling)
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-01T00-00-00Z",
            &oid,
        );
        pin_ref(
            &root,
            "refs/manifold/recovery/alice/2025-01-02T00-00-00Z",
            &oid,
        );

        let result = find_dangling_snapshots(&root).unwrap();
        assert_eq!(result.len(), 2, "only alice's refs should be dangling");
        for d in &result {
            assert_eq!(d.workspace, "alice");
        }
    }
}
