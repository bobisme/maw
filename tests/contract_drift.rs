//! Contract drift CI gate: verifies assurance plan docs stay consistent with code.
//!
//! Run with: `cargo test --test contract_drift`
//!
//! This test reads source files and documentation to detect drift between
//! the assurance plan and the actual codebase. It checks:
//!
//! 1. Failpoint catalog entries vs `fp!()` calls in source
//! 2. G-numbering consistency across assurance docs
//! 3. Test matrix coverage reality (claimed implementations vs actual test files)

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Find the project root by walking up from the test binary's manifest dir.
fn project_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(manifest)
}

/// Recursively collect all files under `dir` matching a predicate on the filename.
fn collect_files(dir: &Path, pred: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if !dir.exists() {
        return result;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                result.extend(collect_files(&path, pred));
            } else if pred(&path) {
                result.push(path);
            }
        }
    }
    result
}

/// Collect all `.rs` files under a directory.
fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    collect_files(dir, &|p| p.extension().is_some_and(|e| e == "rs"))
}

/// Collect all `.md` files under a directory.
fn collect_md_files(dir: &Path) -> Vec<PathBuf> {
    collect_files(dir, &|p| p.extension().is_some_and(|e| e == "md"))
}

// ============================================================================
// Check 1: Failpoint catalog matches source code
// ============================================================================

/// Extract all FP_* identifiers from failpoints.md catalog.
/// Looks for lines containing `FP_` followed by word characters, typically
/// in backtick-quoted identifiers like `FP_COMMIT_AFTER_EPOCH_CAS`.
fn parse_failpoint_catalog(root: &Path) -> BTreeSet<String> {
    let catalog_path = root.join("notes/assurance/failpoints.md");
    let content = fs::read_to_string(&catalog_path)
        .unwrap_or_else(|e| panic!("Cannot read {}: {}", catalog_path.display(), e));

    let re_fp = regex_simple_fp();
    let mut ids = BTreeSet::new();
    for cap in re_fp.find_iter(&content) {
        ids.insert(cap.to_string());
    }
    ids
}

/// Standard failpoint namespace prefixes — only fp! calls using these
/// are checked against the catalog. Test-only names (FP_TEST_*, FP_A, etc.)
/// are excluded.
const FP_STANDARD_PREFIXES: &[&str] = &[
    "FP_PREPARE_", "FP_BUILD_", "FP_VALIDATE_", "FP_COMMIT_",
    "FP_CAPTURE_", "FP_CLEANUP_", "FP_DESTROY_", "FP_RECOVER_",
];

/// Extract all fp!("FP_...") calls from Rust source files under src/.
/// Only includes calls using standard namespace prefixes.
fn parse_fp_calls_in_source(root: &Path) -> BTreeSet<String> {
    let src_dir = root.join("src");
    let files = collect_rs_files(&src_dir);

    let mut ids = BTreeSet::new();
    // Match fp!("FP_SOMETHING") patterns
    for file in &files {
        if let Ok(content) = fs::read_to_string(file) {
            for line in content.lines() {
                // Look for fp!("FP_...") — the macro call pattern
                if let Some(start) = line.find("fp!(\"FP_") {
                    let rest = &line[start + 5..]; // skip fp!("
                    if let Some(end) = rest.find('"') {
                        let id = &rest[..end];
                        // Only include standard-prefix failpoints
                        if FP_STANDARD_PREFIXES.iter().any(|p| id.starts_with(p)) {
                            ids.insert(id.to_string());
                        }
                    }
                }
            }
        }
    }
    ids
}

/// Create an FP_ identifier finder (no regex crate needed).
fn regex_simple_fp() -> FpFinderAdapter {
    FpFinderAdapter
}

struct FpFinderAdapter;

impl FpFinderAdapter {
    fn find_iter<'a>(&self, text: &'a str) -> Vec<&'a str> {
        let bytes = text.as_bytes();
        let mut results = Vec::new();
        let mut i = 0;
        while i + 3 < bytes.len() {
            if bytes[i] == b'F' && bytes[i + 1] == b'P' && bytes[i + 2] == b'_' {
                // Check not part of a larger word
                if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
                    i += 1;
                    continue;
                }
                let start = i;
                i += 3;
                while i < bytes.len()
                    && (bytes[i].is_ascii_uppercase()
                        || bytes[i].is_ascii_digit()
                        || bytes[i] == b'_')
                {
                    i += 1;
                }
                let token = &text[start..i];
                if token.len() > 3 {
                    results.push(token);
                }
            } else {
                i += 1;
            }
        }
        results
    }
}

#[test]
fn check1_failpoint_catalog_matches_source() {
    let root = project_root();
    let catalog_ids = parse_failpoint_catalog(&root);
    let source_ids = parse_fp_calls_in_source(&root);

    println!("=== Check 1: Failpoint catalog vs source code ===");
    println!("  Catalog entries: {}", catalog_ids.len());
    println!("  Source fp!() calls: {}", source_ids.len());

    // If no fp!() calls exist yet (framework not implemented), that's OK.
    // We just verify the catalog is non-empty and well-formed.
    if source_ids.is_empty() {
        println!("  INFO: No fp!() calls found in source (failpoint framework not yet implemented)");
        println!("  Verifying catalog is non-empty and well-formed...");
        assert!(
            !catalog_ids.is_empty(),
            "Failpoint catalog in notes/assurance/failpoints.md is empty — \
             expected at least the planned failpoint IDs"
        );
        // Verify all catalog entries follow naming convention
        let mut malformed = Vec::new();
        for id in &catalog_ids {
            let prefix_ok = id.starts_with("FP_PREPARE_")
                || id.starts_with("FP_BUILD_")
                || id.starts_with("FP_VALIDATE_")
                || id.starts_with("FP_COMMIT_")
                || id.starts_with("FP_CLEANUP_")
                || id.starts_with("FP_DESTROY_")
                || id.starts_with("FP_RECOVER_")
                || id.starts_with("FP_CAPTURE_");
            if !prefix_ok {
                malformed.push(id.clone());
            }
        }
        if !malformed.is_empty() {
            println!(
                "  WARNING: {} catalog entries don't follow standard FP_ prefix convention: {:?}",
                malformed.len(),
                malformed
            );
            // This is a warning, not a failure — the assurance plan documents
            // proposed failpoints that may use new prefixes.
        }
        println!("  PASS (catalog-only mode: {} well-formed entries)", catalog_ids.len());
        return;
    }

    // When fp!() calls exist, check bidirectional consistency
    let mut failures = Vec::new();

    // Source calls without catalog entries
    let uncataloged: Vec<_> = source_ids.difference(&catalog_ids).collect();
    if !uncataloged.is_empty() {
        failures.push(format!(
            "fp!() calls in source with no catalog entry in failpoints.md: {:?}",
            uncataloged
        ));
    }

    // Catalog entries without source calls (not a hard failure — planned FPs are OK)
    let unimplemented: Vec<_> = catalog_ids.difference(&source_ids).collect();
    if !unimplemented.is_empty() {
        println!(
            "  INFO: {} catalog entries not yet in source (planned): {:?}",
            unimplemented.len(),
            unimplemented
        );
    }

    if failures.is_empty() {
        println!("  PASS");
    } else {
        for f in &failures {
            println!("  FAIL: {f}");
        }
        panic!(
            "Check 1 failed: failpoint catalog/source drift detected\n{}",
            failures.join("\n")
        );
    }
}

// ============================================================================
// Check 2: G-numbering consistency across assurance docs
// ============================================================================

/// Extract all G-number references (G1, G2, etc.) from text.
/// Returns the set of unique integer suffixes found.
fn extract_g_numbers(text: &str) -> BTreeSet<u32> {
    let mut numbers = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'G' {
            // Check it's a standalone G reference, not part of a word like "Getting"
            // Must be preceded by non-alphanumeric or start of string
            let preceded_ok =
                i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if preceded_ok && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                // Must NOT be followed by more alphanumeric (e.g., "G1x" is not a G-ref)
                // But G1.1, G1-001, G1: etc. are fine
                let followed_ok =
                    end >= bytes.len() || !bytes[end].is_ascii_alphabetic();
                if followed_ok {
                    if let Ok(n) = text[start..end].parse::<u32>() {
                        numbers.insert(n);
                    }
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    numbers
}

#[test]
fn check2_g_numbering_consistency() {
    let root = project_root();
    let assurance_dir = root.join("notes/assurance");
    let plan_path = root.join("notes/assurance-plan.md");

    println!("=== Check 2: G-numbering consistency ===");

    // The canonical valid G-numbers are G1-G6 per the assurance plan
    let valid_range: BTreeSet<u32> = (1..=6).collect();

    // Read the main plan to confirm the authoritative range
    let plan_content = fs::read_to_string(&plan_path)
        .unwrap_or_else(|e| panic!("Cannot read {}: {}", plan_path.display(), e));
    let plan_g_numbers = extract_g_numbers(&plan_content);

    println!("  Main plan G-numbers: {:?}", plan_g_numbers);

    // The plan should reference exactly G1-G6
    let plan_out_of_range: Vec<_> = plan_g_numbers.difference(&valid_range).collect();
    if !plan_out_of_range.is_empty() {
        // G0 or G7+ in the main plan would be a structural problem
        println!(
            "  WARNING: Main plan references G-numbers outside G1-G6: {:?}",
            plan_out_of_range
        );
    }

    // Scan all subsidiary docs in notes/assurance/
    let md_files = collect_md_files(&assurance_dir);
    let mut failures = Vec::new();

    for file in &md_files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let g_numbers = extract_g_numbers(&content);
        let relative = file.strip_prefix(&root).unwrap_or(file);

        let out_of_range: Vec<_> = g_numbers.difference(&valid_range).collect();
        if !out_of_range.is_empty() {
            failures.push(format!(
                "{}: references G-numbers outside valid range G1-G6: {:?}",
                relative.display(),
                out_of_range
            ));
        }

        // Verify each G-number mentioned in subsidiary docs exists in the main plan
        let not_in_plan: Vec<_> = g_numbers.difference(&plan_g_numbers).collect();
        if !not_in_plan.is_empty() {
            failures.push(format!(
                "{}: references G-numbers not in main plan: {:?}",
                relative.display(),
                not_in_plan
            ));
        }
    }

    if failures.is_empty() {
        println!("  Scanned {} subsidiary docs", md_files.len());
        println!("  PASS");
    } else {
        for f in &failures {
            println!("  FAIL: {f}");
        }
        panic!(
            "Check 2 failed: G-numbering drift detected\n{}",
            failures.join("\n")
        );
    }
}

// ============================================================================
// Check 3: Test matrix coverage reality
// ============================================================================

/// A test ID entry parsed from the assurance plan's "Implemented" table.
#[derive(Debug)]
struct ImplementedTestEntry {
    test_id: String,
    location: String,
}

/// Parse the "Implemented" section of the assurance plan for test ID -> location mappings.
/// Looks for table rows like: `| IT-G1-001 | `tests/recovery_capture.rs` (4 tests) | ... |`
fn parse_implemented_tests(root: &Path) -> Vec<ImplementedTestEntry> {
    let plan_path = root.join("notes/assurance-plan.md");
    let content = fs::read_to_string(&plan_path)
        .unwrap_or_else(|e| panic!("Cannot read {}: {}", plan_path.display(), e));

    let mut entries = Vec::new();
    let mut in_implemented_section = false;
    let mut past_header_separator = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect section boundaries
        if trimmed == "### Implemented" {
            in_implemented_section = true;
            past_header_separator = false;
            continue;
        }
        if in_implemented_section && trimmed.starts_with("### ") && trimmed != "### Implemented" {
            break; // Next section
        }
        if in_implemented_section && trimmed.starts_with("## ") {
            break; // Next top-level section
        }

        if !in_implemented_section {
            continue;
        }

        // Skip the table header row and separator
        if trimmed.starts_with("|---") || trimmed.starts_with("| Test ID") {
            if trimmed.starts_with("|---") {
                past_header_separator = true;
            }
            continue;
        }

        if !past_header_separator {
            continue;
        }

        // Parse table rows: | TEST-ID | location | description |
        if trimmed.starts_with('|') {
            let cols: Vec<&str> = trimmed.split('|').collect();
            if cols.len() >= 4 {
                let test_id = cols[1].trim().to_string();
                let location = cols[2].trim().to_string();
                if !test_id.is_empty() && !location.is_empty() {
                    entries.push(ImplementedTestEntry { test_id, location });
                }
            }
        }
    }
    entries
}

/// Parse test IDs from the test-matrix.md file.
/// Returns all test IDs mentioned (both in matrix table and catalog sections).
fn parse_test_matrix_ids(root: &Path) -> BTreeSet<String> {
    let matrix_path = root.join("notes/assurance/test-matrix.md");
    let content = fs::read_to_string(&matrix_path)
        .unwrap_or_else(|e| panic!("Cannot read {}: {}", matrix_path.display(), e));

    let mut ids = BTreeSet::new();
    // Match patterns like IT-G1-001, UT-G2-001, DST-G1-001, PT-merge-001, etc.
    // Look for backtick-quoted IDs and bare IDs
    for line in content.lines() {
        extract_test_ids_from_line(line, &mut ids);
    }
    ids
}

/// Extract test IDs matching patterns like XX-Gy-NNN or XX-word-NNN from a line.
fn extract_test_ids_from_line(line: &str, ids: &mut BTreeSet<String>) {
    // Look for backtick-quoted test IDs: `IT-G1-001`
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('`') {
            let candidate = &rest[..end];
            if is_test_id(candidate) {
                ids.insert(candidate.to_string());
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
}

/// Check if a string looks like a test ID (IT-*, UT-*, DST-*, PT-*, FM-*).
fn is_test_id(s: &str) -> bool {
    let prefixes = ["IT-", "UT-", "DST-", "PT-", "FM-", "DST-lite-"];
    prefixes.iter().any(|p| s.starts_with(p))
        && s.len() > 4
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/' || c == '.')
}

/// Extract the file path from a location string like "`tests/recovery_capture.rs` (4 tests)"
/// or "`src/workspace/recover.rs` (10 inline tests)".
fn extract_file_path(location: &str) -> Option<String> {
    // Look for backtick-quoted path
    if let Some(start) = location.find('`') {
        let rest = &location[start + 1..];
        if let Some(end) = rest.find('`') {
            return Some(rest[..end].to_string());
        }
    }
    // Fallback: look for .rs path
    for word in location.split_whitespace() {
        let word = word.trim_matches('`').trim_matches(',');
        if word.ends_with(".rs") {
            return Some(word.to_string());
        }
    }
    None
}

#[test]
fn check3_test_matrix_coverage_reality() {
    let root = project_root();

    println!("=== Check 3: Test matrix coverage reality ===");

    // Parse implemented tests from the assurance plan
    let implemented = parse_implemented_tests(&root);
    println!("  Implemented test entries in plan: {}", implemented.len());

    // Parse test IDs from the test matrix
    let matrix_ids = parse_test_matrix_ids(&root);
    println!("  Test IDs in test-matrix.md: {}", matrix_ids.len());

    let mut failures = Vec::new();
    let mut warnings = Vec::new();

    // For each "implemented" test, verify the source file exists
    for entry in &implemented {
        if let Some(file_path) = extract_file_path(&entry.location) {
            let full_path = root.join(&file_path);
            if !full_path.exists() {
                failures.push(format!(
                    "Test {} claims location `{}` but file does not exist",
                    entry.test_id, file_path
                ));
            } else {
                // Verify the file has at least one #[test] or #[cfg(test)]
                if let Ok(content) = fs::read_to_string(&full_path) {
                    let has_tests = content.contains("#[test]")
                        || content.contains("#[cfg(test)]")
                        || content.contains("#[kani::proof]");
                    if !has_tests {
                        failures.push(format!(
                            "Test {} claims location `{}` but file contains no test attributes",
                            entry.test_id, file_path
                        ));
                    }
                }
            }
        } else {
            warnings.push(format!(
                "Test {} has location `{}` — could not extract file path",
                entry.test_id, entry.location
            ));
        }
    }

    // Verify implemented test IDs from the plan are also in the test matrix
    let plan_implemented_ids: BTreeSet<String> =
        implemented.iter().map(|e| e.test_id.clone()).collect();
    for id in &plan_implemented_ids {
        if !matrix_ids.contains(id) {
            // Some IDs in the plan use non-standard formats (e.g., "UT-G1/G4-001",
            // "IT-G2-adj", "DST-lite-001") that may not appear in the matrix.
            // Only warn for standard-format IDs.
            let is_standard = id.contains("-G")
                && id.chars().last().is_some_and(|c| c.is_ascii_digit());
            if is_standard {
                warnings.push(format!(
                    "Plan lists {} as implemented but it's not in test-matrix.md",
                    id
                ));
            }
        }
    }

    // Check that test files in tests/ that have names suggesting assurance coverage
    // are referenced somewhere in the plan
    let test_files_with_assurance_relevance = [
        ("tests/recovery_capture.rs", "IT-G1"),
        ("tests/crash_recovery.rs", "IT-G3"),
        ("tests/destroy_recover.rs", "IT-G5"),
        ("tests/restore.rs", "IT-G5"),
        ("tests/sync.rs", "IT-G2"),
        ("tests/concurrent_safety.rs", "DST"),
    ];

    for (test_file, expected_prefix) in &test_files_with_assurance_relevance {
        let full_path = root.join(test_file);
        if full_path.exists() {
            // Verify this file is referenced in the plan
            let referenced = implemented
                .iter()
                .any(|e| e.location.contains(test_file));
            if !referenced {
                warnings.push(format!(
                    "Test file `{}` exists and appears assurance-relevant (expected {}) \
                     but is not in the plan's implemented table",
                    test_file, expected_prefix
                ));
            }
        }
    }

    // Print results
    for w in &warnings {
        println!("  WARNING: {w}");
    }

    if failures.is_empty() {
        println!("  PASS ({} warnings)", warnings.len());
    } else {
        for f in &failures {
            println!("  FAIL: {f}");
        }
        panic!(
            "Check 3 failed: test matrix drift detected\n{}",
            failures.join("\n")
        );
    }
}

// ============================================================================
// Bonus: Cross-reference invariant IDs
// ============================================================================

/// Extract I-G*.* invariant references from text.
fn extract_invariant_ids(text: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        // Look for "I-G" pattern
        if bytes[i] == b'I' && bytes[i + 1] == b'-' && bytes[i + 2] == b'G' {
            // Check not part of larger word
            let preceded_ok =
                i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if preceded_ok {
                let start = i;
                i += 3;
                // Consume the rest: digits, dots, digits
                while i < bytes.len()
                    && (bytes[i].is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b'*')
                {
                    i += 1;
                }
                let token = &text[start..i];
                // Must have at least I-G followed by a digit
                if token.len() >= 4 {
                    ids.insert(token.to_string());
                }
                continue;
            }
        }
        i += 1;
    }
    ids
}

#[test]
fn check2b_invariant_ids_reference_valid_guarantees() {
    let root = project_root();
    let assurance_dir = root.join("notes/assurance");

    println!("=== Check 2b: Invariant ID consistency ===");

    let valid_g_range: BTreeSet<u32> = (1..=6).collect();
    let md_files = collect_md_files(&assurance_dir);
    let mut failures = Vec::new();

    for file in &md_files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let inv_ids = extract_invariant_ids(&content);
        let relative = file.strip_prefix(&root).unwrap_or(file);

        for inv_id in &inv_ids {
            // Extract the G-number from I-G<N>.<M> or I-G<N>.*
            if let Some(g_part) = inv_id.strip_prefix("I-G") {
                let g_num_str: String = g_part.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(g_num) = g_num_str.parse::<u32>() {
                    if !valid_g_range.contains(&g_num) {
                        failures.push(format!(
                            "{}: invariant {} references G{} which is outside valid range G1-G6",
                            relative.display(),
                            inv_id,
                            g_num
                        ));
                    }
                }
            }
        }
    }

    // Also check the main plan
    let plan_path = root.join("notes/assurance-plan.md");
    if let Ok(content) = fs::read_to_string(&plan_path) {
        let inv_ids = extract_invariant_ids(&content);
        for inv_id in &inv_ids {
            if let Some(g_part) = inv_id.strip_prefix("I-G") {
                let g_num_str: String = g_part.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(g_num) = g_num_str.parse::<u32>() {
                    if !valid_g_range.contains(&g_num) {
                        failures.push(format!(
                            "notes/assurance-plan.md: invariant {} references G{} outside valid range",
                            inv_id, g_num
                        ));
                    }
                }
            }
        }
    }

    if failures.is_empty() {
        println!("  PASS");
    } else {
        for f in &failures {
            println!("  FAIL: {f}");
        }
        panic!(
            "Check 2b failed: invariant ID drift detected\n{}",
            failures.join("\n")
        );
    }
}
