//! Shared LFS smudge logic for direct blob materializers.
//!
//! Several maw paths read raw blobs from a commit and write them without
//! routing through `maw_git::checkout_tree`. Raw LFS blobs are pointer text,
//! so those paths must resolve attributes from the target commit and smudge
//! from the shared LFS store before writing.

use std::io::Read as _;

/// Resolve `content` to real LFS bytes when `attrs` marks `rel_path` as LFS.
///
/// `Ok(None)` means the path is not LFS-tracked or the content is not a
/// pointer. `Err` leaves the caller responsible for retaining the original
/// pointer bytes and surfacing the failure at the appropriate output layer.
pub(super) fn smudge_content(
    repo: &maw_git::GixRepo,
    attrs: Option<&maw_lfs::AttrsMatcher>,
    rel_path: &str,
    content: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    if !attrs.is_some_and(|matcher| matcher.is_lfs(rel_path))
        || maw_lfs::git_lfs_decode(content).is_none()
    {
        return Ok(None);
    }

    // bn-hcbc8: resolve exactly what `git lfs smudge` resolves — lenient
    // for non-canonical pointers, empty for size 0, and refusing extension
    // pointers and size mismatches (git-lfs leaves the pointer there too).
    let store = maw_lfs::Store::open(repo.common_dir())
        .map_err(|e| format!("failed to open local LFS store: {e}"))?;
    match store
        .open_for_smudge(content)
        .map_err(|e| format!("failed to open LFS object for '{rel_path}': {e}"))?
    {
        maw_lfs::SmudgeSource::NotAPointer => Ok(None),
        maw_lfs::SmudgeSource::Unavailable { reason, .. } => Err(reason),
        maw_lfs::SmudgeSource::Content {
            oid_hex,
            size,
            mut reader,
        } => {
            let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            reader
                .read_to_end(&mut bytes)
                .map_err(|e| format!("failed to read LFS object {oid_hex}: {e}"))?;
            Ok(Some(bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::smudge_content;
    use sha2::{Digest, Sha256};

    const V: &str = "version https://git-lfs.github.com/spec/v1";

    fn setup(
        content: &[u8],
    ) -> (
        tempfile::TempDir,
        maw_git::GixRepo,
        maw_lfs::AttrsMatcher,
        String,
    ) {
        let (dir, root) = maw_git::test_support::init_test_repo();
        let repo = maw_git::GixRepo::open(&root).unwrap();
        maw_lfs::Store::open(repo.common_dir())
            .unwrap()
            .insert_from_reader(content)
            .unwrap();
        let attrs = maw_lfs::AttrsMatcher::from_entries(vec![(
            String::new(),
            b"*.bin filter=lfs -text\n".to_vec(),
        )])
        .unwrap();
        let oid = format!("{:x}", Sha256::digest(content));
        (dir, repo, attrs, oid)
    }

    /// bn-hcbc8: the direct materializers (FF-absorb, recover, repair) decode
    /// exactly what `git lfs smudge` decodes.
    #[test]
    fn smudge_content_matches_git_lfs_smudge_rules() {
        let content = b"lfs payload".to_vec();
        let (_dir, repo, attrs, oid) = setup(&content);
        let n = content.len();
        let smudge = |blob: &str| smudge_content(&repo, Some(&attrs), "a.bin", blob.as_bytes());

        // Non-canonical pointers git-lfs smudges.
        for blob in [
            format!("{V}\noid sha256:{oid}\nsize {n}\n"),
            format!("{V}\r\noid sha256:{oid}\r\nsize {n}\r\n"),
            format!("\n{V}\n\noid sha256:{oid}\nsize 0{n}\n\n"),
            format!("version https://hawser.github.com/spec/v1\noid sha256:{oid}\nsize +{n}"),
        ] {
            assert_eq!(smudge(&blob), Ok(Some(content.clone())), "{blob:?}");
        }
        // size 0: empty content, object or not.
        assert_eq!(
            smudge(&format!("{V}\noid sha256:{}\nsize 0\n", "1".repeat(64))),
            Ok(Some(Vec::new()))
        );
        // git-lfs refuses: size mismatch, extensions, missing object.
        assert!(smudge(&format!("{V}\noid sha256:{oid}\nsize {}\n", n + 1)).is_err());
        assert!(
            smudge(&format!(
                "{V}\next-0-foo sha256:{oid}\noid sha256:{oid}\nsize {n}\n"
            ))
            .is_err()
        );
        assert!(smudge(&format!("{V}\noid sha256:{}\nsize 3\n", "1".repeat(64))).is_err());
        // Not a pointer to git-lfs (bn-1b3n: never smudged).
        for blob in [
            format!("{V}\noid sha256:{}\nsize {n}\n", oid.to_uppercase()),
            format!("{V}\nsize {n}\noid sha256:{oid}\n"),
            format!("{V}\noid sha256:{oid}\nsize {n}\nfoo bar\n"),
            format!("{V}\nThis file documents LFS.\n"),
            format!("{V}\noid sha256:{oid}\nsize {n}\n{}", "\n".repeat(1100)),
        ] {
            assert_eq!(smudge(&blob), Ok(None), "{blob:?}");
        }
        // Not LFS-tracked: never smudged.
        let canonical = format!("{V}\noid sha256:{oid}\nsize {n}\n");
        assert_eq!(
            smudge_content(&repo, Some(&attrs), "a.txt", canonical.as_bytes()),
            Ok(None)
        );
    }
}
