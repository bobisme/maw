//! Manifold repository configuration (`config.toml`).
//!
//! Defines the typed configuration for `.manifold/config.toml`, including
//! workspace backend selection, merge validation, and merge drivers.

use std::fmt;
use std::path::Path;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Top-level Manifold repository configuration.
///
/// Parsed from `.manifold/config.toml`. Missing fields use sensible defaults.
/// Missing file → all defaults (no error).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub struct ManifoldConfig {
    /// Repository-level settings.
    #[serde(default)]
    pub repo: RepoConfig,

    /// Workspace backend settings.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Merge settings.
    #[serde(default)]
    pub merge: MergeConfig,
}

// ---------------------------------------------------------------------------
// RepoConfig
// ---------------------------------------------------------------------------

/// Repository-level settings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// The main branch name (default: `"main"`).
    #[serde(default = "default_branch")]
    pub branch: String,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            branch: default_branch(),
        }
    }
}

fn default_branch() -> String {
    "main".to_owned()
}

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

/// Workspace backend selection.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Which backend to use for workspace isolation.
    #[serde(default)]
    pub backend: BackendKind,

    /// Enable Level 1 git compatibility refs (`refs/manifold/ws/<name>`).
    ///
    /// When enabled, workspace state can be inspected with standard git tools,
    /// e.g. `git diff refs/manifold/ws/alice..main`.
    #[serde(default = "default_git_compat_refs")]
    pub git_compat_refs: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::default(),
            git_compat_refs: default_git_compat_refs(),
        }
    }
}

const fn default_git_compat_refs() -> bool {
    true
}

/// The workspace isolation backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// Auto-detect the best available backend.
    #[default]
    Auto,
    /// Git worktree backend (Phase 1).
    GitWorktree,
    /// Reflink/CoW backend (Btrfs/XFS/APFS).
    Reflink,
    /// `OverlayFS` backend (Linux only).
    Overlay,
    /// Plain copy backend (universal fallback).
    Copy,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::GitWorktree => write!(f, "git-worktree"),
            Self::Reflink => write!(f, "reflink"),
            Self::Overlay => write!(f, "overlay"),
            Self::Copy => write!(f, "copy"),
        }
    }
}

// ---------------------------------------------------------------------------
// MergeConfig
// ---------------------------------------------------------------------------

/// Merge behaviour settings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub struct MergeConfig {
    /// Post-merge validation settings.
    #[serde(default)]
    pub validation: ValidationConfig,

    /// Custom merge drivers.
    #[serde(default)]
    pub drivers: Vec<MergeDriver>,

    /// AST-aware merge settings (opt-in per language via tree-sitter).
    #[serde(default)]
    pub ast: AstConfig,
}

// ---------------------------------------------------------------------------
// AstConfig — AST-aware merge settings
// ---------------------------------------------------------------------------

/// Configuration for AST-aware merge via tree-sitter (§6.2).
///
/// Controls which languages use AST-level merge as a fallback when diff3 fails.
/// Enabled by default for all built-in language packs.
///
/// ```toml
/// [merge.ast]
/// languages = ["rust", "python", "typescript", "javascript", "go"]
/// packs = ["core", "web", "backend"]
/// semantic_false_positive_budget_pct = 5
/// semantic_min_confidence = 70
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AstConfig {
    /// Languages for which AST merge is enabled.
    ///
    /// Supported values: `"rust"`, `"python"`, `"typescript"`, `"javascript"`, `"go"`.
    /// Empty by default; language packs control baseline enablement.
    #[serde(default)]
    pub languages: Vec<AstConfigLanguage>,

    /// Optional language packs that expand to multiple languages.
    ///
    /// Packs are additive with `languages` and deduplicated by the merge layer.
    #[serde(default = "default_ast_packs")]
    pub packs: Vec<AstLanguagePack>,

    /// Maximum allowed semantic false-positive rate percentage (0-100).
    ///
    /// Semantic rules with confidence below `min_confidence` are downgraded to
    /// generic AST-node conflict reasons to keep diagnostics conservative.
    #[serde(default = "default_semantic_false_positive_budget_pct")]
    pub semantic_false_positive_budget_pct: u8,

    /// Minimum confidence required for semantic rule-specific diagnostics.
    #[serde(default = "default_semantic_min_confidence")]
    pub semantic_min_confidence: u8,
}

impl Default for AstConfig {
    fn default() -> Self {
        Self {
            languages: Vec::new(),
            packs: default_ast_packs(),
            semantic_false_positive_budget_pct: default_semantic_false_positive_budget_pct(),
            semantic_min_confidence: default_semantic_min_confidence(),
        }
    }
}

fn default_ast_packs() -> Vec<AstLanguagePack> {
    vec![
        AstLanguagePack::Core,
        AstLanguagePack::Web,
        AstLanguagePack::Backend,
    ]
}

const fn default_semantic_false_positive_budget_pct() -> u8 {
    5
}

const fn default_semantic_min_confidence() -> u8 {
    70
}

/// A language supported by the AST merge layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AstConfigLanguage {
    /// Rust (.rs files).
    Rust,
    /// Python (.py files).
    Python,
    /// TypeScript (.ts, .tsx files).
    #[serde(alias = "ts")]
    TypeScript,
    /// JavaScript (.js, .jsx, .mjs, .cjs files).
    JavaScript,
    /// Go (.go files).
    Go,
}

/// A predefined pack of AST grammars that can be enabled together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AstLanguagePack {
    /// Existing stable languages (Rust/Python/TypeScript).
    Core,
    /// Front-end language family (TypeScript/JavaScript).
    Web,
    /// Backend language family (Rust/Go/Python).
    Backend,
}

impl MergeConfig {
    /// Return the effective merge drivers.
    ///
    /// If `[[merge.drivers]]` is omitted, built-in deterministic drivers for
    /// common lockfiles are used.
    #[must_use]
    pub fn effective_drivers(&self) -> Vec<MergeDriver> {
        if self.drivers.is_empty() {
            default_merge_drivers()
        } else {
            self.drivers.clone()
        }
    }
}

fn default_merge_drivers() -> Vec<MergeDriver> {
    vec![
        MergeDriver {
            match_glob: "Cargo.lock".to_owned(),
            kind: MergeDriverKind::Regenerate,
            command: Some("cargo generate-lockfile".to_owned()),
        },
        MergeDriver {
            match_glob: "package-lock.json".to_owned(),
            kind: MergeDriverKind::Regenerate,
            command: Some("npm install --package-lock-only".to_owned()),
        },
    ]
}

// ---------------------------------------------------------------------------
// ValidationConfig
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// LanguagePreset
// ---------------------------------------------------------------------------

/// Built-in per-language validation preset.
///
/// Each preset provides a curated sequence of validation commands for a
/// specific language/ecosystem. Presets activate when no explicit
/// `command` or `commands` are configured.
///
/// Use `"auto"` to let Manifold detect the project type from filesystem
/// markers (e.g. `Cargo.toml` → Rust, `pyproject.toml` → Python,
/// `tsconfig.json` → TypeScript).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguagePreset {
    /// Auto-detect project type from filesystem markers.
    ///
    /// Detection order:
    /// 1. `Cargo.toml` → Rust
    /// 2. `pyproject.toml` / `setup.py` / `setup.cfg` → Python
    /// 3. `tsconfig.json` → TypeScript
    Auto,
    /// Rust preset: `["cargo check", "cargo test --no-run"]`.
    Rust,
    /// Python preset: `["python -m py_compile", "pytest -q --co"]`.
    Python,
    /// TypeScript preset: `["tsc --noEmit"]`.
    TypeScript,
}

impl LanguagePreset {
    /// Returns the validation commands for this preset.
    ///
    /// Returns an empty slice for `Auto` — auto-detection must be performed
    /// externally against the actual project directory.
    #[must_use]
    pub const fn commands(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["cargo check", "cargo test --no-run"],
            Self::Python => &["python -m py_compile", "pytest -q --co"],
            Self::TypeScript => &["tsc --noEmit"],
            Self::Auto => &[],
        }
    }
}

impl fmt::Display for LanguagePreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Rust => write!(f, "rust"),
            Self::Python => write!(f, "python"),
            Self::TypeScript => write!(f, "typescript"),
        }
    }
}

// ---------------------------------------------------------------------------
// ValidationConfig
// ---------------------------------------------------------------------------

/// Post-merge validation command settings.
///
/// Supports both a single `command` string and a `commands` array. When both
/// are specified, `command` runs first, then all entries from `commands`.
/// When neither is set, validation is skipped — unless a `preset` is
/// configured, in which case the preset's commands are used.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationConfig {
    /// Shell command to run for post-merge validation (e.g. `"cargo test"`).
    /// `None` means no validation (unless `commands` or `preset` is set).
    pub command: Option<String>,

    /// Multiple shell commands to run in sequence. Each runs via `sh -c`.
    /// Execution stops on first failure.
    #[serde(default)]
    pub commands: Vec<String>,

    /// Per-language preset. When set and no explicit `command`/`commands`
    /// are configured, the preset's commands are used instead.
    ///
    /// Use `"auto"` to detect the project type from filesystem markers.
    #[serde(default)]
    pub preset: Option<LanguagePreset>,

    /// Timeout in seconds for each validation command.
    #[serde(default = "default_validation_timeout")]
    pub timeout_seconds: u32,

    /// What to do when validation fails.
    #[serde(default)]
    pub on_failure: OnFailure,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            command: None,
            commands: Vec::new(),
            preset: None,
            timeout_seconds: default_validation_timeout(),
            on_failure: OnFailure::default(),
        }
    }
}

impl ValidationConfig {
    /// Returns the explicit commands from `command` and `commands` fields.
    ///
    /// Empty commands are filtered out. If `command` is set, it becomes the
    /// first entry. All entries from `commands` follow.
    ///
    /// **Note:** This does **not** include preset commands. Use
    /// [`effective_commands`] or the validate phase's preset resolution for
    /// the full command list (explicit + preset fallback).
    #[must_use]
    pub fn effective_commands(&self) -> Vec<&str> {
        let mut result = Vec::new();
        if let Some(cmd) = &self.command
            && !cmd.is_empty()
        {
            result.push(cmd.as_str());
        }
        for cmd in &self.commands {
            if !cmd.is_empty() {
                result.push(cmd.as_str());
            }
        }
        result
    }

    /// Returns `true` if at least one explicit validation command is
    /// configured (ignoring presets).
    #[must_use]
    pub fn has_commands(&self) -> bool {
        !self.effective_commands().is_empty()
    }

    /// Returns `true` if validation is configured — either through explicit
    /// commands or a preset.
    #[must_use]
    #[allow(dead_code)]
    pub fn has_any_validation(&self) -> bool {
        self.has_commands() || self.preset.is_some()
    }
}

const fn default_validation_timeout() -> u32 {
    60
}

/// Action to take when post-merge validation fails.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnFailure {
    /// Log a warning but allow the merge.
    Warn,
    /// Block the merge — do not advance the epoch.
    Block,
    /// Create a quarantine workspace with the failed merge result.
    Quarantine,
    /// Block the merge AND create a quarantine workspace.
    #[default]
    BlockQuarantine,
}

impl fmt::Display for OnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warn => write!(f, "warn"),
            Self::Block => write!(f, "block"),
            Self::Quarantine => write!(f, "quarantine"),
            Self::BlockQuarantine => write!(f, "block+quarantine"),
        }
    }
}

// ---------------------------------------------------------------------------
// MergeDriver
// ---------------------------------------------------------------------------

/// A custom merge driver for specific file patterns.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeDriver {
    /// Glob pattern for matching file paths (e.g. `"*.lock"`, `"schema/*.sql"`).
    #[serde(rename = "match")]
    pub match_glob: String,

    /// The driver kind.
    pub kind: MergeDriverKind,

    /// External command for `regenerate` drivers. Ignored for `ours/theirs`.
    pub command: Option<String>,
}

/// Built-in merge driver kinds.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeDriverKind {
    /// Re-generate the file deterministically from merged sources.
    ///
    /// Requires `command` in `[[merge.drivers]]`.
    Regenerate,
    /// Deterministically keep the epoch/main version (base side).
    Ours,
    /// Deterministically keep the workspace side.
    ///
    /// Only valid when exactly one workspace touched the path.
    Theirs,
}

impl fmt::Display for MergeDriverKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Regenerate => write!(f, "regenerate"),
            Self::Ours => write!(f, "ours"),
            Self::Theirs => write!(f, "theirs"),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Error loading a Manifold configuration file.
#[derive(Debug)]
pub struct ConfigError {
    /// The path that was being loaded (if available).
    pub path: Option<std::path::PathBuf>,
    /// Human-readable message with line-level detail when possible.
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(p) = &self.path {
            write!(f, "{}: {}", p.display(), self.message)
        } else {
            write!(f, "config error: {}", self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

impl ManifoldConfig {
    /// Load configuration from a TOML file.
    ///
    /// - If the file does not exist, returns all defaults (not an error).
    /// - If the file exists but contains invalid TOML or unknown fields,
    ///   returns a [`ConfigError`] with line-level detail.
    ///
    /// # Errors
    /// Returns `ConfigError` on I/O errors (other than not-found) or parse errors.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(ConfigError {
                    path: Some(path.to_owned()),
                    message: format!("could not read file: {e}"),
                });
            }
        };
        Self::parse(&contents).map_err(|mut e| {
            e.path = Some(path.to_owned());
            e
        })
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns `ConfigError` on invalid TOML or unknown fields.
    pub fn parse(toml_str: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml_str).map_err(|e| {
            let mut message = e.message().to_owned();
            if let Some(span) = e.span() {
                // Calculate line number from byte offset.
                let line = toml_str[..span.start]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count()
                    + 1;
                message = format!("line {line}: {message}");
            }
            ConfigError {
                path: None,
                message,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_all_fields() {
        let cfg = ManifoldConfig::default();
        assert_eq!(cfg.repo.branch, "main");
        assert_eq!(cfg.workspace.backend, BackendKind::Auto);
        assert!(cfg.workspace.git_compat_refs);
        assert_eq!(cfg.merge.validation.command, None);
        assert!(cfg.merge.validation.commands.is_empty());
        assert_eq!(cfg.merge.validation.timeout_seconds, 60);
        assert_eq!(cfg.merge.validation.on_failure, OnFailure::BlockQuarantine);
        assert!(!cfg.merge.validation.has_commands());
        assert!(cfg.merge.drivers.is_empty());

        let defaults = cfg.merge.effective_drivers();
        assert!(
            defaults
                .iter()
                .any(|d| d.match_glob == "Cargo.lock" && d.kind == MergeDriverKind::Regenerate)
        );
        assert!(
            defaults
                .iter()
                .any(|d| d.match_glob == "package-lock.json"
                    && d.kind == MergeDriverKind::Regenerate)
        );
    }

    #[test]
    fn parse_empty_string() {
        let cfg = ManifoldConfig::parse("").unwrap();
        assert_eq!(cfg, ManifoldConfig::default());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[repo]
branch = "develop"

[workspace]
backend = "git-worktree"

[merge.validation]
command = "cargo test"
timeout_seconds = 120
on_failure = "block"

[[merge.drivers]]
match = "Cargo.lock"
kind = "regenerate"
command = "cargo generate-lockfile"

[[merge.drivers]]
match = "generated/**"
kind = "theirs"
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.repo.branch, "develop");
        assert_eq!(cfg.workspace.backend, BackendKind::GitWorktree);
        assert!(cfg.workspace.git_compat_refs);
        assert_eq!(cfg.merge.validation.command.as_deref(), Some("cargo test"));
        assert_eq!(cfg.merge.validation.timeout_seconds, 120);
        assert_eq!(cfg.merge.validation.on_failure, OnFailure::Block);
        assert_eq!(cfg.merge.drivers.len(), 2);
        assert_eq!(cfg.merge.drivers[0].match_glob, "Cargo.lock");
        assert_eq!(cfg.merge.drivers[0].kind, MergeDriverKind::Regenerate);
        assert_eq!(
            cfg.merge.drivers[0].command.as_deref(),
            Some("cargo generate-lockfile")
        );
        assert_eq!(cfg.merge.drivers[1].match_glob, "generated/**");
        assert_eq!(cfg.merge.drivers[1].kind, MergeDriverKind::Theirs);
        assert!(cfg.merge.drivers[1].command.is_none());
    }

    #[test]
    fn parse_workspace_git_compat_refs_false() {
        let toml = r#"
[workspace]
backend = "git-worktree"
git_compat_refs = false
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.workspace.backend, BackendKind::GitWorktree);
        assert!(!cfg.workspace.git_compat_refs);
    }

    #[test]
    fn parse_commands_array() {
        let toml = r#"
[merge.validation]
commands = ["cargo check", "cargo test"]
timeout_seconds = 120
on_failure = "block"
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.merge.validation.command, None);
        assert_eq!(
            cfg.merge.validation.commands,
            vec!["cargo check", "cargo test"]
        );
        assert_eq!(
            cfg.merge.validation.effective_commands(),
            vec!["cargo check", "cargo test"]
        );
        assert!(cfg.merge.validation.has_commands());
    }

    #[test]
    fn parse_command_and_commands_together() {
        let toml = r#"
[merge.validation]
command = "cargo fmt --check"
commands = ["cargo check", "cargo test"]
on_failure = "block-quarantine"
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(
            cfg.merge.validation.effective_commands(),
            vec!["cargo fmt --check", "cargo check", "cargo test"]
        );
    }

    #[test]
    fn parse_partial_config_uses_defaults() {
        let toml = r#"
[repo]
branch = "trunk"
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.repo.branch, "trunk");
        // Everything else is default.
        assert_eq!(cfg.workspace.backend, BackendKind::Auto);
        assert!(cfg.workspace.git_compat_refs);
        assert_eq!(cfg.merge.validation.timeout_seconds, 60);
        assert!(cfg.merge.validation.commands.is_empty());
    }

    #[test]
    fn parse_rejects_unknown_top_level_field() {
        let toml = r"
unknown_field = true
";
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "error should mention unknown field: {}",
            err.message
        );
    }

    #[test]
    fn parse_rejects_unknown_nested_field() {
        let toml = r#"
[repo]
branch = "main"
extra = "oops"
"#;
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "error should mention unknown field: {}",
            err.message
        );
    }

    #[test]
    fn parse_rejects_invalid_backend() {
        let toml = r#"
[workspace]
backend = "quantum-teleport"
"#;
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("unknown variant"),
            "error should mention unknown variant: {}",
            err.message
        );
    }

    #[test]
    fn parse_rejects_invalid_on_failure() {
        let toml = r#"
[merge.validation]
on_failure = "explode"
"#;
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("unknown variant"),
            "error should mention unknown variant: {}",
            err.message
        );
    }

    #[test]
    fn parse_includes_line_number_on_error() {
        let toml = "good = 1\n[repo]\nbranch = 42\n";
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("line"),
            "error should include line number: {}",
            err.message
        );
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = ManifoldConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(cfg, ManifoldConfig::default());
    }

    #[test]
    fn load_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[repo]
branch = "release"
"#,
        )
        .unwrap();
        let cfg = ManifoldConfig::load(&path).unwrap();
        assert_eq!(cfg.repo.branch, "release");
    }

    #[test]
    fn load_invalid_file_shows_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid [[[toml").unwrap();
        let err = ManifoldConfig::load(&path).unwrap_err();
        assert_eq!(err.path.as_deref(), Some(path.as_path()));
        assert!(!err.message.is_empty());
    }

    // -- BackendKind Display --

    #[test]
    fn backend_kind_display() {
        assert_eq!(format!("{}", BackendKind::Auto), "auto");
        assert_eq!(format!("{}", BackendKind::GitWorktree), "git-worktree");
        assert_eq!(format!("{}", BackendKind::Reflink), "reflink");
        assert_eq!(format!("{}", BackendKind::Overlay), "overlay");
        assert_eq!(format!("{}", BackendKind::Copy), "copy");
    }

    // -- OnFailure Display --

    #[test]
    fn on_failure_display() {
        assert_eq!(format!("{}", OnFailure::Warn), "warn");
        assert_eq!(format!("{}", OnFailure::Block), "block");
        assert_eq!(format!("{}", OnFailure::Quarantine), "quarantine");
        assert_eq!(
            format!("{}", OnFailure::BlockQuarantine),
            "block+quarantine"
        );
    }

    // -- MergeDriverKind Display --

    #[test]
    fn merge_driver_kind_display() {
        assert_eq!(format!("{}", MergeDriverKind::Regenerate), "regenerate");
        assert_eq!(format!("{}", MergeDriverKind::Ours), "ours");
        assert_eq!(format!("{}", MergeDriverKind::Theirs), "theirs");
    }

    // -- All BackendKind variants parse --

    #[test]
    fn all_backend_kinds_parse() {
        for (input, expected) in [
            ("auto", BackendKind::Auto),
            ("git-worktree", BackendKind::GitWorktree),
            ("reflink", BackendKind::Reflink),
            ("overlay", BackendKind::Overlay),
            ("copy", BackendKind::Copy),
        ] {
            let toml = format!("[workspace]\nbackend = \"{input}\"");
            let cfg = ManifoldConfig::parse(&toml).unwrap();
            assert_eq!(cfg.workspace.backend, expected, "variant: {input}");
        }
    }

    // -- All OnFailure variants parse --

    #[test]
    fn all_on_failure_variants_parse() {
        for (input, expected) in [
            ("warn", OnFailure::Warn),
            ("block", OnFailure::Block),
            ("quarantine", OnFailure::Quarantine),
            ("block-quarantine", OnFailure::BlockQuarantine),
        ] {
            let toml = format!("[merge.validation]\non_failure = \"{input}\"");
            let cfg = ManifoldConfig::parse(&toml).unwrap();
            assert_eq!(
                cfg.merge.validation.on_failure, expected,
                "variant: {input}"
            );
        }
    }

    // -- ConfigError Display --

    #[test]
    fn config_error_display_with_path() {
        let err = ConfigError {
            path: Some(std::path::PathBuf::from("/repo/.manifold/config.toml")),
            message: "bad field".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("/repo/.manifold/config.toml"));
        assert!(msg.contains("bad field"));
    }

    #[test]
    fn config_error_display_without_path() {
        let err = ConfigError {
            path: None,
            message: "parse error".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("config error"));
        assert!(msg.contains("parse error"));
    }

    // -- LanguagePreset --

    #[test]
    fn language_preset_display() {
        assert_eq!(format!("{}", LanguagePreset::Auto), "auto");
        assert_eq!(format!("{}", LanguagePreset::Rust), "rust");
        assert_eq!(format!("{}", LanguagePreset::Python), "python");
        assert_eq!(format!("{}", LanguagePreset::TypeScript), "typescript");
    }

    #[test]
    fn language_preset_commands_rust() {
        let cmds = LanguagePreset::Rust.commands();
        assert_eq!(cmds, &["cargo check", "cargo test --no-run"]);
    }

    #[test]
    fn language_preset_commands_python() {
        let cmds = LanguagePreset::Python.commands();
        assert_eq!(cmds, &["python -m py_compile", "pytest -q --co"]);
    }

    #[test]
    fn language_preset_commands_typescript() {
        let cmds = LanguagePreset::TypeScript.commands();
        assert_eq!(cmds, &["tsc --noEmit"]);
    }

    #[test]
    fn language_preset_auto_has_no_commands() {
        // Auto is resolved externally via detect_language_preset.
        assert!(LanguagePreset::Auto.commands().is_empty());
    }

    #[test]
    fn all_language_presets_parse() {
        for (input, expected) in [
            ("auto", LanguagePreset::Auto),
            ("rust", LanguagePreset::Rust),
            ("python", LanguagePreset::Python),
            ("typescript", LanguagePreset::TypeScript),
        ] {
            let toml = format!("[merge.validation]\npreset = \"{input}\"");
            let cfg = ManifoldConfig::parse(&toml).unwrap();
            assert_eq!(
                cfg.merge.validation.preset.as_ref().unwrap(),
                &expected,
                "variant: {input}"
            );
        }
    }

    #[test]
    fn validation_config_preset_defaults_to_none() {
        let cfg = ManifoldConfig::default();
        assert!(cfg.merge.validation.preset.is_none());
    }

    #[test]
    fn validation_config_has_any_validation_with_preset() {
        let cfg = ManifoldConfig::parse("[merge.validation]\npreset = \"rust\"").unwrap();
        assert!(cfg.merge.validation.has_any_validation());
        // No explicit commands set
        assert!(!cfg.merge.validation.has_commands());
    }

    #[test]
    fn validation_config_has_any_validation_with_command() {
        let cfg = ManifoldConfig::parse("[merge.validation]\ncommand = \"cargo test\"").unwrap();
        assert!(cfg.merge.validation.has_any_validation());
        assert!(cfg.merge.validation.has_commands());
    }

    #[test]
    fn validation_config_has_no_validation_by_default() {
        let cfg = ManifoldConfig::default();
        assert!(!cfg.merge.validation.has_any_validation());
    }

    #[test]
    fn parse_preset_with_explicit_commands_coexist() {
        // Explicit commands take precedence over preset in resolve_commands,
        // but both can be specified in TOML.
        let toml = r#"
[merge.validation]
command = "cargo fmt --check"
preset = "rust"
on_failure = "block"
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(
            cfg.merge.validation.command.as_deref(),
            Some("cargo fmt --check")
        );
        assert_eq!(cfg.merge.validation.preset, Some(LanguagePreset::Rust));
        // effective_commands only returns explicit (not preset)
        assert_eq!(
            cfg.merge.validation.effective_commands(),
            vec!["cargo fmt --check"]
        );
        assert!(cfg.merge.validation.has_any_validation());
    }

    #[test]
    fn parse_rejects_invalid_language_preset() {
        let toml = "[merge.validation]\npreset = \"cobol\"";
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("unknown variant"),
            "expected 'unknown variant' but got: {}",
            err.message
        );
    }

    // -----------------------------------------------------------------------
    // AST merge config tests
    // -----------------------------------------------------------------------

    #[test]
    fn ast_config_defaults_to_all_packs() {
        let cfg = ManifoldConfig::default();
        assert!(
            cfg.merge.ast.languages.is_empty(),
            "explicit language list should default to empty"
        );
        assert!(
            cfg.merge.ast.packs.contains(&AstLanguagePack::Core),
            "AST core pack should be enabled by default"
        );
        assert!(
            cfg.merge.ast.packs.contains(&AstLanguagePack::Web),
            "AST web pack should be enabled by default"
        );
        assert!(
            cfg.merge.ast.packs.contains(&AstLanguagePack::Backend),
            "AST backend pack should be enabled by default"
        );
        assert_eq!(cfg.merge.ast.semantic_false_positive_budget_pct, 5);
        assert_eq!(cfg.merge.ast.semantic_min_confidence, 70);
    }

    #[test]
    fn parse_ast_config_all_languages() {
        let toml = r#"
[merge.ast]
languages = ["rust", "python", "typescript"]
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.merge.ast.languages.len(), 3);
        assert!(cfg.merge.ast.languages.contains(&AstConfigLanguage::Rust));
        assert!(cfg.merge.ast.languages.contains(&AstConfigLanguage::Python));
        assert!(
            cfg.merge
                .ast
                .languages
                .contains(&AstConfigLanguage::TypeScript)
        );
    }

    #[test]
    fn parse_ast_config_single_language() {
        let toml = r#"
[merge.ast]
languages = ["rust"]
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.merge.ast.languages.len(), 1);
        assert_eq!(cfg.merge.ast.languages[0], AstConfigLanguage::Rust);
    }

    #[test]
    fn parse_ast_config_ts_alias() {
        let toml = r#"
[merge.ast]
languages = ["ts"]
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.merge.ast.languages.len(), 1);
        assert_eq!(cfg.merge.ast.languages[0], AstConfigLanguage::TypeScript);
    }

    #[test]
    fn parse_ast_config_javascript_and_go() {
        let toml = r#"
[merge.ast]
languages = ["javascript", "go"]
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.merge.ast.languages.len(), 2);
        assert!(
            cfg.merge
                .ast
                .languages
                .contains(&AstConfigLanguage::JavaScript)
        );
        assert!(cfg.merge.ast.languages.contains(&AstConfigLanguage::Go));
    }

    #[test]
    fn parse_ast_config_packs_and_semantic_thresholds() {
        let toml = r#"
[merge.ast]
packs = ["core", "web"]
semantic_false_positive_budget_pct = 3
semantic_min_confidence = 80
"#;
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert_eq!(cfg.merge.ast.packs.len(), 2);
        assert!(cfg.merge.ast.packs.contains(&AstLanguagePack::Core));
        assert!(cfg.merge.ast.packs.contains(&AstLanguagePack::Web));
        assert_eq!(cfg.merge.ast.semantic_false_positive_budget_pct, 3);
        assert_eq!(cfg.merge.ast.semantic_min_confidence, 80);
    }

    #[test]
    fn parse_ast_config_empty_languages() {
        let toml = r"
[merge.ast]
languages = []
";
        let cfg = ManifoldConfig::parse(toml).unwrap();
        assert!(cfg.merge.ast.languages.is_empty());
    }

    #[test]
    fn parse_ast_config_rejects_unknown_language() {
        let toml = r#"
[merge.ast]
languages = ["cobol"]
"#;
        let err = ManifoldConfig::parse(toml).unwrap_err();
        assert!(
            err.message.contains("unknown variant"),
            "expected 'unknown variant' but got: {}",
            err.message
        );
    }
}
