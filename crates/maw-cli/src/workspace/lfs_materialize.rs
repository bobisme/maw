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
        || !maw_lfs::looks_like_pointer(content)
    {
        return Ok(None);
    }

    let pointer = maw_lfs::Pointer::parse(content)
        .map_err(|e| format!("invalid LFS pointer for '{rel_path}': {e}"))?;
    let store = maw_lfs::Store::open(repo.common_dir())
        .map_err(|e| format!("failed to open local LFS store: {e}"))?;
    let Some(mut reader) = store
        .open_object(&pointer.oid)
        .map_err(|e| format!("failed to open LFS object {}: {e}", pointer.oid_hex()))?
    else {
        return Err(format!(
            "LFS object {} is not present in the local store",
            pointer.oid_hex()
        ));
    };

    let mut bytes = Vec::with_capacity(usize::try_from(pointer.size).unwrap_or(0));
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read LFS object {}: {e}", pointer.oid_hex()))?;
    Ok(Some(bytes))
}
