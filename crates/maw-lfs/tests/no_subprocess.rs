//! Static guard: maw-lfs must never invoke `git` or `git-lfs` as a
//! subprocess. The crate exists specifically to replace those binaries
//! in maw's hot paths. This test greps the source tree for
//! `Command::new("git"` and `Command::new("git-lfs"` and fails if found.

use std::fs;
use std::path::{Path, PathBuf};

fn rs_files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("operation should succeed") {
        let entry = entry.expect("operation should succeed");
        let path = entry.path();
        let meta = entry.metadata().expect("operation should succeed");
        if meta.is_dir() {
            // Skip common build/output dirs.
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "target" || name.starts_with('.') {
                continue;
            }
            rs_files_under(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_git_subprocess_in_lfs_code() {
    // CARGO_MANIFEST_DIR = crates/maw-lfs. Scan its src/ tree only.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("src");
    assert!(src.is_dir(), "expected src dir at {src:?}");

    let mut files = Vec::new();
    rs_files_under(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files scanned");

    let forbidden = [
        "Command::new(\"git\"",
        "Command::new(\"git-lfs\"",
        "Command::new(\"/usr/bin/git\"",
        "Command::new(\"/usr/bin/git-lfs\"",
    ];

    let mut offenses: Vec<String> = Vec::new();
    for file in &files {
        let contents = fs::read_to_string(file).expect("operation should succeed");
        for (lineno, line) in contents.lines().enumerate() {
            // Strip // comments — a doc mention of "Command::new" is fine.
            let code = line.find("//").map_or(line, |pos| &line[..pos]);
            for pat in &forbidden {
                if code.contains(pat) {
                    offenses.push(format!(
                        "{}:{}: {}",
                        file.display(),
                        lineno + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        offenses.is_empty(),
        "maw-lfs source invokes git/git-lfs as a subprocess:\n{}",
        offenses.join("\n")
    );
}
