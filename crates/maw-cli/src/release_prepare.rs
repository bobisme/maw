//! `maw release prepare` and `maw release preflight`.
//!
//! Automates the mechanical half of a release so cutting one is a single
//! command plus human review, and so a broken publish chain or version skew is
//! caught in PR CI (via `just release-preflight` + the publish dry-run
//! workflow) rather than on tag day.
//!
//! Two workspace layouts are supported (see [`VersionMode`]):
//!
//!   * **Inherited** — `[workspace.package] version = "…"` with members on
//!     `version.workspace = true`. This is maw's own layout.
//!   * **Single** — `[workspace]` plus a root `[package] version = "…"`, with
//!     members free to pin their own versions independently (sigil's layout).
//!     Only crates already sharing the root version are treated as lockstep
//!     participants; a member deliberately parked at its own version (an
//!     unpublished `0.0.0` helper, say) is left alone by both commands.
//!
//! `prepare vX.Y.Z`:
//!   1. Lockstep version bump — the workspace version plus every internal
//!      path-dep `version = "…"` string across every `Cargo.toml` (outside
//!      `target/` and `.maw/`). In Inherited mode `version.workspace = true`
//!      handles the crate versions themselves; these path-dep strings do not
//!      and had to be moved by hand historically (a ~19-string global sed). In
//!      Single mode every `[package]` version and path-dep string currently
//!      equal to the old root version moves in lockstep; anything else is
//!      treated as deliberate and untouched.
//!   2. Regenerate `Cargo.lock` (`cargo update --workspace` — the cheap path;
//!      it rewrites only the workspace members, no external dep churn).
//!   3. Scaffold a CHANGELOG.md version header if absent, matching that file's
//!      own heading convention — maw's `## vX.Y.Z (YYYY-MM-DD)` or Keep a
//!      Changelog's `## [X.Y.Z] — YYYY-MM-DD` (content stays human-written —
//!      no notes are generated from commits).
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
    let ws = find_workspace_root()?;
    let root = ws.path.clone();
    // The version being moved away from. In Single mode it identifies which
    // version strings are lockstep participants, so it has to be read before
    // anything is rewritten.
    let old_version = read_workspace_version(&ws)?;

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
        bumped += bump_cargo_toml(toml, &version, is_root, ws.mode, &old_version)?;
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
    let ws = find_workspace_root()?;
    let root = ws.path.clone();
    let workspace_version = read_workspace_version(&ws)?;

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

    // Version-consistency across manifests (what this means depends on mode —
    // see `scan_version_skew`).
    for skew in scan_version_skew(&root, &workspace_version, ws.mode)? {
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
            "CHANGELOG.md has no section for v{want_section} \
             (a `## v{want_section}` or `## [{want_section}]` heading; \
             run `maw release prepare v{want_section}` to scaffold one)"
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

/// How a workspace declares the version a release moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VersionMode {
    /// `[workspace.package] version = "…"`, inherited by members through
    /// `version.workspace = true`. Every member moves in lockstep by
    /// construction. (maw's own layout.)
    Inherited,
    /// `[workspace]` plus a root `[package] version = "…"`. Members declare
    /// their own versions and need not share the root's.
    Single,
}

impl VersionMode {
    /// The manifest table the workspace version lives in, for error text.
    const fn version_location(self) -> &'static str {
        match self {
            Self::Inherited => "[workspace.package]",
            Self::Single => "[package]",
        }
    }
}

/// A located workspace root and how it declares its version.
#[derive(Debug)]
struct WorkspaceRoot {
    path: PathBuf,
    mode: VersionMode,
}

/// Classify a `Cargo.toml` as a workspace root, if it is one.
///
/// Returns `None` for a member manifest (no `[workspace]` table) and for a
/// manifest that parses but declares no usable version. A manifest that fails
/// to parse is not a root as far as we are concerned — cargo will complain
/// about it far more usefully than we could.
fn classify_root(manifest: &Path) -> Option<VersionMode> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let value: toml::Value = text.parse().ok()?;
    let table = value.as_table()?;
    let workspace = table.get("workspace")?.as_table()?;
    if workspace
        .get("package")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .is_some()
    {
        return Some(VersionMode::Inherited);
    }
    if root_package_version(table).is_some() {
        return Some(VersionMode::Single);
    }
    None
}

/// The `[package] version = "…"` of an already-parsed manifest table.
fn root_package_version(table: &toml::Table) -> Option<String> {
    table
        .get("package")?
        .as_table()?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// Ascend from the current directory to the nearest `Cargo.toml` that is a
/// workspace root — either layout (see [`VersionMode`]).
///
/// A manifest with a `[workspace]` table always wins over a manifest that only
/// has `[package]`: running from inside a member crate must find the enclosing
/// workspace, not the member. A `[package]`-only manifest is accepted as a
/// last resort so a standalone single-crate repo still works.
fn find_workspace_root() -> Result<WorkspaceRoot> {
    let start = std::env::current_dir().context("cannot determine current directory")?;
    find_workspace_root_from(&start)
}

fn find_workspace_root_from(start: &Path) -> Result<WorkspaceRoot> {
    let mut dir = Some(start);
    // First `[package]`-only manifest seen on the way up, used only if no
    // `[workspace]` manifest exists above it.
    let mut standalone: Option<PathBuf> = None;

    while let Some(current) = dir {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            if let Some(mode) = classify_root(&candidate) {
                return Ok(WorkspaceRoot {
                    path: current.to_path_buf(),
                    mode,
                });
            }
            if standalone.is_none()
                && let Ok(text) = std::fs::read_to_string(&candidate)
                && let Ok(value) = text.parse::<toml::Value>()
                && let Some(table) = value.as_table()
                && !table.contains_key("workspace")
                && root_package_version(table).is_some()
            {
                standalone = Some(current.to_path_buf());
            }
        }
        dir = current.parent();
    }

    if let Some(path) = standalone {
        return Ok(WorkspaceRoot {
            path,
            mode: VersionMode::Single,
        });
    }

    bail!(
        "no cargo workspace root found: walked up from {} without finding a Cargo.toml declaring \
         a version.\n  \
         Expected one of:\n    \
         [workspace.package] version = \"…\"   (members use version.workspace = true)\n    \
         [workspace] + [package] version = \"…\"  (members pin their own versions)\n  \
         Run `maw release prepare` from inside the cargo workspace, or add a version to the root \
         manifest.",
        start.display()
    )
}

/// Read the workspace version from the root `Cargo.toml`, per its mode.
fn read_workspace_version(root: &WorkspaceRoot) -> Result<String> {
    let path = root.path.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let want_section = root.mode.version_location();
    let mut in_section = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == want_section;
            continue;
        }
        if in_section && let Some(val) = parse_quoted_assignment(trimmed, "version") {
            return Ok(val);
        }
    }
    bail!(
        "no `version = \"…\"` under {want_section} in {}",
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
///
/// In [`VersionMode::Single`] the root version is a `[package]` version like
/// any member's, so lockstep membership cannot be read off the manifest
/// structure. It is inferred instead: a `[package]` version or internal
/// path-dep string whose current value is `old_version` was moving with the
/// workspace and keeps moving; every other value is deliberate and is left
/// alone. External dependency versions are never touched in either mode.
fn bump_cargo_toml(
    path: &Path,
    version: &str,
    is_root: bool,
    mode: VersionMode,
    old_version: &str,
) -> Result<usize> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut changed = 0usize;
    let mut section = String::new();
    let mut out = String::with_capacity(text.len());

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.to_string();
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Internal path-dep line: has both `path = "…"` and `version = "…"`.
        let is_path_dep = line.contains("path = \"") && line.contains("version = \"");
        let is_bumpable = match mode {
            VersionMode::Inherited => {
                let is_ws_version = is_root
                    && section == "[workspace.package]"
                    && parse_quoted_assignment(trimmed, "version").is_some();
                is_ws_version || is_path_dep
            }
            VersionMode::Single => {
                let is_pkg_version =
                    section == "[package]" && parse_quoted_assignment(trimmed, "version").is_some();
                (is_pkg_version || is_path_dep)
                    && extract_version_value(line).as_deref() == Some(old_version)
            }
        };

        if is_bumpable && let Some((rewritten, did)) = replace_version_value(line, version) {
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

/// Every lockstep workspace member in `Cargo.lock` must be pinned at the
/// workspace version. Returns a skew message per offender.
fn scan_lock_skew(root: &Path, version: &str) -> Result<Vec<String>> {
    let members = lockstep_member_names(root, version)?;
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

/// Collect the `[package] name` of every workspace member that moves with the
/// workspace version — one that inherits it (`version.workspace = true`) or
/// already declares exactly it.
///
/// A member pinned at some other version (an unpublished `0.0.0` helper crate,
/// say) is deliberately off-lockstep and must not be reported as skew.
fn lockstep_member_names(root: &Path, version: &str) -> Result<std::collections::HashSet<String>> {
    let mut names = std::collections::HashSet::new();
    for toml in collect_cargo_tomls(root) {
        let text = std::fs::read_to_string(&toml).unwrap_or_default();
        let Some(name) = package_field(&text, "name") else {
            continue;
        };
        let lockstep = match package_version_decl(&text) {
            Some(PackageVersion::Inherited) => true,
            Some(PackageVersion::Literal(v)) => v == version,
            None => false,
        };
        if lockstep {
            names.insert(name);
        }
    }
    Ok(names)
}

/// How a manifest's `[package]` declares its version.
enum PackageVersion {
    /// `version.workspace = true`
    Inherited,
    /// `version = "…"`
    Literal(String),
}

/// Read the `[package]` version declaration out of a manifest's text.
fn package_version_decl(text: &str) -> Option<PackageVersion> {
    let mut in_pkg = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_pkg = trimmed == "[package]";
            continue;
        }
        if !in_pkg {
            continue;
        }
        if let Some(v) = parse_quoted_assignment(trimmed, "version") {
            return Some(PackageVersion::Literal(v));
        }
        if trimmed.replace(' ', "") == "version.workspace=true" {
            return Some(PackageVersion::Inherited);
        }
    }
    None
}

/// Read a quoted `[package]` field (e.g. `name`) out of a manifest's text.
fn package_field(text: &str, key: &str) -> Option<String> {
    let mut in_pkg = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_pkg = trimmed == "[package]";
            continue;
        }
        if in_pkg && let Some(val) = parse_quoted_assignment(trimmed, key) {
            return Some(val);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Version-skew scan (the preflight core)
// ---------------------------------------------------------------------------

/// Scan every `Cargo.toml` for version strings that are provably inconsistent.
///
/// What counts as inconsistent depends on the layout:
///
/// * [`VersionMode::Inherited`] — every member shares the workspace version by
///   construction, so any internal path-dep string (and the
///   `[workspace.package]` version itself) that disagrees with it is skew.
/// * [`VersionMode::Single`] — members hold independent versions, so a
///   path-dep string disagreeing with the *workspace* version proves nothing.
///   What it must agree with is the version of the crate it points at; that
///   mismatch would break `cargo publish` and is the check worth making.
fn scan_version_skew(root: &Path, version: &str, mode: VersionMode) -> Result<Vec<String>> {
    match mode {
        VersionMode::Inherited => scan_version_skew_inherited(root, version),
        VersionMode::Single => scan_path_dep_target_skew(root),
    }
}

fn scan_version_skew_inherited(root: &Path, version: &str) -> Result<Vec<String>> {
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

/// Every internal path-dep `version = "…"` must match the version declared by
/// the crate at that `path`. Layout-independent, but only used for
/// [`VersionMode::Single`] — the Inherited scan already subsumes it.
fn scan_path_dep_target_skew(root: &Path) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for toml in collect_cargo_tomls(root) {
        let text = std::fs::read_to_string(&toml)
            .with_context(|| format!("reading {}", toml.display()))?;
        let rel = toml
            .strip_prefix(root)
            .unwrap_or(&toml)
            .display()
            .to_string();
        let Some(dir) = toml.parent() else { continue };
        for (idx, line) in text.lines().enumerate() {
            let (Some(dep_path), Some(pinned)) =
                (extract_path_value(line), extract_version_value(line))
            else {
                continue;
            };
            let target = dir.join(&dep_path).join("Cargo.toml");
            let Ok(target_text) = std::fs::read_to_string(&target) else {
                continue;
            };
            // An inheriting target is by definition at the workspace version,
            // which the caller has already verified.
            if let Some(PackageVersion::Literal(actual)) = package_version_decl(&target_text)
                && actual != pinned
            {
                problems.push(format!(
                    "version skew: {rel}:{} path-dep pins \"{pinned}\" but {dep_path} declares \
                     \"{actual}\"",
                    idx + 1
                ));
            }
        }
    }
    Ok(problems)
}

/// Extract the first `version = "…"` value on a line.
fn extract_version_value(line: &str) -> Option<String> {
    extract_quoted_value(line, "version")
}

/// Extract the first `path = "…"` value on a line.
fn extract_path_value(line: &str) -> Option<String> {
    extract_quoted_value(line, "path")
}

/// Extract the first `{key} = "…"` value anywhere on a line.
fn extract_quoted_value(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key} = \"");
    let start = line.find(&needle)?;
    let rest = &line[start + needle.len()..];
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

/// The version a `## ` heading announces, if any.
///
/// Both common conventions are recognised, since maw runs in repos it does not
/// own: maw's own `## v1.0.0-pre.12 — theme (date)` and Keep a Changelog's
/// `## [0.27.0] — date — title`. A bare `## 0.27.0` counts too. Non-version
/// headings (`## [Unreleased]`) yield `None`.
fn heading_version(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("## ")?;
    let token = rest.split_whitespace().next()?;
    let token = token
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or(token);
    let token = token.strip_prefix('v').unwrap_or(token);
    // A version starts with a digit; `Unreleased` and friends do not.
    if token.starts_with(|c: char| c.is_ascii_digit()) {
        Some(token)
    } else {
        None
    }
}

fn has_section_header(text: &str, version: &str) -> bool {
    text.lines().any(|l| heading_version(l) == Some(version))
}

/// Does `s` have the shape `YYYY-MM-DD`?
fn looks_like_iso_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3
        && [4usize, 2, 2]
            .iter()
            .zip(&parts)
            .all(|(want, p)| p.len() == *want && p.bytes().all(|b| b.is_ascii_digit()))
}

/// How a CHANGELOG writes its version headings, so a scaffolded section looks
/// like the ones around it rather than importing maw's house style.
#[derive(Clone, Copy)]
struct ChangelogStyle {
    bracketed: bool,
    v_prefix: bool,
    dash_date: bool,
}

impl ChangelogStyle {
    /// maw's own convention, used when a CHANGELOG has no versioned heading to
    /// learn from.
    const MAW: Self = Self {
        bracketed: false,
        v_prefix: true,
        dash_date: false,
    };

    /// Infer the style from the first versioned heading in `text`.
    fn detect(text: &str) -> Self {
        for line in text.lines() {
            if heading_version(line).is_none() {
                continue;
            }
            let rest = line.strip_prefix("## ").unwrap_or(line);
            let token = rest.split_whitespace().next().unwrap_or_default();
            // `— 2026-07-29` is a dash-delimited date; maw's own
            // `— theme (2026-07-11)` is a dash-delimited *theme* with a
            // parenthesised date, so the dash alone does not decide it.
            let after = rest[token.len()..].trim_start();
            let dash_date = after
                .strip_prefix(['—', '-'])
                .map(str::trim_start)
                .and_then(|s| s.split_whitespace().next())
                .is_some_and(looks_like_iso_date);
            return Self {
                bracketed: token.starts_with('['),
                v_prefix: token.trim_start_matches('[').starts_with('v'),
                dash_date,
            };
        }
        Self::MAW
    }

    fn header(self, version: &str, date: &str) -> String {
        let v = if self.v_prefix {
            format!("v{version}")
        } else {
            version.to_string()
        };
        let v = if self.bracketed { format!("[{v}]") } else { v };
        if self.dash_date {
            format!("## {v} — {date}")
        } else {
            format!("## {v} ({date})")
        }
    }
}

/// Insert a version header above the first existing versioned section if
/// absent, matching that CHANGELOG's own heading style. Returns whether it
/// added one.
fn scaffold_changelog(root: &Path, version: &str) -> Result<bool> {
    let path = changelog_path(root);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if has_section_header(&text, version) {
        return Ok(false);
    }

    let header = ChangelogStyle::detect(&text).header(version, &today_iso());
    let block = format!("{header}\n\n<!-- release notes: fill in before tagging -->\n\n");

    // Insert above the newest existing release section, leaving any leading
    // `## [Unreleased]` where it is. Fall back to the first `## ` heading, then
    // to appending.
    let anchor = |line: &str| heading_version(line).is_some();
    let has_versioned = text.lines().any(anchor);
    let mut out = String::with_capacity(text.len() + block.len());
    let mut inserted = false;
    for line in text.lines() {
        let is_anchor = if has_versioned {
            anchor(line)
        } else {
            line.starts_with("## ")
        };
        if !inserted && is_anchor {
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
    fn keep_a_changelog_headings_are_recognised() {
        // sigil's convention.
        let text = "# Changelog\n\n## [Unreleased]\n\n## [0.27.0] — 2026-07-29 — Layout\n";
        assert!(has_section_header(text, "0.27.0"));
        assert!(!has_section_header(text, "0.28.0"));
        // `[Unreleased]` is not a version.
        assert_eq!(heading_version("## [Unreleased]"), None);
        // Prefixes still must not false-match.
        assert!(!has_section_header(
            "## [1.0.0-pre.1] — x\n",
            "1.0.0-pre.11"
        ));
        // Bare, unbracketed versions count too.
        assert!(has_section_header("## 2.1.0 (2026-01-01)\n", "2.1.0"));
    }

    #[test]
    fn scaffold_matches_the_files_own_heading_style() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("CHANGELOG.md");

        // Keep a Changelog: bracketed, dash date, section goes under
        // `## [Unreleased]` but above the newest release.
        std::fs::write(
            &path,
            "# Changelog\n\n## [Unreleased]\n\n## [0.27.0] — 2026-07-29 — Layout\n\nnotes\n",
        )
        .unwrap();
        assert!(scaffold_changelog(tmp.path(), "0.28.0").unwrap());
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains(&format!("## [0.28.0] — {}", today_iso())),
            "{out}"
        );
        let unreleased = out.find("## [Unreleased]").unwrap();
        let new_section = out.find("## [0.28.0]").unwrap();
        let previous = out.find("## [0.27.0]").unwrap();
        assert!(unreleased < new_section && new_section < previous, "{out}");
        // Idempotent.
        assert!(!scaffold_changelog(tmp.path(), "0.28.0").unwrap());

        // maw's own style is preserved.
        std::fs::write(
            &path,
            "# Changelog\n\n## v1.0.0-pre.12 — theme (2026-07-11)\n",
        )
        .unwrap();
        assert!(scaffold_changelog(tmp.path(), "1.0.0-pre.13").unwrap());
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains(&format!("## v1.0.0-pre.13 ({})", today_iso())),
            "{out}"
        );
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    // -----------------------------------------------------------------------
    // Workspace-layout detection (bn-3oae)
    // -----------------------------------------------------------------------

    /// Write `contents` to `dir/rel`, creating parent directories.
    fn write_at(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// maw's own layout: `[workspace.package]` with inheriting members.
    fn inherited_workspace(root: &Path) {
        write_at(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n\n\
             [workspace.package]\nversion = \"1.0.0-pre.12\"\n",
        );
        write_at(
            root,
            "crates/maw-git/Cargo.toml",
            "[package]\nname = \"maw-git\"\nversion.workspace = true\n",
        );
        write_at(
            root,
            "crates/maw-cli/Cargo.toml",
            "[package]\nname = \"maw-cli\"\nversion.workspace = true\n\n\
             [dependencies]\n\
             maw-git = { path = \"../maw-git\", version = \"1.0.0-pre.12\" }\n\
             serde = { version = \"1\" }\n",
        );
    }

    /// sigil's layout: `[workspace]` + root `[package]`, member off-lockstep.
    fn single_workspace(root: &Path) {
        write_at(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\".\", \"crates/sigil-browser\"]\n\n\
             [package]\nname = \"sigil\"\nversion = \"0.27.0\"\n\n\
             [dependencies]\n\
             sigil-browser = { path = \"crates/sigil-browser\" }\n\
             clap = { version = \"0.27.0\" }\n",
        );
        write_at(
            root,
            "crates/sigil-browser/Cargo.toml",
            "[package]\nname = \"sigil-browser\"\nversion = \"0.0.0\"\n",
        );
    }

    #[test]
    fn detects_inherited_layout() {
        let tmp = tempfile::tempdir().unwrap();
        inherited_workspace(tmp.path());
        let ws = find_workspace_root_from(tmp.path()).unwrap();
        assert_eq!(ws.mode, VersionMode::Inherited);
        assert_eq!(read_workspace_version(&ws).unwrap(), "1.0.0-pre.12");
    }

    #[test]
    fn detects_single_version_layout() {
        let tmp = tempfile::tempdir().unwrap();
        single_workspace(tmp.path());
        let ws = find_workspace_root_from(tmp.path()).unwrap();
        assert_eq!(ws.mode, VersionMode::Single);
        assert_eq!(read_workspace_version(&ws).unwrap(), "0.27.0");
    }

    #[test]
    fn ascends_from_member_to_enclosing_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        single_workspace(tmp.path());
        // A member that pins its own version must not be mistaken for the root.
        let ws = find_workspace_root_from(&tmp.path().join("crates/sigil-browser")).unwrap();
        assert_eq!(ws.path, tmp.path());
        assert_eq!(ws.mode, VersionMode::Single);
    }

    #[test]
    fn standalone_crate_is_a_last_resort_root() {
        let tmp = tempfile::tempdir().unwrap();
        write_at(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"solo\"\nversion = \"0.3.0\"\n",
        );
        let ws = find_workspace_root_from(tmp.path()).unwrap();
        assert_eq!(ws.mode, VersionMode::Single);
        assert_eq!(read_workspace_version(&ws).unwrap(), "0.3.0");
    }

    #[test]
    fn commented_out_workspace_package_is_not_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        write_at(
            tmp.path(),
            "Cargo.toml",
            "[workspace]\nmembers = []\n# [workspace.package]\n# version = \"9.9.9\"\n",
        );
        // No version anywhere: not a root, and the walk reports it clearly.
        let err = find_workspace_root_from(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no cargo workspace root found"), "{err}");
        assert!(err.contains("[workspace.package]"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Single-mode bump / skew semantics (bn-3oae)
    // -----------------------------------------------------------------------

    #[test]
    fn single_mode_bump_moves_only_lockstep_versions() {
        let tmp = tempfile::tempdir().unwrap();
        single_workspace(tmp.path());
        let root = tmp.path();

        let root_toml = root.join("Cargo.toml");
        let member_toml = root.join("crates/sigil-browser/Cargo.toml");
        let changed = bump_cargo_toml(&root_toml, "0.28.0", true, VersionMode::Single, "0.27.0")
            .unwrap()
            + bump_cargo_toml(&member_toml, "0.28.0", false, VersionMode::Single, "0.27.0")
                .unwrap();
        assert_eq!(changed, 1, "only the root package version moves");

        let root_text = std::fs::read_to_string(&root_toml).unwrap();
        assert!(root_text.contains("version = \"0.28.0\""));
        // An external dep that happened to sit at the old version is untouched.
        assert!(
            root_text.contains("clap = { version = \"0.27.0\" }"),
            "{root_text}"
        );
        // The deliberately off-lockstep member is untouched.
        let member_text = std::fs::read_to_string(&member_toml).unwrap();
        assert!(member_text.contains("version = \"0.0.0\""), "{member_text}");
    }

    #[test]
    fn single_mode_bump_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        single_workspace(tmp.path());
        let root_toml = tmp.path().join("Cargo.toml");
        bump_cargo_toml(&root_toml, "0.28.0", true, VersionMode::Single, "0.27.0").unwrap();
        let after_first = std::fs::read_to_string(&root_toml).unwrap();
        // Second run reads the new version as `old` — a no-op.
        let changed =
            bump_cargo_toml(&root_toml, "0.28.0", true, VersionMode::Single, "0.28.0").unwrap();
        assert_eq!(changed, 0);
        assert_eq!(std::fs::read_to_string(&root_toml).unwrap(), after_first);
    }

    #[test]
    fn off_lockstep_member_is_not_skew() {
        let tmp = tempfile::tempdir().unwrap();
        single_workspace(tmp.path());
        // sigil-browser at 0.0.0 must not be reported against the root version.
        let members = lockstep_member_names(tmp.path(), "0.27.0").unwrap();
        assert!(members.contains("sigil"));
        assert!(!members.contains("sigil-browser"));
        assert!(
            scan_version_skew(tmp.path(), "0.27.0", VersionMode::Single)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn single_mode_flags_path_dep_disagreeing_with_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        single_workspace(tmp.path());
        // Pin the path-dep at a version the target does not declare.
        write_at(
            tmp.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\".\", \"crates/sigil-browser\"]\n\n\
             [package]\nname = \"sigil\"\nversion = \"0.27.0\"\n\n\
             [dependencies]\n\
             sigil-browser = { path = \"crates/sigil-browser\", version = \"0.1.0\" }\n",
        );
        let problems = scan_version_skew(tmp.path(), "0.27.0", VersionMode::Single).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("pins \"0.1.0\""), "{problems:?}");
        assert!(problems[0].contains("\"0.0.0\""), "{problems:?}");
    }

    #[test]
    fn inherited_mode_still_flags_path_dep_skew() {
        let tmp = tempfile::tempdir().unwrap();
        inherited_workspace(tmp.path());
        // Behaviour maw itself depends on: a stale path-dep string is skew.
        write_at(
            tmp.path(),
            "crates/maw-cli/Cargo.toml",
            "[package]\nname = \"maw-cli\"\nversion.workspace = true\n\n\
             [dependencies]\n\
             maw-git = { path = \"../maw-git\", version = \"1.0.0-pre.11\" }\n",
        );
        let problems =
            scan_version_skew(tmp.path(), "1.0.0-pre.12", VersionMode::Inherited).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("internal path-dep"), "{problems:?}");
        // Inheriting members are lockstep regardless of what they declare.
        let members = lockstep_member_names(tmp.path(), "1.0.0-pre.12").unwrap();
        assert!(members.contains("maw-git") && members.contains("maw-cli"));
    }
}
