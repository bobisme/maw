//! LFS conformance tests: maw-lfs must produce byte-identical output to
//! git-lfs 3.7.1 for pointer blobs, stored objects, and smudged working
//! trees. These tests run both tools against the same fixture data and
//! cross-check the results.
//!
//! If `git-lfs` is not installed, every test in this file skips with a
//! diagnostic message so CI on minimal hosts stays green.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use maw_git::repo::GitRepo;
use maw_lfs::{Pointer, Store};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn have_git_lfs() -> bool {
    Command::new("git-lfs")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Skip guard: returns true and prints if git-lfs missing.
macro_rules! skip_if_no_lfs {
    () => {
        if !have_git_lfs() {
            eprintln!("skipping conformance tests: git-lfs not available");
            return;
        }
    };
}

fn run(prog: &str, args: &[&str], cwd: &Path) -> (Vec<u8>, Vec<u8>, bool) {
    let out = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn {prog} {args:?}: {e}"));
    (out.stdout, out.stderr, out.status.success())
}

fn git(args: &[&str], cwd: &Path) -> String {
    let (stdout, stderr, ok) = run("git", args, cwd);
    assert!(
        ok,
        "git {args:?} failed in {}\nstdout:\n{}\nstderr:\n{}",
        cwd.display(),
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    String::from_utf8(stdout).expect("git stdout utf8")
}

fn git_raw(args: &[&str], cwd: &Path) -> Vec<u8> {
    let (stdout, stderr, ok) = run("git", args, cwd);
    assert!(
        ok,
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    stdout
}

fn git_lfs(args: &[&str], cwd: &Path) -> Vec<u8> {
    let (stdout, stderr, ok) = run("git-lfs", args, cwd);
    assert!(
        ok,
        "git-lfs {args:?} failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    stdout
}

/// Fresh repo with user config and the LFS filters configured locally.
fn init_test_repo(dir: &Path) {
    git(&["init", "-q", "-b", "main"], dir);
    git(&["config", "user.email", "conformance@maw.test"], dir);
    git(&["config", "user.name", "Conformance"], dir);
    git(&["config", "commit.gpgsign", "false"], dir);
    // Install LFS filters into this repo's .git/config (belt-and-braces —
    // global install may already provide them, but local overrides are safest).
    let _ = Command::new("git-lfs")
        .args(["install", "--local"])
        .current_dir(dir)
        .output();
}

fn write_gitattributes(dir: &Path, pattern: &str) {
    fs::write(
        dir.join(".gitattributes"),
        format!("{pattern} filter=lfs diff=lfs merge=lfs -text\n"),
    )
    .expect("operation should succeed");
}

fn oid_from_hex(hex: &str) -> [u8; 32] {
    let hex = hex.trim();
    assert_eq!(hex.len(), 64, "bad hex len: {hex:?}");
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("operation should succeed");
    }
    out
}

fn checked_byte(value: u32, modulus: u32) -> u8 {
    u8::try_from(value % modulus).expect("value reduced below byte range")
}

fn store_path(git_dir: &Path, oid_hex: &str) -> PathBuf {
    git_dir
        .join("lfs")
        .join("objects")
        .join(&oid_hex[0..2])
        .join(&oid_hex[2..4])
        .join(oid_hex)
}

// Fixture byte patterns.
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", vec![]),
        ("one_byte", vec![0x00]),
        ("text_100b", b"hello world, this is a sample text fixture for LFS conformance testing!! abcdefghijklmnopqrstuvw".to_vec()),
        ("bytes_4k", (0..4096u32).map(|i| checked_byte(i, 251)).collect()),
        ("all_zeros_1k", vec![0u8; 1024]),
        ("all_ff_1k", vec![0xffu8; 1024]),
        ("mixed_binary_8k", (0..8192u32).map(|i| checked_byte(i.wrapping_mul(31), 256)).collect()),
        ("newlines_only", vec![b'\n'; 512]),
        ("ascii_printable_3k", (0..3000u32).map(|i| checked_byte(i, 95) + 32).collect()),
        ("mib_1", (0..1024 * 1024u32).map(|i| checked_byte(i, 256)).collect()),
    ]
}

// ---------------------------------------------------------------------------
// Scenario 1: pointer byte-identity
// ---------------------------------------------------------------------------

#[test]
fn pointer_bytes_match_git_lfs() {
    skip_if_no_lfs!();

    let tmp = tempfile::tempdir().expect("operation should succeed");
    for (name, data) in fixtures() {
        // git-lfs 3.x intentionally emits no pointer for empty files
        // (they're stored as empty git blobs, never pointers). Skip.
        if data.is_empty() {
            continue;
        }
        let path = tmp.path().join(format!("fx-{name}"));
        fs::write(&path, &data).expect("operation should succeed");

        // git-lfs pointer emits the canonical pointer on stdout.
        let lfs_out = git_lfs(
            &[
                "pointer",
                &format!(
                    "--file={}",
                    path.to_str().expect("operation should succeed")
                ),
            ],
            tmp.path(),
        );

        // maw side: hash ourselves, build pointer.
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let oid: [u8; 32] = hasher.finalize().into();
        let maw_bytes = Pointer {
            oid,
            size: data.len() as u64,
            extensions: vec![],
        }
        .write()
        .expect("valid pointer");

        assert_eq!(
            maw_bytes,
            lfs_out,
            "pointer mismatch for fixture {name:?}\nmaw   : {:?}\nlfs   : {:?}",
            String::from_utf8_lossy(&maw_bytes),
            String::from_utf8_lossy(&lfs_out)
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: clean filter equivalence
// ---------------------------------------------------------------------------

#[test]
fn clean_filter_equivalence() {
    skip_if_no_lfs!();

    for (name, data) in [
        (
            "small",
            b"the quick brown fox jumps over the lazy dog\n".to_vec(),
        ),
        (
            "ten_mib",
            (0..10 * 1024 * 1024u32)
                .map(|i| checked_byte(i.wrapping_mul(17), 256))
                .collect(),
        ),
    ] {
        // Path A: git-lfs clean via git add/commit.
        let tmp_a = tempfile::tempdir().expect("operation should succeed");
        let dir_a = tmp_a.path();
        init_test_repo(dir_a);
        write_gitattributes(dir_a, "*.bin");
        git(&["add", ".gitattributes"], dir_a);
        git(&["commit", "-q", "-m", "attrs"], dir_a);
        fs::write(dir_a.join("test.bin"), &data).expect("operation should succeed");
        git(&["add", "test.bin"], dir_a);
        git(&["commit", "-q", "-m", "add blob"], dir_a);

        let blob_oid_a = git(&["rev-parse", "HEAD:test.bin"], dir_a)
            .trim()
            .to_owned();
        let pointer_bytes_a = git_raw(&["cat-file", "blob", &blob_oid_a], dir_a);
        let parsed_a = Pointer::parse(&pointer_bytes_a)
            .unwrap_or_else(|e| panic!("[{name}] git-lfs emitted non-parseable pointer: {e}"));
        let hex_a = parsed_a.oid_hex();
        let store_path_a = store_path(&dir_a.join(".git"), &hex_a);
        assert!(
            store_path_a.is_file(),
            "[{name}] git-lfs did not create stored object at {store_path_a:?}"
        );
        let stored_bytes_a = fs::read(&store_path_a).expect("operation should succeed");

        // Path B: maw write_blob_with_path via maw-git.
        let tmp_b = tempfile::tempdir().expect("operation should succeed");
        let dir_b = tmp_b.path();
        init_test_repo(dir_b);
        write_gitattributes(dir_b, "*.bin");
        // We don't need to commit the attrs for maw: AttrsMatcher reads the
        // file directly from workdir.
        let repo_b = maw_git::GixRepo::open(dir_b).expect("operation should succeed");
        let blob_oid_b = repo_b
            .write_blob_with_path(&data, "test.bin")
            .expect("operation should succeed");

        // Read back the blob maw wrote.
        let pointer_bytes_b = repo_b
            .read_blob(blob_oid_b)
            .expect("operation should succeed");
        let parsed_b = Pointer::parse(&pointer_bytes_b)
            .unwrap_or_else(|e| panic!("[{name}] maw produced non-parseable pointer: {e}"));
        let hex_b = parsed_b.oid_hex();

        // Cross-check: pointer bytes bit-identical.
        assert_eq!(
            pointer_bytes_a,
            pointer_bytes_b,
            "[{name}] pointer byte mismatch:\nlfs: {:?}\nmaw: {:?}",
            String::from_utf8_lossy(&pointer_bytes_a),
            String::from_utf8_lossy(&pointer_bytes_b)
        );

        // Git blob OIDs must agree (follows from pointer equivalence + git's
        // hash of identical content).
        assert_eq!(
            blob_oid_a,
            blob_oid_b.to_string(),
            "[{name}] git blob OID mismatch"
        );

        // Store path layout is identical — relative path under <git_dir>/lfs.
        assert_eq!(hex_a, hex_b, "[{name}] sha256 mismatch");
        let store_path_b = store_path(&dir_b.join(".git"), &hex_b);
        assert!(
            store_path_b.is_file(),
            "[{name}] maw did not create stored object at {store_path_b:?}"
        );
        let stored_bytes_b = fs::read(&store_path_b).expect("operation should succeed");

        // Stored content bit-identical.
        assert_eq!(
            stored_bytes_a.len(),
            stored_bytes_b.len(),
            "[{name}] stored object size mismatch"
        );
        assert_eq!(
            stored_bytes_a, stored_bytes_b,
            "[{name}] stored object content mismatch"
        );
        // And equal to the original input.
        assert_eq!(stored_bytes_a, data, "[{name}] stored object != input");
    }
}

/// `git-lfs clean` with `data` on stdin; returns what git-lfs hands to git.
fn git_lfs_clean_stdin(dir: &Path, name: &str, data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut child = Command::new("git-lfs")
        .args(["clean", name])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git-lfs clean");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(data)
        .expect("write stdin");
    let out = child.wait_with_output().expect("git-lfs clean");
    assert!(
        out.status.success(),
        "git-lfs clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// bn-ggo5: maw's clean must pass pointer-shaped content through verbatim
/// exactly when git-lfs does, and clean it exactly when git-lfs does.
///
/// git-lfs passes content through iff its lenient `DecodeFrom` accepts it
/// (whitespace-trimmed, blank lines and trailing CRs ignored, legacy
/// `hawser` / `git-media` version URLs, `size 012`, extensions after the
/// oid line). bn-z9t3 replaced maw's prefix sniff with the STRICT canonical
/// parser, so a non-canonical pointer that git-lfs keeps as-is was wrapped
/// into a new LFS object by maw — a pointer to a pointer, and the real
/// object reference silently disappears from the committed tree.
const PTR_OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
const PTR_OID2: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// Pointer-shaped inputs for [`pointer_shaped_clean_passthrough_matches_git_lfs`].
fn pointer_shaped_cases() -> Vec<(&'static str, String)> {
    let v = "version https://git-lfs.github.com/spec/v1";
    vec![
        ("canonical", format!("{v}\noid sha256:{PTR_OID}\nsize 12\n")),
        (
            "no_final_newline",
            format!("{v}\noid sha256:{PTR_OID}\nsize 12"),
        ),
        (
            "extra_blank_line",
            format!("{v}\noid sha256:{PTR_OID}\nsize 12\n\n"),
        ),
        (
            "leading_blank_line",
            format!("\n{v}\noid sha256:{PTR_OID}\nsize 12\n"),
        ),
        (
            "interior_blank_line",
            format!("{v}\n\noid sha256:{PTR_OID}\n\nsize 12\n"),
        ),
        (
            "crlf",
            format!("{v}\r\noid sha256:{PTR_OID}\r\nsize 12\r\n"),
        ),
        (
            "hawser",
            format!("version https://hawser.github.com/spec/v1\noid sha256:{PTR_OID}\nsize 12\n"),
        ),
        (
            "git_media",
            format!("version http://git-media.io/v/2\noid sha256:{PTR_OID}\nsize 12\n"),
        ),
        (
            "size_leading_zero",
            format!("{v}\noid sha256:{PTR_OID}\nsize 012\n"),
        ),
        (
            "size_plus",
            format!("{v}\noid sha256:{PTR_OID}\nsize +12\n"),
        ),
        (
            "size_trailing_space",
            format!("{v}\noid sha256:{PTR_OID}\nsize 12 \n"),
        ),
        (
            "ext_before_oid",
            format!("{v}\next-0-foo sha256:{PTR_OID2}\noid sha256:{PTR_OID}\nsize 12\n"),
        ),
        (
            "ext_after_oid",
            format!("{v}\noid sha256:{PTR_OID}\next-0-foo sha256:{PTR_OID2}\nsize 12\n"),
        ),
        (
            "ext_dup_priority",
            format!(
                "{v}\next-0-a sha256:{PTR_OID2}\next-0-b sha256:{PTR_OID2}\noid sha256:{PTR_OID}\nsize 12\n"
            ),
        ),
        // Content git-lfs cleans (stores as a new object):
        (
            "upper_hex",
            format!("{v}\noid sha256:{}\nsize 12\n", PTR_OID.to_uppercase()),
        ),
        (
            "size_first",
            format!("{v}\nsize 12\noid sha256:{PTR_OID}\n"),
        ),
        (
            "trailing_unknown_key",
            format!("{v}\noid sha256:{PTR_OID}\nsize 12\nfoo bar\n"),
        ),
        (
            "oid_too_long",
            format!("{v}\noid sha256:{PTR_OID}0\nsize 12\n"),
        ),
        (
            "negative_size",
            format!("{v}\noid sha256:{PTR_OID}\nsize -1\n"),
        ),
        ("missing_size", format!("{v}\noid sha256:{PTR_OID}\n")),
        ("doc_about_lfs", format!("{v}\nThis file documents LFS.\n")),
        (
            "bad_version",
            format!("version https://example.com/v9\noid sha256:{PTR_OID}\nsize 12\n"),
        ),
        (
            "big_padding",
            format!("{v}\noid sha256:{PTR_OID}\nsize 12\n{}", "\n".repeat(1100)),
        ),
    ]
}

#[test]
fn pointer_shaped_clean_passthrough_matches_git_lfs() {
    skip_if_no_lfs!();
    let cases = pointer_shaped_cases();

    let tmp_a = tempfile::tempdir().expect("tempdir");
    init_test_repo(tmp_a.path());
    write_gitattributes(tmp_a.path(), "*.bin");
    let tmp_b = tempfile::tempdir().expect("tempdir");
    init_test_repo(tmp_b.path());
    write_gitattributes(tmp_b.path(), "*.bin");
    let repo_b = maw_git::GixRepo::open(tmp_b.path()).expect("open");

    let mut mismatches = Vec::new();
    for (name, data) in &cases {
        let lfs = git_lfs_clean_stdin(tmp_a.path(), "x.bin", data.as_bytes());
        let oid = repo_b
            .write_blob_with_path(data.as_bytes(), "x.bin")
            .expect("maw clean");
        let maw = repo_b.read_blob(oid).expect("read blob");
        if lfs != maw {
            mismatches.push(format!(
                "[{name}] git-lfs {} but maw {}",
                if lfs == data.as_bytes() {
                    "passes through"
                } else {
                    "cleans"
                },
                if maw == data.as_bytes() {
                    "passes through"
                } else {
                    "cleans"
                },
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

// ---------------------------------------------------------------------------
// Scenario 3: smudge filter equivalence
// ---------------------------------------------------------------------------

#[test]
fn smudge_filter_equivalence() {
    skip_if_no_lfs!();

    let data: Vec<u8> = (0..128 * 1024u32)
        .map(|i| checked_byte(i.wrapping_mul(7), 256))
        .collect();

    // Build a repo via git-lfs: this produces a tree with the pointer blob
    // committed and the real object in .git/lfs/objects/.
    let tmp = tempfile::tempdir().expect("operation should succeed");
    let dir = tmp.path();
    init_test_repo(dir);
    write_gitattributes(dir, "*.bin");
    git(&["add", ".gitattributes"], dir);
    git(&["commit", "-q", "-m", "attrs"], dir);
    fs::write(dir.join("test.bin"), &data).expect("operation should succeed");
    git(&["add", "test.bin"], dir);
    git(&["commit", "-q", "-m", "add"], dir);

    // Path A: git-lfs smudge. Remove the working file then checkout to force
    // a smudge pass.
    fs::remove_file(dir.join("test.bin")).expect("operation should succeed");
    git(&["checkout", "--", "test.bin"], dir);
    let smudged_a = fs::read(dir.join("test.bin")).expect("operation should succeed");
    assert_eq!(smudged_a, data, "git-lfs smudge produced wrong content");

    // Path B: maw checkout_tree into a fresh workdir. Use the same .git
    // (with .git/lfs/objects already populated) to validate smudge.
    let tree_oid_str = git(&["rev-parse", "HEAD^{tree}"], dir).trim().to_owned();
    let tree_oid: maw_git::types::GitOid = tree_oid_str.parse().expect("operation should succeed");

    // Checkout into a separate workdir root so we don't clobber git-lfs's output.
    let alt_workdir = tempfile::tempdir().expect("operation should succeed");
    let repo = maw_git::GixRepo::open(dir).expect("operation should succeed");
    repo.checkout_tree(tree_oid, alt_workdir.path())
        .expect("operation should succeed");

    let smudged_b = fs::read(alt_workdir.path().join("test.bin")).unwrap_or_else(|e| {
        panic!(
            "maw did not produce test.bin in alt workdir {:?}: {e}",
            alt_workdir.path()
        )
    });

    assert_eq!(
        smudged_a.len(),
        smudged_b.len(),
        "smudged size mismatch (lfs {} vs maw {})",
        smudged_a.len(),
        smudged_b.len()
    );
    assert_eq!(smudged_a, smudged_b, "smudged content mismatch");
}

// ---------------------------------------------------------------------------
// Scenario 4: store layout interop (both directions)
// ---------------------------------------------------------------------------

#[test]
fn store_interop_maw_to_lfs() {
    skip_if_no_lfs!();

    // Stand up a real git repo so that `git-lfs fsck` can run.
    let tmp = tempfile::tempdir().expect("operation should succeed");
    let dir = tmp.path();
    init_test_repo(dir);
    write_gitattributes(dir, "*.bin");

    // maw stores an object via Store::insert_from_reader.
    let store = Store::open(&dir.join(".git")).expect("operation should succeed");
    let data: Vec<u8> = (0..32_768u32).map(|i| checked_byte(i, 173)).collect();
    let (pointer, _size) = store
        .insert_from_reader(std::io::Cursor::new(data))
        .expect("operation should succeed");

    // Commit a pointer blob that references it.
    fs::write(
        dir.join("test.bin"),
        pointer.write().expect("valid pointer"),
    )
    .expect("operation should succeed");
    // Write .gitattributes THEN commit attrs+pointer in one shot — that way
    // git won't try to re-clean test.bin (it's already a pointer).
    git(&["add", ".gitattributes", "test.bin"], dir);
    git(&["commit", "-q", "-m", "pointer+attrs"], dir);

    // git-lfs fsck should accept the object maw placed.
    let (stdout, stderr, ok) = run("git-lfs", &["fsck", "--objects"], dir);
    assert!(
        ok,
        "git-lfs fsck --objects rejected maw-stored object\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn store_interop_lfs_to_maw() {
    skip_if_no_lfs!();

    // git-lfs stores an object via the clean filter during commit.
    let tmp = tempfile::tempdir().expect("operation should succeed");
    let dir = tmp.path();
    init_test_repo(dir);
    write_gitattributes(dir, "*.bin");
    git(&["add", ".gitattributes"], dir);
    git(&["commit", "-q", "-m", "attrs"], dir);

    let data: Vec<u8> = (0..7777u32)
        .map(|i| checked_byte(i.wrapping_mul(11), 256))
        .collect();
    fs::write(dir.join("test.bin"), &data).expect("operation should succeed");
    git(&["add", "test.bin"], dir);
    git(&["commit", "-q", "-m", "blob"], dir);

    // Parse the pointer git-lfs committed, extract the oid.
    let blob_oid = git(&["rev-parse", "HEAD:test.bin"], dir).trim().to_owned();
    let pointer_bytes = git_raw(&["cat-file", "blob", &blob_oid], dir);
    let parsed = Pointer::parse(&pointer_bytes).expect("operation should succeed");

    // maw reads it through Store::open_object.
    let store = Store::open(&dir.join(".git")).expect("operation should succeed");
    assert!(
        store.contains(&parsed.oid),
        "maw Store cannot see git-lfs-stored object"
    );
    let mut reader = store
        .open_object(&parsed.oid)
        .expect("operation should succeed")
        .expect("operation should succeed");
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .expect("operation should succeed");
    assert_eq!(out, data, "maw read wrong bytes for git-lfs object");

    // Cross-check oid: maw's recomputed hash must match parsed oid.
    let mut h = Sha256::new();
    h.update(&data);
    let recomputed: [u8; 32] = h.finalize().into();
    assert_eq!(recomputed, parsed.oid);

    // And the oid hex maps to the filesystem layout git-lfs used.
    let expected_path = store_path(&dir.join(".git"), &parsed.oid_hex());
    assert!(expected_path.is_file(), "expected {expected_path:?}");
    let _ = oid_from_hex; // silence unused in case of refactor
}

// ---------------------------------------------------------------------------
// Scenario 5 (bn-hcbc8): smudge decodes exactly what git-lfs smudge decodes
// ---------------------------------------------------------------------------

/// `git-lfs smudge` with `data` on stdin: `(stdout, success)`.
fn git_lfs_smudge_stdin(dir: &Path, name: &str, data: &[u8]) -> (Vec<u8>, bool) {
    use std::io::Write as _;
    let mut child = Command::new("git-lfs")
        .args(["smudge", name])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git-lfs smudge");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(data)
        .expect("write stdin");
    let out = child.wait_with_output().expect("git-lfs smudge");
    (out.stdout, out.status.success())
}

/// Pointer-shaped blobs for [`smudge_decodes_what_git_lfs_smudge_decodes`],
/// pointing at `oid` (a `size`-byte object present in the local store).
fn smudge_cases(oid: &str, size: usize) -> Vec<(&'static str, String)> {
    let v = "version https://git-lfs.github.com/spec/v1";
    let upper = oid.to_uppercase();
    let big = size + 7;
    vec![
        ("canonical", format!("{v}\noid sha256:{oid}\nsize {size}\n")),
        (
            "no_final_newline",
            format!("{v}\noid sha256:{oid}\nsize {size}"),
        ),
        (
            "extra_blank_line",
            format!("{v}\noid sha256:{oid}\nsize {size}\n\n"),
        ),
        (
            "leading_blank_line",
            format!("\n{v}\noid sha256:{oid}\nsize {size}\n"),
        ),
        (
            "interior_blank_line",
            format!("{v}\n\noid sha256:{oid}\n\nsize {size}\n"),
        ),
        (
            "crlf",
            format!("{v}\r\noid sha256:{oid}\r\nsize {size}\r\n"),
        ),
        (
            "leading_tab",
            format!("\t{v}\noid sha256:{oid}\nsize {size}\n"),
        ),
        (
            "trailing_spaces",
            format!("{v}\noid sha256:{oid}\nsize {size}\n  "),
        ),
        (
            "hawser",
            format!("version https://hawser.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n"),
        ),
        (
            "git_media",
            format!("version http://git-media.io/v/2\noid sha256:{oid}\nsize {size}\n"),
        ),
        (
            "size_leading_zero",
            format!("{v}\noid sha256:{oid}\nsize 0{size}\n"),
        ),
        (
            "size_plus",
            format!("{v}\noid sha256:{oid}\nsize +{size}\n"),
        ),
        (
            "size_trailing_space",
            format!("{v}\noid sha256:{oid}\nsize {size} \n"),
        ),
        // git-lfs writes nothing for a size-0 pointer, object or not.
        ("size_zero", format!("{v}\noid sha256:{oid}\nsize 0\n")),
        (
            "size_zero_missing",
            format!("{v}\noid sha256:{PTR_OID2}\nsize 0\n"),
        ),
        // git-lfs refuses (pointer stays): size mismatch, missing object,
        // unconfigured extension.
        (
            "size_mismatch",
            format!("{v}\noid sha256:{oid}\nsize {big}\n"),
        ),
        (
            "missing_object",
            format!("{v}\noid sha256:{PTR_OID2}\nsize 12\n"),
        ),
        (
            "extension",
            format!("{v}\next-0-foo sha256:{PTR_OID2}\noid sha256:{oid}\nsize {size}\n"),
        ),
        // Not pointers to git-lfs smudge (content passes through):
        (
            "upper_hex",
            format!("{v}\noid sha256:{upper}\nsize {size}\n"),
        ),
        (
            "size_first",
            format!("{v}\nsize {size}\noid sha256:{oid}\n"),
        ),
        (
            "trailing_unknown_key",
            format!("{v}\noid sha256:{oid}\nsize {size}\nfoo bar\n"),
        ),
        ("negative_size", format!("{v}\noid sha256:{oid}\nsize -1\n")),
        (
            "space_only_line",
            format!("{v}\n \noid sha256:{oid}\nsize {size}\n"),
        ),
        (
            "dup_ext_priority",
            format!(
                "{v}\next-0-a sha256:{PTR_OID2}\next-0-b sha256:{PTR_OID2}\noid sha256:{oid}\nsize {size}\n"
            ),
        ),
        ("doc_about_lfs", format!("{v}\nThis file documents LFS.\n")),
        (
            "bad_version",
            format!("version https://example.com/v9\noid sha256:{oid}\nsize {size}\n"),
        ),
    ]
}

/// Commit `blob` verbatim (no filters) as `x.bin` under an LFS
/// `.gitattributes`, and return the tree oid.
fn raw_lfs_tree(dir: &Path, blob: &[u8]) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--no-filters", "--stdin"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(blob)
        .expect("write");
    let out = child.wait_with_output().expect("hash-object");
    assert!(out.status.success());
    let blob_oid = String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let attrs_oid = git(&["hash-object", "-w", ".gitattributes"], dir)
        .trim()
        .to_owned();
    let index = dir.join(".git").join("maw-conformance-index");
    let _ = fs::remove_file(&index);
    let with_index = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8")
    };
    with_index(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{attrs_oid},.gitattributes"),
    ]);
    with_index(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob_oid},x.bin"),
    ]);
    with_index(&["write-tree"]).trim().to_owned()
}

/// bn-hcbc8: a blob git-lfs smudge decodes must be smudged by maw's checkout
/// too, and a blob git-lfs leaves alone must stay byte-for-byte as committed.
///
/// maw used the STRICT canonical parser on the smudge side, so a
/// non-canonical pointer (blank lines, CRLF, legacy version URL, `size 012`,
/// …) that git-lfs smudges stayed as pointer text in maw-materialized
/// worktrees. Conversely maw smudged extension pointers (git-lfs refuses them
/// without the extension program) and ignored size mismatches.
#[test]
fn smudge_decodes_what_git_lfs_smudge_decodes() {
    skip_if_no_lfs!();
    let content = b"hello world, smudge me!".to_vec();
    let oid_hex = format!("{:x}", Sha256::digest(&content));

    let tmp_a = tempfile::tempdir().expect("tempdir");
    init_test_repo(tmp_a.path());
    write_gitattributes(tmp_a.path(), "*.bin");
    let tmp_b = tempfile::tempdir().expect("tempdir");
    init_test_repo(tmp_b.path());
    write_gitattributes(tmp_b.path(), "*.bin");
    let store_b = Store::open(&tmp_b.path().join(".git")).expect("store");
    store_b
        .insert_from_reader(content.as_slice())
        .expect("store object");
    let repo_b = maw_git::GixRepo::open(tmp_b.path()).expect("open");

    let mut mismatches = Vec::new();
    for (name, blob) in smudge_cases(&oid_hex, content.len()) {
        // git-lfs may delete a local object whose size disagrees with the
        // pointer; re-store it before every case.
        let _ = git_lfs_clean_stdin(tmp_a.path(), "x.bin", &content);
        let (lfs_out, lfs_ok) = git_lfs_smudge_stdin(tmp_a.path(), "x.bin", blob.as_bytes());
        // On failure git-lfs leaves the pointer: the committed blob stays.
        let expected = if lfs_ok {
            lfs_out
        } else {
            blob.as_bytes().to_vec()
        };

        let tree = raw_lfs_tree(tmp_b.path(), blob.as_bytes());
        let workdir = tempfile::tempdir().expect("tempdir");
        repo_b
            .checkout_tree(tree.parse().expect("oid"), workdir.path())
            .expect("maw checkout_tree");
        let maw_out = fs::read(workdir.path().join("x.bin")).expect("read x.bin");
        if maw_out != expected {
            let describe = |b: &[u8]| {
                if b == content.as_slice() {
                    "smudges".to_owned()
                } else if b == blob.as_bytes() {
                    "leaves the pointer".to_owned()
                } else if b.is_empty() {
                    "writes an empty file".to_owned()
                } else {
                    format!("writes {:?}", String::from_utf8_lossy(b))
                }
            };
            mismatches.push(format!(
                "[{name}] git-lfs {} but maw {}",
                describe(&expected),
                describe(&maw_out)
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// bn-hcbc8, deliberate divergence: git-lfs smudge decodes only the first
/// 1024 bytes of a blob, so a pointer followed by >1024 bytes of whitespace
/// AND further content is smudged, discarding the tail. git-lfs's own clean
/// treats that blob as content, not a pointer; maw keeps it verbatim rather
/// than drop bytes (bn-1b3n: never smudge a non-pointer file).
#[test]
fn smudge_never_discards_content_past_the_pointer_cutoff() {
    skip_if_no_lfs!();
    let content = b"hello world, smudge me!".to_vec();
    let oid_hex = format!("{:x}", Sha256::digest(&content));
    let tmp = tempfile::tempdir().expect("tempdir");
    init_test_repo(tmp.path());
    write_gitattributes(tmp.path(), "*.bin");
    Store::open(&tmp.path().join(".git"))
        .expect("store")
        .insert_from_reader(content.as_slice())
        .expect("store object");
    let blob = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid_hex}\nsize {}\n{}TAIL\n",
        content.len(),
        "\n".repeat(1100)
    );
    let tree = raw_lfs_tree(tmp.path(), blob.as_bytes());
    let workdir = tempfile::tempdir().expect("tempdir");
    maw_git::GixRepo::open(tmp.path())
        .expect("open")
        .checkout_tree(tree.parse().expect("oid"), workdir.path())
        .expect("checkout");
    assert_eq!(
        fs::read(workdir.path().join("x.bin")).expect("read"),
        blob.as_bytes()
    );
}
