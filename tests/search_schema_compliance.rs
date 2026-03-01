//! Schema compliance tests for `maw ws recover --search --format json`.
//!
//! Validates that the JSON output from recovery search conforms to the
//! expected envelope schema: top-level fields, hit structure, type
//! correctness, determinism, truncation, empty results, and
//! case-insensitive search.
//!
//! Bone: bn-3fmh

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helper: run search and parse JSON
// ---------------------------------------------------------------------------

fn search_json(repo: &TestRepo, args: &[&str]) -> serde_json::Value {
    let mut full_args = vec!["ws", "recover"];
    full_args.extend_from_slice(args);
    full_args.extend_from_slice(&["--format", "json"]);
    let output = repo.maw_ok(&full_args);
    serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("search JSON parse failed: {e}\nraw output:\n{output}"))
}

/// Create a workspace with known content, destroy it (force), return the repo.
fn repo_with_destroyed_ws(ws_name: &str, files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_workspace(ws_name);
    for (path, content) in files {
        repo.add_file(ws_name, path, content);
    }
    repo.maw_ok(&["ws", "destroy", ws_name, "--force"]);
    repo
}

// ---------------------------------------------------------------------------
// Test 1: Required fields present with correct types
// ---------------------------------------------------------------------------

#[test]
fn search_json_has_required_top_level_fields_with_correct_types() {
    let repo = repo_with_destroyed_ws(
        "schema-test",
        &[("needle.txt", "find the needle in the haystack\n")],
    );

    let json = search_json(&repo, &["--search", "needle"]);

    // Top-level envelope fields
    assert!(
        json["pattern"].is_string(),
        "pattern should be a string, got: {}",
        json["pattern"]
    );
    assert_eq!(
        json["pattern"].as_str().unwrap(),
        "needle",
        "pattern should match the search query"
    );

    assert!(
        json["hit_count"].is_number(),
        "hit_count should be a number, got: {}",
        json["hit_count"]
    );

    assert!(
        json["truncated"].is_boolean(),
        "truncated should be a boolean, got: {}",
        json["truncated"]
    );

    assert!(
        json["hits"].is_array(),
        "hits should be an array, got: {}",
        json["hits"]
    );

    assert!(
        json["scanned_refs"].is_number(),
        "scanned_refs should be a number, got: {}",
        json["scanned_refs"]
    );

    assert!(
        json["advice"].is_array(),
        "advice should be an array, got: {}",
        json["advice"]
    );

    // Verify hits array is non-empty (we know the pattern matches)
    let hits = json["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "hits should contain at least one match for 'needle'"
    );

    // Validate each hit has required fields with correct types
    for (i, hit) in hits.iter().enumerate() {
        assert!(
            hit["ref_name"].is_string(),
            "hit[{i}].ref_name should be a string"
        );
        assert!(
            hit["workspace"].is_string(),
            "hit[{i}].workspace should be a string"
        );
        assert!(
            hit["timestamp"].is_string(),
            "hit[{i}].timestamp should be a string"
        );
        assert!(hit["oid"].is_string(), "hit[{i}].oid should be a string");
        assert!(
            hit["oid_short"].is_string(),
            "hit[{i}].oid_short should be a string"
        );
        assert!(
            hit["path"].is_string(),
            "hit[{i}].path should be a string"
        );
        assert!(
            hit["line"].is_number(),
            "hit[{i}].line should be a number"
        );
        assert!(
            hit["snippet"].is_array(),
            "hit[{i}].snippet should be an array"
        );

        // Validate snippet lines
        let snippet = hit["snippet"].as_array().unwrap();
        assert!(
            !snippet.is_empty(),
            "hit[{i}].snippet should be non-empty"
        );
        for (j, sl) in snippet.iter().enumerate() {
            assert!(
                sl["line"].is_number(),
                "hit[{i}].snippet[{j}].line should be a number"
            );
            assert!(
                sl["text"].is_string(),
                "hit[{i}].snippet[{j}].text should be a string"
            );
            assert!(
                sl["is_match"].is_boolean(),
                "hit[{i}].snippet[{j}].is_match should be a boolean"
            );
        }

        // Verify values make semantic sense
        assert_eq!(
            hit["workspace"].as_str().unwrap(),
            "schema-test",
            "hit[{i}].workspace should match the destroyed workspace name"
        );
        assert_eq!(
            hit["path"].as_str().unwrap(),
            "needle.txt",
            "hit[{i}].path should match the file containing the match"
        );
        assert!(
            hit["line"].as_u64().unwrap() >= 1,
            "hit[{i}].line should be >= 1"
        );

        // oid_short should be a prefix of oid
        let oid = hit["oid"].as_str().unwrap();
        let oid_short = hit["oid_short"].as_str().unwrap();
        assert!(
            oid.starts_with(oid_short),
            "hit[{i}].oid_short should be a prefix of oid"
        );

        // ref_name should start with the recovery ref prefix
        let ref_name = hit["ref_name"].as_str().unwrap();
        assert!(
            ref_name.starts_with("refs/manifold/recovery/"),
            "hit[{i}].ref_name should start with refs/manifold/recovery/, got: {ref_name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Determinism — same search twice produces identical output
// ---------------------------------------------------------------------------

#[test]
fn search_json_is_deterministic() {
    let repo = repo_with_destroyed_ws(
        "determinism-test",
        &[
            ("a.txt", "alpha pattern here\n"),
            ("b.txt", "beta pattern here\n"),
            ("c.txt", "gamma pattern here\n"),
        ],
    );

    let json1 = search_json(&repo, &["--search", "pattern"]);
    let json2 = search_json(&repo, &["--search", "pattern"]);

    assert_eq!(
        json1, json2,
        "two identical searches should produce identical JSON output"
    );

    // Additional check: hit ordering should be stable
    let hits1 = json1["hits"].as_array().unwrap();
    let hits2 = json2["hits"].as_array().unwrap();
    assert_eq!(hits1.len(), hits2.len(), "hit counts should match");

    for (i, (h1, h2)) in hits1.iter().zip(hits2.iter()).enumerate() {
        assert_eq!(
            h1["path"], h2["path"],
            "hit[{i}].path should be identical across runs"
        );
        assert_eq!(
            h1["line"], h2["line"],
            "hit[{i}].line should be identical across runs"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Truncation with --max-hits
// ---------------------------------------------------------------------------

#[test]
fn search_json_truncation_with_max_hits() {
    // Create a file with many occurrences of the pattern
    let many_lines = (1..=20)
        .map(|i| format!("line {i}: pattern_match here"))
        .collect::<Vec<_>>()
        .join("\n");

    let repo = repo_with_destroyed_ws("truncation-test", &[("many.txt", &many_lines)]);

    let json = search_json(&repo, &["--search", "pattern_match", "--max-hits", "2"]);

    let hits = json["hits"].as_array().unwrap();
    assert_eq!(
        hits.len(),
        2,
        "hits array should have exactly 2 entries when --max-hits 2, got: {}",
        hits.len()
    );

    assert!(
        json["truncated"].as_bool().unwrap(),
        "truncated should be true when results exceed --max-hits"
    );

    assert_eq!(
        json["hit_count"].as_u64().unwrap(),
        2,
        "hit_count should equal the number of returned hits (truncated)"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Empty results shape
// ---------------------------------------------------------------------------

#[test]
fn search_json_empty_results_shape() {
    let repo = repo_with_destroyed_ws(
        "empty-test",
        &[("content.txt", "nothing special here\n")],
    );

    let json = search_json(&repo, &["--search", "ZZZNONEXISTENT999"]);

    assert_eq!(
        json["hit_count"].as_u64().unwrap(),
        0,
        "hit_count should be 0 for no matches"
    );

    let hits = json["hits"].as_array().unwrap();
    assert!(
        hits.is_empty(),
        "hits should be an empty array for no matches"
    );

    assert!(
        !json["truncated"].as_bool().unwrap(),
        "truncated should be false when no results"
    );

    // Pattern should still be present
    assert_eq!(
        json["pattern"].as_str().unwrap(),
        "ZZZNONEXISTENT999",
        "pattern should echo the search query even with no results"
    );

    // scanned_refs should be >= 1 (we have a recovery snapshot)
    assert!(
        json["scanned_refs"].as_u64().unwrap() >= 1,
        "scanned_refs should be >= 1 even with no matches"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Case-insensitive search
// ---------------------------------------------------------------------------

#[test]
fn search_json_case_insensitive() {
    let repo = repo_with_destroyed_ws(
        "case-test",
        &[("greeting.txt", "HelloWorld is here\n")],
    );

    // Exact case should find it
    let json_exact = search_json(&repo, &["--search", "HelloWorld"]);
    let hits_exact = json_exact["hits"].as_array().unwrap();
    assert!(
        !hits_exact.is_empty(),
        "exact case search should find HelloWorld"
    );

    // Wrong case without -i should NOT find it (fixed-string search)
    let json_no_flag = search_json(&repo, &["--search", "helloworld"]);
    let hits_no_flag = json_no_flag["hits"].as_array().unwrap();
    assert!(
        hits_no_flag.is_empty(),
        "case-sensitive search for 'helloworld' should not match 'HelloWorld'"
    );

    // Wrong case WITH --ignore-case should find it
    let json_ci = search_json(&repo, &["--search", "helloworld", "--ignore-case"]);
    let hits_ci = json_ci["hits"].as_array().unwrap();
    assert!(
        !hits_ci.is_empty(),
        "case-insensitive search for 'helloworld' should match 'HelloWorld'"
    );

    // Verify the match details
    let hit = &hits_ci[0];
    assert_eq!(hit["path"].as_str().unwrap(), "greeting.txt");
    assert_eq!(hit["workspace"].as_str().unwrap(), "case-test");
}

// ---------------------------------------------------------------------------
// Test 6: workspace_filter and ref_filter in envelope
// ---------------------------------------------------------------------------

#[test]
fn search_json_includes_filter_fields() {
    let repo = repo_with_destroyed_ws(
        "filter-test",
        &[("data.txt", "searchable content\n")],
    );

    // Without workspace filter: workspace_filter should be null
    let json_all = search_json(&repo, &["--search", "searchable"]);
    assert!(
        json_all["workspace_filter"].is_null(),
        "workspace_filter should be null when no workspace specified"
    );
    assert!(
        json_all["ref_filter"].is_null(),
        "ref_filter should be null when no --ref specified"
    );

    // With workspace filter
    let json_filtered = search_json(
        &repo,
        &["filter-test", "--search", "searchable"],
    );
    assert_eq!(
        json_filtered["workspace_filter"].as_str().unwrap(),
        "filter-test",
        "workspace_filter should be set when workspace name is provided"
    );
}

// ---------------------------------------------------------------------------
// Test 7: snippet context correctness
// ---------------------------------------------------------------------------

#[test]
fn search_json_snippet_context_includes_surrounding_lines() {
    let content = "line one\nline two\nNEEDLE here\nline four\nline five\n";
    let repo = repo_with_destroyed_ws("snippet-test", &[("ctx.txt", content)]);

    // Default context is 2 lines
    let json = search_json(&repo, &["--search", "NEEDLE"]);
    let hits = json["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "should find NEEDLE");

    let snippet = hits[0]["snippet"].as_array().unwrap();

    // With context=2 (default), we should see lines around the match line
    // Match is on line 3; context=2 means lines 1..5
    assert!(
        snippet.len() >= 3,
        "snippet with default context should include surrounding lines, got {} lines",
        snippet.len()
    );

    // Verify that exactly one snippet line has is_match=true
    let match_count = snippet.iter().filter(|sl| sl["is_match"].as_bool() == Some(true)).count();
    assert_eq!(
        match_count, 1,
        "exactly one snippet line should have is_match=true"
    );

    // Verify the matching line contains our search term
    let match_line = snippet
        .iter()
        .find(|sl| sl["is_match"].as_bool() == Some(true))
        .unwrap();
    assert!(
        match_line["text"].as_str().unwrap().contains("NEEDLE"),
        "the matched snippet line should contain the search term"
    );
}

// ---------------------------------------------------------------------------
// Test 8: --max-hits larger than actual match count => truncated is false
// ---------------------------------------------------------------------------

#[test]
fn search_json_max_hits_above_actual_count_is_not_truncated() {
    let content = "match_here\nmatch_here\nmatch_here\n";
    let repo = repo_with_destroyed_ws("exact-count-test", &[("three.txt", content)]);

    // There are exactly 3 matches; set --max-hits to 10 (well above)
    let json = search_json(&repo, &["--search", "match_here", "--max-hits", "10"]);

    let hits = json["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3, "should have exactly 3 hits");

    assert!(
        !json["truncated"].as_bool().unwrap(),
        "truncated should be false when actual hits < max_hits"
    );

    assert_eq!(
        json["hit_count"].as_u64().unwrap(),
        3,
        "hit_count should equal the actual number of matches"
    );
}
