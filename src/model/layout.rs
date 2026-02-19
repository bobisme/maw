//! Manifold directory layout and initialization.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Manifold metadata directory name.
pub const MANIFOLD_DIR: &str = ".manifold";

/// Subdirectory for epoch data.
pub const EPOCHS_DIR: &str = "epochs";

/// Subdirectory for artifacts.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Subdirectory for workspace artifacts.
pub const WS_ARTIFACTS_DIR: &str = "ws";

/// Subdirectory for merge artifacts.
pub const MERGE_ARTIFACTS_DIR: &str = "merge";

/// Config file name.
pub const CONFIG_FILE: &str = "config.toml";

/// Patterns to add to .gitignore.
pub const GITIGNORE_PATTERNS: &[&str] = &[
    "ws/",
    ".manifold/epochs/",
    ".manifold/cow/",
    ".manifold/artifacts/",
];

/// Initialize the .manifold directory structure and update .gitignore.
///
/// This function is idempotent:
/// - Missing directories are created.
/// - Existing directories are left alone.
/// - `config.toml` is created with defaults if missing.
/// - `.gitignore` is updated with necessary patterns if missing.
pub fn init_manifold_dir(root: &Path) -> io::Result<()> {
    let manifold_path = root.join(MANIFOLD_DIR);
    
    // Create directory structure
    create_dir_all_idempotent(&manifold_path)?;
    create_dir_all_idempotent(&manifold_path.join(EPOCHS_DIR))?;
    
    let artifacts_path = manifold_path.join(ARTIFACTS_DIR);
    create_dir_all_idempotent(&artifacts_path)?;
    create_dir_all_idempotent(&artifacts_path.join(WS_ARTIFACTS_DIR))?;
    create_dir_all_idempotent(&artifacts_path.join(MERGE_ARTIFACTS_DIR))?;

    // Initialize default config if missing
    init_config_if_missing(&manifold_path.join(CONFIG_FILE))?;

    // Update .gitignore
    update_gitignore(root)?;

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
        writeln!(file, "# For full options see: https://github.com/mariozechner/manifold")?;
        writeln!(file, "")?;
        writeln!(file, "[repo]")?;
        writeln!(file, "branch = \"main\"")?;
    }
    Ok(())
}

fn update_gitignore(root: &Path) -> io::Result<()> {
    let gitignore_path = root.join(".gitignore");
    let mut content = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    let existing_patterns: std::collections::HashSet<_> = content.lines().map(|l| l.trim()).collect();
    
    let mut patterns_to_add = Vec::new();
    for p in GITIGNORE_PATTERNS {
        if !existing_patterns.contains(p) {
            patterns_to_add.push(*p);
        }
    }

    if !patterns_to_add.is_empty() {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("\n# Manifold\n");
        for p in patterns_to_add {
            content.push_str(p);
            content.push('\n');
        }
        fs::write(gitignore_path, content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_init_manifold_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        init_manifold_dir(root).unwrap();

        assert!(root.join(".manifold").is_dir());
        assert!(root.join(".manifold/epochs").is_dir());
        assert!(root.join(".manifold/artifacts").is_dir());
        assert!(root.join(".manifold/artifacts/ws").is_dir());
        assert!(root.join(".manifold/artifacts/merge").is_dir());
        assert!(root.join(".manifold/config.toml").is_file());
        assert!(root.join(".gitignore").is_file());

        let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
        for pattern in GITIGNORE_PATTERNS {
            assert!(gitignore.contains(pattern));
        }
    }

    #[test]
    fn test_idempotency() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        init_manifold_dir(root).unwrap();
        let config_first = fs::read_to_string(root.join(".manifold/config.toml")).unwrap();
        let gitignore_first = fs::read_to_string(root.join(".gitignore")).unwrap();

        init_manifold_dir(root).unwrap();
        let config_second = fs::read_to_string(root.join(".manifold/config.toml")).unwrap();
        let gitignore_second = fs::read_to_string(root.join(".gitignore")).unwrap();

        assert_eq!(config_first, config_second);
        assert_eq!(gitignore_first, gitignore_second);
    }
}
