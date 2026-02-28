use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde::Serialize;

use crate::format::OutputFormat;
use crate::workspace;

// ---------------------------------------------------------------------------
// Git version check
// ---------------------------------------------------------------------------

/// Minimum supported git version. Features used by maw (e.g. `git worktree`
/// improvements, `--orphan` flag) require at least this version.
const MIN_GIT_VERSION: (u32, u32, u32) = (2, 40, 0);

/// Parse a git version string like "git version 2.47.1" into (major, minor, patch).
///
/// Tolerates extra suffixes (e.g. "2.47.1.windows.1" or "2.39.3 (Apple Git-146)").
fn parse_git_version(version_output: &str) -> Option<(u32, u32, u32)> {
    // Expect first line to start with "git version "
    let line = version_output.lines().next()?;
    let version_str = line.strip_prefix("git version ")?;

    // Split on '.' and parse up to 3 numeric components
    let mut parts = version_str.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    // Patch may contain extra suffixes (e.g. "1 (Apple Git-146)") — take digits only
    let patch: u32 = parts
        .next()
        .and_then(|s| {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);

    Some((major, minor, patch))
}

/// Get the installed git version by running `git --version`.
fn get_git_version() -> Option<(u32, u32, u32)> {
    let output = Command::new("git").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_git_version(&stdout)
}

/// Emit a warning to stderr if the installed git version is below the minimum.
///
/// This is a no-op if git is not found or the version is at or above the minimum.
/// Intended to be called from `maw init` and other entry points as a soft check.
pub fn warn_git_version_if_old() {
    if let Some(version) = get_git_version() {
        if version < MIN_GIT_VERSION {
            eprintln!(
                "WARNING: git {}.{}.{} detected; maw requires git {}.{}.{} or later. \
                 Some features may not work correctly.\n  \
                 Upgrade: https://git-scm.com/downloads",
                version.0,
                version.1,
                version.2,
                MIN_GIT_VERSION.0,
                MIN_GIT_VERSION.1,
                MIN_GIT_VERSION.2,
            );
        }
    }
}

#[derive(Serialize)]
struct DoctorEnvelope {
    checks: Vec<DoctorCheck>,
    all_ok: bool,
    advice: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct DoctorCheck {
    name: String,
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
}

fn print_check(check: &DoctorCheck) {
    let prefix = match check.status.as_str() {
        "ok" => "[OK]",
        "warn" => "[WARN]",
        "fail" => "[FAIL]",
        _ => "[???]",
    };
    println!("{} {}", prefix, check.message);
    if let Some(fix) = &check.fix {
        println!("       {fix}");
    }
}

#[allow(clippy::unnecessary_wraps)]
pub fn run(format: Option<OutputFormat>) -> Result<()> {
    let format = OutputFormat::resolve(format);
    let mut checks = Vec::new();

    checks.push(check_tool(
        "git",
        &["--version"],
        "https://git-scm.com/downloads",
    ));
    checks.push(check_git_version());

    let root = workspace::repo_root().ok();

    checks.push(check_manifold_initialized(root.as_deref()));
    checks.push(check_default_workspace(root.as_deref()));
    checks.push(check_root_bare(root.as_deref()));
    checks.push(check_ghost_working_copy(root.as_deref()));
    checks.push(check_git_head());

    let all_ok = checks.iter().all(|c| c.status == "ok");

    match format {
        OutputFormat::Json => {
            let envelope = DoctorEnvelope {
                checks,
                all_ok,
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text | OutputFormat::Pretty => {
            println!("maw doctor");
            println!("==========");
            println!();

            for check in &checks {
                print_check(check);
            }

            println!();
            if all_ok {
                println!("All checks passed!");
            } else {
                println!("Some checks failed. See above for details.");
            }
        }
    }

    Ok(())
}

fn check_tool(name: &str, args: &[&str], install_url: &str) -> DoctorCheck {
    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version = version.lines().next().unwrap_or("unknown").trim();
            DoctorCheck {
                name: name.to_string(),
                status: "ok".to_string(),
                message: format!("{name}: {version}"),
                fix: None,
            }
        }
        Ok(_) => DoctorCheck {
            name: name.to_string(),
            status: "fail".to_string(),
            message: format!("{name}: found but returned error"),
            fix: Some(format!("Install: {install_url}")),
        },
        Err(_) => DoctorCheck {
            name: name.to_string(),
            status: "fail".to_string(),
            message: format!("{name}: not found"),
            fix: Some(format!("Install: {install_url}")),
        },
    }
}

fn check_git_version() -> DoctorCheck {
    match get_git_version() {
        Some(version) if version >= MIN_GIT_VERSION => DoctorCheck {
            name: "git version".to_string(),
            status: "ok".to_string(),
            message: format!("git version: {}.{}.{} (>= {}.{}.{})", version.0, version.1, version.2, MIN_GIT_VERSION.0, MIN_GIT_VERSION.1, MIN_GIT_VERSION.2),
            fix: None,
        },
        Some(version) => DoctorCheck {
            name: "git version".to_string(),
            status: "warn".to_string(),
            message: format!(
                "git version: {}.{}.{} (minimum {}.{}.{} recommended)",
                version.0, version.1, version.2, MIN_GIT_VERSION.0, MIN_GIT_VERSION.1, MIN_GIT_VERSION.2
            ),
            fix: Some("Upgrade: https://git-scm.com/downloads".to_string()),
        },
        None => DoctorCheck {
            name: "git version".to_string(),
            status: "warn".to_string(),
            message: "git version: could not determine version".to_string(),
            fix: Some("Ensure git is installed: https://git-scm.com/downloads".to_string()),
        },
    }
}

fn check_manifold_initialized(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "manifold metadata".to_string(),
            status: "warn".to_string(),
            message: "manifold metadata: could not determine repo root".to_string(),
            fix: None,
        };
    };

    if root.join(".manifold").exists() {
        DoctorCheck {
            name: "manifold metadata".to_string(),
            status: "ok".to_string(),
            message: "manifold metadata: .manifold/ exists".to_string(),
            fix: None,
        }
    } else {
        DoctorCheck {
            name: "manifold metadata".to_string(),
            status: "fail".to_string(),
            message: "manifold metadata: .manifold/ is missing".to_string(),
            fix: Some("Run: maw init".to_string()),
        }
    }
}

fn check_default_workspace(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "warn".to_string(),
            message: "default workspace: could not determine repo root".to_string(),
            fix: None,
        };
    };

    let default_ws = root.join("ws").join("default");
    if !default_ws.exists() {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "fail".to_string(),
            message: "default workspace: ws/default/ does not exist".to_string(),
            fix: Some("Run: maw init".to_string()),
        };
    }

    if !is_valid_default_worktree(root, &default_ws) {
        return DoctorCheck {
            name: "default workspace".to_string(),
            status: "fail".to_string(),
            message: "default workspace: ws/default/ exists but is not a registered git worktree"
                .to_string(),
            fix: Some("Fix: maw init (repairs default workspace registration)".to_string()),
        };
    }

    let has_files = std::fs::read_dir(&default_ws)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(false);

    if has_files {
        DoctorCheck {
            name: "default workspace".to_string(),
            status: "ok".to_string(),
            message: "default workspace: ws/default/ exists with source files".to_string(),
            fix: None,
        }
    } else {
        DoctorCheck {
            name: "default workspace".to_string(),
            status: "warn".to_string(),
            message: "default workspace: ws/default/ exists but appears empty".to_string(),
            fix: Some("Run: maw init".to_string()),
        }
    }
}

fn is_valid_default_worktree(root: &Path, default_ws: &Path) -> bool {
    if !is_inside_worktree(default_ws) {
        return false;
    }

    is_registered_worktree(root, default_ws)
}

fn is_inside_worktree(path: &Path) -> bool {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output();

    let Ok(output) = output else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    String::from_utf8_lossy(&output.stdout).trim() == "true"
}

fn is_registered_worktree(root: &Path, ws_path: &Path) -> bool {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(root)
        .output();

    let Ok(output) = output else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    let ws_path = std::fs::canonicalize(ws_path).unwrap_or_else(|_| ws_path.to_path_buf());
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| {
        let Some(path) = line.strip_prefix("worktree ") else {
            return false;
        };
        let listed = Path::new(path.trim());
        let listed = std::fs::canonicalize(listed).unwrap_or_else(|_| listed.to_path_buf());
        listed == ws_path
    })
}

fn check_root_bare(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "repo root".to_string(),
            status: "ok".to_string(),
            message: "repo root: could not check (no root)".to_string(),
            fix: None,
        };
    };

    let stray = stray_root_entries(root);
    if stray.is_empty() {
        DoctorCheck {
            name: "repo root".to_string(),
            status: "ok".to_string(),
            message: "repo root: bare (no source files)".to_string(),
            fix: None,
        }
    } else {
        DoctorCheck {
            name: "repo root".to_string(),
            status: "fail".to_string(),
            message: format!(
                "repo root: {} unexpected file(s)/dir(s) — should be bare: {}",
                stray.len(),
                stray.join(", ")
            ),
            fix: Some("Fix: maw init (moves tracked files) or move/remove manually".to_string()),
        }
    }
}

const BARE_ROOT_ALLOWED: &[&str] = &["ws", "repo.git", "AGENTS.md", "CLAUDE.md", "GEMINI.md"];

fn check_ghost_working_copy(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "legacy jj metadata".to_string(),
            status: "ok".to_string(),
            message: "legacy jj metadata: could not check (no root)".to_string(),
            fix: None,
        };
    };

    let ghost_wc = root.join(".jj").join("working_copy");
    if ghost_wc.exists() {
        DoctorCheck {
            name: "legacy jj metadata".to_string(),
            status: "warn".to_string(),
            message: "legacy jj metadata: .jj/working_copy/ exists at repo root".to_string(),
            fix: Some("Migration cleanup: rm -rf .jj/working_copy/".to_string()),
        }
    } else {
        DoctorCheck {
            name: "legacy jj metadata".to_string(),
            status: "ok".to_string(),
            message: "legacy jj metadata: none".to_string(),
            fix: None,
        }
    }
}

pub fn stray_root_entries(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || BARE_ROOT_ALLOWED.contains(&name.as_str()) {
                None
            } else {
                Some(name)
            }
        })
        .collect()
}

fn check_git_head() -> DoctorCheck {
    let output = Command::new("git").args(["symbolic-ref", "HEAD"]).output();

    match output {
        Ok(out) if out.status.success() => {
            let head_ref = String::from_utf8_lossy(&out.stdout);
            DoctorCheck {
                name: "git HEAD".to_string(),
                status: "ok".to_string(),
                message: format!("git HEAD: {}", head_ref.trim()),
                fix: None,
            }
        }
        _ => {
            let root = crate::workspace::repo_root().unwrap_or_else(|_| ".".into());
            let branch = crate::workspace::MawConfig::load(&root)
                .map_or_else(|_| "main".to_string(), |c| c.branch().to_string());
            DoctorCheck {
                name: "git HEAD".to_string(),
                status: "fail".to_string(),
                message: "git HEAD: detached (git log may be stale)".to_string(),
                fix: Some(format!(
                    "Fix: git symbolic-ref HEAD refs/heads/{branch}  (or run: maw init)"
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_git_version() {
        assert_eq!(
            parse_git_version("git version 2.47.1"),
            Some((2, 47, 1))
        );
    }

    #[test]
    fn parse_git_version_two_components() {
        // Some distributions emit "git version 2.40" with no patch
        assert_eq!(
            parse_git_version("git version 2.40"),
            Some((2, 40, 0))
        );
    }

    #[test]
    fn parse_git_version_with_suffix() {
        // macOS Apple Git
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parse_git_version_windows() {
        assert_eq!(
            parse_git_version("git version 2.43.0.windows.1"),
            Some((2, 43, 0))
        );
    }

    #[test]
    fn parse_git_version_multiline() {
        // Only the first line matters
        assert_eq!(
            parse_git_version("git version 2.45.2\nsome extra info"),
            Some((2, 45, 2))
        );
    }

    #[test]
    fn parse_git_version_invalid() {
        assert_eq!(parse_git_version("not git output"), None);
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("git version "), None);
        assert_eq!(parse_git_version("git version abc.def.ghi"), None);
    }

    #[test]
    fn version_comparison_at_minimum() {
        let v = (2, 40, 0);
        assert!(v >= MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_above_minimum() {
        let v = (2, 47, 1);
        assert!(v >= MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_below_minimum() {
        let v = (2, 39, 3);
        assert!(v < MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_major_below() {
        let v = (1, 99, 99);
        assert!(v < MIN_GIT_VERSION);
    }

    #[test]
    fn version_comparison_major_above() {
        let v = (3, 0, 0);
        assert!(v >= MIN_GIT_VERSION);
    }
}
