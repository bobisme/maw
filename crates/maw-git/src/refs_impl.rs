//! gix-backed ref, rev-parse, and ancestry operations.

use gix::refs::transaction::{Change, LogChange, PreviousValue};
use gix::refs::{Target, FullName};

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

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
        .reference(
            name.as_str(),
            gix_oid,
            PreviousValue::Any,
            log_message,
        )
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
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

pub fn atomic_ref_update(repo: &GixRepo, edits: &[RefEdit]) -> Result<(), GitError> {
    let gix_edits: Vec<gix::refs::transaction::RefEdit> = edits
        .iter()
        .map(|edit| {
            let name: FullName = edit
                .name
                .as_str()
                .try_into()
                .map_err(|e: gix::validate::reference::name::Error| GitError::BackendError {
                    message: e.to_string(),
                })?;

            let new_oid = to_gix_oid(&edit.new_oid);
            let expected = if edit.expected_old_oid.is_zero() {
                PreviousValue::MustNotExist
            } else {
                PreviousValue::MustExistAndMatch(Target::Object(to_gix_oid(
                    &edit.expected_old_oid,
                )))
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
        .map_err(|e| {
            let msg = e.to_string();
            // Detect CAS failures from the error message
            if msg.contains("existing object id")
                || msg.contains("MustExistAndMatch")
                || msg.contains("did not match")
                || msg.contains("mustNotExist")
                || msg.contains("MustNotExist")
            {
                // Try to extract the ref name from the edits for a better error
                let ref_name = edits
                    .first()
                    .map(|e| e.name.as_str().to_string())
                    .unwrap_or_default();
                GitError::RefConflict {
                    ref_name,
                    message: msg,
                }
            } else {
                GitError::BackendError { message: msg }
            }
        })?;
    Ok(())
}

pub fn list_refs(repo: &GixRepo, prefix: &str) -> Result<Vec<(RefName, GitOid)>, GitError> {
    let platform = repo
        .repo
        .references()
        .map_err(|e| GitError::BackendError {
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
            message: format!("rev-parse '{}': {}", spec, e),
        })?;
    Ok(from_gix_oid(id.as_ref()))
}

pub fn rev_parse_opt(repo: &GixRepo, spec: &str) -> Result<Option<GitOid>, GitError> {
    match repo.repo.rev_parse_single(spec) {
        Ok(id) => Ok(Some(from_gix_oid(id.as_ref()))),
        Err(e) => {
            // gix rev_parse errors are all resolution failures —
            // malformed specs, missing refs, unborn HEAD, etc.
            // These all map to None (spec could not be resolved).
            Ok(None)
        }
    }
}

pub fn is_ancestor(
    repo: &GixRepo,
    ancestor: GitOid,
    descendant: GitOid,
) -> Result<bool, GitError> {
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

pub fn merge_base(
    repo: &GixRepo,
    a: GitOid,
    b: GitOid,
) -> Result<Option<GitOid>, GitError> {
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
