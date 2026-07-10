//! `maw release prepare` and `maw release preflight`.
//!
//! Automates the mechanical half of a release so cutting one is a single
//! command plus human review, and so a broken publish chain or version skew is
//! caught in PR CI (via `just release-preflight` + the publish dry-run
//! workflow) rather than on tag day.
//!
//! `prepare vX.Y.Z`:
//!   1. Lockstep version bump — the `[workspace.package]` version plus every
//!      internal path-dep `version = "…"` string across every `Cargo.toml`
//!      (outside `target/` and `.maw/`). `version.workspace = true` handles the
//!      crate versions themselves; these path-dep strings do not and have to be
//!      moved by hand historically (a ~19-string global sed).
//!   2. Regenerate `Cargo.lock` (`cargo update --workspace` — the cheap path;
//!      it rewrites only the workspace members, no external dep churn).
//!   3. Scaffold a `## vX.Y.Z (YYYY-MM-DD)` CHANGELOG.md header if absent
//!      (content stays human-written — no notes are generated from commits).
//!   4. Check README.md for stale version references (warn only).
//!
//! Everything is left UNCOMMITTED for review. `prepare` is idempotent: a second
//! run with the same version is a no-op. It refuses on a dirty tree, except for
//! its own edit surface (Cargo.toml / Cargo.lock / CHANGELOG.md / README.md) so
//! a re-run after a partial prepare still works.
//!
//! `preflight [vX.Y.Z]`: read-only release-readiness gate. Verifies version
//! consistency (workspace + every internal path-dep + Cargo.lock), that the
//! CHANGELOG has the target section, and that the tree is clean. Never runs the
//! test suite — it prints the reminder to run `just check` instead.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

/// Files `prepare` is allowed to have already modified when re-run on a
/// not-yet-clean tree. Matched by file name.
const PREPARE_EDIT_FILES: &[&str] = &["Cargo.toml", "Cargo.lock", "CHANGELOG.md", "README.md"];

#[derive(Args)]
#[command(disable_version_flag = true)]
pub struct PrepareArgs {
    /// Version to prepare, e.g. `v1.0.0` (the leading `v` is optional).
    #[arg(value_name = "VERSION")]
    pub version: String,
}

#[derive(Args)]
#[command(disable_version_flag = true)]
pub struct PreflightArgs {
    /// Target release version, e.g. `v1.0.0`. Omit to check the current tree's
    /// own internal consistency (the mode CI runs on every PR).
    #[arg(value_name = "VERSION")]
    pub version: Option<String>,

    /// Skip the working-tree-clean check (useful mid-prepare, before committing
    /// the bump).
    #[arg(long)]
    pub allow_dirty: bool,
}

/// Prepare a release: lockstep version bump, Cargo.lock regen, CHANGELOG
/// scaffold. Leaves everything uncommitted.
///
/// # Errors
///
/// Returns an error if the version is malformed, the working tree is dirty
/// outside prepare's own edit surface, or a filesystem / `cargo` step fails.
pub fn run_prepare(args: &PrepareArgs) -> Result<()> {
    let version = normalize_version(&args.version)?;
    let root = find_workspace_root()?;

    // Refuse on a dirty tree, tolerating only prepare's own edit surface so a
    // re-run after a partial prepare still proceeds.
    let stray = dirty_paths_outside_edit_surface(&root)?;
    if !stray.is_empty() {
        let mut msg = String::from(
            "working tree has changes outside the release edit surface; commit or stash them first:\n",
        );
        for p in &stray {
            let _ = writeln!(msg, "  {p}");
        }
        msg.push_str(
            "  (prepare only expects to touch Cargo.toml, Cargo.lock, CHANGELOG.md, README.md)",
        );
        bail!(msg);
    }

    let tomls = collect_cargo_tomls(&root);
    let mut bumped = 0usize;

    // 1. Lockstep version bump across every Cargo.toml.
    for toml in &tomls {
        let is_root = toml == &root.join("Cargo.toml");
        bumped += bump_cargo_toml(toml, &version, is_root)?;
    }

    // 2. Regenerate Cargo.lock (workspace members only — cheap, no external
    //    dep churn). Only when the lock is actually stale.
    let lock_changed = regenerate_lock(&root, &version)?;

    // 3. Scaffold the CHANGELOG section header if absent.
    let changelog_added = scaffold_changelog(&root, &version)?;

    // 4. README version-reference check (warn only).
    let readme_warnings = check_readme_versions(&root, &version)?;

    let no_op = bumped == 0 && !lock_changed && !changelog_added;

    println!();
    if no_op {
        println!(
            "already prepared for v{version} — versions consistent, CHANGELOG section present, Cargo.lock current. No changes."
        );
    } else {
        let mut summary = format!("prepared v{version}:");
        if bumped > 0 {
            let _ = write!(summary, " bumped {bumped} version string(s);");
        }
        if lock_changed {
            summary.push_str(" regenerated Cargo.lock;");
        }
        if changelog_added {
            summary.push_str(" scaffolded CHANGELOG section;");
        }
        println!("{}", summary.trim_end_matches(';'));
    }

    for w in &readme_warnings {
        println!("warning: {w}");
    }

    println!();
    println!("next:");
    println!("  1. edit CHANGELOG.md — fill in the v{version} section (content is human-written)");
    println!("  2. review:    git -C {} diff", root.display());
    println!("  3. verify:    just check   (prepare does NOT run the suite)");
    println!("  4. preflight: maw release preflight v{version} --allow-dirty");
    println!(
        "  5. commit:    git -C {} commit -am \"chore(release): bump to {version} + CHANGELOG\"",
        root.display()
    );
    println!("  6. tag+push:  maw release v{version}");

    Ok(())
}

/// Release-readiness preflight: version consistency, CHANGELOG section, clean
/// tree. Read-only.
///
/// # Errors
///
/// Returns an error listing every problem found (version skew naming the
/// offending file, a missing CHANGELOG section, or a dirty tree).
pub fn run_preflight(args: &PreflightArgs) -> Result<()> {
    let root = find_workspace_root()?;
    let workspace_version = read_workspace_version(&root)?;

    // If a target was given, the workspace must already be at it.
    let target = match &args.version {
        Some(v) => Some(normalize_version(v)?),
        None => None,
    };

    let mut problems: Vec<String> = Vec::new();

    if let Some(target) = &target
        && target != &workspace_version
    {
        problems.push(format!(
            "workspace version is {workspace_version} but preflight target is v{target} \
             (run `maw release prepare v{target}`)"
        ));
    }

    // Version-consistency: every internal path-dep string == workspace version.
    for skew in scan_version_skew(&root, &workspace_version)? {
        problems.push(skew);
    }

    // Cargo.lock: every workspace member is pinned at the workspace version.
    for skew in scan_lock_skew(&root, &workspace_version)? {
        problems.push(skew);
    }

    // CHANGELOG has a section for the target (or the current workspace version).
    let want_section = target.as_ref().unwrap_or(&workspace_version);
    if !changelog_has_section(&root, want_section)? {
        problems.push(format!(
            "CHANGELOG.md has no `## v{want_section}` section \
             (run `maw release prepare v{want_section}` to scaffold one)"
        ));
    }

    // Working tree clean (unless explicitly allowed).
    if !args.allow_dirty {
        let dirty = dirty_paths(&root)?;
        if !dirty.is_empty() {
            let mut msg = String::from("working tree is not clean:");
            for p in dirty.iter().take(10) {
                let _ = write!(msg, "\n    {p}");
            }
            if dirty.len() > 10 {
                let _ = write!(msg, "\n    …and {} more", dirty.len() - 10);
            }
            problems.push(msg);
        }
    }

    if problems.is_empty() {
        let shown = target.as_ref().unwrap_or(&workspace_version);
        println!("release preflight OK for v{shown}");
        println!(
            "  versions consistent (workspace + internal path-deps + Cargo.lock), CHANGELOG section present{}.",
            if args.allow_dirty { "" } else { ", tree clean" }
        );
        println!("  reminder: ensure `just check` is green before tagging.");
        return Ok(());
    }

    let mut msg = format!("release preflight FAILED ({} problem(s)):", problems.len());
    for p in &problems {
        let _ = write!(msg, "\n  - {p}");
    }
    bail!(msg);
}

// ---------------------------------------------------------------------------
// Version parsing / workspace discovery
// ---------------------------------------------------------------------------

/// Strip a leading `v` and sanity-check the shape (`MAJOR.MINOR.PATCH[-pre]`).
fn normalize_version(raw: &str) -> Result<String> {
    let v = raw.strip_prefix('v').unwrap_or(raw).trim();
    let core = v.split('-').next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    let well_formed = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !well_formed {
        bail!(
            "invalid version {raw:?}: expected MAJOR.MINOR.PATCH with an optional -prerelease \
             (e.g. v1.0.0 or v1.0.0-pre.12)"
        );
    }
    Ok(v.to_string())
}

/// Ascend from the current directory to the nearest `Cargo.toml` that declares
/// `[workspace.package]` — the maw workspace root.
fn find_workspace_root() -> Result<PathBuf> {
    let start = std::env::current_dir().context("cannot determine current directory")?;
    let mut dir = start.as_path();
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).unwrap_or_default();
            if text.contains("[workspace.package]") {
                return Ok(dir.to_path_buf());
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!(
                "no workspace root found: walked up from {} without finding a Cargo.toml with a \
                 [workspace.package] section",
                start.display()
            ),
        }
    }
}

/// Read the `version` value from the root `Cargo.toml`'s `[workspace.package]`.
fn read_workspace_version(root: &Path) -> Result<String> {
    let path = root.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut in_ws_pkg = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_ws_pkg = trimmed == "[workspace.package]";
            continue;
        }
        if in_ws_pkg && let Some(val) = parse_quoted_assignment(trimmed, "version") {
            return Ok(val);
        }
    }
    bail!(
        "no `version = \"…\"` under [workspace.package] in {}",
        path.display()
    )
}

/// Walk the tree collecting `Cargo.toml` files, skipping `target/`, `.maw/`,
/// `.git/`, and any hidden directory.
fn collect_cargo_tomls(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == "target" || name == ".maw" || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if entry.file_name() == "Cargo.toml" {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Cargo.toml editing
// ---------------------------------------------------------------------------

/// Bump one `Cargo.toml` in place. When `is_root`, also set the
/// `[workspace.package]` version. Returns the number of version strings
/// changed.
fn bump_cargo_toml(path: &Path, version: &str, is_root: bool) -> Result<usize> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut changed = 0usize;
    let mut in_ws_pkg = false;
    let mut out = String::with_capacity(text.len());

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_ws_pkg = trimmed == "[workspace.package]";
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Root workspace version line.
        if is_root
            && in_ws_pkg
            && parse_quoted_assignment(trimmed, "version").is_some()
            && let Some((rewritten, did)) = replace_version_value(line, version)
        {
            changed += usize::from(did);
            out.push_str(&rewritten);
            out.push('\n');
            continue;
        }

        // Internal path-dep line: has both `path = "…"` and `version = "…"`.
        if line.contains("path = \"")
            && line.contains("version = \"")
            && let Some((rewritten, did)) = replace_version_value(line, version)
        {
            changed += usize::from(did);
            out.push_str(&rewritten);
            out.push('\n');
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    // Preserve a trailing-newline-free original faithfully.
    let final_text = if text.ends_with('\n') {
        out
    } else {
        out.trim_end_matches('\n').to_string()
    };

    if final_text != text {
        std::fs::write(path, &final_text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(changed)
}

/// Replace the first `version = "…"` value on a line. Returns the rewritten
/// line and whether the value actually changed, or `None` if there is no
/// `version = "…"` on the line.
fn replace_version_value(line: &str, new_version: &str) -> Option<(String, bool)> {
    let key = "version = \"";
    let start = line.find(key)?;
    let value_start = start + key.len();
    let rest = &line[value_start..];
    let end = rest.find('"')?;
    let old = &rest[..end];
    let changed = old != new_version;
    let rewritten = format!(
        "{}{new_version}{}",
        &line[..value_start],
        &line[value_start + end..]
    );
    Some((rewritten, changed))
}

/// Parse `key = "value"` from a trimmed line, returning the value.
fn parse_quoted_assignment(trimmed: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = \"");
    let rest = trimmed.strip_prefix(&prefix)?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ---------------------------------------------------------------------------
// Cargo.lock
// ---------------------------------------------------------------------------

/// Regenerate `Cargo.lock` for the workspace members. Returns whether the lock
/// changed.
fn regenerate_lock(root: &Path, _version: &str) -> Result<bool> {
    let lock_path = root.join("Cargo.lock");
    let before = std::fs::read_to_string(&lock_path).unwrap_or_default();

    let output = Command::new("cargo")
        .args(["update", "--workspace", "--offline"])
        .current_dir(root)
        .output();
    // `--offline` avoids a network round-trip; if the index isn't cached it can
    // fail, so fall back to an online update.
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => Command::new("cargo")
            .args(["update", "--workspace"])
            .current_dir(root)
            .output()
            .context("running `cargo update --workspace` to regenerate Cargo.lock")?,
    };
    if !output.status.success() {
        bail!(
            "`cargo update --workspace` failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let after = std::fs::read_to_string(&lock_path).unwrap_or_default();
    Ok(before != after)
}

/// Every workspace member package in `Cargo.lock` must be pinned at the
/// workspace version. Returns a skew message per offender.
fn scan_lock_skew(root: &Path, version: &str) -> Result<Vec<String>> {
    let members = workspace_member_names(root)?;
    let lock_path = root.join("Cargo.lock");
    let Ok(text) = std::fs::read_to_string(&lock_path) else {
        return Ok(vec![format!(
            "Cargo.lock missing at {}",
            lock_path.display()
        )]);
    };

    let mut problems = Vec::new();
    let mut cur_name: Option<String> = None;
    for line in text.lines() {
        if line == "[[package]]" {
            cur_name = None;
        } else if let Some(name) = parse_quoted_assignment(line.trim(), "name") {
            cur_name = Some(name);
        } else if let Some(ver) = parse_quoted_assignment(line.trim(), "version")
            && let Some(name) = &cur_name
            && members.contains(name)
            && ver != version
        {
            problems.push(format!(
                "Cargo.lock: {name} is {ver} but workspace version is {version} \
                 (run `maw release prepare v{version}`)"
            ));
        }
    }
    Ok(problems)
}

/// Collect the `[package] name` of every workspace member (each member
/// `Cargo.toml`'s package name).
fn workspace_member_names(root: &Path) -> Result<std::collections::HashSet<String>> {
    let mut names = std::collections::HashSet::new();
    for toml in collect_cargo_tomls(root) {
        let text = std::fs::read_to_string(&toml).unwrap_or_default();
        let mut in_pkg = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_pkg = trimmed == "[package]";
                continue;
            }
            if in_pkg && let Some(name) = parse_quoted_assignment(trimmed, "name") {
                names.insert(name);
                break;
            }
        }
    }
    Ok(names)
}

// ---------------------------------------------------------------------------
// Version-skew scan (the preflight core)
// ---------------------------------------------------------------------------

/// Scan every `Cargo.toml` for internal path-dep `version` strings (and the
/// root `[workspace.package]` version) that disagree with `version`. Returns a
/// message naming `file:line` for each offender.
fn scan_version_skew(root: &Path, version: &str) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let root_toml = root.join("Cargo.toml");
    for toml in collect_cargo_tomls(root) {
        let is_root = toml == root_toml;
        let text = std::fs::read_to_string(&toml)
            .with_context(|| format!("reading {}", toml.display()))?;
        let rel = toml
            .strip_prefix(root)
            .unwrap_or(&toml)
            .display()
            .to_string();
        let mut in_ws_pkg = false;
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_ws_pkg = trimmed == "[workspace.package]";
                continue;
            }
            let is_ws_version =
                is_root && in_ws_pkg && parse_quoted_assignment(trimmed, "version").is_some();
            let is_path_dep = line.contains("path = \"") && line.contains("version = \"");
            if (is_ws_version || is_path_dep)
                && let Some(val) = extract_version_value(line)
                && val != version
            {
                let lineno = idx + 1;
                let what = if is_ws_version {
                    "workspace version"
                } else {
                    "internal path-dep"
                };
                problems.push(format!(
                    "version skew: {rel}:{lineno} {what} = \"{val}\" but workspace version is \"{version}\""
                ));
            }
        }
    }
    Ok(problems)
}

/// Extract the first `version = "…"` value on a line.
fn extract_version_value(line: &str) -> Option<String> {
    let key = "version = \"";
    let start = line.find(key)?;
    let rest = &line[start + key.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ---------------------------------------------------------------------------
// CHANGELOG
// ---------------------------------------------------------------------------

fn changelog_path(root: &Path) -> PathBuf {
    root.join("CHANGELOG.md")
}

/// Does CHANGELOG.md already have a `## v{version}` section header?
fn changelog_has_section(root: &Path, version: &str) -> Result<bool> {
    let path = changelog_path(root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    Ok(has_section_header(&text, version))
}

fn has_section_header(text: &str, version: &str) -> bool {
    let want = format!("v{version}");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ")
            && rest.split_whitespace().next() == Some(want.as_str())
        {
            return true;
        }
    }
    false
}

/// Insert a `## v{version} (YYYY-MM-DD)` header above the first existing
/// `## v…` section if absent. Returns whether it added one.
fn scaffold_changelog(root: &Path, version: &str) -> Result<bool> {
    let path = changelog_path(root);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if has_section_header(&text, version) {
        return Ok(false);
    }

    let header = format!("## v{version} ({})", today_iso());
    let block = format!("{header}\n\n<!-- release notes: fill in before tagging -->\n\n");

    // Insert before the first existing `## ` section; else append.
    let mut out = String::with_capacity(text.len() + block.len());
    let mut inserted = false;
    for line in text.lines() {
        if !inserted && line.starts_with("## ") {
            out.push_str(&block);
            inserted = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !inserted {
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&block);
    }

    std::fs::write(&path, &out).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Today's date as `YYYY-MM-DD` (UTC), computed from the system clock without a
/// date-library dependency.
fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days-since-Unix-epoch to a civil (year, month, day) using Howard
/// Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

// ---------------------------------------------------------------------------
// README
// ---------------------------------------------------------------------------

/// Warn about README.md lines that reference a maw version other than the new
/// one. Check-only — README prose is never auto-edited.
fn check_readme_versions(root: &Path, version: &str) -> Result<Vec<String>> {
    let path = root.join("README.md");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    // Reference shape: the tail of the version's semver core, e.g. "1.0.0-pre".
    // Only flag lines that look like an install/version reference to avoid
    // false positives on unrelated numbers.
    let mut warnings = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let l = line.to_ascii_lowercase();
        let mentions_version = l.contains("version") || l.contains("maw ") || l.contains("v1.");
        if mentions_version && line.contains("-pre.") && !line.contains(version) {
            warnings.push(format!(
                "README.md:{}: possible stale version reference — verify against v{version}",
                idx + 1
            ));
        }
    }
    Ok(warnings)
}

// ---------------------------------------------------------------------------
// git working-tree state
// ---------------------------------------------------------------------------

/// Porcelain paths of every changed file (staged or unstaged, incl. untracked).
fn dirty_paths(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .context("running `git status --porcelain`")?;
    if !output.status.success() {
        bail!(
            "`git status` failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter_map(|l| l.get(3..).map(str::to_string))
        .collect())
}

/// Dirty paths whose file name is NOT part of prepare's own edit surface.
fn dirty_paths_outside_edit_surface(root: &Path) -> Result<Vec<String>> {
    Ok(dirty_paths(root)?
        .into_iter()
        .filter(|p| {
            let name = Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            !PREPARE_EDIT_FILES.contains(&name.as_str())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_v_and_validates() {
        assert_eq!(normalize_version("v1.0.0").unwrap(), "1.0.0");
        assert_eq!(normalize_version("1.2.3-pre.4").unwrap(), "1.2.3-pre.4");
        assert!(normalize_version("1.0").is_err());
        assert!(normalize_version("vabc").is_err());
    }

    #[test]
    fn replace_version_value_rewrites_and_reports_change() {
        let line =
            r#"maw-lfs = { path = "../maw-lfs", version = "1.0.0-pre.10", optional = true }"#;
        let (out, changed) = replace_version_value(line, "1.0.0-pre.11").unwrap();
        assert!(changed);
        assert_eq!(
            out,
            r#"maw-lfs = { path = "../maw-lfs", version = "1.0.0-pre.11", optional = true }"#
        );
        // Idempotent second application reports no change.
        let (out2, changed2) = replace_version_value(&out, "1.0.0-pre.11").unwrap();
        assert!(!changed2);
        assert_eq!(out2, out);
    }

    #[test]
    fn replace_version_value_none_without_version_key() {
        let line = r#"maw = { path = "../..", package = "maw-workspaces" }"#;
        assert!(replace_version_value(line, "1.0.0").is_none());
    }

    #[test]
    fn parse_quoted_assignment_ignores_dotted_keys() {
        assert_eq!(
            parse_quoted_assignment(r#"version = "1.0.0""#, "version").as_deref(),
            Some("1.0.0")
        );
        // `version.workspace = true` must not parse as a quoted version.
        assert!(parse_quoted_assignment("version.workspace = true", "version").is_none());
    }

    #[test]
    fn section_header_detection() {
        let text = "# Changelog\n\n## v1.0.0-pre.11 — theme (2026-07-09)\n";
        assert!(has_section_header(text, "1.0.0-pre.11"));
        assert!(!has_section_header(text, "1.0.0-pre.12"));
        // A prefix must not false-match.
        assert!(!has_section_header("## v1.0.0-pre.1 (x)\n", "1.0.0-pre.11"));
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
