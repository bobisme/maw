//! LFS conformance tests: maw-lfs must produce byte-identical output to
//! git-lfs 3.7.1 for pointer blobs, stored objects, and smudged working
//! trees. These tests run both tools against the same fixture data and
//! cross-check the results.
//!
//! If `git-lfs` is not installed, every test in this file skips with a
//! diagnostic message so CI on minimal hosts stays green.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use maw_lfs::{Pointer, Store};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn have_git_lfs() -> bool {
    Command::new("git-lfs")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    if !ok {
        panic!(
            "git {args:?} failed in {cwd:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }
    String::from_utf8(stdout).expect("git stdout utf8")
}

fn git_raw(args: &[&str], cwd: &Path) -> Vec<u8> {
    let (stdout, stderr, ok) = run("git", args, cwd);
    if !ok {
        panic!("git {args:?} failed: {}", String::from_utf8_lossy(&stderr));
    }
    stdout
}

fn git_lfs(args: &[&str], cwd: &Path) -> Vec<u8> {
    let (stdout, stderr, ok) = run("git-lfs", args, cwd);
    if !ok {
        panic!(
            "git-lfs {args:?} failed: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
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
    .unwrap();
}

fn oid_from_hex(hex: &str) -> [u8; 32] {
    let hex = hex.trim();
    assert_eq!(hex.len(), 64, "bad hex len: {hex:?}");
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
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
        ("bytes_4k", (0..4096u32).map(|i| (i % 251) as u8).collect()),
        ("all_zeros_1k", vec![0u8; 1024]),
        ("all_ff_1k", vec![0xffu8; 1024]),
        ("mixed_binary_8k", (0..8192u32).map(|i| ((i.wrapping_mul(31)) % 256) as u8).collect()),
        ("newlines_only", vec![b'\n'; 512]),
        ("ascii_printable_3k", (0..3000).map(|i| ((i % 95) as u8) + 32).collect()),
        ("mib_1", (0..1024 * 1024u32).map(|i| (i % 256) as u8).collect()),
    ]
}

// ---------------------------------------------------------------------------
// Scenario 1: pointer byte-identity
// ---------------------------------------------------------------------------

#[test]
fn pointer_bytes_match_git_lfs() {
    skip_if_no_lfs!();

    use sha2::{Digest, Sha256};

    let tmp = tempfile::tempdir().unwrap();
    for (name, data) in fixtures() {
        // git-lfs 3.x intentionally emits no pointer for empty files
        // (they're stored as empty git blobs, never pointers). Skip.
        if data.is_empty() {
            continue;
        }
        let path = tmp.path().join(format!("fx-{name}"));
        fs::write(&path, &data).unwrap();

        // git-lfs pointer emits the canonical pointer on stdout.
        let lfs_out = git_lfs(
            &["pointer", &format!("--file={}", path.to_str().unwrap())],
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
        .write();

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
                .map(|i| (i.wrapping_mul(17) % 256) as u8)
                .collect(),
        ),
    ] {
        // Path A: git-lfs clean via git add/commit.
        let tmp_a = tempfile::tempdir().unwrap();
        let dir_a = tmp_a.path();
        init_test_repo(dir_a);
        write_gitattributes(dir_a, "*.bin");
        git(&["add", ".gitattributes"], dir_a);
        git(&["commit", "-q", "-m", "attrs"], dir_a);
        fs::write(dir_a.join("test.bin"), &data).unwrap();
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
        let stored_bytes_a = fs::read(&store_path_a).unwrap();

        // Path B: maw write_blob_with_path via maw-git.
        let tmp_b = tempfile::tempdir().unwrap();
        let dir_b = tmp_b.path();
        init_test_repo(dir_b);
        write_gitattributes(dir_b, "*.bin");
        // We don't need to commit the attrs for maw: AttrsMatcher reads the
        // file directly from workdir.
        let repo_b = maw_git::GixRepo::open(dir_b).unwrap();
        use maw_git::repo::GitRepo;
        let blob_oid_b = repo_b.write_blob_with_path(&data, "test.bin").unwrap();

        // Read back the blob maw wrote.
        let pointer_bytes_b = repo_b.read_blob(blob_oid_b).unwrap();
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
        let stored_bytes_b = fs::read(&store_path_b).unwrap();

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

// ---------------------------------------------------------------------------
// Scenario 3: smudge filter equivalence
// ---------------------------------------------------------------------------

#[test]
fn smudge_filter_equivalence() {
    skip_if_no_lfs!();

    let data: Vec<u8> = (0..128 * 1024u32)
        .map(|i| (i.wrapping_mul(7) % 256) as u8)
        .collect();

    // Build a repo via git-lfs: this produces a tree with the pointer blob
    // committed and the real object in .git/lfs/objects/.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_test_repo(dir);
    write_gitattributes(dir, "*.bin");
    git(&["add", ".gitattributes"], dir);
    git(&["commit", "-q", "-m", "attrs"], dir);
    fs::write(dir.join("test.bin"), &data).unwrap();
    git(&["add", "test.bin"], dir);
    git(&["commit", "-q", "-m", "add"], dir);

    // Path A: git-lfs smudge. Remove the working file then checkout to force
    // a smudge pass.
    fs::remove_file(dir.join("test.bin")).unwrap();
    git(&["checkout", "--", "test.bin"], dir);
    let smudged_a = fs::read(dir.join("test.bin")).unwrap();
    assert_eq!(smudged_a, data, "git-lfs smudge produced wrong content");

    // Path B: maw checkout_tree into a fresh workdir. Use the same .git
    // (with .git/lfs/objects already populated) to validate smudge.
    let tree_oid_str = git(&["rev-parse", "HEAD^{tree}"], dir).trim().to_owned();
    let tree_oid: maw_git::types::GitOid = tree_oid_str.parse().unwrap();

    // Checkout into a separate workdir root so we don't clobber git-lfs's output.
    let alt_workdir = tempfile::tempdir().unwrap();
    let repo = maw_git::GixRepo::open(dir).unwrap();
    use maw_git::repo::GitRepo;
    repo.checkout_tree(tree_oid, alt_workdir.path()).unwrap();

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
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_test_repo(dir);
    write_gitattributes(dir, "*.bin");

    // maw stores an object via Store::insert_from_reader.
    let store = Store::open(&dir.join(".git")).unwrap();
    let data: Vec<u8> = (0..32_768u32).map(|i| (i % 173) as u8).collect();
    let (pointer, _size) = store
        .insert_from_reader(std::io::Cursor::new(data.clone()))
        .unwrap();

    // Commit a pointer blob that references it.
    fs::write(dir.join("test.bin"), pointer.write()).unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_test_repo(dir);
    write_gitattributes(dir, "*.bin");
    git(&["add", ".gitattributes"], dir);
    git(&["commit", "-q", "-m", "attrs"], dir);

    let data: Vec<u8> = (0..7777u32)
        .map(|i| (i.wrapping_mul(11) % 256) as u8)
        .collect();
    fs::write(dir.join("test.bin"), &data).unwrap();
    git(&["add", "test.bin"], dir);
    git(&["commit", "-q", "-m", "blob"], dir);

    // Parse the pointer git-lfs committed, extract the oid.
    let blob_oid = git(&["rev-parse", "HEAD:test.bin"], dir).trim().to_owned();
    let pointer_bytes = git_raw(&["cat-file", "blob", &blob_oid], dir);
    let parsed = Pointer::parse(&pointer_bytes).unwrap();

    // maw reads it through Store::open_object.
    let store = Store::open(&dir.join(".git")).unwrap();
    assert!(
        store.contains(&parsed.oid),
        "maw Store cannot see git-lfs-stored object"
    );
    let mut reader = store.open_object(&parsed.oid).unwrap().unwrap();
    use std::io::Read;
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    assert_eq!(out, data, "maw read wrong bytes for git-lfs object");

    // Cross-check oid: maw's recomputed hash must match parsed oid.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&data);
    let recomputed: [u8; 32] = h.finalize().into();
    assert_eq!(recomputed, parsed.oid);

    // And the oid hex maps to the filesystem layout git-lfs used.
    let expected_path = store_path(&dir.join(".git"), &parsed.oid_hex());
    assert!(expected_path.is_file(), "expected {expected_path:?}");
    let _ = oid_from_hex; // silence unused in case of refactor
}
