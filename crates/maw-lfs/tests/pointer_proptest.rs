//! Property tests for the LFS pointer codec (bn-z9t3).
//!
//! Invariants:
//! - For every valid pointer `p`: `parse(write(p)) == p`.
//! - For every byte string `b` (here: canonical pointers with 1-3 random
//!   edits): `parse(b).is_ok()` implies `looks_like_pointer(b)` and
//!   `write(parse(b)) == b` (parse accepts only canonical encodings).
//! - Differential, when `git-lfs` is installed: whatever maw parses as a
//!   pointer, `git lfs pointer --check` also accepts (no false positives
//!   relative to git-lfs), and every pointer maw writes passes
//!   `git lfs pointer --check --strict` (except size 0, which git-lfs calls
//!   non-canonical because it writes empty files as empty blobs).

use std::fmt::Write as _;
use std::process::Command;

use maw_lfs::{Pointer, looks_like_pointer};
use proptest::prelude::*;

fn pt_config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    }
}

fn hex(oid: &[u8; 32]) -> String {
    // Independent of maw_lfs::hex on purpose.
    oid.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn arb_size() -> impl Strategy<Value = u64> {
    prop_oneof![
        0u64..=1024,
        any::<u64>().prop_map(|n| n >> 1), // 0..=i64::MAX
        Just(i64::MAX.unsigned_abs()),
    ]
}

fn arb_extensions() -> impl Strategy<Value = Vec<(String, String)>> {
    proptest::collection::btree_map(
        0u8..=9,
        ("[A-Za-z0-9_][A-Za-z0-9_.-]{0,8}", any::<[u8; 32]>()),
        0..=3,
    )
    .prop_map(|m| {
        m.into_iter()
            .map(|(prio, (name, oid))| {
                (
                    format!("ext-{prio}-{name}"),
                    format!("sha256:{}", hex(&oid)),
                )
            })
            .collect()
    })
}

fn arb_pointer() -> impl Strategy<Value = Pointer> {
    (any::<[u8; 32]>(), arb_size(), arb_extensions()).prop_map(|(oid, size, extensions)| Pointer {
        oid,
        size,
        extensions,
    })
}

/// Bytes that are likely to produce near-miss pointers.
fn arb_edit_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        any::<u8>(),
        proptest::sample::select(b"+-0123456789 \n\r\tAaFfgxz:._".to_vec()),
    ]
}

#[derive(Debug, Clone)]
enum Edit {
    Replace(usize, u8),
    Insert(usize, u8),
    Delete(usize),
    SwapLines(usize, usize),
    DupLine(usize),
    Truncate(usize),
    /// Insert a byte at the start of line `.0`'s value (after the first
    /// space), e.g. `size +5`, `size 05`, `oid  sha256:`.
    ValueStart(usize, u8),
    /// Insert a whole line before line `.0`.
    InsertLine(usize, Vec<u8>),
}

/// Lines that are plausible in or near a pointer.
fn arb_line() -> impl Strategy<Value = Vec<u8>> {
    proptest::sample::select(vec![
        "\n".to_owned(),
        "foo bar\n".to_owned(),
        "extra value-x\n".to_owned(),
        format!("ext-5-q sha256:{}\n", "0".repeat(64)),
        format!("ext-0-A_b.c sha256:{}\n", "e".repeat(64)),
        format!("ext-9-z bogus{}\n", "0".repeat(64)),
        format!("extension sha256:{}\n", "1".repeat(64)),
        format!("ext-10-q sha256:{}\n", "2".repeat(64)),
        format!("ext-4- sha256:{}\n", "3".repeat(64)),
        "size 3\n".to_owned(),
        format!("oid sha256:{}\n", "a".repeat(64)),
        "version https://git-lfs.github.com/spec/v1\n".to_owned(),
        "version https://hawser.github.com/spec/v1\n".to_owned(),
    ])
    .prop_map(String::into_bytes)
}

fn arb_edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<usize>(), arb_edit_byte()).prop_map(|(i, b)| Edit::Replace(i, b)),
        (any::<usize>(), arb_edit_byte()).prop_map(|(i, b)| Edit::Insert(i, b)),
        any::<usize>().prop_map(Edit::Delete),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Edit::SwapLines(a, b)),
        any::<usize>().prop_map(Edit::DupLine),
        any::<usize>().prop_map(Edit::Truncate),
        (any::<usize>(), proptest::sample::select(b"+-0 \t".to_vec()))
            .prop_map(|(i, b)| Edit::ValueStart(i, b)),
        (any::<usize>(), arb_line()).prop_map(|(i, l)| Edit::InsertLine(i, l)),
    ]
}

fn apply(bytes: &mut Vec<u8>, edit: &Edit) {
    let n = bytes.len();
    match edit {
        &Edit::Replace(i, b) if n > 0 => bytes[i % n] = b,
        &Edit::Insert(i, b) => bytes.insert(i % (n + 1), b),
        &Edit::Delete(i) if n > 0 => {
            bytes.remove(i % n);
        }
        &Edit::SwapLines(a, b) => edit_lines(bytes, |lines| {
            let len = lines.len();
            lines.swap(a % len, b % len);
        }),
        &Edit::DupLine(a) => edit_lines(bytes, |lines| {
            let i = a % lines.len();
            let l = lines[i].clone();
            lines.insert(i, l);
        }),
        &Edit::Truncate(i) => bytes.truncate(i % (n + 1)),
        &Edit::ValueStart(i, b) => edit_lines(bytes, |lines| {
            let len = lines.len();
            let line = &mut lines[i % len];
            let at = line.iter().position(|&c| c == b' ').map_or(0, |p| p + 1);
            line.insert(at, b);
        }),
        Edit::InsertLine(i, l) => edit_lines(bytes, |lines| {
            let at = i % (lines.len() + 1);
            lines.insert(at, l.clone());
        }),
        _ => {}
    }
}

fn edit_lines(bytes: &mut Vec<u8>, f: impl FnOnce(&mut Vec<Vec<u8>>)) {
    let mut lines: Vec<Vec<u8>> = bytes
        .split_inclusive(|&c| c == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if lines.is_empty() {
        return;
    }
    f(&mut lines);
    *bytes = lines.concat();
}

/// Independent spec oracle for the line structure git-lfs accepts (see
/// `decodeKVData` / `parsePointerExtension` in git-lfs `lfs/pointer.go`):
/// `version` first, then `ext-<digit>-<word>...` lines with distinct,
/// ascending priorities and `sha256:` values, then `oid`, then `size`, then
/// nothing. Checks structure only; value encodings are covered by the
/// canonical round trip and the git-lfs differential.
fn spec_line_structure_ok(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some(body) = text.strip_suffix('\n') else {
        return false;
    };
    let lines: Vec<&str> = body.split('\n').collect();
    let n = lines.len();
    if n < 3
        || lines[0] != "version https://git-lfs.github.com/spec/v1"
        || !lines[n - 2].starts_with("oid sha256:")
        || !lines[n - 1].starts_with("size ")
    {
        return false;
    }
    let mut last_priority: Option<u32> = None;
    for line in &lines[1..n - 2] {
        let Some((key, value)) = line.split_once(' ') else {
            return false;
        };
        let parts: Vec<&str> = key.splitn(3, '-').collect();
        let [ext, digit, name] = parts.as_slice() else {
            return false;
        };
        let Some(priority) = digit.chars().next().and_then(|c| c.to_digit(10)) else {
            return false;
        };
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        if *ext != "ext"
            || digit.len() != 1
            || !name.starts_with(word)
            || !value.starts_with("sha256:")
            || last_priority.is_some_and(|p| p >= priority)
        {
            return false;
        }
        last_priority = Some(priority);
    }
    true
}

fn arb_near_pointer() -> impl Strategy<Value = Vec<u8>> {
    (arb_pointer(), proptest::collection::vec(arb_edit(), 1..=3)).prop_map(|(p, edits)| {
        let mut bytes = p.write().expect("generated pointers are valid");
        for e in &edits {
            apply(&mut bytes, e);
        }
        bytes
    })
}

proptest! {
    #![proptest_config(pt_config(2048))]

    #[test]
    fn write_then_parse_is_identity(p in arb_pointer()) {
        let bytes = p.write().expect("generated pointers are valid");
        prop_assert!(looks_like_pointer(&bytes));
        prop_assert_eq!(Pointer::parse(&bytes), Ok(p));
    }

    #[test]
    fn parse_accepts_only_canonical_and_sniffer_agrees(bytes in arb_near_pointer()) {
        if let Ok(p) = Pointer::parse(&bytes) {
            prop_assert!(looks_like_pointer(&bytes));
            prop_assert!(
                spec_line_structure_ok(&bytes),
                "parse accepted a line structure git-lfs rejects: {:?}",
                String::from_utf8_lossy(&bytes)
            );
            prop_assert_eq!(p.write(), Ok(bytes));
        }
    }

    #[test]
    fn parse_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..1100)) {
        if Pointer::parse(&bytes).is_ok() {
            prop_assert!(looks_like_pointer(&bytes));
        }
    }
}

// ---------------------------------------------------------------------------
// Differential against the real git-lfs binary (skips if not installed).
// ---------------------------------------------------------------------------

fn have_git_lfs() -> bool {
    Command::new("git-lfs")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Exit status of `git lfs pointer --check [--strict] --file <bytes>`.
fn git_lfs_check(bytes: &[u8], strict: bool) -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("p");
    std::fs::write(&path, bytes).expect("write pointer file");
    let mut cmd = Command::new("git-lfs");
    cmd.args(["pointer", "--check"]);
    if strict {
        cmd.arg("--strict");
    }
    cmd.arg("--file").arg(&path).current_dir(dir.path());
    cmd.output().expect("run git-lfs").status.success()
}

proptest! {
    #![proptest_config(pt_config(128))]

    #[test]
    fn differential_maw_accepts_subset_of_git_lfs(bytes in arb_near_pointer()) {
        if !have_git_lfs() {
            return Ok(());
        }
        if Pointer::parse(&bytes).is_ok() {
            prop_assert!(
                git_lfs_check(&bytes, false),
                "maw parses as a pointer but git-lfs rejects: {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }

    #[test]
    fn differential_maw_writes_git_lfs_strict_pointers(p in arb_pointer()) {
        if !have_git_lfs() {
            return Ok(());
        }
        let bytes = p.write().expect("generated pointers are valid");
        prop_assert!(git_lfs_check(&bytes, false), "{:?}", String::from_utf8_lossy(&bytes));
        if p.size > 0 {
            prop_assert!(git_lfs_check(&bytes, true), "{:?}", String::from_utf8_lossy(&bytes));
        }
    }
}

/// Hand-picked near misses: each is accepted by `git lfs pointer --check`
/// (non-strict) but is not something git-lfs writes. maw rejects them all.
/// Documents the deliberate gap between the two accept sets.
#[test]
fn git_lfs_lenient_forms_maw_rejects() {
    let o = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
    let v = "version https://git-lfs.github.com/spec/v1";
    let cases = [
        format!("{v}\noid sha256:{o}\nsize +5\n"),
        format!("{v}\noid sha256:{o}\nsize 05\n"),
        format!("{v}\noid sha256:{o}\nsize -0\n"),
        format!("{v}\noid sha256:{o}\nsize 5 \n"),
        format!("{v}\n\noid sha256:{o}\nsize 5\n"),
        format!("{v}\noid sha256:{o}\nsize 5"),
        format!("{v}\r\noid sha256:{o}\r\nsize 5\r\n"),
        format!("{v}\noid sha256:{o}\next-0-foo sha256:{o}\nsize 5\n"),
        format!("version https://hawser.github.com/spec/v1\noid sha256:{o}\nsize 5\n"),
    ];
    let lfs = have_git_lfs();
    for c in &cases {
        assert!(Pointer::parse(c.as_bytes()).is_err(), "maw accepted {c:?}");
        if lfs {
            assert!(git_lfs_check(c.as_bytes(), false), "git-lfs rejected {c:?}");
            assert!(
                !git_lfs_check(c.as_bytes(), true),
                "git-lfs --strict accepted {c:?}"
            );
        }
    }
}

/// Non-vacuity: the near-pointer generator must produce both accepted and
/// rejected inputs, or the implication properties above test nothing.
#[test]
fn near_pointer_generator_is_not_vacuous() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strategy = arb_near_pointer();
    let (mut ok, mut err) = (0u32, 0u32);
    for _ in 0..2000 {
        let bytes = strategy.new_tree(&mut runner).expect("generate").current();
        if Pointer::parse(&bytes).is_ok() {
            ok += 1;
        } else {
            err += 1;
        }
    }
    eprintln!("near pointers: {ok} accepted, {err} rejected");
    assert!(ok >= 25, "too few accepted near pointers: {ok}");
    assert!(err >= 500, "too few rejected near pointers: {err}");
}
