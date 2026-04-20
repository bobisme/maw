//! Resolve working-copy conflicts in a workspace.
//!
//! After a merge produces local-vs-merge conflicts (the target workspace had
//! uncommitted edits overlapping with merged files), this module resolves
//! them by parsing diff3 conflict markers and replacing file content with the
//! chosen side.
//!
//! Conflict markers use labeled sides (workspace names) so agents and humans
//! can resolve by name rather than ours/theirs.
//!
//! `--keep` accepts three forms:
//! - `NAME` — resolve all conflicted files to NAME's version
//! - `PATH=NAME` — resolve one file
//! - `cf-N=NAME` — resolve one conflict block within a file (see `--list`)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::format::OutputFormat;

use super::repo_root;

// ---------------------------------------------------------------------------
// Parsed conflict block
// ---------------------------------------------------------------------------

/// One conflict block parsed from diff3 markers in a file.
#[derive(Debug, Clone)]
struct ConflictBlock {
    /// Left side label (from `<<<<<<<`), e.g. "bn-2sc3"
    left_name: String,
    /// Left side content
    left_content: String,
    /// Base content (from `|||||||`)
    base_content: String,
    /// Right side label (from `>>>>>>>`), e.g. "default"
    right_name: String,
    /// Right side content
    right_content: String,
}

/// A parsed file: interleaved context lines and conflict blocks.
#[derive(Debug)]
enum FileChunk {
    Context(String),
    Conflict(ConflictBlock),
}

// ---------------------------------------------------------------------------
// Keep spec parsing
// ---------------------------------------------------------------------------

/// Parsed `--keep` value.
enum KeepSpec {
    /// `--keep NAME` — resolve all to this side
    All(String),
    /// `--keep PATH=NAME` — resolve one file
    File(PathBuf, String),
    /// `--keep cf-N=NAME` — resolve one block
    Block(String, String),
}

fn parse_keep_specs(raw: &[String]) -> Result<Vec<KeepSpec>> {
    let mut specs = Vec::new();
    for s in raw {
        if let Some((left, right)) = s.split_once('=') {
            let left = left.trim();
            let right = right.trim();
            if right.is_empty() {
                bail!("Invalid --keep '{s}': empty side name after '='");
            }
            if left.starts_with("cf-") {
                specs.push(KeepSpec::Block(left.to_string(), right.to_string()));
            } else {
                specs.push(KeepSpec::File(PathBuf::from(left), right.to_string()));
            }
        } else {
            specs.push(KeepSpec::All(s.clone()));
        }
    }
    Ok(specs)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    workspace: &str,
    paths: &[String],
    keep: &[String],
    list: bool,
    format: OutputFormat,
) -> Result<()> {
    let root = repo_root()?;
    let ws_path = root.join("ws").join(workspace);
    if !ws_path.exists() {
        bail!(
            "Workspace '{}' not found at {}\n  To fix: check workspace name with `maw ws list`",
            workspace,
            ws_path.display()
        );
    }

    // bn-3rah: prefer the structured sidecar when present. Falls back to the
    // legacy marker-scan below when absent or unparseable. This keeps
    // pre-gjm8 workspaces working unchanged.
    if let Some(tree) = super::resolve_structured::read_conflict_tree_sidecar(&root, workspace) {
        return super::resolve_structured::run_structured(
            &root, workspace, &ws_path, paths, keep, list, format, tree,
        )
        .map(|_| ());
    }

    if list {
        return list_conflicts(&ws_path, workspace, paths, format);
    }

    if keep.is_empty() {
        bail!(
            "Must specify --keep or --list.\n\
             \n  Examples:\n\
             \n    maw ws resolve {workspace} --keep <workspace-name>       # resolve all\n\
             \n    maw ws resolve {workspace} --keep src/main.rs=<name>     # resolve one file\n\
             \n    maw ws resolve {workspace} --keep cf-0=<name>            # resolve one block\n\
             \n    maw ws resolve {workspace} --list                        # list conflicts"
        );
    }

    let specs = parse_keep_specs(keep)?;

    // Classify: do we have an All spec, file-level specs, or block-level specs?
    let mut all_side: Option<&str> = None;
    let mut file_sides: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut block_sides: BTreeMap<String, String> = BTreeMap::new();

    for spec in &specs {
        match spec {
            KeepSpec::All(name) => {
                if all_side.is_some() {
                    bail!("Multiple blanket --keep flags. Use one, or use PATH=NAME for per-file.");
                }
                all_side = Some(name);
            }
            KeepSpec::File(path, name) => {
                file_sides.insert(path.clone(), name.clone());
            }
            KeepSpec::Block(id, name) => {
                block_sides.insert(id.clone(), name.clone());
            }
        }
    }

    // Find files to process
    let target_files: Vec<PathBuf> = if !file_sides.is_empty() && all_side.is_none() && block_sides.is_empty() {
        // Only file-level specs: process just those files
        file_sides.keys().cloned().collect()
    } else if !block_sides.is_empty() && all_side.is_none() && file_sides.is_empty() {
        // Block-level specs only: need a file context. Use paths arg or find all.
        if !paths.is_empty() {
            paths.iter().map(PathBuf::from).collect()
        } else {
            find_conflicted_files(&ws_path)?
        }
    } else {
        // All or mixed: process all conflicted files
        find_conflicted_files(&ws_path)?
    };

    if target_files.is_empty() {
        if format == OutputFormat::Json {
            println!(r#"{{"status":"clean","workspace":"{workspace}","message":"No conflicted files found."}}"#);
        } else {
            println!("No conflicted files found in '{workspace}'.");
        }
        return Ok(());
    }

    let mut resolved_count = 0;
    let mut partially_resolved = Vec::new();
    let mut skipped = Vec::new();

    for rel_path in &target_files {
        let full_path = ws_path.join(rel_path);
        if !full_path.is_file() {
            skipped.push((rel_path.clone(), "file not found".to_string()));
            continue;
        }

        let content = std::fs::read_to_string(&full_path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", rel_path.display()))?;

        if !content.contains("<<<<<<<") {
            skipped.push((rel_path.clone(), "no conflict markers".to_string()));
            continue;
        }

        // Determine resolution for this file
        let file_side = file_sides.get(rel_path).map(|s| s.as_str()).or(all_side);

        let chunks = parse_file_conflicts(&content);
        let total_blocks = chunks
            .iter()
            .filter(|c| matches!(c, FileChunk::Conflict(_)))
            .count();

        match resolve_chunks(&chunks, file_side, &block_sides) {
            Ok(resolved) => {
                std::fs::write(&full_path, resolved.as_bytes())?;

                // Count blocks that were NOT resolved (re-emitted as markers).
                // This catches the bn-2wnt case: --keep with per-block flags
                // can leave some blocks as markers even though resolve_chunks
                // returns Ok.
                let remaining_markers = resolved
                    .lines()
                    .filter(|l| l.starts_with("<<<<<<<"))
                    .count();

                if remaining_markers == 0 {
                    resolved_count += 1;
                    if format != OutputFormat::Json {
                        println!("  resolved: {}", rel_path.display());
                    }
                } else {
                    let done = total_blocks.saturating_sub(remaining_markers);
                    partially_resolved.push((rel_path.clone(), done, remaining_markers));
                    if format != OutputFormat::Json {
                        println!(
                            "  partial:  {} ({} of {} blocks resolved, {} still marked)",
                            rel_path.display(),
                            done,
                            total_blocks,
                            remaining_markers
                        );
                    }
                }
            }
            Err(e) => {
                skipped.push((rel_path.clone(), e.to_string()));
            }
        }
    }

    // After resolving, check if all conflicts are gone and update metadata.
    let remaining_conflicts = find_conflicted_files(&ws_path)?;
    let conflicts_cleared = remaining_conflicts.is_empty() && resolved_count > 0;

    if conflicts_cleared {
        // Clean up rebase conflict metadata file.
        let _ = super::sync::delete_rebase_conflicts(&root, workspace);
    }

    if format == OutputFormat::Json {
        let skipped_json: Vec<String> = skipped
            .iter()
            .map(|(p, r)| format!(r#"{{"path":"{}","reason":"{}"}}"#, p.display(), r))
            .collect();
        let partial_json: Vec<String> = partially_resolved
            .iter()
            .map(|(p, done, remaining)| {
                format!(
                    r#"{{"path":"{}","blocks_resolved":{done},"blocks_remaining":{remaining}}}"#,
                    p.display()
                )
            })
            .collect();
        println!(
            r#"{{"status":"ok","workspace":"{workspace}","resolved":{resolved_count},"partially_resolved":[{}],"conflicts_remaining":{},"skipped":[{}]}}"#,
            partial_json.join(","),
            remaining_conflicts.len(),
            skipped_json.join(",")
        );
    } else {
        if resolved_count > 0 {
            println!("\n{resolved_count} file(s) resolved.");
        }
        if !partially_resolved.is_empty() {
            println!(
                "\n{} file(s) partially resolved (some blocks still have markers):",
                partially_resolved.len()
            );
            for (path, done, remaining) in &partially_resolved {
                println!(
                    "  {} — resolved {done} block(s), {remaining} still marked",
                    path.display()
                );
            }
            println!(
                "\n  To finish: re-run `maw ws resolve {workspace} --keep ...` for the remaining blocks,"
            );
            println!("  or edit the files directly and commit.");
        }
        if !skipped.is_empty() {
            eprintln!();
            for (path, reason) in &skipped {
                eprintln!("  skipped: {} — {reason}", path.display());
            }
        }
        if resolved_count == 0 && skipped.is_empty() && partially_resolved.is_empty() {
            println!("Nothing to resolve.");
        }
        if conflicts_cleared {
            println!("All conflicts resolved — workspace is ready for merge.");
        } else if !remaining_conflicts.is_empty() {
            println!(
                "{} file(s) still have conflict markers.",
                remaining_conflicts.len()
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// List conflicts
// ---------------------------------------------------------------------------

fn list_conflicts(ws_path: &Path, workspace: &str, filter_paths: &[String], format: OutputFormat) -> Result<()> {
    let all_files = find_conflicted_files(ws_path)?;

    // If specific paths given, list blocks for those files only
    let files: Vec<PathBuf> = if filter_paths.is_empty() {
        all_files
    } else {
        let filter_set: std::collections::HashSet<PathBuf> =
            filter_paths.iter().map(PathBuf::from).collect();
        all_files.into_iter().filter(|f| filter_set.contains(f)).collect()
    };

    if format == OutputFormat::Json {
        let mut file_entries = Vec::new();
        for f in &files {
            let full = ws_path.join(f);
            let blocks = if let Ok(content) = std::fs::read_to_string(&full) {
                let chunks = parse_file_conflicts(&content);
                collect_block_info(&chunks)
            } else {
                vec![]
            };
            let blocks_json: Vec<String> = blocks
                .iter()
                .map(|b| {
                    format!(
                        r#"{{"id":"{}","left":"{}","right":"{}","preview":"{}"}}"#,
                        b.id,
                        b.left_name,
                        b.right_name,
                        b.preview.replace('"', "\\\"").replace('\n', "\\n"),
                    )
                })
                .collect();
            file_entries.push(format!(
                r#"{{"path":"{}","block_count":{},"blocks":[{}]}}"#,
                f.display(),
                blocks.len(),
                blocks_json.join(",")
            ));
        }
        println!(
            r#"{{"workspace":"{workspace}","conflict_count":{},"files":[{}]}}"#,
            files.len(),
            file_entries.join(",")
        );
        return Ok(());
    }

    if files.is_empty() {
        println!("No conflicted files in '{workspace}'.");
        return Ok(());
    }

    let show_blocks = !filter_paths.is_empty();

    if show_blocks {
        // Detailed per-block listing for specific files
        for f in &files {
            let full = ws_path.join(f);
            let content = std::fs::read_to_string(&full)?;
            let chunks = parse_file_conflicts(&content);
            let blocks = collect_block_info(&chunks);

            println!("{}  ({} conflict block(s))", f.display(), blocks.len());
            for b in &blocks {
                println!("  {} — {} vs {}", b.id, b.left_name, b.right_name);
                // Show a preview (first line of each side)
                let left_preview = b.left_preview.lines().next().unwrap_or("(empty)");
                let right_preview = b.right_preview.lines().next().unwrap_or("(empty)");
                println!("    {}: {}", b.left_name, truncate(left_preview, 60));
                println!("    {}: {}", b.right_name, truncate(right_preview, 60));
            }
        }
    } else {
        // Summary listing
        println!("{} conflicted file(s) in '{workspace}':", files.len());
        for f in &files {
            let full = ws_path.join(f);
            let block_count = if let Ok(content) = std::fs::read_to_string(&full) {
                let chunks = parse_file_conflicts(&content);
                chunks.iter().filter(|c| matches!(c, FileChunk::Conflict(_))).count()
            } else {
                0
            };
            if block_count > 1 {
                println!("  {} ({block_count} blocks)", f.display());
            } else {
                println!("  {}", f.display());
            }
        }

        // Extract side names for helpful output
        if let Some(first) = files.first() {
            let full = ws_path.join(first);
            if let Ok(content) = std::fs::read_to_string(&full) {
                let names = extract_side_names(&content);
                if let Some((left, right)) = names {
                    println!();
                    println!("To resolve:");
                    println!("  maw ws resolve {workspace} --keep {left}    # keep merged version");
                    println!("  maw ws resolve {workspace} --keep {right}    # keep local edits");
                    println!("  maw ws resolve {workspace} --keep both    # keep both (concatenated)");
                }
            }
        }
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

struct BlockInfo {
    id: String,
    left_name: String,
    right_name: String,
    preview: String,
    left_preview: String,
    right_preview: String,
}

fn collect_block_info(chunks: &[FileChunk]) -> Vec<BlockInfo> {
    let mut blocks = Vec::new();
    let mut idx = 0;
    for chunk in chunks {
        if let FileChunk::Conflict(block) = chunk {
            let preview = format!("{} vs {}", block.left_name, block.right_name);
            blocks.push(BlockInfo {
                id: format!("cf-{idx}"),
                left_name: block.left_name.clone(),
                right_name: block.right_name.clone(),
                preview,
                left_preview: block.left_content.clone(),
                right_preview: block.right_content.clone(),
            });
            idx += 1;
        }
    }
    blocks
}

// ---------------------------------------------------------------------------
// Find conflicted files (scan for conflict markers)
// ---------------------------------------------------------------------------

pub(crate) fn find_conflicted_files(ws_path: &Path) -> Result<Vec<PathBuf>> {
    // We're looking for conflict markers that a rebase (or similar op)
    // INTRODUCED to files in this workspace — not pre-existing marker
    // literals in source fixtures. The reliable signal is: the workspace
    // diff against its base contains added lines starting with `<<<<<<<`.
    //
    // A file like `crates/maw-cli/src/workspace/resolve.rs` that has raw
    // string fixtures containing `<<<<<<<` at column 0 is not a false
    // positive under this definition — the diff shows only the lines the
    // workspace actually added, and the fixtures predate the workspace.
    //
    // This evolved through bn-3h90:
    //   v0.58.3: reconcile cached counter against worktree scan
    //   v0.58.4: full-file scan (but 256KB limit → missed large files)
    //            fix was streaming scan (but false-positived on fixtures)
    //   v0.58.5: scan only modified files (but false-positived on files
    //            the workspace legitimately edited that still contained
    //            pre-existing fixtures — e.g. bn-19tb editing resolve.rs)
    //   v0.58.5 final: scan the DIFF for newly-added marker lines only
    if let Some(base) = resolve_workspace_base_ref(ws_path) {
        return Ok(find_files_with_new_conflict_markers(ws_path, &base));
    }

    // Fallback: if we can't resolve the base, walk the worktree and do a
    // full-content scan. Noisier, but safe for the unknown-state case.
    let mut results = Vec::new();
    walk_for_conflicts(ws_path, ws_path, &mut results)?;
    results.sort();
    results.dedup();
    Ok(results)
}

/// Scan the diff between the workspace base and the current state (HEAD +
/// working tree) for added lines that look like conflict markers.
///
/// Returns the set of files where at least one `<<<<<<<` line is NEW in the
/// workspace. Files whose existing content contained marker literals (e.g.
/// test fixtures) are not flagged because those lines aren't in the diff.
fn find_files_with_new_conflict_markers(ws_path: &Path, base: &str) -> Vec<PathBuf> {
    use std::collections::BTreeSet;
    use std::process::Command;

    let mut files: BTreeSet<PathBuf> = BTreeSet::new();

    // Committed changes since base.
    run_diff_and_collect_marker_files(
        ws_path,
        &["diff", "-U0", "--no-color", &format!("{base}..HEAD")],
        &mut files,
    );
    // Uncommitted changes (staged + unstaged).
    run_diff_and_collect_marker_files(
        ws_path,
        &["diff", "-U0", "--no-color", "HEAD"],
        &mut files,
    );
    // Untracked files: scan their full content, since there's no diff base.
    if let Ok(out) = Command::new("git")
        .args([
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(ws_path)
        .output()
        && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.is_empty() {
                continue;
            }
            let full = ws_path.join(line);
            if full.is_file() && file_has_conflict_markers(&full) {
                files.insert(PathBuf::from(line));
            }
        }
    }

    let mut vec: Vec<_> = files.into_iter().collect();
    vec.sort();
    vec
}

/// Run `git diff` with the given args and look for added lines that start
/// with `<<<<<<<`. Files with such lines get added to `out`.
///
/// Parses unified-diff output:
///   `diff --git a/<path> b/<path>` — starts a file block
///   `+++ b/<path>` — confirms the target path for the block
///   `+<<<<<<<` — an added marker line
///
/// Skips the `+++ b/<path>` header (3 pluses + space) so it isn't mistaken
/// for content. Marker pattern starts with `<`, so `+<<<<<<<` doesn't
/// collide with `+++ ` anyway, but we're explicit.
fn run_diff_and_collect_marker_files(
    ws_path: &Path,
    args: &[&str],
    out: &mut std::collections::BTreeSet<PathBuf>,
) {
    use std::process::Command;

    let output = match Command::new("git")
        .arg("-c")
        .arg("core.quotePath=false")
        .args(args)
        .current_dir(ws_path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut current_file: Option<PathBuf> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            // `diff --git a/<path> b/<path>`
            if let Some(sep) = rest.find(" b/") {
                current_file = Some(PathBuf::from(&rest[..sep]));
            } else {
                current_file = None;
            }
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            // File header lines — not content.
            continue;
        }
        if line.starts_with("+<<<<<<<")
            && let Some(ref path) = current_file
        {
            out.insert(path.clone());
        }
    }
}

/// Best-effort resolution of the workspace's base commit.
///
/// Order of preference:
/// 1. `refs/manifold/epoch/ws/<ws_name>` — the per-workspace creation epoch
/// 2. `refs/manifold/epoch/current` — the repo's current epoch
/// 3. The first commit on HEAD's history — fallback
fn resolve_workspace_base_ref(ws_path: &Path) -> Option<String> {
    use std::process::Command;

    let ws_name = ws_path.file_name()?.to_str()?;

    // Walk up to the repo root to run ref queries against the common git dir.
    let repo_root = ws_path.parent()?.parent()?;

    for candidate in [
        format!("refs/manifold/epoch/ws/{ws_name}"),
        "refs/manifold/epoch/current".to_owned(),
    ] {
        if let Ok(out) = Command::new("git")
            .args(["rev-parse", "--verify", &candidate])
            .current_dir(repo_root)
            .output()
            && out.status.success()
        {
            let oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !oid.is_empty() {
                return Some(oid);
            }
        }
    }
    None
}


fn walk_for_conflicts(
    base: &Path,
    dir: &Path,
    results: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden dirs, .git, common non-source dirs
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }

        if path.is_dir() {
            walk_for_conflicts(base, &path, results)?;
        } else if path.is_file() {
            if file_has_conflict_markers(&path) {
                if let Ok(rel) = path.strip_prefix(base) {
                    results.push(rel.to_path_buf());
                }
            }
        }
    }

    Ok(())
}

fn file_has_conflict_markers(path: &Path) -> bool {
    use std::io::{BufRead, BufReader};

    // Stream the file line-by-line so the whole file is scanned regardless
    // of size. Conflict markers are always at the start of a line, so a
    // line-prefix check is sufficient and efficient.
    //
    // Previously this read only the first 256KB, which silently missed
    // markers in large files like `tests/cve-registry/manifest.toml` with
    // thousands of entries (bn-3h90 follow-up).
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Upper-bound extremely large files so we don't hang on giant binaries
    // that can't realistically contain git conflict markers.
    if let Ok(meta) = file.metadata()
        && meta.len() > 256 * 1024 * 1024
    {
        tracing::debug!(
            "file_has_conflict_markers: skipping {} ({} bytes, over 256MB cap)",
            path.display(),
            meta.len()
        );
        return false;
    }

    let mut reader = BufReader::new(file);
    let mut line = Vec::with_capacity(256);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return false, // EOF
            Ok(_) => {
                if line.starts_with(b"<<<<<<<") {
                    return true;
                }
            }
            Err(_) => return false, // I/O error — best-effort false
        }
    }
}

// ---------------------------------------------------------------------------
// Parse file into chunks
// ---------------------------------------------------------------------------

/// Push a line into the appropriate content buffer based on parser state.
fn push_content_line(
    line: &str,
    in_right: bool,
    in_base: bool,
    right_content: &mut String,
    base_content: &mut String,
    left_content: &mut String,
) {
    let target = if in_right {
        right_content
    } else if in_base {
        base_content
    } else {
        left_content
    };
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(line);
}

fn parse_file_conflicts(content: &str) -> Vec<FileChunk> {
    let mut chunks = Vec::new();
    let mut context = String::new();
    let mut lines = content.lines().peekable();

    while let Some(line) = lines.next() {
        if line.starts_with("<<<<<<<") {
            // Flush context
            if !context.is_empty() {
                chunks.push(FileChunk::Context(std::mem::take(&mut context)));
            }

            let left_name = extract_name_from_marker(line);
            let mut left_content = String::new();
            let mut base_content = String::new();
            let mut right_content = String::new();
            let mut right_name = String::new();
            let mut in_base = false;
            let mut in_right = false;
            // Track nested conflict markers so that <<<<<<< / >>>>>>>
            // pairs inside the content (e.g. from previously unresolved
            // conflicts in stashed files) don't cause premature break.
            let mut nested_depth: usize = 0;

            for inner in lines.by_ref() {
                if inner.starts_with("<<<<<<<") {
                    // Nested conflict marker — track depth and treat as content.
                    nested_depth += 1;
                    push_content_line(inner, in_right, in_base, &mut right_content, &mut base_content, &mut left_content);
                } else if inner.starts_with(">>>>>>>") {
                    if nested_depth == 0 {
                        // This is our actual closing marker.
                        right_name = extract_name_from_marker(inner);
                        break;
                    }
                    // Closing a nested marker — treat as content.
                    nested_depth -= 1;
                    push_content_line(inner, in_right, in_base, &mut right_content, &mut base_content, &mut left_content);
                } else if nested_depth == 0 && inner.starts_with("|||||||") {
                    in_base = true;
                } else if nested_depth == 0 && inner.starts_with("=======") {
                    in_base = false;
                    in_right = true;
                } else if in_right {
                    if !right_content.is_empty() {
                        right_content.push('\n');
                    }
                    right_content.push_str(inner);
                } else if in_base {
                    if !base_content.is_empty() {
                        base_content.push('\n');
                    }
                    base_content.push_str(inner);
                } else {
                    if !left_content.is_empty() {
                        left_content.push('\n');
                    }
                    left_content.push_str(inner);
                }
            }

            chunks.push(FileChunk::Conflict(ConflictBlock {
                left_name,
                left_content,
                base_content,
                right_name,
                right_content,
            }));
        } else {
            if !context.is_empty() {
                context.push('\n');
            }
            context.push_str(line);
        }
    }

    // Flush trailing context
    if !context.is_empty() {
        chunks.push(FileChunk::Context(context));
    }

    chunks
}

// ---------------------------------------------------------------------------
// Resolve chunks
// ---------------------------------------------------------------------------

/// Resolve a parsed file's conflict blocks.
///
/// - `file_side`: if Some, resolve ALL blocks in this file to this side
/// - `block_sides`: per-block overrides keyed by "cf-N"
///
/// A block is resolved if it has an entry in `block_sides` or if `file_side`
/// is set. Unresolved blocks are left as conflict markers.
fn resolve_chunks(
    chunks: &[FileChunk],
    file_side: Option<&str>,
    block_sides: &BTreeMap<String, String>,
) -> Result<String> {
    let mut output = String::new();
    let mut block_idx = 0;
    let mut any_resolved = false;

    for (i, chunk) in chunks.iter().enumerate() {
        match chunk {
            FileChunk::Context(text) => {
                output.push_str(text);
                // Add newline between chunks (but not after the last one)
                if i + 1 < chunks.len() {
                    output.push('\n');
                }
            }
            FileChunk::Conflict(block) => {
                let block_id = format!("cf-{block_idx}");
                let side = block_sides
                    .get(&block_id)
                    .map(|s| s.as_str())
                    .or(file_side);

                if let Some(side_name) = side {
                    if side_name == "both" {
                        // Concatenate both sides (left then right)
                        output.push_str(&block.left_content);
                        output.push('\n');
                        output.push_str(&block.right_content);
                        output.push('\n');
                    } else {
                        let chosen = if name_matches(side_name, &block.left_name) {
                            &block.left_content
                        } else if name_matches(side_name, &block.right_name) {
                            &block.right_content
                        } else {
                            bail!(
                                "Side '{}' not found in conflict block {}.\n  \
                                 Available sides: '{}', '{}', 'both'\n  \
                                 To fix: use one of the side names shown above.",
                                side_name,
                                block_id,
                                block.left_name,
                                block.right_name
                            );
                        };
                        output.push_str(chosen);
                        output.push('\n');
                    }
                    any_resolved = true;
                } else {
                    // No resolution for this block — re-emit the markers
                    output.push_str(&format!("<<<<<<< {}\n", block.left_name));
                    output.push_str(&block.left_content);
                    output.push('\n');
                    if !block.base_content.is_empty() {
                        output.push_str("||||||| base\n");
                        output.push_str(&block.base_content);
                        output.push('\n');
                    }
                    output.push_str("=======\n");
                    output.push_str(&block.right_content);
                    output.push('\n');
                    output.push_str(&format!(">>>>>>> {}\n", block.right_name));
                }

                block_idx += 1;
            }
        }
    }

    if !any_resolved {
        bail!("No conflict blocks were resolved. Check --keep side names.");
    }

    // Ensure file ends with newline
    if !output.ends_with('\n') {
        output.push('\n');
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Marker helpers
// ---------------------------------------------------------------------------

/// Extract the workspace name from a conflict marker line.
///
/// `<<<<<<< bn-2sc3 (merged workspace)` → `bn-2sc3`
/// `>>>>>>> default (local edits)` → `default`
/// `<<<<<<< bn-2sc3, bn-4xyz (merged workspaces)` → `bn-2sc3, bn-4xyz`
fn extract_name_from_marker(line: &str) -> String {
    let trimmed = line
        .trim_start_matches('<')
        .trim_start_matches('>')
        .trim_start_matches('|')
        .trim();

    if let Some(paren_pos) = trimmed.find('(') {
        trimmed[..paren_pos].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// Check if a keep_name matches a marker label.
///
/// For multi-workspace labels like "bn-2sc3, bn-4xyz", the keep_name
/// matches if it equals the full label OR any individual workspace in it.
fn name_matches(keep_name: &str, label: &str) -> bool {
    if keep_name == label {
        return true;
    }
    label.split(',').any(|part| part.trim() == keep_name)
}

/// Extract the two side names from the first conflict block in a file.
fn extract_side_names(content: &str) -> Option<(String, String)> {
    let mut left = None;
    let mut right = None;

    for line in content.lines() {
        if line.starts_with("<<<<<<<") && left.is_none() {
            left = Some(extract_name_from_marker(line));
        }
        if line.starts_with(">>>>>>>") && right.is_none() {
            right = Some(extract_name_from_marker(line));
        }
        if left.is_some() && right.is_some() {
            break;
        }
    }

    match (left, right) {
        (Some(l), Some(r)) => Some((l, r)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_name_simple() {
        assert_eq!(
            extract_name_from_marker("<<<<<<< bn-2sc3 (merged workspace)"),
            "bn-2sc3"
        );
        assert_eq!(
            extract_name_from_marker(">>>>>>> default (local edits)"),
            "default"
        );
    }

    #[test]
    fn extract_name_multi_workspace() {
        assert_eq!(
            extract_name_from_marker("<<<<<<< ws-a, ws-b (merged workspaces)"),
            "ws-a, ws-b"
        );
    }

    #[test]
    fn extract_name_no_parens() {
        assert_eq!(extract_name_from_marker("<<<<<<< alice"), "alice");
    }

    #[test]
    fn name_matches_exact() {
        assert!(name_matches("bn-2sc3", "bn-2sc3"));
        assert!(!name_matches("default", "bn-2sc3"));
    }

    #[test]
    fn name_matches_multi() {
        assert!(name_matches("ws-a", "ws-a, ws-b"));
        assert!(name_matches("ws-b", "ws-a, ws-b"));
        assert!(!name_matches("ws-c", "ws-a, ws-b"));
    }

    #[test]
    fn parse_keep_spec_all() {
        let specs = parse_keep_specs(&["bn-2sc3".into()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], KeepSpec::All(n) if n == "bn-2sc3"));
    }

    #[test]
    fn parse_keep_spec_file() {
        let specs = parse_keep_specs(&["src/main.rs=bn-2sc3".into()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], KeepSpec::File(p, n) if p == Path::new("src/main.rs") && n == "bn-2sc3"));
    }

    #[test]
    fn parse_keep_spec_block() {
        let specs = parse_keep_specs(&["cf-0=bn-2sc3".into()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], KeepSpec::Block(id, n) if id == "cf-0" && n == "bn-2sc3"));
    }

    #[test]
    fn parse_keep_spec_mixed() {
        let specs = parse_keep_specs(&[
            "cf-0=ws-a".into(),
            "cf-1=default".into(),
        ]).unwrap();
        assert_eq!(specs.len(), 2);
        assert!(matches!(&specs[0], KeepSpec::Block(id, _) if id == "cf-0"));
        assert!(matches!(&specs[1], KeepSpec::Block(id, _) if id == "cf-1"));
    }

    #[test]
    fn resolve_all_keeps_left() {
        let content = "\
before
<<<<<<< ws-a (merged workspace)
merge line
||||||| base
original line
=======
local line
>>>>>>> default (local edits)
after";
        let chunks = parse_file_conflicts(content);
        let result = resolve_chunks(&chunks, Some("ws-a"), &BTreeMap::new()).unwrap();
        assert!(result.contains("merge line"));
        assert!(!result.contains("local line"));
        assert!(result.contains("before"));
        assert!(result.contains("after"));
    }

    #[test]
    fn resolve_all_keeps_right() {
        let content = "\
before
<<<<<<< ws-a (merged workspace)
merge line
||||||| base
original line
=======
local line
>>>>>>> default (local edits)
after";
        let chunks = parse_file_conflicts(content);
        let result = resolve_chunks(&chunks, Some("default"), &BTreeMap::new()).unwrap();
        assert!(!result.contains("merge line"));
        assert!(result.contains("local line"));
    }

    #[test]
    fn resolve_per_block_mixed() {
        let content = "\
line 1
<<<<<<< ws-a (merged workspace)
merge A
=======
local A
>>>>>>> default (local edits)
line 2
<<<<<<< ws-a (merged workspace)
merge B
=======
local B
>>>>>>> default (local edits)
line 3";
        let chunks = parse_file_conflicts(content);
        let mut block_sides = BTreeMap::new();
        block_sides.insert("cf-0".into(), "ws-a".into());
        block_sides.insert("cf-1".into(), "default".into());
        let result = resolve_chunks(&chunks, None, &block_sides).unwrap();
        assert!(result.contains("merge A"), "block 0 should keep ws-a");
        assert!(!result.contains("local A"));
        assert!(!result.contains("merge B"));
        assert!(result.contains("local B"), "block 1 should keep default");
        assert!(result.contains("line 1"));
        assert!(result.contains("line 2"));
        assert!(result.contains("line 3"));
    }

    #[test]
    fn resolve_partial_leaves_unresolved_markers() {
        let content = "\
<<<<<<< ws-a (merged workspace)
merge A
=======
local A
>>>>>>> default (local edits)
middle
<<<<<<< ws-a (merged workspace)
merge B
=======
local B
>>>>>>> default (local edits)";
        let chunks = parse_file_conflicts(content);
        let mut block_sides = BTreeMap::new();
        block_sides.insert("cf-0".into(), "ws-a".into());
        // cf-1 left unresolved
        let result = resolve_chunks(&chunks, None, &block_sides).unwrap();
        assert!(result.contains("merge A"));
        assert!(!result.contains("local A"));
        // Second block should still have markers
        assert!(result.contains("<<<<<<<"));
        assert!(result.contains("local B"));
    }

    #[test]
    fn resolve_unknown_side_errors() {
        let content = "\
<<<<<<< ws-a (merged workspace)
merge
=======
local
>>>>>>> default (local edits)";
        let chunks = parse_file_conflicts(content);
        let result = resolve_chunks(&chunks, Some("nonexistent"), &BTreeMap::new());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent"));
        assert!(err.contains("ws-a"));
        assert!(err.contains("default"));
    }

    #[test]
    fn resolve_multi_workspace_label() {
        let content = "\
<<<<<<< ws-a, ws-b (merged workspaces)
merged content
||||||| base
base content
=======
local content
>>>>>>> default (local edits)";
        let chunks = parse_file_conflicts(content);
        let result = resolve_chunks(&chunks, Some("ws-a"), &BTreeMap::new()).unwrap();
        assert!(result.contains("merged content"));
    }

    #[test]
    fn extract_side_names_from_content() {
        let content = "\
some code
<<<<<<< bn-2sc3 (merged workspace)
merged
=======
local
>>>>>>> default (local edits)
more code";
        let (left, right) = extract_side_names(content).unwrap();
        assert_eq!(left, "bn-2sc3");
        assert_eq!(right, "default");
    }

    #[test]
    fn resolve_both_concatenates() {
        let content = "\
before
<<<<<<< ws-a (merged workspace)
merge line 1
merge line 2
||||||| base
original
=======
local line 1
local line 2
>>>>>>> default (local edits)
after";
        let chunks = parse_file_conflicts(content);
        let result = resolve_chunks(&chunks, Some("both"), &BTreeMap::new()).unwrap();
        assert!(result.contains("merge line 1"));
        assert!(result.contains("merge line 2"));
        assert!(result.contains("local line 1"));
        assert!(result.contains("local line 2"));
        assert!(result.contains("before"));
        assert!(result.contains("after"));
        assert!(!result.contains("<<<<<<<"));
        assert!(!result.contains(">>>>>>>"));
        // Left comes before right
        let merge_pos = result.find("merge line 1").unwrap();
        let local_pos = result.find("local line 1").unwrap();
        assert!(merge_pos < local_pos, "left side should come before right side");
    }

    /// Regression test for bn-2wnt (partial-resolution reporting bug).
    ///
    /// When `--keep cf-0=alice` is used on a file with 2 conflict blocks,
    /// resolve_chunks returns Ok with block 1 as alice content and block 2
    /// re-emitted as markers. The run() function would write the half-
    /// resolved file and count it as "resolved" (+1 to `resolved_count`),
    /// then re-scan and see "1 file still has conflict markers", producing
    /// confusing output.
    ///
    /// The fix (v0.58.6): run() inspects the resolved string for remaining
    /// `<<<<<<<` lines and reports partial resolutions separately from fully
    /// resolved ones.
    #[test]
    fn resolve_chunks_partial_resolution_preserves_unresolved_blocks() {
        let content = "\
<<<<<<< alice
aaa
=======
bbb
>>>>>>> bob
middle
<<<<<<< alice
ccc
=======
ddd
>>>>>>> bob";
        let chunks = parse_file_conflicts(content);
        let mut block_sides = BTreeMap::new();
        block_sides.insert("cf-0".into(), "alice".into());
        // cf-1 is NOT specified — should remain as markers
        let result = resolve_chunks(&chunks, None, &block_sides).unwrap();
        assert!(result.contains("aaa"), "block 0 alice content missing");
        assert!(!result.contains("bbb"), "block 0 bob content should be gone");
        // block 1 should still have markers
        assert!(result.contains("<<<<<<< alice"), "block 1 marker missing");
        assert!(result.contains("ccc"), "block 1 alice content missing");
        assert!(result.contains("ddd"), "block 1 bob content missing");
        assert!(result.contains(">>>>>>> bob"), "block 1 closing marker missing");

        // Count remaining `<<<<<<<` markers in the resolved output — the run()
        // function uses this exact check to detect partial resolution.
        let remaining = result
            .lines()
            .filter(|l| l.starts_with("<<<<<<<"))
            .count();
        assert_eq!(
            remaining, 1,
            "partial resolution should leave exactly 1 marker"
        );
    }

    /// Regression test for bn-2wnt (the multi-block --keep both path).
    ///
    /// Sanity check that resolve_chunks with Some("both") on a 2-block file
    /// fully resolves ALL blocks in a single pass. If this test fails, the
    /// original bn-2wnt agent report is a real parser/resolver bug.
    #[test]
    fn resolve_both_handles_multiple_blocks_in_one_pass() {
        let content = "\
before
<<<<<<< alice
aaa
=======
bbb
>>>>>>> bob
middle
<<<<<<< alice
ccc
=======
ddd
>>>>>>> bob
after";
        let chunks = parse_file_conflicts(content);
        let conflict_count = chunks
            .iter()
            .filter(|c| matches!(c, FileChunk::Conflict(_)))
            .count();
        assert_eq!(conflict_count, 2, "parser should see 2 conflict blocks, got {conflict_count}");

        let result = resolve_chunks(&chunks, Some("both"), &BTreeMap::new()).unwrap();
        assert!(
            !result.contains("<<<<<<<"),
            "all markers should be gone in one pass, got:\n{result}"
        );
        assert!(result.contains("aaa") && result.contains("bbb"), "block 1 missing content:\n{result}");
        assert!(result.contains("ccc") && result.contains("ddd"), "block 2 missing content:\n{result}");
        assert!(result.contains("before") && result.contains("middle") && result.contains("after"));
    }

    #[test]
    fn resolve_both_per_block() {
        let content = "\
<<<<<<< ws-a (merged)
merge A
=======
local A
>>>>>>> default (local)
mid
<<<<<<< ws-a (merged)
merge B
=======
local B
>>>>>>> default (local)";
        let chunks = parse_file_conflicts(content);
        let mut block_sides = BTreeMap::new();
        block_sides.insert("cf-0".into(), "both".into());
        block_sides.insert("cf-1".into(), "ws-a".into());
        let result = resolve_chunks(&chunks, None, &block_sides).unwrap();
        // Block 0: both
        assert!(result.contains("merge A"));
        assert!(result.contains("local A"));
        // Block 1: ws-a only
        assert!(result.contains("merge B"));
        assert!(!result.contains("local B"));
    }

    /// Regression test for bn-27ve: nested conflict markers in content
    /// caused the parser to break early at the inner >>>>>>> line, leaving
    /// the actual closing >>>>>>> marker as trailing context.
    #[test]
    fn resolve_nested_conflict_markers_no_trailing_marker() {
        // Simulate a file where the local (stash) content itself contains
        // old unresolved conflict markers — e.g. from a previous merge.
        let content = "\
<<<<<<< bn-1fj7 (merged workspace)
clean merge result
||||||| base
original base
=======
some local code
<<<<<<< old-ws
old merged
||||||| old-base
old original
=======
old local
>>>>>>> default (old conflict)
more local code
>>>>>>> default (local edits)";
        let chunks = parse_file_conflicts(content);

        // Should be exactly ONE conflict block, not split into pieces.
        let conflict_count = chunks.iter().filter(|c| matches!(c, FileChunk::Conflict(_))).count();
        assert_eq!(conflict_count, 1, "should parse as a single conflict block");

        // No context chunks should contain >>>>>>>
        for chunk in &chunks {
            if let FileChunk::Context(text) = chunk {
                assert!(!text.contains(">>>>>>>"),
                    "context chunk should not contain >>>>>>> marker, got: {text}");
            }
        }

        // Resolving should not leave any >>>>>>> markers in the output.
        let result = resolve_chunks(&chunks, Some("default"), &BTreeMap::new()).unwrap();
        assert!(!result.contains(">>>>>>> default (local edits)"),
            "resolved output should not contain trailing >>>>>>> marker");
        assert!(result.contains("some local code"),
            "resolved output should contain the local content");
        assert!(result.contains("more local code"),
            "resolved output should contain content after nested block");
        // The nested conflict markers should be preserved as content.
        assert!(result.contains("<<<<<<< old-ws"),
            "nested <<<<<<< should be preserved in content");
        assert!(result.contains(">>>>>>> default (old conflict)"),
            "nested >>>>>>> should be preserved in content");
    }

    /// Test that nested markers in the left (merge) side are handled correctly.
    #[test]
    fn resolve_nested_markers_in_left_side() {
        let content = "\
<<<<<<< ws-a (merged workspace)
code with nested
<<<<<<< nested
nested left
=======
nested right
>>>>>>> nested-end
end of merge
||||||| base
original
=======
local content
>>>>>>> default (local edits)";
        let chunks = parse_file_conflicts(content);
        let conflict_count = chunks.iter().filter(|c| matches!(c, FileChunk::Conflict(_))).count();
        assert_eq!(conflict_count, 1, "should parse as a single conflict block");

        let result = resolve_chunks(&chunks, Some("ws-a"), &BTreeMap::new()).unwrap();
        assert!(result.contains("code with nested"));
        assert!(result.contains("end of merge"));
        assert!(!result.contains("local content"));
        assert!(!result.contains(">>>>>>> default (local edits)"));
    }

    /// Test that ======= lines inside nested markers are treated as content.
    #[test]
    fn nested_markers_equals_treated_as_content() {
        let content = "\
<<<<<<< ws-a (merged)
before nested
<<<<<<< inner
inner left
=======
inner right
>>>>>>> inner-end
after nested
=======
local side
>>>>>>> default (local)";
        let chunks = parse_file_conflicts(content);
        let conflict_count = chunks.iter().filter(|c| matches!(c, FileChunk::Conflict(_))).count();
        assert_eq!(conflict_count, 1);

        // Verify the left content includes everything up to the real =======
        if let FileChunk::Conflict(block) = &chunks[0] {
            assert!(block.left_content.contains("before nested"));
            assert!(block.left_content.contains("<<<<<<< inner"));
            assert!(block.left_content.contains("inner left"));
            assert!(block.left_content.contains("======="));
            assert!(block.left_content.contains("inner right"));
            assert!(block.left_content.contains(">>>>>>> inner-end"));
            assert!(block.left_content.contains("after nested"));
            assert_eq!(block.right_content, "local side");
        } else {
            panic!("expected conflict chunk");
        }
    }

    #[test]
    fn collect_block_info_counts() {
        let content = "\
line
<<<<<<< ws-a (merged)
a1
=======
b1
>>>>>>> default (local)
mid
<<<<<<< ws-a (merged)
a2
=======
b2
>>>>>>> default (local)
end";
        let chunks = parse_file_conflicts(content);
        let blocks = collect_block_info(&chunks);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, "cf-0");
        assert_eq!(blocks[1].id, "cf-1");
        assert_eq!(blocks[0].left_name, "ws-a");
        assert_eq!(blocks[0].right_name, "default");
    }
}

// ---------------------------------------------------------------------------
// Property-based fuzz tests (bn-19tb)
// ---------------------------------------------------------------------------
//
// These exercise `parse_file_conflicts` — the diff3 conflict marker parser —
// with a wide variety of inputs including pathological ones (unterminated
// markers, nested markers, random bytes, etc.).
//
// Invariants asserted:
// - `parse_file_conflicts` never panics on any `&str` input.
// - Well-formed conflict blocks preserve left/right/base content exactly.
// - Context-only input (no markers) produces exactly one `Context` chunk
//   equal to the original content.
// - Block IDs (`cf-0`, `cf-1`, ...) are stable — same input produces the
//   same sequence of block indices in `resolve_chunks`.

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    fn pt_config() -> ProptestConfig {
        ProptestConfig {
            cases: 256,
            max_shrink_iters: 128,
            .. ProptestConfig::default()
        }
    }

    proptest! {
        #![proptest_config(pt_config())]

        /// `parse_file_conflicts` must never panic, regardless of input.
        #[test]
        fn parse_never_panics(content in any::<String>()) {
            let _ = parse_file_conflicts(&content);
        }

        /// Marker-free input produces zero or one `Context` chunk and no
        /// `Conflict` chunks. Note: `parse_file_conflicts` collapses leading
        /// blank lines into its context buffer, which is why we don't
        /// require exact byte equality here.
        #[test]
        fn non_marker_content_has_no_conflicts(
            // Printable ASCII minus `<`, `|`, `=`, `>` to avoid markers.
            text in "[a-zA-Z0-9 ._/!\\?\n]{0,200}"
        ) {
            prop_assume!(!text.contains("<<<<<<<"));
            prop_assume!(!text.contains("|||||||"));
            prop_assume!(!text.contains("======="));
            prop_assume!(!text.contains(">>>>>>>"));

            let chunks = parse_file_conflicts(&text);
            for c in &chunks {
                prop_assert!(matches!(c, FileChunk::Context(_)));
            }
            // Non-empty content (with any non-newline char) produces at
            // least one chunk.
            if text.chars().any(|c| c != '\n') {
                prop_assert!(!chunks.is_empty());
            }
        }

        /// Well-formed diff3 blocks preserve left/right/base content exactly.
        #[test]
        fn well_formed_diff3_preserves_sides(
            left_name in "[a-z][a-z0-9-]{0,8}",
            right_name in "[a-z][a-z0-9-]{0,8}",
            left in "[a-zA-Z0-9 _.]{0,40}",
            base in "[a-zA-Z0-9 _.]{0,40}",
            right in "[a-zA-Z0-9 _.]{0,40}",
        ) {
            let content = format!(
                "<<<<<<< {left_name}\n{left}\n||||||| merged common ancestors\n{base}\n=======\n{right}\n>>>>>>> {right_name}\n"
            );
            let chunks = parse_file_conflicts(&content);
            let conflict = chunks.iter().find_map(|c| match c {
                FileChunk::Conflict(b) => Some(b),
                _ => None,
            }).expect("should parse one conflict block");
            prop_assert_eq!(&conflict.left_name, &left_name);
            prop_assert_eq!(&conflict.right_name, &right_name);
            prop_assert_eq!(&conflict.left_content, &left);
            prop_assert_eq!(&conflict.base_content, &base);
            prop_assert_eq!(&conflict.right_content, &right);
        }

        /// Block indices are deterministic: the same input always produces
        /// the same number of `Conflict` chunks in the same positions.
        #[test]
        fn block_ids_stable(content in any::<String>()) {
            let a = parse_file_conflicts(&content);
            let b = parse_file_conflicts(&content);
            prop_assert_eq!(a.len(), b.len());
            for (ca, cb) in a.iter().zip(b.iter()) {
                match (ca, cb) {
                    (FileChunk::Context(x), FileChunk::Context(y)) => prop_assert_eq!(x, y),
                    (FileChunk::Conflict(x), FileChunk::Conflict(y)) => {
                        prop_assert_eq!(&x.left_name, &y.left_name);
                        prop_assert_eq!(&x.right_name, &y.right_name);
                        prop_assert_eq!(&x.left_content, &y.left_content);
                        prop_assert_eq!(&x.base_content, &y.base_content);
                        prop_assert_eq!(&x.right_content, &y.right_content);
                    }
                    _ => prop_assert!(false, "chunk kinds differ between runs"),
                }
            }
        }

        /// Unterminated `<<<<<<<` blocks (EOF before `>>>>>>>`) don't panic
        /// and don't silently drop the starting marker's data — at least one
        /// `Conflict` chunk is produced.
        #[test]
        fn unterminated_markers_dont_panic(
            name in "[a-z][a-z0-9-]{0,8}",
            body in "[a-zA-Z0-9 _.\n]{0,80}",
        ) {
            let content = format!("prefix line\n<<<<<<< {name}\n{body}");
            let chunks = parse_file_conflicts(&content);
            // Should contain at least one Conflict chunk (the unterminated one).
            let has_conflict = chunks.iter().any(|c| matches!(c, FileChunk::Conflict(_)));
            prop_assert!(has_conflict);
        }

        /// Nested `<<<<<<<` / `>>>>>>>` markers inside a conflict block are
        /// kept as content, not flattened — the outer block's boundaries
        /// remain correct.
        #[test]
        fn nested_markers_kept_as_content(
            outer_left in "[a-z][a-z0-9-]{0,6}",
            outer_right in "[a-z][a-z0-9-]{0,6}",
            inner_left in "[a-z][a-z0-9-]{0,6}",
            inner_right in "[a-z][a-z0-9-]{0,6}",
        ) {
            let content = format!(
                "<<<<<<< {outer_left}\nouter-left\n<<<<<<< {inner_left}\ninner-left\n=======\ninner-right\n>>>>>>> {inner_right}\n=======\nouter-right\n>>>>>>> {outer_right}\n"
            );
            let chunks = parse_file_conflicts(&content);
            // Expect exactly one top-level conflict chunk.
            let conflicts: Vec<_> = chunks.iter().filter_map(|c| match c {
                FileChunk::Conflict(b) => Some(b),
                _ => None,
            }).collect();
            prop_assert_eq!(conflicts.len(), 1);
            let b = conflicts[0];
            prop_assert_eq!(&b.left_name, &outer_left);
            prop_assert_eq!(&b.right_name, &outer_right);
            // The inner markers should appear inside left_content or right_content.
            let combined = format!("{}\n{}", b.left_content, b.right_content);
            let open_marker = format!("<<<<<<< {inner_left}");
            let close_marker = format!(">>>>>>> {inner_right}");
            prop_assert!(combined.contains(&open_marker));
            prop_assert!(combined.contains(&close_marker));
        }
    }
}
