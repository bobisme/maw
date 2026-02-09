use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde::Serialize;

use crate::format::OutputFormat;
use crate::workspace;

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
        println!("       {}", fix);
    }
}

/// Check system requirements and configuration
#[allow(clippy::unnecessary_wraps)]
pub fn run(format: Option<OutputFormat>) -> Result<()> {
    let format = OutputFormat::resolve(format);
    let mut checks = Vec::new();

    // Check jj (required)
    checks.push(check_tool(
        "jj",
        &["--version"],
        "https://martinvonz.github.io/jj/latest/install-and-setup/",
    ));

    // Find repo root and jj cwd (best-effort — may fail if not in a repo)
    let root = workspace::repo_root().ok();
    let cwd = workspace::jj_cwd().ok();

    // Check jj repo — uses jj_cwd() to avoid stale errors at bare root
    checks.push(check_jj_repo(cwd.as_deref()));

    // Check ws/default/ exists and looks correct
    checks.push(check_default_workspace(root.as_deref()));

    // Check repo root is bare (no source files leaked)
    checks.push(check_root_bare(root.as_deref()));

    // Check for ghost working copy at root (causes root pollution)
    checks.push(check_ghost_working_copy(root.as_deref()));

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

/// Check if we're in a jj repo. Uses `jj_cwd()` to avoid stale errors at bare root.
fn check_jj_repo(cwd: Option<&Path>) -> DoctorCheck {
    let cwd = cwd.unwrap_or(Path::new("."));

    let Ok(output) = Command::new("jj").args(["status"]).current_dir(cwd).output() else {
        // jj not installed — already reported by check_tool
        return DoctorCheck {
            name: "jj repository".to_string(),
            status: "ok".to_string(),
            message: "jj repository: skipped (jj not available)".to_string(),
            fix: None,
        };
    };

    if output.status.success() {
        DoctorCheck {
            name: "jj repository".to_string(),
            status: "ok".to_string(),
            message: "jj repository: found".to_string(),
            fix: None,
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no jj repo") || stderr.contains("There is no jj repo") {
            DoctorCheck {
                name: "jj repository".to_string(),
                status: "fail".to_string(),
                message: "jj repository: not in a jj repo".to_string(),
                fix: Some("Run: maw init".to_string()),
            }
        } else {
            DoctorCheck {
                name: "jj repository".to_string(),
                status: "warn".to_string(),
                message: format!(
                    "jj repository: {}",
                    stderr.lines().next().unwrap_or("unknown error")
                ),
                fix: None,
            }
        }
    }
}

/// Check that ws/default/ exists and contains the expected repo structure.
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

    // Check that it has a .gitignore with ws/ entry
    let gitignore = default_ws.join(".gitignore");
    if gitignore.exists()
        && let Ok(content) = std::fs::read_to_string(&gitignore) {
            let has_ws = content
                .lines()
                .any(|l| matches!(l.trim(), "ws" | "ws/" | "/ws" | "/ws/"));
            if !has_ws {
                return DoctorCheck {
                    name: "default workspace".to_string(),
                    status: "warn".to_string(),
                    message: "default workspace: .gitignore missing ws/ entry".to_string(),
                    fix: Some("Run: maw init".to_string()),
                };
            }
        }

    // Check that source files exist (not an empty workspace)
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

/// Check that the repo root is bare — only .git/, .jj/, ws/ allowed.
/// Source files at root indicate a corrupted v2 layout.
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
        let files_list = stray.join(", ");
        DoctorCheck {
            name: "repo root".to_string(),
            status: "fail".to_string(),
            message: format!(
                "repo root: {} unexpected file(s)/dir(s) — should be bare: {}",
                stray.len(),
                files_list
            ),
            fix: Some("Fix: maw init (moves files into ws/default/)".to_string()),
        }
    }
}

/// Non-dotfile entries allowed at the bare repo root.
/// Dotfiles/dotdirs (`.git`, `.jj`, `.claude`, `.pi`, etc.) are always allowed.
/// AGENTS.md and CLAUDE.md are redirect stubs pointing into ws/default/.
const BARE_ROOT_ALLOWED: &[&str] = &["ws", "AGENTS.md", "CLAUDE.md"];

/// Check for ghost .jj/working_copy/ at root that causes file materialization.
/// After `jj workspace forget`, jj leaves behind working copy metadata on disk.
/// If any jj command runs from root, jj sees the stale metadata and materializes
/// files into root — polluting the bare repo.
fn check_ghost_working_copy(root: Option<&Path>) -> DoctorCheck {
    let Some(root) = root else {
        return DoctorCheck {
            name: "ghost working copy".to_string(),
            status: "ok".to_string(),
            message: "ghost working copy: could not check (no root)".to_string(),
            fix: None,
        };
    };

    let ghost_wc = root.join(".jj").join("working_copy");
    if ghost_wc.exists() {
        DoctorCheck {
            name: "ghost working copy".to_string(),
            status: "fail".to_string(),
            message: "ghost working copy: .jj/working_copy/ exists at root (causes file leaks)".to_string(),
            fix: Some("Fix: rm -rf .jj/working_copy/  (or run: maw init)".to_string()),
        }
    } else {
        DoctorCheck {
            name: "ghost working copy".to_string(),
            status: "ok".to_string(),
            message: "ghost working copy: none (root has no working copy metadata)".to_string(),
            fix: None,
        }
    }
}

/// Return names of files/dirs at root that shouldn't be there.
/// Dotfiles/dotdirs are always allowed (agent config, VCS internals).
/// Source files (src/, Cargo.toml, etc.) indicate a corrupted v2 layout.
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
