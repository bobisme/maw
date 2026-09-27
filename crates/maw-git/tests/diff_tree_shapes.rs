//! bn-1ijl: `diff_trees_with_renames` must only ever report file-level
//! entries (blobs, symlinks, gitlinks), never directory (tree) entries —
//! even when gix's rewrite tracker matches a whole directory as renamed.
//!
//! Field failure: `maw ws sync` replaying a commit that deleted `.crit/` and
//! added `.seal/` died with "Object named <oid> was supposed to be of kind
//! blob, but was kind tree" because an identical subtree moved between the
//! two directories surfaced as a tree-level `Renamed` entry and
//! `diff_patchset` then tried to `read_blob` the tree OID.
//!
//! The table below covers the directory/file/gitlink shape class. For every
//! shape and several similarity thresholds we check:
//!
//! 1. no entry carries a `Tree` mode on either side;
//! 2. every blob/symlink-mode OID reported actually reads as a blob;
//! 3. replaying the entries over the old tree's flat file map reproduces the
//!    new tree's flat file map exactly (nothing dropped, nothing invented).

use std::collections::BTreeMap;

use maw_git::test_support::init_test_repo;
use maw_git::{ChangeType, DiffEntry, EntryMode, GitOid, GitRepo, GixRepo, TreeEntry};

/// Flat tree description: path -> (mode, content). For `Commit` (gitlink)
/// entries `content` is hashed into a fake commit id (never dereferenced).
type Spec<'a> = &'a [(&'a str, EntryMode, &'a str)];

type Flat = BTreeMap<String, (EntryMode, GitOid)>;

/// One leaf split into path components, for the recursive tree writer.
type Leaf<'a> = (Vec<&'a str>, EntryMode, GitOid);

fn fake_gitlink_oid(seed: &str) -> GitOid {
    // Deterministic, non-existent object id for a submodule commit.
    let mut bytes = [0u8; 20];
    for (i, b) in seed.bytes().enumerate() {
        bytes[i % 20] ^= b
            .wrapping_mul(31)
            .wrapping_add(u8::try_from(i % 256).unwrap_or(0));
    }
    bytes[0] = 0xfe;
    GitOid::from_bytes(bytes)
}

fn flat_from_spec(repo: &GixRepo, spec: Spec<'_>) -> Flat {
    let mut flat = Flat::new();
    for (path, mode, content) in spec {
        let oid = match mode {
            EntryMode::Commit => fake_gitlink_oid(content),
            EntryMode::Tree => panic!("spec must list leaf entries only"),
            _ => repo
                .write_blob(content.as_bytes())
                .expect("write blob should succeed"),
        };
        flat.insert((*path).to_owned(), (*mode, oid));
    }
    flat
}

/// Recursively write a tree for a flat path map.
fn write_flat(repo: &GixRepo, flat: &Flat) -> GitOid {
    fn go(repo: &GixRepo, entries: &[Leaf<'_>]) -> GitOid {
        let mut leaves: Vec<TreeEntry> = Vec::new();
        let mut dirs: BTreeMap<&str, Vec<Leaf<'_>>> = BTreeMap::new();
        for (parts, mode, oid) in entries {
            if parts.len() == 1 {
                leaves.push(TreeEntry {
                    name: parts[0].to_owned(),
                    mode: *mode,
                    oid: *oid,
                });
            } else {
                dirs.entry(parts[0])
                    .or_default()
                    .push((parts[1..].to_vec(), *mode, *oid));
            }
        }
        for (name, children) in dirs {
            let sub = go(repo, &children);
            leaves.push(TreeEntry {
                name: name.to_owned(),
                mode: EntryMode::Tree,
                oid: sub,
            });
        }
        repo.write_tree(&leaves).expect("write tree should succeed")
    }
    let entries: Vec<Leaf<'_>> = flat
        .iter()
        .map(|(p, (m, o))| (p.split('/').collect(), *m, *o))
        .collect();
    go(repo, &entries)
}

/// Read a tree back into its flat leaf map.
fn read_flat(repo: &GixRepo, tree: GitOid) -> Flat {
    fn go(repo: &GixRepo, tree: GitOid, prefix: &str, out: &mut Flat) {
        for e in repo.read_tree(tree).expect("read tree should succeed") {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            if e.mode == EntryMode::Tree {
                go(repo, e.oid, &path, out);
            } else {
                out.insert(path, (e.mode, e.oid));
            }
        }
    }
    let mut out = Flat::new();
    go(repo, tree, "", &mut out);
    out
}

/// Check the three invariants for one diff.
fn check_diff(repo: &GixRepo, name: &str, pct: u32, old: &Flat, new: &Flat, diff: &[DiffEntry]) {
    let mut replay = old.clone();
    for e in diff {
        for (side, mode, oid) in [
            ("old", e.old_mode, e.old_oid),
            ("new", e.new_mode, e.new_oid),
        ] {
            assert_ne!(
                mode,
                Some(EntryMode::Tree),
                "[{name} @{pct}%] {side}-side tree entry leaked into diff: {e:?}\nfull diff: {diff:?}"
            );
            if matches!(
                mode,
                Some(EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link)
            ) {
                repo.read_blob(oid).unwrap_or_else(|err| {
                    panic!("[{name} @{pct}%] {side}-side oid of {e:?} is not a blob: {err}")
                });
            }
        }
        match &e.change_type {
            ChangeType::Added => {
                let prev = replay.insert(
                    e.path.clone(),
                    (e.new_mode.expect("added has new mode"), e.new_oid),
                );
                assert!(prev.is_none(), "[{name} @{pct}%] Added over existing {e:?}");
            }
            ChangeType::Modified => {
                let prev = replay.insert(
                    e.path.clone(),
                    (e.new_mode.expect("modified has new mode"), e.new_oid),
                );
                assert_eq!(
                    prev,
                    Some((e.old_mode.expect("modified has old mode"), e.old_oid)),
                    "[{name} @{pct}%] Modified base mismatch {e:?}"
                );
            }
            ChangeType::Deleted => {
                let prev = replay.remove(&e.path);
                assert_eq!(
                    prev,
                    Some((e.old_mode.expect("deleted has old mode"), e.old_oid)),
                    "[{name} @{pct}%] Deleted base mismatch {e:?}"
                );
            }
            ChangeType::Renamed { from } => {
                let prev = replay.remove(from);
                assert_eq!(
                    prev,
                    Some((e.old_mode.expect("renamed has old mode"), e.old_oid)),
                    "[{name} @{pct}%] Renamed source mismatch {e:?}"
                );
                let prev = replay.insert(
                    e.path.clone(),
                    (e.new_mode.expect("renamed has new mode"), e.new_oid),
                );
                assert!(
                    prev.is_none(),
                    "[{name} @{pct}%] Renamed onto existing path {e:?}"
                );
            }
        }
    }
    assert_eq!(
        &replay, new,
        "[{name} @{pct}%] replaying the diff over the old tree does not reproduce the new tree\n\
         diff: {diff:?}"
    );
}

const B: EntryMode = EntryMode::Blob;
const X: EntryMode = EntryMode::BlobExecutable;
const L: EntryMode = EntryMode::Link;
const G: EntryMode = EntryMode::Commit;

#[expect(clippy::too_many_lines, reason = "flat table of tree shapes")]
fn shapes() -> Vec<(&'static str, Spec<'static>, Spec<'static>)> {
    vec![
        // The field report shape: `.crit/` deleted, `.seal/` added, with an
        // identical subtree (`reviews/`) moving between them.
        (
            "field_crit_to_seal",
            &[
                (".crit/.gitignore", B, "ignored\n"),
                (".crit/reviews/r1/events.jsonl", B, "{\"e\":1}\n"),
                (".crit/reviews/r2/events.jsonl", B, "{\"e\":2}\n"),
                (".crit/version", B, "1\n"),
                (".critignore", B, "target\n"),
                ("README", B, "readme\n"),
            ],
            &[
                (".seal/reviews/r1/events.jsonl", B, "{\"e\":1}\n"),
                (".seal/reviews/r2/events.jsonl", B, "{\"e\":2}\n"),
                (".seal/config.toml", B, "x = 1\n"),
                ("README", B, "readme\n"),
            ],
        ),
        (
            "dir_renamed_identical",
            &[("a/x", B, "one\n"), ("a/y", B, "two\n"), ("keep", B, "k\n")],
            &[("b/x", B, "one\n"), ("b/y", B, "two\n"), ("keep", B, "k\n")],
        ),
        (
            "dir_renamed_nested",
            &[("a/b/c/x", B, "one\n"), ("a/b/c/y", X, "two\n")],
            &[("z/b/c/x", B, "one\n"), ("z/b/c/y", X, "two\n")],
        ),
        (
            "dir_renamed_one_file_edited",
            &[("a/x", B, "one\nline\nline\nline\n"), ("a/y", B, "two\n")],
            &[("b/x", B, "one\nline\nline\nLINE\n"), ("b/y", B, "two\n")],
        ),
        (
            "dir_swap",
            &[("a/x", B, "one\n"), ("b/y", B, "two\n")],
            &[("a/y", B, "two\n"), ("b/x", B, "one\n")],
        ),
        (
            "dir_deleted",
            &[
                ("a/x", B, "one\n"),
                ("a/sub/y", B, "two\n"),
                ("keep", B, "k\n"),
            ],
            &[("keep", B, "k\n")],
        ),
        (
            "dir_added",
            &[("keep", B, "k\n")],
            &[
                ("a/x", B, "one\n"),
                ("a/sub/y", B, "two\n"),
                ("keep", B, "k\n"),
            ],
        ),
        (
            "dir_replaced_by_file",
            &[("a/x", B, "one\n"), ("a/y", B, "two\n")],
            &[("a", B, "now a file\n")],
        ),
        (
            "dir_replaced_by_file_same_content",
            &[("a/a", B, "same\n")],
            &[("a", B, "same\n")],
        ),
        (
            "file_replaced_by_dir",
            &[("a", B, "a file\n")],
            &[("a/x", B, "one\n"), ("a/y", B, "two\n")],
        ),
        (
            "file_replaced_by_dir_same_content",
            &[("a", B, "same\n")],
            &[("a/a", B, "same\n")],
        ),
        (
            "dir_duplicated_and_original_deleted",
            &[("a/x", B, "one\n"), ("c/x", B, "one\n")],
            &[("b/x", B, "one\n"), ("c/x", B, "one\n")],
        ),
        (
            "symlink_in_renamed_dir",
            &[("a/link", L, "../target"), ("a/f", B, "f\n")],
            &[("b/link", L, "../target"), ("b/f", B, "f\n")],
        ),
        (
            "gitlink_added",
            &[("keep", B, "k\n")],
            &[("keep", B, "k\n"), ("vendor/sub", G, "sub@1")],
        ),
        (
            "gitlink_deleted",
            &[("keep", B, "k\n"), ("vendor/sub", G, "sub@1")],
            &[("keep", B, "k\n")],
        ),
        (
            "gitlink_updated",
            &[("vendor/sub", G, "sub@1")],
            &[("vendor/sub", G, "sub@2")],
        ),
        (
            "gitlink_dir_renamed",
            &[("vendor/sub", G, "sub@1"), ("vendor/README", B, "r\n")],
            &[
                ("third_party/sub", G, "sub@1"),
                ("third_party/README", B, "r\n"),
            ],
        ),
        (
            "dir_replaced_by_gitlink",
            &[("sub/x", B, "one\n")],
            &[("sub", G, "sub@1")],
        ),
        (
            "gitlink_replaced_by_dir",
            &[("sub", G, "sub@1")],
            &[("sub/x", B, "one\n")],
        ),
        (
            "file_replaced_by_gitlink",
            &[("sub", B, "file\n")],
            &[("sub", G, "sub@1")],
        ),
    ]
}

#[test]
fn diff_trees_with_renames_never_reports_tree_entries_bn_1ijl() {
    let (_dir, root) = init_test_repo();
    let repo = GixRepo::open(&root).expect("open repo");
    for (name, old_spec, new_spec) in shapes() {
        let old = flat_from_spec(&repo, old_spec);
        let new = flat_from_spec(&repo, new_spec);
        let old_tree = write_flat(&repo, &old);
        let new_tree = write_flat(&repo, &new);
        // Sanity: the fixture writer round-trips.
        assert_eq!(read_flat(&repo, old_tree), old, "[{name}] fixture old");
        assert_eq!(read_flat(&repo, new_tree), new, "[{name}] fixture new");
        for pct in [0, 50, 100] {
            let diff = repo
                .diff_trees_with_renames(Some(old_tree), new_tree, pct)
                .unwrap_or_else(|e| panic!("[{name} @{pct}%] diff failed: {e}"));
            check_diff(&repo, name, pct, &old, &new, &diff);
            // And the reverse direction exercises the mirrored shape.
            let back = repo
                .diff_trees_with_renames(Some(new_tree), old_tree, pct)
                .unwrap_or_else(|e| panic!("[{name} reversed @{pct}%] diff failed: {e}"));
            check_diff(&repo, &format!("{name} (reversed)"), pct, &new, &old, &back);
        }
        // Plain (non-rename) diff must satisfy the same invariants.
        let plain = repo
            .diff_trees(Some(old_tree), new_tree)
            .unwrap_or_else(|e| panic!("[{name}] plain diff failed: {e}"));
        check_diff(&repo, &format!("{name} (plain)"), 0, &old, &new, &plain);
    }
}

/// A directory rename must still surface as per-file renames (identity is
/// preserved for downstream overlap detection), not collapse into bare
/// delete+add pairs.
#[test]
fn diff_trees_with_renames_dir_rename_yields_file_renames_bn_1ijl() {
    let (_dir, root) = init_test_repo();
    let repo = GixRepo::open(&root).expect("open repo");
    let old = flat_from_spec(&repo, &[("a/x", B, "one\n"), ("a/y", B, "two\n")]);
    let new = flat_from_spec(&repo, &[("b/x", B, "one\n"), ("b/y", B, "two\n")]);
    let diff = repo
        .diff_trees_with_renames(Some(write_flat(&repo, &old)), write_flat(&repo, &new), 50)
        .expect("diff");
    let mut renames: Vec<(String, String)> = diff
        .iter()
        .filter_map(|e| match &e.change_type {
            ChangeType::Renamed { from } => Some((from.clone(), e.path.clone())),
            _ => None,
        })
        .collect();
    renames.sort();
    assert_eq!(
        renames,
        vec![
            ("a/x".to_owned(), "b/x".to_owned()),
            ("a/y".to_owned(), "b/y".to_owned())
        ],
        "full diff: {diff:?}"
    );
    assert_eq!(
        diff.len(),
        2,
        "only the two file renames expected: {diff:?}"
    );
}
