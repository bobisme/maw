//! gix-backed ref, rev-parse, and ancestry operations.

use std::io::Read as _;
use std::path::Path;

use gix::refs::transaction::{Change, LogChange, PreviousValue};
use gix::refs::{FullName, Target};

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{GitOid, RefEdit, RefName};

/// Ensure a loose ref file ends with `\n`.
///
/// gix writes refs as 40 hex bytes without a trailing newline, but git's
/// canonical format requires one. `git fsck` warns `refMissingNewline`
/// for refs that lack it, and some hosting services reject pushes that
/// contain such refs.
fn ensure_ref_newline(git_dir: &Path, ref_name: &str) {
    let ref_path = git_dir.join(ref_name);
    let Ok(mut f) = std::fs::File::open(&ref_path) else {
        return; // packed ref or missing — nothing to fix
    };
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() && !buf.ends_with(b"\n") {
        drop(f);
        // Re-open for append and add the newline
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&ref_path) {
            let _ = std::io::Write::write_all(&mut f, b"\n");
        }
    }
}

/// Convert a `GitOid` to a `gix::ObjectId`.
fn to_gix_oid(oid: &GitOid) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(oid.as_bytes())
}

/// Convert a `gix::ObjectId` (or `&gix::oid`) to a `GitOid`.
fn from_gix_oid(oid: &gix::oid) -> GitOid {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(oid.as_bytes());
    GitOid::from_bytes(bytes)
}

pub fn read_ref(repo: &GixRepo, name: &RefName) -> Result<Option<GitOid>, GitError> {
    match repo.repo.try_find_reference(name.as_str()) {
        Ok(Some(mut r)) => {
            let id = r
                .peel_to_id_in_place()
                .map_err(|e| GitError::BackendError {
                    message: e.to_string(),
                })?;
            Ok(Some(from_gix_oid(id.as_ref())))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(GitError::BackendError {
            message: e.to_string(),
        }),
    }
}

pub fn write_ref(
    repo: &GixRepo,
    name: &RefName,
    oid: GitOid,
    log_message: &str,
) -> Result<(), GitError> {
    let gix_oid = to_gix_oid(&oid);
    repo.repo
        .reference(name.as_str(), gix_oid, PreviousValue::Any, log_message)
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
    ensure_ref_newline(repo.repo.git_dir(), name.as_str());
    Ok(())
}

pub fn delete_ref(repo: &GixRepo, name: &RefName) -> Result<(), GitError> {
    let r = repo
        .repo
        .try_find_reference(name.as_str())
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
    // No-op if the ref does not exist (per trait contract).
    if let Some(r) = r {
        r.delete().map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
    }
    Ok(())
}

/// Compare-and-swap delete: remove `name` only if it points at `expected`.
///
/// One gix ref transaction with `PreviousValue::MustExistAndMatch`, so the
/// check and the delete are atomic w.r.t. other ref writers (loose and
/// packed). A missing ref or a different value is [`GitError::RefConflict`].
pub fn delete_ref_cas(repo: &GixRepo, name: &RefName, expected: GitOid) -> Result<(), GitError> {
    let full: FullName =
        name.as_str()
            .try_into()
            .map_err(
                |e: gix::validate::reference::name::Error| GitError::BackendError {
                    message: e.to_string(),
                },
            )?;
    let edit = gix::refs::transaction::RefEdit {
        change: Change::Delete {
            expected: PreviousValue::MustExistAndMatch(Target::Object(to_gix_oid(&expected))),
            log: gix::refs::transaction::RefLog::AndReference,
        },
        name: full,
        deref: false,
    };
    repo.repo
        .edit_references([edit])
        .map_err(|e| classify_edit_error(&e))?;
    Ok(())
}

/// Classify a `gix::Repository::edit_references` failure as a CAS conflict
/// or an opaque backend error, by matching the *typed* gix error variant
/// rather than substring-matching its `Display` text (bn-36id).
///
/// `edit_references` returns [`gix::reference::edit::Error`], whose
/// `FileTransactionPrepare` variant wraps
/// [`gix::refs::file::transaction::prepare::Error`] — the type that reports
/// compare-and-swap precondition failures for loose refs:
///
/// - `MustNotExist` — the edit required the ref to not exist yet (create
///   only, `PreviousValue::MustNotExist`), but it already does.
/// - `ReferenceOutOfDate` — the edit required the ref's current value to
///   match an expected old value (`PreviousValue::MustExistAndMatch` /
///   `ExistingMustMatch`), but it held something else.
/// - `MustExist` — the edit required the ref to already exist
///   (`PreviousValue::MustExist`), but it was missing.
///
/// - `DeleteReferenceMustExist` — a CAS delete found the ref missing.
///
/// All four are optimistic-concurrency precondition failures: the ref was
/// not in the state the caller's compare-and-swap assumed. They all map to
/// [`GitError::RefConflict`]. Every other prepare/commit error (lock
/// contention, I/O, malformed packed-refs, ...) stays [`GitError::BackendError`].
fn classify_edit_error(err: &gix::reference::edit::Error) -> GitError {
    use gix::refs::file::transaction::prepare::Error as PrepareError;

    let message = err.to_string();

    let cas_ref_name = match err {
        gix::reference::edit::Error::FileTransactionPrepare(
            PrepareError::MustNotExist { full_name, .. }
            | PrepareError::ReferenceOutOfDate { full_name, .. }
            | PrepareError::MustExist { full_name, .. }
            // A CAS delete (`delete_ref_cas`) of a ref that no longer exists.
            | PrepareError::DeleteReferenceMustExist { full_name },
        ) => Some(full_name.to_string()),
        _ => None,
    };

    match cas_ref_name {
        Some(ref_name) => GitError::RefConflict { ref_name, message },
        None => GitError::BackendError { message },
    }
}

pub fn atomic_ref_update(repo: &GixRepo, edits: &[RefEdit]) -> Result<(), GitError> {
    let gix_edits: Vec<gix::refs::transaction::RefEdit> = edits
        .iter()
        .map(|edit| {
            let name: FullName = edit.name.as_str().try_into().map_err(
                |e: gix::validate::reference::name::Error| GitError::BackendError {
                    message: e.to_string(),
                },
            )?;

            let new_oid = to_gix_oid(&edit.new_oid);
            let expected = if edit.expected_old_oid.is_zero() {
                PreviousValue::MustNotExist
            } else {
                PreviousValue::MustExistAndMatch(Target::Object(to_gix_oid(&edit.expected_old_oid)))
            };

            Ok(gix::refs::transaction::RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: gix::refs::transaction::RefLog::AndReference,
                        force_create_reflog: false,
                        message: "atomic ref update".into(),
                    },
                    expected,
                    new: Target::Object(new_oid),
                },
                name,
                deref: false,
            })
        })
        .collect::<Result<Vec<_>, GitError>>()?;

    repo.repo
        .edit_references(gix_edits)
        .map_err(|e| classify_edit_error(&e))?;
    let git_dir = repo.repo.git_dir();
    for edit in edits {
        ensure_ref_newline(git_dir, edit.name.as_str());
    }
    Ok(())
}

pub fn list_refs(repo: &GixRepo, prefix: &str) -> Result<Vec<(RefName, GitOid)>, GitError> {
    let platform = repo.repo.references().map_err(|e| GitError::BackendError {
        message: e.to_string(),
    })?;
    let refs_iter = platform
        .prefixed(prefix)
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;

    let mut result = Vec::new();
    for r in refs_iter {
        let mut r = r.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        let name_str = r.name().as_bstr().to_string();
        let id = r
            .peel_to_id_in_place()
            .map_err(|e| GitError::BackendError {
                message: e.to_string(),
            })?;
        let oid = from_gix_oid(id.as_ref());
        if let Ok(ref_name) = RefName::new(&name_str) {
            result.push((ref_name, oid));
        }
    }
    Ok(result)
}

pub fn rev_parse(repo: &GixRepo, spec: &str) -> Result<GitOid, GitError> {
    let id = repo
        .repo
        .rev_parse_single(spec)
        .map_err(|e| GitError::NotFound {
            message: format!("rev-parse '{spec}': {e}"),
        })?;
    Ok(from_gix_oid(id.as_ref()))
}

pub fn rev_parse_opt(repo: &GixRepo, spec: &str) -> Result<Option<GitOid>, GitError> {
    match repo.repo.rev_parse_single(spec) {
        Ok(id) => Ok(Some(from_gix_oid(id.as_ref()))),
        Err(_e) => {
            // gix rev_parse errors are all resolution failures —
            // malformed specs, missing refs, unborn HEAD, etc.
            // These all map to None (spec could not be resolved).
            Ok(None)
        }
    }
}

pub fn is_ancestor(repo: &GixRepo, ancestor: GitOid, descendant: GitOid) -> Result<bool, GitError> {
    if ancestor == descendant {
        return Ok(true);
    }

    let ancestor_gix = to_gix_oid(&ancestor);
    let descendant_gix = to_gix_oid(&descendant);

    // Walk from descendant back through history, looking for ancestor
    let walk = repo
        .repo
        .rev_walk([descendant_gix])
        .all()
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;

    for info in walk {
        let info = info.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        if info.id == ancestor_gix {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn merge_base(repo: &GixRepo, a: GitOid, b: GitOid) -> Result<Option<GitOid>, GitError> {
    let a_gix = to_gix_oid(&a);
    let b_gix = to_gix_oid(&b);

    match repo.repo.merge_base(a_gix, b_gix) {
        Ok(id) => Ok(Some(from_gix_oid(id.as_ref()))),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(GitError::BackendError {
            message: e.to_string(),
        }),
    }
}
