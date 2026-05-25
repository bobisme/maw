//! Manifold directory layout and initialization.
//!
//! Supports two flavors:
//!
//! - **`V2WsRoot`** (legacy, v2 bare-repo): `<root>/.manifold/` for metadata,
//!   `<root>/ws/<name>/` for workspaces, root is a bare repo with the
//!   privileged checkout at `<root>/ws/default/`.
//! - **`ConsolidatedMawDir`** (v1.0 default for new repos):
//!   `<root>/.maw/manifold/` for metadata, `<root>/.maw/workspaces/<name>/`
//!   for workspaces, `<root>/.maw/config.toml` for bootstrap config,
//!   `<root>/.maw/.gitignore` to track config.toml and ignore runtime
//!   subtrees, `<root>/.maw/cache/` reserved for future. The root itself
//!   is a normal checkout and IS the merge target / push source.
//!
//! Detection is presence-based: a `<root>/.maw/manifold/` directory selects
//! the consolidated layout; a `<root>/.manifold/` directory at repo root
//! selects v2. New `maw init` defaults to consolidated.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Legacy v2 Manifold metadata directory name (lives at `<root>/.manifold/`).
pub const MANIFOLD_DIR: &str = ".manifold";

/// New consolidated maw admin directory name (lives at `<root>/.maw/`).
pub const MAW_DIR: &str = ".maw";

/// Subdirectory of `.maw/` that holds the workspaces (formerly `<root>/ws/`).
pub const MAW_WORKSPACES_SUBDIR: &str = "workspaces";

/// Subdirectory of `.maw/` that holds the manifold metadata (formerly
/// `<root>/.manifold/`).
pub const MAW_MANIFOLD_SUBDIR: &str = "manifold";

/// Subdirectory of `.maw/` reserved for runtime caches.
pub const MAW_CACHE_SUBDIR: &str = "cache";

/// Bootstrap config filename inside the consolidated `.maw/`.
pub const MAW_CONFIG_FILE: &str = "config.toml";

/// Legacy v2 workspaces directory name (lives at `<root>/ws/`).
pub const V2_WORKSPACES_DIR: &str = "ws";

/// Subdirectory for epoch data.
pub const EPOCHS_DIR: &str = "epochs";

/// Subdirectory for artifacts.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Subdirectory for workspace artifacts.
pub const WS_ARTIFACTS_DIR: &str = "ws";

/// Subdirectory for merge artifacts.
pub const MERGE_ARTIFACTS_DIR: &str = "merge";

/// Config file name (inside the manifold dir).
pub const CONFIG_FILE: &str = "config.toml";

/// Which on-disk layout a Manifold repository uses.
///
/// All path derivations route through this enum so the rest of the codebase
/// is layout-agnostic by construction (T3.2 / bn-2sw3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LayoutFlavor {
    /// Legacy v2 bare-repo layout.
    ///
    /// - Repo root is bare (`core.bare = true`).
    /// - Privileged checkout at `<root>/ws/default/`.
    /// - Workspaces at `<root>/ws/<name>/`.
    /// - Manifold metadata at `<root>/.manifold/`.
    V2WsRoot,

    /// New consolidated `.maw/` layout (v1.0 default for new repos).
    ///
    /// - Repo root is a normal checkout; root IS the merge target.
    /// - Workspaces at `<root>/.maw/workspaces/<name>/`.
    /// - Manifold metadata at `<root>/.maw/manifold/`.
    /// - Bootstrap config at `<root>/.maw/config.toml`.
    ConsolidatedMawDir,
}

impl LayoutFlavor {
    /// Detect which layout a repo is using by looking at on-disk markers.
    ///
    /// Detection precedence:
    /// 1. `<root>/.maw/manifold/` exists → `ConsolidatedMawDir`.
    /// 2. `<root>/ws/default/` or `<root>/.manifold/` exists → `V2WsRoot`.
    /// 3. Otherwise default to `V2WsRoot` (the safer back-compat default —
    ///    everything currently on disk is v2; `maw init` will explicitly
    ///    create the consolidated `.maw/manifold/` marker for new repos
    ///    and detection will then flip to `ConsolidatedMawDir`).
    #[must_use]
    pub fn detect(root: &Path) -> Self {
        if root.join(MAW_DIR).join(MAW_MANIFOLD_SUBDIR).is_dir() {
            return Self::ConsolidatedMawDir;
        }
        Self::V2WsRoot
    }

    /// Detect the layout from an environment override or the on-disk markers.
    ///
    /// If `MAW_LAYOUT` is set to `v2` (case-insensitive), force the legacy
    /// layout. If set to `consolidated` or `maw`, force the new layout.
    /// Otherwise defer to [`Self::detect`].
    #[must_use]
    pub fn detect_with_env(root: &Path) -> Self {
        if let Ok(override_val) = std::env::var("MAW_LAYOUT") {
            match override_val.to_ascii_lowercase().as_str() {
                "v2" | "ws" | "legacy" => return Self::V2WsRoot,
                "consolidated" | "maw" | ".maw" | "v3" => return Self::ConsolidatedMawDir,
                _ => {}
            }
        }
        Self::detect(root)
    }

    /// Whether this layout uses a bare root (v2) vs a live checkout at root
    /// (consolidated).
    #[must_use]
    pub const fn root_is_bare(self) -> bool {
        matches!(self, Self::V2WsRoot)
    }

    /// Directory under `<root>` that holds individual workspace worktrees.
    ///
    /// - V2: `<root>/ws/`
    /// - Consolidated: `<root>/.maw/workspaces/`
    #[must_use]
    pub fn workspaces_dir(self, root: &Path) -> PathBuf {
        match self {
            Self::V2WsRoot => root.join(V2_WORKSPACES_DIR),
            Self::ConsolidatedMawDir => root.join(MAW_DIR).join(MAW_WORKSPACES_SUBDIR),
        }
    }

    /// Path to a specific workspace's worktree directory.
    #[must_use]
    pub fn workspace_path(self, root: &Path, name: &str) -> PathBuf {
        self.workspaces_dir(root).join(name)
    }

    /// Directory that holds Manifold metadata (epochs/, artifacts/,
    /// `config.toml`).
    ///
    /// - V2: `<root>/.manifold/`
    /// - Consolidated: `<root>/.maw/manifold/`
    #[must_use]
    pub fn manifold_dir(self, root: &Path) -> PathBuf {
        match self {
            Self::V2WsRoot => root.join(MANIFOLD_DIR),
            Self::ConsolidatedMawDir => root.join(MAW_DIR).join(MAW_MANIFOLD_SUBDIR),
        }
    }

    /// Resolve the privileged "default" target path used as the merge target
    /// and push source.
    ///
    /// - V2: `<root>/ws/<default_name>` (typically `ws/default`).
    /// - Consolidated: the repo root itself.
    #[must_use]
    pub fn default_target_path(self, root: &Path, default_name: &str) -> PathBuf {
        match self {
            Self::V2WsRoot => root.join(V2_WORKSPACES_DIR).join(default_name),
            Self::ConsolidatedMawDir => root.to_path_buf(),
        }
    }

    /// Where Manifold's bootstrap config (`config.toml`) is read from.
    ///
    /// - V2: `<root>/.manifold/config.toml`
    /// - Consolidated: `<root>/.maw/config.toml`
    #[must_use]
    pub fn bootstrap_config_path(self, root: &Path) -> PathBuf {
        match self {
            Self::V2WsRoot => root.join(MANIFOLD_DIR).join(CONFIG_FILE),
            Self::ConsolidatedMawDir => root.join(MAW_DIR).join(MAW_CONFIG_FILE),
        }
    }

    /// Where the user-editable `.maw.toml` lives (the file that contains
    /// `[repo]`, `[hooks]`, etc.).
    ///
    /// - V2: `<root>/.maw.toml` then `<root>/ws/default/.maw.toml`.
    /// - Consolidated: `<root>/.maw.toml` at the live root checkout (and
    ///   `.maw/config.toml` as a bootstrap fallback).
    ///
    /// Returns the first existing candidate, falling back to the canonical
    /// path even if it doesn't exist (caller decides whether missing means
    /// "use defaults").
    #[must_use]
    pub fn maw_toml_search_paths(self, root: &Path, default_name: &str) -> Vec<PathBuf> {
        match self {
            Self::V2WsRoot => vec![
                root.join(".maw.toml"),
                root.join(V2_WORKSPACES_DIR)
                    .join(default_name)
                    .join(".maw.toml"),
            ],
            Self::ConsolidatedMawDir => vec![
                root.join(".maw.toml"),
                root.join(MAW_DIR).join(MAW_CONFIG_FILE),
            ],
        }
    }

    /// The cache directory (consolidated only; v2 has none).
    #[must_use]
    pub fn cache_dir(self, root: &Path) -> Option<PathBuf> {
        match self {
            Self::V2WsRoot => None,
            Self::ConsolidatedMawDir => Some(root.join(MAW_DIR).join(MAW_CACHE_SUBDIR)),
        }
    }
}

/// Initialize the manifold metadata directory structure for the **legacy v2
/// layout** (`<root>/.manifold/`).
///
/// Kept for back-compat with brownfield init paths that target the v2 layout.
/// New init paths should use [`init_manifold_layout`] which dispatches on
/// [`LayoutFlavor`].
///
/// This function is idempotent.
#[allow(clippy::missing_errors_doc)]
pub fn init_manifold_dir(root: &Path) -> io::Result<()> {
    init_manifold_layout(root, LayoutFlavor::V2WsRoot)
}

/// Initialize the manifold metadata directory structure for the given
/// [`LayoutFlavor`].
///
/// For the consolidated layout, this also creates `.maw/.gitignore` (which
/// tracks `config.toml` and ignores runtime subtrees) and the reserved
/// `.maw/cache/` directory.
///
/// This function is idempotent.
#[allow(clippy::missing_errors_doc)]
pub fn init_manifold_layout(root: &Path, flavor: LayoutFlavor) -> io::Result<()> {
    let manifold_path = flavor.manifold_dir(root);

    // Create the manifold metadata tree.
    create_dir_all_idempotent(&manifold_path)?;
    create_dir_all_idempotent(&manifold_path.join(EPOCHS_DIR))?;

    let artifacts_path = manifold_path.join(ARTIFACTS_DIR);
    create_dir_all_idempotent(&artifacts_path)?;
    create_dir_all_idempotent(&artifacts_path.join(WS_ARTIFACTS_DIR))?;
    create_dir_all_idempotent(&artifacts_path.join(MERGE_ARTIFACTS_DIR))?;

    // Initialize default config if missing (manifold-side, for back-compat).
    init_config_if_missing(&manifold_path.join(CONFIG_FILE))?;

    // Consolidated-only: create .maw/.gitignore + .maw/config.toml +
    // .maw/cache/ + .maw/workspaces/.
    if flavor == LayoutFlavor::ConsolidatedMawDir {
        let maw_root = root.join(MAW_DIR);
        create_dir_all_idempotent(&maw_root)?;
        create_dir_all_idempotent(&maw_root.join(MAW_WORKSPACES_SUBDIR))?;
        create_dir_all_idempotent(&maw_root.join(MAW_CACHE_SUBDIR))?;
        init_maw_gitignore_if_missing(&maw_root.join(".gitignore"))?;
        init_maw_bootstrap_config_if_missing(&maw_root.join(MAW_CONFIG_FILE))?;
    }

    Ok(())
}

fn create_dir_all_idempotent(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn init_config_if_missing(path: &Path) -> io::Result<()> {
    if !path.exists() {
        let mut file = fs::File::create(path)?;
        writeln!(file, "# Manifold repository configuration")?;
        writeln!(
            file,
            "# For full options see: https://github.com/mariozechner/manifold"
        )?;
        writeln!(file)?;
        writeln!(file, "[repo]")?;
        writeln!(file, "branch = \"main\"")?;
    }
    Ok(())
}

/// Write `.maw/.gitignore` that tracks `config.toml` and ignores everything
/// else (runtime subtrees: `manifold/`, `cache/`, `workspaces/`).
///
/// The file is also itself tracked so the policy is checked into the repo
/// (per SP5 §6 risk #4 — migration parity).
fn init_maw_gitignore_if_missing(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut file = fs::File::create(path)?;
    writeln!(file, "# maw consolidated layout — runtime state under .maw/")?;
    writeln!(file, "# is never tracked; only this .gitignore and")?;
    writeln!(file, "# config.toml are.")?;
    writeln!(file, "*")?;
    writeln!(file, "!.gitignore")?;
    writeln!(file, "!{MAW_CONFIG_FILE}")?;
    Ok(())
}

/// Write `.maw/config.toml` — the bootstrap config for the consolidated
/// layout. Distinct from `.maw.toml` (the user-editable config that lives
/// at repo root in either layout).
fn init_maw_bootstrap_config_if_missing(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut file = fs::File::create(path)?;
    writeln!(file, "# maw bootstrap config (.maw/config.toml)")?;
    writeln!(
        file,
        "# Fixed location; consulted by `maw` before any other config."
    )?;
    writeln!(
        file,
        "# Mirror your repository defaults here; user-editable settings"
    )?;
    writeln!(file, "# also live at <root>/.maw.toml.")?;
    writeln!(file)?;
    writeln!(file, "[repo]")?;
    writeln!(file, "branch = \"main\"")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detect_v2_layout_via_manifold_dir() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();
        fs::create_dir_all(root.join(".manifold")).expect("operation should succeed");
        assert_eq!(LayoutFlavor::detect(root), LayoutFlavor::V2WsRoot);
    }

    #[test]
    fn detect_consolidated_layout_via_maw_manifold_dir() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();
        fs::create_dir_all(root.join(".maw").join("manifold")).expect("operation should succeed");
        assert_eq!(
            LayoutFlavor::detect(root),
            LayoutFlavor::ConsolidatedMawDir
        );
    }

    #[test]
    fn detect_prefers_maw_dir_when_both_present() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();
        fs::create_dir_all(root.join(".manifold")).expect("operation should succeed");
        fs::create_dir_all(root.join(".maw").join("manifold")).expect("operation should succeed");
        // The consolidated layout takes precedence (a fully-migrated repo
        // may still have a residual .manifold/ artifact briefly).
        assert_eq!(
            LayoutFlavor::detect(root),
            LayoutFlavor::ConsolidatedMawDir
        );
    }

    #[test]
    fn detect_defaults_to_v2_in_empty_dir() {
        // V2 is the safer back-compat default. Greenfield `maw init` will
        // create the consolidated .maw/manifold/ marker explicitly, after
        // which detection flips. Empty dirs that haven't yet been
        // initialised report v2 so tests that pre-existed the layout split
        // (and never created .manifold/) keep their original semantics.
        let dir = tempdir().expect("operation should succeed");
        assert_eq!(LayoutFlavor::detect(dir.path()), LayoutFlavor::V2WsRoot);
    }

    #[test]
    fn paths_for_v2_layout() {
        let root = Path::new("/repo");
        let f = LayoutFlavor::V2WsRoot;
        assert_eq!(f.workspaces_dir(root), Path::new("/repo/ws"));
        assert_eq!(f.workspace_path(root, "alice"), Path::new("/repo/ws/alice"));
        assert_eq!(f.manifold_dir(root), Path::new("/repo/.manifold"));
        assert_eq!(
            f.default_target_path(root, "default"),
            Path::new("/repo/ws/default")
        );
        assert_eq!(
            f.bootstrap_config_path(root),
            Path::new("/repo/.manifold/config.toml")
        );
        assert_eq!(f.cache_dir(root), None);
        assert!(f.root_is_bare());
    }

    #[test]
    fn paths_for_consolidated_layout() {
        let root = Path::new("/repo");
        let f = LayoutFlavor::ConsolidatedMawDir;
        assert_eq!(f.workspaces_dir(root), Path::new("/repo/.maw/workspaces"));
        assert_eq!(
            f.workspace_path(root, "alice"),
            Path::new("/repo/.maw/workspaces/alice")
        );
        assert_eq!(f.manifold_dir(root), Path::new("/repo/.maw/manifold"));
        assert_eq!(f.default_target_path(root, "default"), Path::new("/repo"));
        assert_eq!(
            f.bootstrap_config_path(root),
            Path::new("/repo/.maw/config.toml")
        );
        assert_eq!(f.cache_dir(root), Some(PathBuf::from("/repo/.maw/cache")));
        assert!(!f.root_is_bare());
    }

    #[test]
    fn init_consolidated_creates_full_layout() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();

        init_manifold_layout(root, LayoutFlavor::ConsolidatedMawDir)
            .expect("operation should succeed");

        // .maw/ tree
        assert!(root.join(".maw").is_dir());
        assert!(root.join(".maw/manifold").is_dir());
        assert!(root.join(".maw/manifold/epochs").is_dir());
        assert!(root.join(".maw/manifold/artifacts").is_dir());
        assert!(root.join(".maw/manifold/artifacts/ws").is_dir());
        assert!(root.join(".maw/manifold/artifacts/merge").is_dir());
        assert!(root.join(".maw/manifold/config.toml").is_file());
        assert!(root.join(".maw/workspaces").is_dir());
        assert!(root.join(".maw/cache").is_dir());
        assert!(root.join(".maw/.gitignore").is_file());
        assert!(root.join(".maw/config.toml").is_file());

        // Detection now sees consolidated.
        assert_eq!(
            LayoutFlavor::detect(root),
            LayoutFlavor::ConsolidatedMawDir
        );

        // .gitignore content has the right ignore policy.
        let gitignore =
            fs::read_to_string(root.join(".maw/.gitignore")).expect("operation should succeed");
        assert!(gitignore.contains('*'));
        assert!(gitignore.contains("!.gitignore"));
        assert!(gitignore.contains("!config.toml"));
    }

    #[test]
    fn init_v2_layout_back_compat() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();

        init_manifold_dir(root).expect("operation should succeed");

        assert!(root.join(".manifold").is_dir());
        assert!(root.join(".manifold/epochs").is_dir());
        assert!(root.join(".manifold/artifacts").is_dir());
        assert!(root.join(".manifold/artifacts/ws").is_dir());
        assert!(root.join(".manifold/artifacts/merge").is_dir());
        assert!(root.join(".manifold/config.toml").is_file());
        assert!(!root.join(".gitignore").exists());
        // V2 init does NOT create the consolidated .maw/ tree.
        assert!(!root.join(".maw").exists());
    }

    #[test]
    fn test_idempotency() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();

        init_manifold_dir(root).expect("operation should succeed");
        let config_first = fs::read_to_string(root.join(".manifold/config.toml"))
            .expect("operation should succeed");

        init_manifold_dir(root).expect("operation should succeed");
        let config_second = fs::read_to_string(root.join(".manifold/config.toml"))
            .expect("operation should succeed");

        assert_eq!(config_first, config_second);
    }

    #[test]
    fn idempotency_consolidated() {
        let dir = tempdir().expect("operation should succeed");
        let root = dir.path();
        init_manifold_layout(root, LayoutFlavor::ConsolidatedMawDir)
            .expect("operation should succeed");
        init_manifold_layout(root, LayoutFlavor::ConsolidatedMawDir)
            .expect("operation should succeed");
        assert_eq!(
            LayoutFlavor::detect(root),
            LayoutFlavor::ConsolidatedMawDir
        );
    }

    // Env-override behaviour is exercised through `detect_with_env`; we
    // intentionally avoid setting MAW_LAYOUT in tests because the codebase
    // forbids `unsafe` (deny(unsafe_code)) and mutating env vars in tests
    // is racy across the workspace anyway. The detection precedence rules
    // are covered by the on-disk tests above.
}
