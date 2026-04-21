//! Content-addressed local LFS object store under `.git/lfs/objects/`.
//!
//! Layout: `<git_dir>/lfs/objects/<sha[0:2]>/<sha[2:4]>/<full-sha>`.
//! Bit-identical to git-lfs's on-disk layout for full interoperability.
//!
//! All writes are atomic (tempfile + rename within the destination directory)
//! and streaming (constant memory usage regardless of file size).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::pointer::Pointer;

const BUF_SIZE: usize = 64 * 1024;

/// A handle to the local LFS object store.
pub struct Store {
    /// The LFS root: `<git_dir>/lfs`.
    root: PathBuf,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "content mismatch: expected oid {expected} size {expected_size}, got oid {got} size {got_size}"
    )]
    ContentMismatch {
        expected: String,
        expected_size: u64,
        got: String,
        got_size: u64,
    },
}

fn io_err(path: impl Into<PathBuf>, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.into(),
        source,
    }
}

fn oid_hex(oid: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in oid {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

impl Store {
    /// Open (and create if missing) the LFS store under `<git_dir>/lfs`.
    pub fn open(git_dir: &Path) -> Result<Self, StoreError> {
        let root = git_dir.join("lfs");
        let objects = root.join("objects");
        fs::create_dir_all(&objects).map_err(|e| io_err(&objects, e))?;
        Ok(Store { root })
    }

    /// Path to the real file for a given oid (may or may not exist).
    pub fn object_path(&self, oid: &[u8; 32]) -> PathBuf {
        let hex = oid_hex(oid);
        self.root
            .join("objects")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }

    /// Is this object present in the local store?
    pub fn contains(&self, oid: &[u8; 32]) -> bool {
        self.object_path(oid).is_file()
    }

    /// Open a reader for the real bytes, or None if the object is missing.
    pub fn open_object(&self, oid: &[u8; 32]) -> Result<Option<Box<dyn Read + Send>>, StoreError> {
        let path = self.object_path(oid);
        match fs::File::open(&path) {
            Ok(f) => Ok(Some(Box::new(f))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    /// Stream bytes in from a reader, hashing as we go, then atomically
    /// land the file at its content-addressed final path.
    ///
    /// Returns the built pointer and the byte count.
    /// Idempotent: if an object with the computed oid already exists, the
    /// temp file is dropped and the existing file is kept.
    pub fn insert_from_reader<R: Read>(&self, reader: R) -> Result<(Pointer, u64), StoreError> {
        let (oid, size, tmp_path) = self.stream_to_tmp(reader, None, None)?;
        let final_path = self.object_path(&oid);
        self.commit_tmp(tmp_path, &final_path)?;
        Ok((
            Pointer {
                oid,
                size,
                extensions: Vec::new(),
            },
            size,
        ))
    }

    /// Stream bytes in while verifying them against a known oid + size.
    /// On mismatch, the temp file is cleaned up.
    pub fn insert_from_stream<R: Read>(
        &self,
        expected_oid: &[u8; 32],
        expected_size: u64,
        reader: R,
    ) -> Result<(), StoreError> {
        let (oid, size, tmp_path) =
            self.stream_to_tmp(reader, Some(*expected_oid), Some(expected_size))?;
        if oid != *expected_oid || size != expected_size {
            // stream_to_tmp already cleaned up on mismatch. Belt-and-braces:
            let _ = fs::remove_file(&tmp_path);
            return Err(StoreError::ContentMismatch {
                expected: oid_hex(expected_oid),
                expected_size,
                got: oid_hex(&oid),
                got_size: size,
            });
        }
        let final_path = self.object_path(&oid);
        self.commit_tmp(tmp_path, &final_path)?;
        Ok(())
    }

    /// Stream `reader` to a temp file inside the lfs tmp directory, hashing
    /// on the fly. If `expected_*` are set and verification fails, the
    /// temp file is removed before returning.
    fn stream_to_tmp<R: Read>(
        &self,
        mut reader: R,
        expected_oid: Option<[u8; 32]>,
        expected_size: Option<u64>,
    ) -> Result<([u8; 32], u64, PathBuf), StoreError> {
        let tmp_dir = self.root.join("tmp");
        fs::create_dir_all(&tmp_dir).map_err(|e| io_err(&tmp_dir, e))?;

        // Create an anonymous temp file in the lfs tmp dir. Same filesystem
        // as the final destination so rename is atomic.
        let mut named =
            tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(&tmp_dir, e))?;
        let tmp_path = named.path().to_owned();

        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BUF_SIZE];
        let mut total: u64 = 0;

        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    let _ = named.close();
                    return Err(io_err(&tmp_path, e));
                }
            };
            hasher.update(&buf[..n]);
            if let Err(e) = named.as_file_mut().write_all(&buf[..n]) {
                let _ = named.close();
                return Err(io_err(&tmp_path, e));
            }
            total += n as u64;

            // Early abort on size overflow if verifying.
            if let Some(es) = expected_size {
                if total > es {
                    let _ = named.close();
                    return Err(StoreError::ContentMismatch {
                        expected: expected_oid.as_ref().map(oid_hex).unwrap_or_default(),
                        expected_size: es,
                        got: String::new(),
                        got_size: total,
                    });
                }
            }
        }

        if let Err(e) = named.as_file_mut().sync_all() {
            let _ = named.close();
            return Err(io_err(&tmp_path, e));
        }

        let oid_bytes: [u8; 32] = hasher.finalize().into();

        // Persist keeps the file around. We rename in commit_tmp().
        // Detach from NamedTempFile so its Drop doesn't delete.
        let (_file, persisted) = named.keep().map_err(|e| io_err(&tmp_path, e.error))?;

        if let (Some(eo), Some(es)) = (expected_oid, expected_size) {
            if oid_bytes != eo || total != es {
                let _ = fs::remove_file(&persisted);
                return Err(StoreError::ContentMismatch {
                    expected: oid_hex(&eo),
                    expected_size: es,
                    got: oid_hex(&oid_bytes),
                    got_size: total,
                });
            }
        }

        Ok((oid_bytes, total, persisted))
    }

    /// Move a completed temp file to its final content-addressed path.
    /// Idempotent: if the target already exists, drop the tmp file.
    fn commit_tmp(&self, tmp: PathBuf, final_path: &Path) -> Result<(), StoreError> {
        if final_path.exists() {
            let _ = fs::remove_file(&tmp);
            return Ok(());
        }
        // Ensure parent dirs exist.
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        match fs::rename(&tmp, final_path) {
            Ok(()) => Ok(()),
            Err(_) if final_path.exists() => {
                // Lost a race to another writer — tmp already gone OR the
                // rename succeeded under us. Either way target is present.
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
            Err(e) => Err(io_err(final_path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn new_store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        (tmp, store)
    }

    const HELLO_OID_HEX: &str = "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";

    fn hex_to_oid(hex: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn insert_from_reader_computes_correct_oid() {
        let (_tmp, store) = new_store();
        let (pointer, size) = store
            .insert_from_reader(Cursor::new(b"hello world\n".to_vec()))
            .unwrap();
        assert_eq!(size, 12);
        assert_eq!(pointer.oid_hex(), HELLO_OID_HEX);
        assert!(store.contains(&pointer.oid));
    }

    #[test]
    fn object_path_layout_matches_git_lfs() {
        let (_tmp, store) = new_store();
        let oid = hex_to_oid(HELLO_OID_HEX);
        let path = store.object_path(&oid);
        let s = path.to_string_lossy();
        // <lfs-root>/objects/a9/48/a948904f...
        assert!(s.contains("/lfs/objects/a9/48/"));
        assert!(s.ends_with(HELLO_OID_HEX));
    }

    #[test]
    fn contains_false_when_absent() {
        let (_tmp, store) = new_store();
        let oid = [0u8; 32];
        assert!(!store.contains(&oid));
    }

    #[test]
    fn open_object_returns_none_for_missing() {
        let (_tmp, store) = new_store();
        let oid = [0u8; 32];
        assert!(store.open_object(&oid).unwrap().is_none());
    }

    #[test]
    fn open_object_returns_bytes_after_insert() {
        let (_tmp, store) = new_store();
        let (p, _) = store
            .insert_from_reader(Cursor::new(b"hello world\n".to_vec()))
            .unwrap();
        let mut reader = store.open_object(&p.oid).unwrap().unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello world\n");
    }

    #[test]
    fn insert_twice_same_content_is_idempotent() {
        let (_tmp, store) = new_store();
        let (p1, _) = store
            .insert_from_reader(Cursor::new(b"same".to_vec()))
            .unwrap();
        let (p2, _) = store
            .insert_from_reader(Cursor::new(b"same".to_vec()))
            .unwrap();
        assert_eq!(p1.oid, p2.oid);
        assert!(store.contains(&p1.oid));
    }

    #[test]
    fn insert_from_stream_verifies_match() {
        let (_tmp, store) = new_store();
        let data = b"hello world\n";
        let oid = hex_to_oid(HELLO_OID_HEX);
        store
            .insert_from_stream(&oid, 12, Cursor::new(data.to_vec()))
            .unwrap();
        assert!(store.contains(&oid));
    }

    #[test]
    fn insert_from_stream_rejects_wrong_size() {
        let (_tmp, store) = new_store();
        let data = b"hello world\n";
        let oid = hex_to_oid(HELLO_OID_HEX);
        let err = store
            .insert_from_stream(&oid, 999, Cursor::new(data.to_vec()))
            .unwrap_err();
        assert!(matches!(err, StoreError::ContentMismatch { .. }));
        assert!(!store.contains(&oid));
    }

    #[test]
    fn insert_from_stream_rejects_wrong_oid() {
        let (_tmp, store) = new_store();
        let data = b"different content";
        let fake_oid = hex_to_oid(HELLO_OID_HEX);
        let err = store
            .insert_from_stream(&fake_oid, 17, Cursor::new(data.to_vec()))
            .unwrap_err();
        assert!(matches!(err, StoreError::ContentMismatch { .. }));
        assert!(!store.contains(&fake_oid));
    }

    #[test]
    fn insert_from_stream_early_aborts_oversize() {
        let (_tmp, store) = new_store();
        let data = vec![b'x'; 100];
        let oid = [0u8; 32];
        let err = store
            .insert_from_stream(&oid, 10, Cursor::new(data))
            .unwrap_err();
        assert!(matches!(err, StoreError::ContentMismatch { .. }));
        assert!(!store.contains(&oid));
    }

    #[test]
    fn concurrent_insert_same_content_is_safe() {
        use std::sync::Arc;
        use std::thread;

        let (tmp, _) = new_store();
        let git_dir = Arc::new(tmp.path().to_owned());
        // Deliberately reopen from each thread to exercise the full race.
        let data = vec![b'y'; 10 * 1024 * 1024]; // 10 MiB
        let data = Arc::new(data);

        let mut handles = vec![];
        for _ in 0..4 {
            let gd = git_dir.clone();
            let d = data.clone();
            handles.push(thread::spawn(move || {
                let store = Store::open(&gd).unwrap();
                store.insert_from_reader(Cursor::new((*d).clone())).unwrap()
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All threads compute the same oid.
        let first_oid = results[0].0.oid;
        for (p, _) in &results {
            assert_eq!(p.oid, first_oid);
        }
        let store = Store::open(&git_dir).unwrap();
        assert!(store.contains(&first_oid));
        // File content is intact.
        let mut reader = store.open_object(&first_oid).unwrap().unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), data.len());
        assert_eq!(out[0], b'y');
    }

    #[test]
    fn large_file_streams_without_full_load() {
        // 10 MiB of pseudo-random-ish bytes.
        let (_tmp, store) = new_store();
        let data: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
        let (p, size) = store.insert_from_reader(Cursor::new(data.clone())).unwrap();
        assert_eq!(size, data.len() as u64);
        // Round-trip equality.
        let mut out = Vec::new();
        store
            .open_object(&p.oid)
            .unwrap()
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn git_lfs_written_object_readable_by_maw() {
        // Simulate a file written by git-lfs by hand-placing it at the
        // expected path. maw's contains/open_object must find it.
        let (_tmp, store) = new_store();
        let oid = hex_to_oid(HELLO_OID_HEX);
        let path = store.object_path(&oid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"hello world\n").unwrap();
        assert!(store.contains(&oid));
        let mut reader = store.open_object(&oid).unwrap().unwrap();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello world\n");
    }

    #[test]
    fn empty_file_is_valid() {
        let (_tmp, store) = new_store();
        let (p, size) = store
            .insert_from_reader(Cursor::new(Vec::<u8>::new()))
            .unwrap();
        assert_eq!(size, 0);
        // sha256 of empty: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            p.oid_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(store.contains(&p.oid));
    }
}
