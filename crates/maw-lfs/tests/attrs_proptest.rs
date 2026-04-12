//! Property-based fuzz tests for the `.gitattributes` parser (`AttrsMatcher`).
//!
//! Target bone: bn-19tb.
//!
//! Invariants:
//! - `AttrsMatcher::from_entries` never panics on arbitrary bytes.
//! - `is_lfs(path)` / `merge_driver(path)` are deterministic for the same input.
//! - Round-trip: serializing known-good rules and re-parsing preserves semantics.

use maw_lfs::AttrsMatcher;
use proptest::prelude::*;

// Keep case counts modest so the whole file runs well under 30s.
fn pt_config() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        max_shrink_iters: 64,
        .. ProptestConfig::default()
    }
}

/// Known failure — see bn-3t55. `AttrsMatcher::is_lfs` panics in
/// `gix-glob`'s `matches_repo_relative_path` when given an absolute path.
/// Left ignored here as a repro so the fix can flip this to `#[test]`.
#[test]
#[ignore = "bn-3t55: AttrsMatcher panics on absolute paths"]
fn bn_3t55_absolute_path_panics() {
    let entries = vec![(String::new(), b"0 filter=lfs\n".to_vec())];
    let m = AttrsMatcher::from_entries(entries).unwrap();
    let _ = m.is_lfs("/abs/path");
}

proptest! {
    #![proptest_config(pt_config())]

    /// `from_entries` must never panic regardless of the bytes passed to it.
    #[test]
    fn from_entries_never_panics(
        entries in proptest::collection::vec(
            (
                // Directory prefix: empty or something like "a/", "a/b/".
                "(|[a-z]{1,6}/|[a-z]{1,6}/[a-z]{1,6}/)",
                proptest::collection::vec(any::<u8>(), 0..256),
            ),
            0..6,
        )
    ) {
        // Construction alone.
        let _ = AttrsMatcher::from_entries(entries);
    }

    /// Querying the same matcher twice with the same path must return the
    /// same results (`is_lfs` and `merge_driver` are pure functions).
    #[test]
    fn queries_are_deterministic(
        attrs_bytes in proptest::collection::vec(any::<u8>(), 0..512),
        // Non-slash first char: absolute paths trigger bn-3t55 (gix-glob
        // panics on absolute input). Query paths in maw are always
        // repo-relative, so matching that contract here.
        path in "[a-zA-Z0-9._-][a-zA-Z0-9/._-]{0,59}",
    ) {
        let entries = vec![(String::new(), attrs_bytes)];
        if let Ok(m) = AttrsMatcher::from_entries(entries) {
            let a1 = m.is_lfs(&path);
            let a2 = m.is_lfs(&path);
            prop_assert_eq!(a1, a2);
            let b1 = m.merge_driver(&path);
            let b2 = m.merge_driver(&path);
            prop_assert_eq!(b1, b2);
        }
    }

    /// Restrict bytes to a printable ASCII subset so the generated content
    /// looks more like real `.gitattributes` files. Still shouldn't panic.
    #[test]
    fn printable_attrs_never_panic(
        content in "([a-zA-Z0-9*./_-]{1,12} (filter=lfs|filter=other|-filter|merge=union|merge=binary|merge=ours|merge=my-driver|-merge|merge|diff=lfs|-text)\n){0,10}",
        // Non-slash first char: absolute paths trigger bn-3t55 (gix-glob
        // panics on absolute input). Query paths in maw are always
        // repo-relative, so matching that contract here.
        path in "[a-zA-Z0-9._-][a-zA-Z0-9/._-]{0,59}",
    ) {
        let entries = vec![(String::new(), content.into_bytes())];
        if let Ok(m) = AttrsMatcher::from_entries(entries) {
            let _ = m.is_lfs(&path);
            let _ = m.merge_driver(&path);
        }
    }

    /// Round-trip: a simple rule with known semantics serializes to a line
    /// that re-parses to produce the same matching behavior.
    #[test]
    fn round_trip_simple_lfs_rule(
        ext in "[a-z]{1,8}",
    ) {
        let content = format!("*.{ext} filter=lfs\n");
        let entries = vec![(String::new(), content.into_bytes())];
        let m = AttrsMatcher::from_entries(entries).unwrap();
        let root_path = format!("file.{ext}");
        let sub_path = format!("sub/file.{ext}");
        prop_assert!(m.is_lfs(&root_path));
        prop_assert!(m.is_lfs(&sub_path));
        prop_assert!(!m.is_lfs("other.rs"));
    }

    /// Round-trip: merge driver rule survives a parse cycle.
    #[test]
    fn round_trip_merge_driver(
        ext in "[a-z]{1,8}",
        driver in "(union|binary|ours|my-driver)",
    ) {
        let content = format!("*.{ext} merge={driver}\n");
        let entries = vec![(String::new(), content.into_bytes())];
        let m = AttrsMatcher::from_entries(entries).unwrap();
        let path = format!("file.{ext}");
        prop_assert_eq!(m.merge_driver(&path), Some(driver.clone()));
    }

    /// Multiple rules — later-line-wins semantics.
    #[test]
    fn later_line_wins_for_filter(
        ext in "[a-z]{1,6}",
        name in "[a-z]{1,6}",
    ) {
        prop_assume!(name != "default"); // avoid accidental collisions
        let content = format!("*.{ext} filter=lfs\n{name}.{ext} -filter\n");
        let entries = vec![(String::new(), content.into_bytes())];
        let m = AttrsMatcher::from_entries(entries).unwrap();
        let hit = format!("{name}.{ext}");
        let other = format!("other.{ext}");
        prop_assert!(!m.is_lfs(&hit));
        prop_assert!(m.is_lfs(&other));
    }
}
