use std::path::Path;
use std::process::Command;

use anyhow::Result;

/// Check system requirements and configuration
#[allow(clippy::unnecessary_wraps)]
pub fn run() -> Result<()> {
    println!("maw doctor");
    println!("==========");
    println!();

    let mut all_ok = true;

    // Check jj (required)
    all_ok &= check_tool(
        "jj",
        &["--version"],
        true,
        "https://martinvonz.github.io/jj/latest/install-and-setup/",
    );

    // Check if we're in a jj repo
    all_ok &= check_jj_repo();

    // Check botbus (optional)
    check_tool(
        "botbus",
        &["--version"],
        false,
        "https://github.com/anthropics/botbus",
    );

    // Check beads (optional)
    check_tool(
        "br",
        &["--version"],
        false,
        "https://github.com/Dicklesworthstone/beads_rust",
    );

    // Check .gitignore has .workspaces/
    all_ok &= check_gitignore();

    println!();
    if all_ok {
        println!("All required checks passed!");
    } else {
        println!("Some required checks failed. See above for details.");
    }

    Ok(())
}

fn check_tool(name: &str, args: &[&str], required: bool, install_url: &str) -> bool {
    let label = if required { "required" } else { "optional" };

    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version = version.lines().next().unwrap_or("unknown").trim();
            println!("[OK] {name} ({label}): {version}");
            true
        }
        Ok(_) => {
            println!("[FAIL] {name} ({label}): found but returned error");
            println!("       Install: {install_url}");
            !required
        }
        Err(_) => {
            if required {
                println!("[FAIL] {name} ({label}): not found");
                println!("       Install: {install_url}");
                false
            } else {
                println!("[SKIP] {name} ({label}): not found");
                println!("       Install: {install_url}");
                true
            }
        }
    }
}

fn check_gitignore() -> bool {
    let gitignore = Path::new(".gitignore");

    if !gitignore.exists() {
        println!("[FAIL] .gitignore: not found");
        println!("       Run: maw init");
        return false;
    }

    let Ok(content) = std::fs::read_to_string(gitignore) else {
        println!("[FAIL] .gitignore: could not read file");
        println!("       Check file permissions, then run: maw init");
        return false;
    };

    let has_entry = content.lines().any(|line| {
        let line = line.trim();
        line == ".workspaces"
            || line == ".workspaces/"
            || line == "/.workspaces"
            || line == "/.workspaces/"
    });

    if has_entry {
        println!("[OK] .gitignore: .workspaces/ is ignored");
        true
    } else {
        println!("[FAIL] .gitignore: .workspaces/ is NOT ignored");
        println!("       Run: maw init");
        false
    }
}

fn check_jj_repo() -> bool {
    let Ok(output) = Command::new("jj").args(["status"]).output() else {
        // jj not installed, already reported above
        return true;
    };

    if output.status.success() {
        println!("[OK] jj repository: found");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no jj repo") || stderr.contains("There is no jj repo") {
            println!("[WARN] jj repository: not in a jj repo");
            println!("       Run: jj git init");
        } else {
            println!(
                "[WARN] jj repository: {}",
                stderr.lines().next().unwrap_or("unknown error")
            );
        }
    }
    true
}
