//! CI lane-wiring meta-test (bn-1n2b).
//!
//! GOAL: make it impossible for a defined CI *gate* recipe to silently stop
//! running. bn-1nmh: the `--features bench` build rotted for a full release
//! cycle because no workflow compiled it. The same day, `sg1-assurance-clippy`
//! turned out to have the identical defect — defined in the Justfile,
//! commented as "a dedicated gate", wired into NO workflow.
//!
//! CONVENTION (documented at the top of the Justfile): any recipe whose name
//! ends with `-clippy` or `-check` (a hyphenated suffix — bare `clippy`/
//! `check` don't count; this repo has no general build+test+lint CI
//! workflow, see the Justfile header) is a "gate recipe" and must be either:
//!   (a) referenced as `just <recipe-name>` from at least one file under
//!       `.github/workflows/*.yml` ("wired"), OR
//!   (b) opted out with a `# ci: local-only` marker comment in the
//!       contiguous comment block directly above the recipe.
//!
//! Parsing is textual/grep-level (no `just`/yaml crate dependency) — see
//! `tests/contract_drift.rs` for the established idiom in this repo.
//!
//! Run with: `cargo test --test ci_lane_wiring` (also runs as part of
//! `just check` / `just test`, so it can never silently stop running either).

use std::fs;
use std::path::PathBuf;

/// Find the project root by walking up from the test binary's manifest dir.
fn project_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(manifest)
}

/// A gate recipe parsed from the Justfile: its name, the 1-based line number
/// it's defined on (for diagnostics), and whether it carries the
/// `# ci: local-only` opt-out marker.
#[derive(Debug, Clone)]
struct GateRecipe {
    name: String,
    line: usize,
    local_only: bool,
}

/// Suffixes that mark a Justfile recipe as a "gate recipe" per the
/// bn-1n2b convention. Must be a hyphenated suffix — bare `clippy`/`check`
/// (no leading hyphen) are ordinary local dev-workflow recipes, not gates.
const GATE_SUFFIXES: &[&str] = &["-clippy", "-check"];

/// Extract the recipe name from a Justfile header line (column 0, not a
/// comment, contains a top-level `:`), e.g. `sg2-report dir *flags:` -> `sg2-report`.
fn recipe_name_from_header(line: &str) -> Option<&str> {
    let colon_idx = line.find(':')?;
    let before_colon = &line[..colon_idx];
    let name = before_colon.split_whitespace().next()?;
    if name.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name)
}

/// True if `line` is a Justfile recipe *header* line: not indented (recipe
/// bodies in this Justfile are always indented), not blank, not a comment.
fn is_header_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with(char::is_whitespace) && !line.starts_with('#')
}

/// True if `comment_line` (already known to start with `#`) carries the
/// `# ci: local-only` opt-out marker.
fn is_local_only_marker(comment_line: &str) -> bool {
    comment_line
        .trim_start()
        .trim_start_matches('#')
        .trim_start()
        .starts_with("ci: local-only")
}

/// Parse every gate recipe (`*-clippy` / `*-check`) out of the Justfile,
/// deciding `local_only` from the contiguous `#`-comment block immediately
/// preceding each recipe's header line.
fn parse_gate_recipes(justfile: &str) -> Vec<GateRecipe> {
    let lines: Vec<&str> = justfile.lines().collect();
    let mut gates = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if !is_header_line(line) {
            continue;
        }
        let Some(name) = recipe_name_from_header(line) else {
            continue;
        };
        if !GATE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
            continue;
        }

        // Walk upward collecting the contiguous block of comment lines
        // directly above this header (no blank-line break).
        let mut local_only = false;
        let mut j = i;
        while j > 0 {
            let prev = lines[j - 1];
            if prev.starts_with('#') {
                if is_local_only_marker(prev) {
                    local_only = true;
                }
                j -= 1;
            } else {
                break;
            }
        }

        gates.push(GateRecipe {
            name: name.to_owned(),
            line: i + 1,
            local_only,
        });
    }

    gates
}

/// Collect the text of every `.github/workflows/*.yml` file.
fn collect_workflow_text(root: &std::path::Path) -> String {
    let dir = root.join(".github/workflows");
    let mut combined = String::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return combined;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "yml" || e == "yaml")
            && let Ok(content) = fs::read_to_string(&path)
        {
            combined.push_str(&content);
            combined.push('\n');
        }
    }
    combined
}

/// True if `workflow_text` invokes `just <recipe_name>` as a whole recipe
/// name (not merely as a prefix of a longer recipe name, e.g. `just
/// sg1-per-commit` must NOT match inside `just sg1-per-commit-smoke`).
fn is_wired(workflow_text: &str, recipe_name: &str) -> bool {
    let needle = format!("just {recipe_name}");
    let bytes = workflow_text.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut start = 0;
    while let Some(rel) = workflow_text[start..].find(&needle) {
        let match_start = start + rel;
        let after = match_start + needle_bytes.len();
        // The char immediately after the match must not continue the
        // identifier (alnum/underscore/hyphen) — otherwise this is a
        // different, longer recipe name that happens to share this prefix.
        let boundary_ok = after >= bytes.len() || {
            let c = bytes[after] as char;
            !(c.is_ascii_alphanumeric() || c == '_' || c == '-')
        };
        if boundary_ok {
            return true;
        }
        start = match_start + 1;
    }
    false
}

#[test]
fn every_gate_recipe_is_wired_or_marked_local_only() {
    let root = project_root();

    let justfile_path = root.join("Justfile");
    let justfile = fs::read_to_string(&justfile_path)
        .unwrap_or_else(|e| panic!("Cannot read {}: {}", justfile_path.display(), e));

    let gates = parse_gate_recipes(&justfile);
    assert!(
        !gates.is_empty(),
        "parsed zero `*-clippy`/`*-check` recipes from the Justfile — the \
         parser is almost certainly broken (sg1-faithful-clippy etc. should \
         always be present)"
    );

    let workflow_text = collect_workflow_text(&root);
    assert!(
        !workflow_text.is_empty(),
        "found no .github/workflows/*.yml content — workflow discovery is broken"
    );

    println!("=== CI lane-wiring: gate recipes ===");
    let mut orphans = Vec::new();
    for gate in &gates {
        let wired = is_wired(&workflow_text, &gate.name);
        let status = if wired {
            "WIRED"
        } else if gate.local_only {
            "LOCAL-ONLY"
        } else {
            "ORPHAN"
        };
        println!("  {:<24} (Justfile:{}) -> {status}", gate.name, gate.line);
        if !wired && !gate.local_only {
            orphans.push(gate.clone());
        }
    }

    if !orphans.is_empty() {
        let names: Vec<&str> = orphans.iter().map(|g| g.name.as_str()).collect();
        let count = orphans.len();
        panic!(
            "CI lane-wiring check failed: {count} orphan gate recipe(s) defined in \
             the Justfile but neither referenced from any \
             .github/workflows/*.yml file nor marked `# ci: local-only`: \
             {names:?}\n\
             Fix: either add a `run: just <recipe>` step to a workflow, or \
             add a `# ci: local-only` marker comment (with a reason) directly \
             above the recipe if it's genuinely not meant for automation."
        );
    }
}
