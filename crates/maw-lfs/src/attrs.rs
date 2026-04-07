//! LFS attributes matcher — resolves `filter=lfs` for a repo-relative path.
//!
//! Reads `.gitattributes` files from the working directory (or from a git
//! tree, for checkout-time use) and answers "is this path LFS-tracked?"
//! following git's attribute precedence rules:
//!
//! - Within a single `.gitattributes` file, later patterns override earlier ones.
//! - `.gitattributes` in subdirectories override parent directories.

use std::fs;
use std::path::{Path, PathBuf};

use gix::bstr::BStr;
use gix::glob::pattern::{Case, Mode as PatternMode};
use gix::glob::{wildmatch, Pattern};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AttrsError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path} line {line}: {message}")]
    Parse {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

/// Per-line decision about the `filter` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterDecision {
    /// `filter=lfs` assigned.
    SetLfs,
    /// `filter=<other>` or `-filter` or `!filter`.
    NotLfs,
    /// Line doesn't mention `filter` at all.
    NoChange,
}

#[derive(Debug, Clone)]
struct Rule {
    pattern: Pattern,
    decision: FilterDecision,
}

/// One parsed `.gitattributes` file with its directory prefix.
#[derive(Debug, Clone)]
struct AttrsFile {
    /// Directory containing this file, relative to workdir, with trailing
    /// slash (or empty for the root file).
    dir_prefix: String,
    rules: Vec<Rule>,
}

/// Matches repo-relative paths against `filter=lfs` rules.
pub struct AttrsMatcher {
    /// In order from root → deepest.
    files: Vec<AttrsFile>,
}

impl AttrsMatcher {
    /// Empty matcher — nothing is LFS.
    pub fn empty() -> Self {
        Self { files: Vec::new() }
    }

    /// True if no `.gitattributes` files were loaded (no rules to match).
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Load all `.gitattributes` files under `workdir`.
    pub fn from_workdir(workdir: &Path) -> Result<Self, AttrsError> {
        let mut files = Vec::new();
        collect_attrs_files(workdir, workdir, &mut files)?;
        // Sort by depth: shortest prefix first (root), longest last.
        files.sort_by_key(|f| f.dir_prefix.matches('/').count());
        Ok(Self { files })
    }

    /// Build a matcher from pre-parsed file contents. Used when loading
    /// `.gitattributes` from a git tree (no working directory).
    ///
    /// Each entry is `(dir_prefix, file_contents)` where `dir_prefix` is
    /// the repo-relative directory containing the `.gitattributes`, with
    /// trailing slash (or empty string for the root).
    pub fn from_entries(entries: Vec<(String, Vec<u8>)>) -> Result<Self, AttrsError> {
        let mut files = Vec::new();
        for (dir_prefix, bytes) in entries {
            let rules = parse_rules(&bytes, &dir_prefix)?;
            files.push(AttrsFile { dir_prefix, rules });
        }
        files.sort_by_key(|f| f.dir_prefix.matches('/').count());
        Ok(Self { files })
    }

    /// Returns true if `filter=lfs` applies to the given repo-relative path
    /// (forward-slash separated, no leading slash).
    pub fn is_lfs(&self, rel_path: &str) -> bool {
        let mut current = false;
        for file in &self.files {
            // Only apply rules from files whose directory is an ancestor of the path.
            if !rel_path.starts_with(&file.dir_prefix) {
                continue;
            }
            let rel_to_file = &rel_path[file.dir_prefix.len()..];
            for rule in &file.rules {
                if rule.decision == FilterDecision::NoChange {
                    continue;
                }
                if pattern_matches(&rule.pattern, rel_to_file) {
                    current = matches!(rule.decision, FilterDecision::SetLfs);
                }
            }
        }
        current
    }
}

fn pattern_matches(pattern: &Pattern, rel_path: &str) -> bool {
    let bytes: &BStr = rel_path.as_bytes().into();
    let basename_pos = rel_path.rfind('/').map(|p| p + 1);
    pattern.matches_repo_relative_path(
        bytes,
        basename_pos,
        None, // is_dir unknown; caller knows it's a file usually
        Case::Sensitive,
        wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
    )
}

/// Recursively collect every `.gitattributes` file under `root`, skipping `.git`.
fn collect_attrs_files(
    workdir: &Path,
    dir: &Path,
    out: &mut Vec<AttrsFile>,
) -> Result<(), AttrsError> {
    let attrs_path = dir.join(".gitattributes");
    if attrs_path.is_file() {
        let bytes = fs::read(&attrs_path).map_err(|e| AttrsError::Io {
            path: attrs_path.clone(),
            source: e,
        })?;
        let dir_prefix = dir_prefix_for(workdir, dir);
        let rules = parse_rules(&bytes, &dir_prefix)?;
        out.push(AttrsFile { dir_prefix, rules });
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            collect_attrs_files(workdir, &path, out)?;
        }
    }
    Ok(())
}

fn dir_prefix_for(workdir: &Path, dir: &Path) -> String {
    if dir == workdir {
        return String::new();
    }
    let rel = dir
        .strip_prefix(workdir)
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");
    if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    }
}

fn parse_rules(bytes: &[u8], source_prefix: &str) -> Result<Vec<Rule>, AttrsError> {
    let mut rules = Vec::new();
    let mut line_no = 0usize;
    for line in bytes.split(|b| *b == b'\n') {
        line_no += 1;
        // Trim leading whitespace and skip comments/blank.
        let line = trim_line(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        // Split pattern from the rest of the attributes.
        let (pat_bytes, attrs_bytes) = split_pattern(line);
        // Skip macro declarations (`[attr]name ...`).
        if pat_bytes.starts_with(b"[attr]") {
            continue;
        }
        let pattern = match Pattern::from_bytes(pat_bytes) {
            Some(p) => p,
            None => continue,
        };
        if pattern.mode.contains(PatternMode::NEGATIVE) {
            // Gitattributes forbids negated patterns; skip to match git's behavior.
            return Err(AttrsError::Parse {
                path: PathBuf::from(format!("<{source_prefix}.gitattributes>")),
                line: line_no,
                message: "negated pattern not allowed in .gitattributes".to_string(),
            });
        }
        let decision = extract_filter_decision(attrs_bytes);
        rules.push(Rule { pattern, decision });
    }
    Ok(rules)
}

fn trim_line(line: &[u8]) -> &[u8] {
    // Strip trailing \r (CRLF tolerance) and leading spaces/tabs.
    let mut start = 0;
    while start < line.len() && (line[start] == b' ' || line[start] == b'\t') {
        start += 1;
    }
    let mut end = line.len();
    while end > start && (line[end - 1] == b'\r' || line[end - 1] == b' ' || line[end - 1] == b'\t')
    {
        end -= 1;
    }
    &line[start..end]
}

fn split_pattern(line: &[u8]) -> (&[u8], &[u8]) {
    // Quoted patterns not supported in MVP; match git's default path.
    for (i, &b) in line.iter().enumerate() {
        if b == b' ' || b == b'\t' {
            let pat = &line[..i];
            // Skip whitespace to find attrs start.
            let mut j = i;
            while j < line.len() && (line[j] == b' ' || line[j] == b'\t') {
                j += 1;
            }
            return (pat, &line[j..]);
        }
    }
    (line, &[])
}

fn extract_filter_decision(attrs: &[u8]) -> FilterDecision {
    // Attributes are whitespace-separated; a filter decision may appear as:
    //   filter=lfs        → SetLfs
    //   filter=<other>    → NotLfs
    //   -filter           → NotLfs
    //   !filter           → NotLfs (unspecified resets)
    // If multiple `filter` tokens appear, LAST wins.
    let mut decision = FilterDecision::NoChange;
    for token in attrs.split(|b| *b == b' ' || *b == b'\t') {
        if token.is_empty() {
            continue;
        }
        let (attr_name, assigned) = match token.iter().position(|b| *b == b'=') {
            Some(i) => (&token[..i], Some(&token[i + 1..])),
            None => (token, None),
        };
        let (name_bytes, is_reset) = match attr_name.first() {
            Some(b'-') => (&attr_name[1..], true),
            Some(b'!') => (&attr_name[1..], true),
            _ => (attr_name, false),
        };
        if name_bytes != b"filter" {
            continue;
        }
        decision = if is_reset {
            FilterDecision::NotLfs
        } else {
            match assigned {
                Some(v) if v == b"lfs" => FilterDecision::SetLfs,
                Some(_) => FilterDecision::NotLfs,
                None => FilterDecision::NotLfs, // bare `filter` — no value
            }
        };
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, content).unwrap();
        }
        dir
    }

    #[test]
    fn simple_pattern_matches() {
        let dir = tmp_repo_with(&[
            (".gitattributes", "assets/**/*.png filter=lfs diff=lfs merge=lfs -text\n"),
        ]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("assets/hero.png"));
        assert!(m.is_lfs("assets/sub/foo.png"));
        assert!(!m.is_lfs("assets/hero.jpg"));
        assert!(!m.is_lfs("src/main.rs"));
    }

    #[test]
    fn multiple_patterns() {
        let dir = tmp_repo_with(&[(
            ".gitattributes",
            "*.png filter=lfs\n*.ogg filter=lfs\n*.txt -text\n",
        )]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("music.ogg"));
        assert!(m.is_lfs("pic.png"));
        assert!(!m.is_lfs("notes.txt"));
    }

    #[test]
    fn later_pattern_overrides_earlier() {
        let dir = tmp_repo_with(&[(
            ".gitattributes",
            "*.png filter=lfs\nlogo.png -filter\n",
        )]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("hero.png"));
        assert!(!m.is_lfs("logo.png"));
    }

    #[test]
    fn nested_gitattributes_overrides_parent() {
        let dir = tmp_repo_with(&[
            (".gitattributes", "*.png filter=lfs\n"),
            ("assets/.gitattributes", "hero.png -filter\n"),
        ]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("foo.png"));
        assert!(m.is_lfs("assets/other.png"));
        assert!(!m.is_lfs("assets/hero.png"));
    }

    #[test]
    fn no_gitattributes_means_no_lfs() {
        let dir = tempfile::tempdir().unwrap();
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(!m.is_lfs("anything.png"));
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let dir = tmp_repo_with(&[(
            ".gitattributes",
            "# comment\n\n  # indented comment\n*.png filter=lfs\n\n",
        )]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("foo.png"));
    }

    #[test]
    fn filter_other_than_lfs_is_not_lfs() {
        let dir = tmp_repo_with(&[(".gitattributes", "*.png filter=other-lfs\n")]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(!m.is_lfs("foo.png"));
    }

    #[test]
    fn dash_filter_resets() {
        let dir = tmp_repo_with(&[(
            ".gitattributes",
            "assets/** filter=lfs\nassets/logo.png -filter\n",
        )]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("assets/hero.png"));
        assert!(!m.is_lfs("assets/logo.png"));
    }

    #[test]
    fn from_entries_no_workdir() {
        // Simulate loading from a tree.
        let entries = vec![
            ("".to_owned(), b"*.png filter=lfs\n".to_vec()),
            ("assets/".to_owned(), b"logo.png -filter\n".to_vec()),
        ];
        let m = AttrsMatcher::from_entries(entries).unwrap();
        assert!(m.is_lfs("assets/hero.png"));
        assert!(!m.is_lfs("assets/logo.png"));
        assert!(m.is_lfs("foo.png"));
    }

    #[test]
    fn empty_matcher() {
        let m = AttrsMatcher::empty();
        assert!(!m.is_lfs("anything.png"));
    }

    #[test]
    fn many_patterns_performance() {
        // Sanity-check: shouldn't be catastrophically slow.
        let mut content = String::new();
        for i in 0..50 {
            content.push_str(&format!("*.ext{i} filter=lfs\n"));
        }
        let dir = tmp_repo_with(&[(".gitattributes", &content)]);
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        let start = std::time::Instant::now();
        for i in 0..10_000 {
            m.is_lfs(&format!("file{i}.ext7"));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 500,
            "10k lookups × 50 patterns took {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod interop_tests {
    use super::*;

    #[test]
    fn matches_git_check_attr_ground_truth() {
        // Ground truth from `git check-attr filter` on this .gitattributes:
        //   assets/hero.png    → lfs
        //   assets/logo.png    → unset
        //   music.ogg          → lfs
        //   src/main.rs        → unspecified
        //   assets/sub/foo.png → lfs
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitattributes"),
            "assets/**/*.png filter=lfs diff=lfs merge=lfs -text\n\
             *.ogg filter=lfs\n\
             assets/logo.png -filter\n",
        )
        .unwrap();
        let m = AttrsMatcher::from_workdir(dir.path()).unwrap();
        assert!(m.is_lfs("assets/hero.png"));
        assert!(!m.is_lfs("assets/logo.png"));
        assert!(m.is_lfs("music.ogg"));
        assert!(!m.is_lfs("src/main.rs"));
        assert!(m.is_lfs("assets/sub/foo.png"));
    }
}

#[cfg(test)]
mod bare_repo_tests {
    use super::*;

    #[test]
    fn from_entries_assets_glob_star_star() {
        let entries = vec![
            ("".to_owned(), b"assets/**/*.bin filter=lfs diff=lfs merge=lfs -text\n*.dat filter=lfs\n".to_vec()),
        ];
        let m = AttrsMatcher::from_entries(entries).unwrap();
        assert!(m.is_lfs("assets/sprites/debug-test.bin"), "assets/**/*.bin should match");
        assert!(m.is_lfs("assets/hero.bin"), "assets/hero.bin should match");
        assert!(m.is_lfs("level.dat"), "*.dat should match");
        assert!(!m.is_lfs("src/main.rs"), "*.rs should not match");
    }
}
