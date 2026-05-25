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
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use maw_git::{EntryMode, GitOid, GitRepo as _};
use serde::Serialize;

use crate::audit::{self, AuditEvent};
use crate::format::OutputFormat;
use maw_core::merge_state::MergeStateFile;

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
                    "Restore file: maw ws recover <name> --restore-file <path>".to_string(),
                    "Restore workspace: maw ws recover <name> --to <new-name>".to_string(),
                    "Search: maw ws recover --search <pattern>".to_string(),
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

/// Marker shown in the SNAPSHOT column for `capture: none` rows. The trailing
/// `*` ties the row to the footnote printed below the table.
const NONE_CAPTURE_MARKER: &str = "-*";

fn has_none_capture(summaries: &[DestroyedWorkspaceSummary]) -> bool {
    summaries.iter().any(|s| s.capture_mode == "none")
}

fn snapshot_display_for(s: &DestroyedWorkspaceSummary) -> &str {
    if s.capture_mode == "none" {
        NONE_CAPTURE_MARKER
    } else {
        s.snapshot_oid.as_deref().unwrap_or("-")
    }
}

fn print_list_text(summaries: &[DestroyedWorkspaceSummary]) {
    println!("NAME\tDESTROYED_AT\tCAPTURE\tSNAPSHOT\tDIRTY_FILES");
    for s in summaries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            s.name,
            s.destroyed_at,
            s.capture_mode,
            snapshot_display_for(s),
            s.dirty_file_count,
        );
    }
    println!();
    print_list_footer(summaries, false);
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
        let snapshot_display = snapshot_display_for(s);
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
    print_list_footer(summaries, use_color);
}

fn print_list_footer(summaries: &[DestroyedWorkspaceSummary], use_color: bool) {
    let lines = [
        "Next: maw ws recover <name>                        # destroy history for one",
        "      maw ws recover <name> --show <path>          # show a file from latest snapshot",
        "      maw ws recover <name> --restore-file <path>  # restore a file into the default workspace",
        "      maw ws recover --search <pattern>            # search across all snapshots",
    ];
    for line in lines {
        if use_color {
            println!("\x1b[90m{line}\x1b[0m");
        } else {
            println!("{line}");
        }
    }

    if has_none_capture(summaries) {
        println!();
        let note = "* \"none\" capture: work was already merged at destroy time; check git log on the merge target.";
        if use_color {
            println!("\x1b[90m{note}\x1b[0m");
        } else {
            println!("{note}");
        }
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
#[expect(
    clippy::too_many_lines,
    reason = "search command performs ref scan, matching, truncation, and rendering"
)]
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
        if format == OutputFormat::Json {
            let envelope = RecoverSearchEnvelope {
                pattern: pattern.to_string(),
                workspace_filter: workspace_filter.map(std::string::ToString::to_string),
                ref_filter: ref_filter.map(std::string::ToString::to_string),
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
        } else {
            println!("No pinned recovery snapshots found to search.");
            println!("List refs: git for-each-ref {RECOVERY_PREFIX}");
        }
        return Ok(());
    }

    let mut hits: Vec<SearchHit> = Vec::new();
    let mut truncated = false;
    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();

    maw::fp!("FP_RECOVER_BEFORE_SEARCH")?;
    'scan: for r in &refs {
        let grep_hits = grep_snapshot(&git_cwd, &r.oid, pattern, regex, ignore_case, text)?;
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
        workspace_filter: workspace_filter.map(std::string::ToString::to_string),
        ref_filter: ref_filter.map(std::string::ToString::to_string),
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
        workspace_filter: workspace_filter.map(std::string::ToString::to_string),
        ref_filter: ref_filter.map(std::string::ToString::to_string),
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
    let (ws, ts) = rest.split_once('/')?;

    Some((ws.to_string(), ts.to_string()))
}

fn list_recovery_refs(git_cwd: &Path) -> Result<Vec<RecoveryRef>> {
    let repo = maw_git::GixRepo::open(git_cwd)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", git_cwd.display()))?;
    let refs = repo
        .list_refs(RECOVERY_PREFIX)
        .map_err(|e| anyhow::anyhow!("list_refs failed: {e}"))?;

    let mut out: Vec<RecoveryRef> = Vec::new();

    for (ref_name, oid) in refs {
        if let Some((ws, ts)) = parse_recovery_ref_name(ref_name.as_str()) {
            out.push(RecoveryRef {
                ref_name: ref_name.as_str().to_string(),
                workspace: ws,
                timestamp: ts,
                oid: oid.to_string(),
            });
        }
    }

    Ok(out)
}

/// Open a `GixRepo` at `git_cwd` with a uniform error message.
fn open_repo(git_cwd: &Path) -> Result<maw_git::GixRepo> {
    maw_git::GixRepo::open(git_cwd)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", git_cwd.display()))
}

/// Parse a 40-char hex OID string from a recovery snapshot.
fn parse_oid(oid: &str) -> Result<GitOid> {
    oid.parse::<GitOid>()
        .map_err(|e| anyhow::anyhow!("invalid OID '{oid}': {e}"))
}

/// Mirror `git grep -I`'s binary detection: a blob is "binary" if any of the
/// first 8000 bytes is a NUL.
fn looks_binary(content: &[u8]) -> bool {
    let probe_len = content.len().min(8000);
    content[..probe_len].contains(&0)
}

/// Walk all blobs in the snapshot at `oid` and return one [`GrepHit`] per
/// matching line. Mirrors the semantics of
/// `git grep -z -n [-I|-a] [-i] [-F] -e <pattern> <oid>`.
///
/// - When `text` is false, blobs that look binary are skipped (matches `-I`).
/// - When `text` is true, binary blobs are searched as text (matches `-a`).
/// - `regex` toggles regex vs. fixed-string search.
/// - `ignore_case` makes matching case-insensitive.
fn grep_snapshot(
    git_cwd: &Path,
    oid: &str,
    pattern: &str,
    regex: bool,
    ignore_case: bool,
    text: bool,
) -> Result<Vec<GrepHit>> {
    let repo = open_repo(git_cwd)?;
    let tree_oid = parse_oid(oid)?;

    let matcher = if regex {
        let mut builder = regex::RegexBuilder::new(pattern);
        builder.case_insensitive(ignore_case);
        let re = builder
            .build()
            .map_err(|e| anyhow::anyhow!("invalid regex pattern: {e}"))?;
        Matcher::Regex(re)
    } else if ignore_case {
        Matcher::FixedFold(pattern.to_lowercase())
    } else {
        Matcher::Fixed(pattern.to_string())
    };

    let mut hits: Vec<GrepHit> = Vec::new();
    repo.walk_tree_blobs(tree_oid, |entry, content| {
        // Skip symlinks (their "content" is a target path, not file text).
        if entry.mode == EntryMode::Link {
            return Ok(());
        }
        if !text && looks_binary(content) {
            return Ok(());
        }

        // Split on '\n', preserving git grep's CRLF stripping for line text.
        let text_str = match std::str::from_utf8(content) {
            Ok(s) => s,
            // If content isn't UTF-8 but we're in -a mode, fall back to lossy
            // matching: do a lossy decode of the whole blob in one allocation.
            // The decoded length differs from the raw bytes when invalid
            // sequences are replaced with U+FFFD; line numbers stay correct
            // because invalid bytes never form `\n`.
            Err(_) if text => {
                let lossy = String::from_utf8_lossy(content);
                for (idx, line) in lossy.split('\n').enumerate() {
                    let line_no = idx + 1;
                    let stripped = line.strip_suffix('\r').unwrap_or(line);
                    if matcher.matches(stripped) {
                        hits.push(GrepHit {
                            path: entry.path.clone(),
                            line: line_no,
                            line_text: stripped.to_string(),
                        });
                    }
                }
                return Ok(());
            }
            // Non-UTF8 and not in -a mode: skip (matches `-I` "treat as binary"
            // behavior for un-decodable bytes).
            Err(_) => return Ok(()),
        };

        for (idx, line) in text_str.split('\n').enumerate() {
            let line_no = idx + 1;
            let stripped = line.strip_suffix('\r').unwrap_or(line);
            if matcher.matches(stripped) {
                hits.push(GrepHit {
                    path: entry.path.clone(),
                    line: line_no,
                    line_text: stripped.to_string(),
                });
            }
        }
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("snapshot blob walk failed: {e}"))?;

    Ok(hits)
}

/// Pattern matcher used by [`grep_snapshot`].
enum Matcher {
    /// Literal substring, case-sensitive.
    Fixed(String),
    /// Literal substring, case-insensitive (pre-lowercased pattern).
    FixedFold(String),
    /// Compiled regular expression.
    Regex(regex::Regex),
}

impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Self::Fixed(pat) => line.contains(pat.as_str()),
            Self::FixedFold(pat) => line.to_lowercase().contains(pat.as_str()),
            Self::Regex(re) => re.is_match(line),
        }
    }
}

/// Read the snapshot file at `oid:path` and return its lines.
///
/// Uses gix tree traversal — equivalent to `git show <oid>:<path>` but with no
/// subprocess. Returns an empty list if the path is missing or names a tree.
fn read_file_lines(git_cwd: &Path, oid: &str, path: &str) -> Result<Vec<String>> {
    let repo = open_repo(git_cwd)?;
    let tree_oid = parse_oid(oid)?;
    let Some((_mode, _blob_oid, content)) = repo
        .read_blob_at_path(tree_oid, path)
        .map_err(|e| anyhow::anyhow!("read_blob_at_path failed: {e}"))?
    else {
        bail!(
            "Path '{path}' not found in snapshot {oid_short}",
            oid_short = &oid[..oid.len().min(12)]
        );
    };
    let text = String::from_utf8_lossy(&content);
    Ok(text.lines().map(std::string::ToString::to_string).collect())
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
    let repo = maw_git::GixRepo::open(git_cwd)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", git_cwd.display()))?;
    let spec = format!("{reference}^{{commit}}");
    let oid = repo
        .rev_parse(&spec)
        .map_err(|e| anyhow::anyhow!("Failed to resolve recovery ref '{reference}': {e}"))?;
    Ok(oid.to_string())
}

/// Write the snapshot file at `oid:path` to stdout (binary-safe).
fn show_file_at_oid(git_cwd: &Path, oid: &str, path: &str) -> Result<()> {
    let repo = open_repo(git_cwd)?;
    let tree_oid = parse_oid(oid)?;
    let oid_short = &oid[..oid.len().min(12)];
    let Some((_mode, _blob_oid, content)) =
        repo.read_blob_at_path(tree_oid, path).map_err(|e| {
            anyhow::anyhow!("Failed to show file '{path}' from snapshot {oid_short}: {e}")
        })?
    else {
        bail!("Failed to show file '{path}' from snapshot {oid_short}: path not found");
    };
    std::io::stdout().write_all(&content)?;
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

    // Resolve <oid>:<path> via gix tree traversal.
    // Run from the git common dir (repo root) so the ref resolves.
    let git_cwd = super::git_cwd()?;
    let repo = open_repo(&git_cwd)?;
    let tree_oid = parse_oid(&oid)?;
    let Some((_mode, _blob_oid, content)) = repo
        .read_blob_at_path(tree_oid, path)
        .map_err(|e| anyhow::anyhow!("snapshot lookup failed: {e}"))?
    else {
        bail!(
            "File '{path}' not found in snapshot {oid_short} for workspace '{name}'.\n  \
             List dirty files: maw ws recover {name}",
            oid_short = &oid[..oid.len().min(12)],
        );
    };

    // Write raw content to stdout (binary-safe).
    std::io::stdout()
        .write_all(&content)
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

// ---------------------------------------------------------------------------
// Restore a single file from a snapshot into the default workspace
// ---------------------------------------------------------------------------

/// One entry resolved from the tree at a snapshot commit OID.
///
/// `mode` mirrors the `git ls-tree` mode string (`"100644"`, `"100755"`,
/// `"120000"`, ...) so it can be passed straight to [`write_with_mode`].
struct LsTreeEntry {
    mode: String,
    oid: String,
}

/// Look up `path` in the tree at `commit_oid`. Returns `None` if missing or
/// when the entry is a tree / submodule.
fn ls_tree_entry(git_cwd: &Path, commit_oid: &str, path: &str) -> Result<Option<LsTreeEntry>> {
    let repo = open_repo(git_cwd)?;
    let tree_oid = parse_oid(commit_oid)?;
    let Some((mode, oid)) = repo
        .find_entry_at_path(tree_oid, path)
        .map_err(|e| anyhow::anyhow!("find_entry_at_path failed: {e}"))?
    else {
        return Ok(None);
    };
    let mode_str = match mode {
        EntryMode::Blob => "100644",
        EntryMode::BlobExecutable => "100755",
        EntryMode::Link => "120000",
        // Restoring a directory or submodule via --restore-file isn't meaningful.
        EntryMode::Tree | EntryMode::Commit => return Ok(None),
    };
    Ok(Some(LsTreeEntry {
        mode: mode_str.to_owned(),
        oid: oid.to_string(),
    }))
}

/// Read the raw bytes of a blob (replaces `git cat-file blob <oid>`).
fn cat_file_blob(git_cwd: &Path, blob_oid: &str) -> Result<Vec<u8>> {
    let repo = open_repo(git_cwd)?;
    let oid = parse_oid(blob_oid)?;
    repo.read_blob(oid)
        .map_err(|e| anyhow::anyhow!("read_blob failed: {e}"))
}

/// List blob/symlink paths reachable from `oid`. Used for the
/// "available paths" hint when `--restore-file` cannot find the target.
fn ls_tree_paths(git_cwd: &Path, oid: &str) -> Result<Vec<String>> {
    let repo = open_repo(git_cwd)?;
    let tree_oid = parse_oid(oid)?;
    let entries = repo
        .walk_tree_blob_paths(tree_oid)
        .map_err(|e| anyhow::anyhow!("walk_tree_blob_paths failed: {e}"))?;
    Ok(entries.into_iter().map(|e| e.path).collect())
}

/// Check whether the destination has uncommitted changes affecting `path`.
fn dest_has_uncommitted(default_ws: &Path, path: &str) -> Result<bool> {
    let repo = open_repo(default_ws)?;
    // HEAD→worktree (incl. staged): `git status --porcelain` reports staged
    // changes too, so the plain index→worktree `status()` would under-report
    // and let `--restore-file` clobber staged work without `--force`.
    let entries = repo
        .status_head_to_worktree()
        .map_err(|e| anyhow::anyhow!("status failed: {e}"))?;
    // Match git's `git status --porcelain -- <path>` semantics: a path is
    // "uncommitted" if it appears in status output exactly, or if a directory
    // path was requested and any tracked file under it has changes.
    let path_has_trailing = path.ends_with('/');
    let dir_prefix = if path_has_trailing {
        path.to_string()
    } else {
        format!("{path}/")
    };
    Ok(entries
        .iter()
        .any(|e| e.path == path || e.path.starts_with(&dir_prefix)))
}

#[cfg(unix)]
fn write_with_mode(dest: &Path, content: &[u8], mode: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    if mode == "120000" {
        // Symlink: blob content is the target path.
        let target = std::str::from_utf8(content).context("symlink target is not valid UTF-8")?;
        if dest.exists() || dest.symlink_metadata().is_ok() {
            std::fs::remove_file(dest)
                .with_context(|| format!("remove existing {}", dest.display()))?;
        }
        std::os::unix::fs::symlink(target, dest)
            .with_context(|| format!("create symlink {}", dest.display()))?;
        return Ok(());
    }

    std::fs::write(dest, content).with_context(|| format!("write to {}", dest.display()))?;

    let perm_bits: u32 = if mode == "100755" { 0o755 } else { 0o644 };
    let perms = std::fs::Permissions::from_mode(perm_bits);
    std::fs::set_permissions(dest, perms)
        .with_context(|| format!("set permissions on {}", dest.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_with_mode(dest: &Path, content: &[u8], _mode: &str) -> Result<()> {
    std::fs::write(dest, content).with_context(|| format!("write to {}", dest.display()))?;
    Ok(())
}

/// Restore a single file from snapshot `oid` into the default workspace's worktree.
fn restore_file_at_oid(
    git_cwd: &Path,
    default_ws: &Path,
    oid: &str,
    path: &str,
    force: bool,
    audit_ref: &str,
) -> Result<()> {
    let Some(entry) = ls_tree_entry(git_cwd, oid, path)? else {
        let oid_short = &oid[..oid.len().min(12)];
        // Best-effort hint: list paths in the snapshot, or suggest --search.
        match ls_tree_paths(git_cwd, oid) {
            Ok(mut listed) if !listed.is_empty() => {
                listed.sort();
                let preview: Vec<String> = listed.iter().take(20).cloned().collect();
                let suffix = if listed.len() > preview.len() {
                    format!("\n  ... and {} more", listed.len() - preview.len())
                } else {
                    String::new()
                };
                bail!(
                    "Path '{path}' not found in snapshot {oid_short}.\n  \
                     Available paths in snapshot:\n  {}{suffix}\n  \
                     Or search across snapshots: maw ws recover --search <pattern>",
                    preview.join("\n  "),
                )
            }
            _ => bail!(
                "Path '{path}' not found in snapshot {oid_short}.\n  \
                 Search across snapshots: maw ws recover --search <pattern>"
            ),
        }
    };

    if !force && dest_has_uncommitted(default_ws, path)? {
        bail!(
            "Refusing to overwrite '{path}' — destination has uncommitted changes.\n  \
             Re-run with --force to overwrite, or commit/stash first."
        );
    }

    let content = cat_file_blob(git_cwd, &entry.oid)?;

    let dest = default_ws.join(path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", dest.display()))?;
    }

    write_with_mode(&dest, &content, &entry.mode)?;

    audit::log_audit(&AuditEvent::Show {
        ref_name: audit_ref.to_string(),
        path: path.to_string(),
    });

    let oid_short = &oid[..oid.len().min(12)];
    println!("restored: {path} from {oid_short}");
    println!("Next: maw exec default -- git diff {path}  # review before commit");
    Ok(())
}

/// `maw ws recover --ref <recovery-ref> --restore-file <path>`.
pub fn restore_file_by_ref(recovery_ref: &str, path: &str, force: bool) -> Result<()> {
    validate_recovery_ref(recovery_ref)?;
    validate_show_path(path)?;

    let git_cwd = super::git_cwd()?;
    let default_ws = workspace_path(super::DEFAULT_WORKSPACE)?;
    if !default_ws.exists() {
        bail!(
            "Default workspace not found at {}.\n  \
             --restore-file writes into the default workspace's worktree.",
            default_ws.display()
        );
    }
    let oid = resolve_ref_to_oid(&git_cwd, recovery_ref)?;
    restore_file_at_oid(&git_cwd, &default_ws, &oid, path, force, recovery_ref)
}

/// `maw ws recover <name> --restore-file <path>` (latest destroy snapshot).
pub fn restore_file(name: &str, path: &str, force: bool) -> Result<()> {
    validate_workspace_name(name)?;
    validate_show_path(path)?;
    let root = repo_root()?;

    let record = destroy_record::read_latest_record(&root, name)?
        .with_context(|| format!("No destroy records found for workspace '{name}'"))?;

    let oid = resolve_recoverable_oid(&record)?;

    let git_cwd = super::git_cwd()?;
    let default_ws = workspace_path(super::DEFAULT_WORKSPACE)?;
    if !default_ws.exists() {
        bail!(
            "Default workspace not found at {}.\n  \
             --restore-file writes into the default workspace's worktree.",
            default_ws.display()
        );
    }

    let audit_ref = record
        .snapshot_ref
        .unwrap_or_else(|| format!("(workspace:{name})"));

    restore_file_at_oid(&git_cwd, &default_ws, &oid, path, force, &audit_ref)
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
    super::create::create(new_name, None, None, false, None, None)?;
    let new_path = workspace_path(new_name)?;

    if let Err(e) = populate_from_snapshot(&new_path, &oid) {
        eprintln!("Populate failed, rolling back workspace '{new_name}': {e:#}");
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
    maw::fp!("FP_RECOVER_BEFORE_RESTORE")?;
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
    super::create::create(new_name, None, None, false, None, None)?;

    // Step 2: Populate from the snapshot using git read-tree + checkout-index
    let new_ws_path = workspace_path(new_name)?;
    if let Err(e) = populate_from_snapshot(&new_ws_path, &oid) {
        eprintln!("Populate failed, rolling back workspace '{new_name}': {e:#}");
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

/// Populate a workspace from a snapshot OID.
///
/// Restores the workspace to the exact snapshot commit with tracked index +
/// working tree state, so recovered files are not left as untracked dirtiness.
///
/// Equivalent to `git checkout --detach <oid>`: writes the snapshot tree into
/// the worktree (and index), then points HEAD at the snapshot commit so the
/// workspace is detached at the exact recovered revision.
fn populate_from_snapshot(ws_path: &std::path::Path, oid: &str) -> Result<()> {
    let repo = open_repo(ws_path)?;
    let target = parse_oid(oid)?;
    repo.checkout_tree(target, ws_path)
        .map_err(|e| anyhow::anyhow!("checkout_tree {oid} failed: {e}"))?;
    repo.set_head(target)
        .map_err(|e| anyhow::anyhow!("set_head {oid} failed: {e}"))?;
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
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let default_ws = flavor.default_target_path(root, "default");
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

        if ws_exists {
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
        } else {
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
                    reason: if r.ref_name
                        == ws_refs.last().expect("operation should succeed").ref_name
                    {
                        DanglingReason::WorkspaceDestroyed
                    } else {
                        DanglingReason::SupersededByNewer
                    },
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
        maw_core::refs::delete_ref(root, &d.ref_name)
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
                    removed: to_show.iter().copied().cloned().collect(),
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

/// List active workspace names by scanning the workspaces directory
/// (layout-aware: `ws/` in v2, `.maw/workspaces/` in consolidated).
fn list_active_workspace_names(root: &Path) -> HashSet<String> {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let ws_dir = flavor.workspaces_dir(root);
    let mut names = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&ws_dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let name = entry.file_name().to_string_lossy().to_string();
                names.insert(name);
            }
        }
    }
    names
}

/// Get workspace names involved in an active merge (if any).
fn active_merge_workspaces(root: &Path) -> HashSet<String> {
    let state_path = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("merge-state.json");
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
    use std::process::Command;

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
        let oid = resolve_recoverable_oid(&record).expect("operation should succeed");
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
        let oid = resolve_recoverable_oid(&record).expect("operation should succeed");
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
        let (ws, ts) = parse_recovery_ref_name("refs/manifold/recovery/alice/2025-01-01T00-00-00Z")
            .expect("operation should succeed");
        assert_eq!(ws, "alice");
        assert_eq!(ts, "2025-01-01T00-00-00Z");
    }

    #[test]
    fn list_and_grep_recovery_refs_in_temp_repo() {
        // bn-5rdz: init + identity + commit via shared helper.
        let (_dir, root) = maw_git::test_support::init_test_repo();

        fs::write(
            root.join("a.txt"),
            "one
needle
three
",
        )
        .expect("operation should succeed");
        let oid = maw_git::test_support::commit_all(&root, "init");

        // pin recovery ref
        let ref_name = "refs/manifold/recovery/alice/2025-01-01T00-00-00Z";
        let upd = Command::new("git")
            .args(["update-ref", ref_name, &oid])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        assert!(upd.status.success());

        let refs = list_recovery_refs(&root).expect("operation should succeed");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_name, ref_name);
        assert_eq!(refs[0].workspace, "alice");
        assert_eq!(refs[0].oid, oid);

        let hits = grep_snapshot(&root, &oid, "needle", false, false, false)
            .expect("operation should succeed");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.txt");
        assert_eq!(hits[0].line, 2);

        let mut cache = HashMap::new();
        let snippet = build_snippet(&root, &oid, "a.txt", 2, 1, &hits[0].line_text, &mut cache)
            .expect("operation should succeed");
        assert_eq!(snippet.len(), 3);
        assert_eq!(snippet[1].line, 2);
        assert!(snippet[1].is_match);
    }

    #[test]
    fn grep_snapshot_handles_paths_with_colons() {
        // bn-5rdz: init + identity via shared helper, then seed + commit.
        let (_dir, root) = maw_git::test_support::init_test_repo();
        fs::write(root.join("with:colon.txt"), "first\ncolon-needle\n")
            .expect("operation should succeed");
        let oid = maw_git::test_support::commit_all(&root, "colon path");

        let hits = grep_snapshot(&root, &oid, "colon-needle", false, false, false)
            .expect("operation should succeed");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "with:colon.txt");
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].line_text, "colon-needle");
    }

    // -----------------------------------------------------------------------
    // Dangling snapshot detection
    // -----------------------------------------------------------------------

    /// Helper: create a git repo with a commit and ws/ directory structure.
    /// Returns (tempdir, root, HEAD oid).
    fn setup_dangling_test_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
        // bn-5rdz: init + identity + commit via shared helpers; then add the
        // file this test cares about and the `ws/` marker dir.
        let (dir, root) = maw_git::test_support::init_test_repo();
        fs::write(root.join("a.txt"), "content\n").expect("operation should succeed");
        let oid = maw_git::test_support::commit_all(&root, "init");
        fs::create_dir_all(root.join("ws")).expect("operation should succeed");
        (dir, root, oid)
    }

    fn pin_ref(root: &std::path::Path, ref_name: &str, oid: &str) {
        let out = Command::new("git")
            .args(["update-ref", ref_name, oid])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        assert!(out.status.success(), "update-ref failed for {ref_name}");
    }

    #[test]
    fn dangling_no_refs_returns_empty() {
        let (_dir, root, _oid) = setup_dangling_test_repo();
        let result = find_dangling_snapshots(&root).expect("operation should succeed");
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
        let result = find_dangling_snapshots(&root).expect("operation should succeed");
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
        let result = find_dangling_snapshots(&root).expect("operation should succeed");
        assert_eq!(
            result.len(),
            2,
            "both refs for destroyed ws should be dangling"
        );

        // Check reasons
        let superseded = result
            .iter()
            .filter(|d| d.reason == DanglingReason::SupersededByNewer)
            .count();
        let destroyed = result
            .iter()
            .filter(|d| d.reason == DanglingReason::WorkspaceDestroyed)
            .count();
        assert_eq!(superseded, 1, "older ref should be superseded");
        assert_eq!(destroyed, 1, "most recent ref should be 'destroyed'");
    }

    #[test]
    fn single_ref_for_active_workspace_is_not_dangling() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Create workspace directory
        fs::create_dir_all(root.join("ws").join("bob")).expect("operation should succeed");

        pin_ref(
            &root,
            "refs/manifold/recovery/bob/2025-01-01T00-00-00Z",
            &oid,
        );

        let result = find_dangling_snapshots(&root).expect("operation should succeed");
        assert!(
            result.is_empty(),
            "single ref for active ws should not be dangling"
        );
    }

    #[test]
    fn multiple_refs_for_active_workspace_marks_old_as_superseded() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Create workspace directory
        fs::create_dir_all(root.join("ws").join("bob")).expect("operation should succeed");

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

        let result = find_dangling_snapshots(&root).expect("operation should succeed");
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
        let removed = cleanup_dangling_snapshots(&root, false).expect("operation should succeed");
        assert_eq!(removed.len(), 1, "only superseded ref should be removed");
        assert_eq!(removed[0].reason, DanglingReason::SupersededByNewer);

        // Verify the ref is actually gone
        let refs = list_recovery_refs(&root).expect("operation should succeed");
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
        let removed = cleanup_dangling_snapshots(&root, true).expect("operation should succeed");
        assert_eq!(removed.len(), 2, "all refs should be removed");

        // Verify all refs are gone
        let refs = list_recovery_refs(&root).expect("operation should succeed");
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
        fs::create_dir_all(&manifold_dir).expect("operation should succeed");
        let merge_state = serde_json::json!({
            "phase": "build",
            "sources": ["carol"],
            "epoch_before": "a".repeat(40),
            "started_at": 1_704_067_200_u64,
            "updated_at": 1_704_067_200_u64
        });
        fs::write(
            manifold_dir.join("merge-state.json"),
            serde_json::to_string_pretty(&merge_state).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        // Should find no dangling refs because carol is in an active merge
        let result = find_dangling_snapshots(&root).expect("operation should succeed");
        assert!(
            result.is_empty(),
            "refs for workspace in active merge should not be dangling"
        );
    }

    #[test]
    fn list_active_workspace_names_finds_directories() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        fs::create_dir_all(root.join("ws").join("alice")).expect("operation should succeed");
        fs::create_dir_all(root.join("ws").join("bob")).expect("operation should succeed");
        // File, not directory — should be excluded
        fs::write(root.join("ws").join("not-a-ws"), "").expect("operation should succeed");

        let names = list_active_workspace_names(root);
        assert!(names.contains("alice"));
        assert!(names.contains("bob"));
        assert!(!names.contains("not-a-ws"));
    }

    #[test]
    fn mixed_workspaces_only_dangling_for_destroyed() {
        let (_dir, root, oid) = setup_dangling_test_repo();

        // Create one active workspace
        fs::create_dir_all(root.join("ws").join("bob")).expect("operation should succeed");

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

        let result = find_dangling_snapshots(&root).expect("operation should succeed");
        assert_eq!(result.len(), 2, "only alice's refs should be dangling");
        for d in &result {
            assert_eq!(d.workspace, "alice");
        }
    }
}
