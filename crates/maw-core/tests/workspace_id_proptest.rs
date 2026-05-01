//! Property-based fuzz tests for `WorkspaceId::new`.
//!
//! Target bone: bn-19tb.
//!
//! Invariants:
//! - Never panics on arbitrary input.
//! - Valid grammar (lowercase alnum + hyphens, 1..=64 chars, no leading/
//!   trailing hyphen, no `--`) always succeeds.
//! - Names outside the grammar always fail.
//! - Successful names round-trip through `as_str()`.

use maw_core::model::types::WorkspaceId;
use proptest::prelude::*;

fn pt_config() -> ProptestConfig {
    ProptestConfig {
        cases: 512,
        max_shrink_iters: 128,
        ..ProptestConfig::default()
    }
}

/// Predicate mirroring the rules in `WorkspaceId::validate`, used to check
/// "invalid never succeeds" from the negative direction.
fn is_valid_name(s: &str) -> bool {
    if s.is_empty() || s.len() > WorkspaceId::MAX_LEN {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }
    if s.contains("--") {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

proptest! {
    #![proptest_config(pt_config())]

    /// `WorkspaceId::new` must never panic on any string input.
    #[test]
    fn never_panics(input in any::<String>()) {
        let _ = WorkspaceId::new(&input);
    }

    /// Every name matching the grammar must parse and round-trip.
    #[test]
    fn valid_grammar_round_trips(
        // Start with alnum, allow internal hyphens (but not consecutive),
        // end with alnum. Length 1..=64.
        name in "[a-z0-9]([a-z0-9]|-[a-z0-9]){0,63}"
    ) {
        // The regex above may generate strings longer than 64 chars because of
        // the `{0,63}` repeating group (each slot may itself be 2 chars).
        // Filter those out instead of failing.
        prop_assume!(name.len() <= WorkspaceId::MAX_LEN);
        prop_assume!(!name.contains("--"));

        let ws = WorkspaceId::new(&name).expect("grammar should validate");
        prop_assert_eq!(ws.as_str(), &name);
    }

    /// Names that fail our reference predicate must also fail `WorkspaceId::new`.
    #[test]
    fn invalid_names_never_succeed(input in any::<String>()) {
        if !is_valid_name(&input) {
            prop_assert!(WorkspaceId::new(&input).is_err());
        }
    }

    /// Any name that passes our reference predicate must succeed (the two
    /// definitions are equivalent).
    #[test]
    fn valid_predicate_implies_success(
        // Use a relaxed generator — mostly noise, but occasionally valid.
        input in "[a-z0-9-]{0,80}"
    ) {
        if is_valid_name(&input) {
            let ws = WorkspaceId::new(&input).expect("predicate said valid");
            prop_assert_eq!(ws.as_str(), &input);
        } else {
            prop_assert!(WorkspaceId::new(&input).is_err());
        }
    }

    /// Spot-check: obvious invalids always fail.
    #[test]
    fn obvious_invalids_fail(
        bad in prop_oneof![
            Just(String::new()),
            Just("-lead".to_string()),
            Just("trail-".to_string()),
            Just("dou--ble".to_string()),
            Just("Upper".to_string()),
            Just("under_score".to_string()),
            Just("slash/name".to_string()),
            Just("a".repeat(65)),
        ]
    ) {
        prop_assert!(WorkspaceId::new(&bad).is_err());
    }
}
