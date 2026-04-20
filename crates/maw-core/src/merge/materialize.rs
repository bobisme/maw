//! Project a [`ConflictTree`] into a form the rebase pipeline can commit
//! (Phase 4 — bn-11b0, part of bn-gjm8 refactor).
//!
//! # What this module does
//!
//! Phases 1–3 produce a [`ConflictTree`]: `clean` paths have a materialized
//! `(mode, oid)`; `conflicts` paths have a structured [`Conflict`]. Phase 5's
//! rebase integration must turn that tree into something git can commit —
//! every path needs a concrete blob OID.
//!
//! This module is the shape-shifter in between. It takes a `ConflictTree` and
//! returns a [`MaterializedOutput`] whose `entries` map is path-indexed and
//! uses [`FinalEntry`] to distinguish:
//!
//! * [`FinalEntry::Clean`] — already has an OID (carried straight through from
//!   `tree.clean`).
//! * [`FinalEntry::Rendered`] — we rendered the structured conflict to a
//!   diff3-style markers blob. Phase 5 passes the content bytes to
//!   [`maw_git::GitRepo::write_blob`] to get the OID before building the tree.
//!
//! # Design decisions (answers to the Phase 4 design questions)
//!
//! ## `MaterializedEntry` vs new type
//!
//! Phase 1 defined [`MaterializedEntry`] as a `(mode, oid)` struct used in
//! `ConflictTree::clean`. Rather than migrate that struct to an enum and
//! disrupt Phases 1/2/3, we introduce a **new** [`FinalEntry`] enum here that
//! lives only in [`MaterializedOutput`]. This keeps the clean-path invariant
//! that "a path in `tree.clean` is already materialized" intact, and lets
//! Phase 5 consumers pattern-match on whether they need to write a blob first.
//!
//! ## I/O hygiene
//!
//! `maw-core` must not do git I/O. Hashing a blob (even via SHA-1 of
//! `"blob <len>\0<content>"`) would duplicate work that `maw_git::GitRepo`
//! is already set up to do via `write_blob`, and would tempt callers to skip
//! the object-store write. We therefore return **content bytes** for rendered
//! conflicts, not precomputed OIDs — the Phase 5 rebase driver pipes those
//! bytes through `write_blob` and only *then* has an OID to put in the tree.
//!
//! # Sidecars
//!
//! Two JSON sidecars are projected from the same `ConflictTree`:
//!
//! * **Structured** (`conflict-tree.json`): `serde_json::to_string_pretty`
//!   of the tree itself. Intended consumer is a future `maw ws resolve`
//!   rewrite (bn-3rah) that reads structured conflicts directly.
//!
//! * **Legacy** (`rebase-conflicts.json`): the flat `{ path, original_commit,
//!   base, ours, theirs }` shape that today's `maw ws resolve` and marker-gate
//!   consumers expect. Phase 4 ships a minimal duplicate of the schema here
//!   so `maw-core` can emit it without pulling a cycle through `maw-cli`.
//!
//! TODO(bn-consolidate-sidecars): fold the legacy schema into a single
//! shared type once the structured-resolve work lands. Today's `maw-cli`
//! still owns the canonical definition in
//! `crates/maw-cli/src/workspace/sync/rebase.rs`; our duplicate here must
//! stay structurally compatible (serde field names & JSON shape).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::types::{ConflictTree, EntryMode, MaterializedEntry};
use crate::model::conflict::{Conflict, ConflictSide};
use crate::model::types::GitOid;

// ---------------------------------------------------------------------------
// FinalEntry + MaterializedOutput
// ---------------------------------------------------------------------------

/// A single path's final state after [`materialize`].
///
/// See the module docs for the rationale behind introducing this type instead
/// of migrating [`MaterializedEntry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalEntry {
    /// The path was already cleanly resolved in `tree.clean`. Phase 5 writes
    /// this entry into the new tree using the existing blob OID — no new
    /// object-store writes needed.
    Clean {
        /// File mode (regular / executable / symlink / submodule / subtree).
        mode: EntryMode,
        /// Existing git blob OID from the workspace's pre-rebase state.
        oid: GitOid,
    },

    /// The path was a structured conflict; we rendered it to a marker blob.
    /// Phase 5 writes `content` via `GitRepo::write_blob` to get the OID,
    /// then inserts `(path, mode, new_oid)` into the tree.
    Rendered {
        /// File mode. For V1 this is always [`EntryMode::Blob`] — we default
        /// to a regular file on render. See the `V1 simplifications` note in
        /// the module docs for why we don't carry per-side mode yet.
        mode: EntryMode,
        /// The bytes of the rendered conflict (diff3-style markers).
        content: Vec<u8>,
    },
}

/// Output of [`materialize`]: every path in the input tree gets an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedOutput {
    /// Path → final-state entry. Sorted (BTreeMap) for determinism.
    pub entries: BTreeMap<PathBuf, FinalEntry>,
}

impl MaterializedOutput {
    /// Number of entries. Useful in tests and diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if there are no entries at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over `(path, FinalEntry::Rendered)` pairs — the ones Phase 5
    /// needs to push through `GitRepo::write_blob`.
    pub fn rendered_entries(&self) -> impl Iterator<Item = (&PathBuf, &[u8], EntryMode)> {
        self.entries.iter().filter_map(|(p, e)| match e {
            FinalEntry::Rendered { mode, content } => Some((p, content.as_slice(), *mode)),
            FinalEntry::Clean { .. } => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure modes for [`materialize`].
#[derive(Debug)]
pub enum MaterializeError {
    /// The tree still carries a [`Conflict::DivergentRename`] — V1 does not
    /// know how to project this to a single path with a single blob. Phase 5
    /// must resolve it upstream (via structured resolve) before calling us.
    UnsupportedDivergentRename {
        /// Path the rename is keyed under.
        path: PathBuf,
    },
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedDivergentRename { path } => write!(
                f,
                "materialize: divergent-rename conflict at {} cannot be projected to a single path in V1",
                path.display()
            ),
        }
    }
}

impl std::error::Error for MaterializeError {}

/// Failures that writing sidecar files can encounter.
#[derive(Debug)]
pub enum SidecarError {
    /// Creating the sidecar directory or writing the file failed.
    Io(std::io::Error),
    /// `serde_json` refused to serialize the tree.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "sidecar I/O error: {e}"),
            Self::Serialize(e) => write!(f, "sidecar serialization error: {e}"),
        }
    }
}

impl std::error::Error for SidecarError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Serialize(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for SidecarError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for SidecarError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialize(e)
    }
}

// ---------------------------------------------------------------------------
// Marker rendering
// ---------------------------------------------------------------------------

/// Conflict marker labels — kept identical to what `sync/rebase.rs::relabel_conflict_markers`
/// writes today, so `maw ws resolve` can match either source.
const MARKER_OURS: &str = "<<<<<<< epoch (current)";
const MARKER_BASE: &str = "||||||| base";
const MARKER_SEP: &str = "=======";

/// Build the trailing marker line for a given workspace.
fn marker_theirs(ws_name: &str) -> String {
    format!(">>>>>>> {ws_name} (workspace changes)")
}

/// Render a [`Conflict::Content`] atom collection as a diff3-style markers
/// blob.
///
/// # V1 simplification
///
/// Because `ConflictSide::content` carries a **blob OID**, not the blob bytes,
/// we can't reach into the git object store from `maw-core`. V1 renders an
/// *advisory* marker block that:
///
/// * tells the resolver which workspace contributed the conflicting content
///   and the blob OID of each side;
/// * includes every atom's `base_region` summary so the resolver can see
///   which regions need attention.
///
/// The resulting file is not byte-equivalent to what `git cherry-pick`
/// produces (which embeds the side contents inline). Phase 5 may enrich
/// this by reading blobs via `GitRepo` and passing the byte payloads in as
/// an argument — we deliberately do not do that here to keep `maw-core` free
/// of I/O.
///
/// This is a V1 compromise: `maw ws resolve` scans for `<<<<<<<` lines, and
/// that still works — it just doesn't see the side contents inline yet.
fn render_content_conflict(
    path: &Path,
    base: Option<&GitOid>,
    sides: &[ConflictSide],
    atoms: &[crate::model::conflict::ConflictAtom],
) -> Vec<u8> {
    let mut out = String::new();

    // Header. Strictly informational — the grep-for-`<<<<<<<` gate and
    // `maw ws resolve --keep` parser only look at marker lines.
    out.push_str(&format!("# structured conflict at {}\n", path.display()));
    if let Some(b) = base {
        out.push_str(&format!("# base blob: {b}\n"));
    } else {
        out.push_str("# base blob: (none)\n");
    }
    if !atoms.is_empty() {
        out.push_str("# atoms:\n");
        for atom in atoms {
            out.push_str(&format!("#   - {}\n", atom.summary()));
        }
    }
    out.push('\n');

    // Single whole-file marker block. V1 collapses all sides into one block
    // because we don't have the side bytes on hand to interleave atom-by-atom.
    // Follow-up bone (Phase 5 enrichment): per-atom marker blocks once we
    // can read side contents through `GitRepo`.
    out.push_str(MARKER_OURS);
    out.push('\n');
    // Show each side that is tagged "ours" by being part of the current
    // epoch — V1 doesn't distinguish ours/theirs per side, so we list every
    // side below `>>>>>>> <workspace>` marker blocks, one per workspace.
    // The resolver matches on `<<<<<<< epoch` / `>>>>>>> <ws>`, so collapsing
    // to a single block with multiple `>>>>>>>` markers is legal but
    // unusual; instead we emit:
    //     <<<<<<< epoch (current)
    //     (no epoch content captured in V1)
    //     ||||||| base
    //     (base blob: <oid-or-none>)
    //     =======
    //     # side: <ws>  @  <oid>
    //     ...
    //     >>>>>>> <first-ws> (workspace changes)
    //
    // If there are multiple sides we append additional `# side:` lines
    // between `=======` and the `>>>>>>>` closer so the resolver can see
    // them but we keep a *single* marker block per path.
    out.push_str("(epoch content not inlined in V1 structured render)\n");
    out.push_str(MARKER_BASE);
    out.push('\n');
    if let Some(b) = base {
        out.push_str(&format!("(base blob: {b})\n"));
    } else {
        out.push_str("(no base)\n");
    }
    out.push_str(MARKER_SEP);
    out.push('\n');
    for side in sides {
        out.push_str(&format!(
            "# side: {}  @  {}\n",
            side.workspace, side.content
        ));
    }
    // Closing marker uses the first side's workspace name. V1 limitation:
    // multi-side conflicts only get one `>>>>>>>` footer even if several
    // workspaces diverge. The `# side:` lines above capture the rest.
    let theirs_ws = sides
        .first()
        .map_or("workspace", |s| s.workspace.as_str());
    out.push_str(&marker_theirs(theirs_ws));
    out.push('\n');

    out.into_bytes()
}

/// Render an [`Conflict::AddAdd`] conflict as a marker block.
///
/// No base (by definition). We emit a marker block listing each side's OID
/// so the resolver can see there's a divergent add.
fn render_add_add_conflict(path: &Path, sides: &[ConflictSide]) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!("# add/add conflict at {}\n\n", path.display()));
    out.push_str(MARKER_OURS);
    out.push('\n');
    out.push_str("(no epoch content — file did not exist at base)\n");
    out.push_str(MARKER_BASE);
    out.push('\n');
    out.push_str("(no base)\n");
    out.push_str(MARKER_SEP);
    out.push('\n');
    for side in sides {
        out.push_str(&format!(
            "# side: {}  @  {}\n",
            side.workspace, side.content
        ));
    }
    let theirs_ws = sides
        .first()
        .map_or("workspace", |s| s.workspace.as_str());
    out.push_str(&marker_theirs(theirs_ws));
    out.push('\n');
    out.into_bytes()
}

/// Render a [`Conflict::ModifyDelete`] conflict as a marker block.
///
/// The `ours` side is the modifier (has real content), the `theirs` side is
/// the deleter. We label both explicitly so the resolver can tell them apart.
fn render_modify_delete_conflict(
    path: &Path,
    modifier: &ConflictSide,
    deleter: &ConflictSide,
    modified_content: &GitOid,
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!(
        "# modify/delete conflict at {}\n",
        path.display()
    ));
    out.push_str(&format!(
        "# modifier: {} @ {}\n",
        modifier.workspace, modifier.content
    ));
    out.push_str(&format!(
        "# deleter:  {} @ {}\n",
        deleter.workspace, deleter.content
    ));
    out.push('\n');
    out.push_str(MARKER_OURS);
    out.push('\n');
    out.push_str(&format!(
        "(modifier blob: {})\n",
        modified_content
    ));
    out.push_str(MARKER_BASE);
    out.push('\n');
    out.push_str("(base blob: see modifier)\n");
    out.push_str(MARKER_SEP);
    out.push('\n');
    out.push_str(&format!("(deleted by: {})\n", deleter.workspace));
    out.push_str(&marker_theirs(&deleter.workspace));
    out.push('\n');
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// materialize
// ---------------------------------------------------------------------------

/// Project a [`ConflictTree`] into a [`MaterializedOutput`].
///
/// For every `(path, MaterializedEntry)` in `tree.clean` we emit a
/// [`FinalEntry::Clean`]; for every `(path, Conflict)` in `tree.conflicts` we
/// render the conflict to marker bytes and emit a [`FinalEntry::Rendered`].
///
/// # Errors
///
/// * [`MaterializeError::UnsupportedDivergentRename`] — V1 cannot project
///   a [`Conflict::DivergentRename`] to a single path. Phase 5 must resolve
///   such conflicts before materialization.
pub fn materialize(tree: &ConflictTree) -> Result<MaterializedOutput, MaterializeError> {
    let mut entries: BTreeMap<PathBuf, FinalEntry> = BTreeMap::new();

    // Clean entries pass through unchanged.
    for (path, entry) in &tree.clean {
        let MaterializedEntry { mode, oid } = entry.clone();
        entries.insert(path.clone(), FinalEntry::Clean { mode, oid });
    }

    // Conflicts are rendered into marker blobs.
    for (path, conflict) in &tree.conflicts {
        let content = match conflict {
            Conflict::Content {
                base, sides, atoms, ..
            } => render_content_conflict(path, base.as_ref(), sides, atoms),
            Conflict::AddAdd { sides, .. } => render_add_add_conflict(path, sides),
            Conflict::ModifyDelete {
                modifier,
                deleter,
                modified_content,
                ..
            } => render_modify_delete_conflict(path, modifier, deleter, modified_content),
            Conflict::DivergentRename { .. } => {
                return Err(MaterializeError::UnsupportedDivergentRename {
                    path: path.clone(),
                });
            }
        };

        entries.insert(
            path.clone(),
            FinalEntry::Rendered {
                // TODO(follow-up bone): propagate per-side mode through the
                // conflict so we don't lose executable-bit / symlink-ness at
                // the marker-render step.
                mode: EntryMode::Blob,
                content,
            },
        );
    }

    Ok(MaterializedOutput { entries })
}

// ---------------------------------------------------------------------------
// Sidecar schemas (legacy compatible — duplicate of maw-cli's RebaseConflicts)
// ---------------------------------------------------------------------------

/// V1 duplicate of the legacy sidecar entry from
/// `maw-cli::workspace::sync::rebase::RebaseConflict`.
///
/// Field names and `serde` attributes are kept structurally identical so that
/// a file written by one side can be read by the other.
///
/// ## Field semantics (V1)
///
/// * `path` — the conflicted path (workspace-root relative).
/// * `original_commit` — in the legacy flow this was the SHA of the commit
///   being cherry-picked when the conflict happened. The structured pipeline
///   no longer cherry-picks per-commit, so V1 fills this with the **base
///   epoch OID** from the [`ConflictTree`]. Phase 5 can override with a more
///   specific value if the replay flow provides one.
/// * `base` / `ours` / `theirs` — the legacy rebase pipeline stored the
///   git-index stage contents here as UTF-8 strings. `maw-core` cannot read
///   blobs (no I/O), so V1 instead stores the **hex blob OID** of each side
///   with a `blob:` prefix, e.g. `"blob:abc123…"`. Consumers that just want
///   "some non-empty string identifying this side" (the existing uses) still
///   work; consumers that want the bytes can resolve via git themselves.
///   Phase 5 may upgrade this to inlined content by routing through
///   `GitRepo::read_blob`.
///
/// TODO(bn-consolidate-sidecars): move the canonical definition into
/// `maw-core::merge::materialize` and have `maw-cli` re-export. Today we
/// duplicate so Phase 4 ships without touching `sync/rebase.rs`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyRebaseConflict {
    /// File path relative to workspace root.
    pub path: String,
    /// The commit SHA being replayed when the conflict occurred.
    pub original_commit: String,
    /// Base content or OID identifier, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// "Ours" content or OID identifier, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    /// "Theirs" content or OID identifier, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

/// V1 duplicate of the legacy sidecar root from
/// `maw-cli::workspace::sync::rebase::RebaseConflicts`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyRebaseConflicts {
    /// All conflicts from the rebase.
    pub conflicts: Vec<LegacyRebaseConflict>,
    /// The epoch OID before the rebase.
    pub rebase_from: String,
    /// The epoch OID after the rebase (target).
    pub rebase_to: String,
}

/// Encode a blob OID as a `"blob:<hex>"` string for the legacy sidecar.
///
/// Keeps the field type `Option<String>` (so `serde_json::from_str::<RebaseConflicts>`
/// over in `maw-cli` still deserializes) while making it obvious the content
/// is an identifier, not raw bytes.
fn encode_blob_ref(oid: &GitOid) -> String {
    format!("blob:{oid}")
}

// ---------------------------------------------------------------------------
// Sidecar paths
// ---------------------------------------------------------------------------

/// Directory where sidecars live for a given workspace path.
///
/// The workspace's name is taken from `ws_path.file_name()`.
fn sidecar_dir_for(ws_path: &Path) -> PathBuf {
    // `ws_path` points to `<repo>/ws/<name>/`. The sidecar lives at
    // `<repo>/.manifold/artifacts/ws/<name>/`. Walk up two parents to reach
    // the repo root (<repo>/ws/<name> -> <repo>/ws -> <repo>) and derive the
    // workspace name from the final component.
    let ws_name = ws_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let repo_root = ws_path
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| ws_path.to_path_buf(), Path::to_path_buf);
    repo_root
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws_name)
}

/// Absolute path to the legacy `rebase-conflicts.json` sidecar for a
/// workspace.
fn legacy_sidecar_path(ws_path: &Path) -> PathBuf {
    sidecar_dir_for(ws_path).join("rebase-conflicts.json")
}

/// Absolute path to the new structured `conflict-tree.json` sidecar.
fn structured_sidecar_path(ws_path: &Path) -> PathBuf {
    sidecar_dir_for(ws_path).join("conflict-tree.json")
}

// ---------------------------------------------------------------------------
// Sidecar writers
// ---------------------------------------------------------------------------

/// Project `tree.conflicts` into the legacy flat schema and write it to
/// `<ws_path>/../../.manifold/artifacts/ws/<name>/rebase-conflicts.json`.
///
/// Only [`Conflict::Content`], [`Conflict::AddAdd`], and
/// [`Conflict::ModifyDelete`] produce a legacy entry; a
/// [`Conflict::DivergentRename`] is skipped (the legacy schema has no
/// concept of rename destinations — Phase 5 / structured resolve handles it).
///
/// # Errors
///
/// * [`SidecarError::Io`] if the directory can't be created or the file
///   can't be written.
/// * [`SidecarError::Serialize`] if `serde_json` refuses the record.
pub fn write_legacy_sidecar(
    ws_path: &Path,
    tree: &ConflictTree,
    rebase_from: &GitOid,
    rebase_to: &GitOid,
) -> Result<(), SidecarError> {
    let mut records = Vec::new();
    let original_commit = tree.base_epoch.to_string();

    for (path, conflict) in &tree.conflicts {
        match conflict {
            Conflict::Content {
                base, sides, ..
            } => {
                // V1: ours/theirs are OID identifiers (see schema doc above).
                // Take the first two sides — the legacy schema only has two
                // "ours"/"theirs" slots. For 3+ sides the first two win;
                // the full set lives in the structured sidecar.
                let ours = sides.first().map(|s| encode_blob_ref(&s.content));
                let theirs = sides.get(1).map(|s| encode_blob_ref(&s.content));
                records.push(LegacyRebaseConflict {
                    path: path.to_string_lossy().into_owned(),
                    original_commit: original_commit.clone(),
                    base: base.as_ref().map(encode_blob_ref),
                    ours,
                    theirs,
                });
            }
            Conflict::AddAdd { sides, .. } => {
                let ours = sides.first().map(|s| encode_blob_ref(&s.content));
                let theirs = sides.get(1).map(|s| encode_blob_ref(&s.content));
                records.push(LegacyRebaseConflict {
                    path: path.to_string_lossy().into_owned(),
                    original_commit: original_commit.clone(),
                    base: None,
                    ours,
                    theirs,
                });
            }
            Conflict::ModifyDelete {
                modifier, deleter, ..
            } => {
                records.push(LegacyRebaseConflict {
                    path: path.to_string_lossy().into_owned(),
                    original_commit: original_commit.clone(),
                    // No base is recorded for modify/delete in the legacy
                    // schema — downstream resolver doesn't need one.
                    base: None,
                    ours: Some(encode_blob_ref(&modifier.content)),
                    theirs: Some(encode_blob_ref(&deleter.content)),
                });
            }
            Conflict::DivergentRename { .. } => {
                // Legacy schema has no way to represent this. Skip.
            }
        }
    }

    let payload = LegacyRebaseConflicts {
        conflicts: records,
        rebase_from: rebase_from.to_string(),
        rebase_to: rebase_to.to_string(),
    };

    let out_path = legacy_sidecar_path(ws_path);
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&payload)?;
    std::fs::write(&out_path, json)?;
    Ok(())
}

/// Write the structured sidecar — the full [`ConflictTree`] as pretty JSON.
///
/// Lives at `<repo>/.manifold/artifacts/ws/<name>/conflict-tree.json`.
///
/// # Errors
///
/// * [`SidecarError::Io`] if the directory can't be created or the file
///   can't be written.
/// * [`SidecarError::Serialize`] if `serde_json` refuses the tree.
pub fn write_structured_sidecar(ws_path: &Path, tree: &ConflictTree) -> Result<(), SidecarError> {
    let out_path = structured_sidecar_path(ws_path);
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(tree)?;
    std::fs::write(&out_path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::types::MaterializedEntry;
    use crate::model::conflict::{Conflict, ConflictAtom, ConflictSide};
    use crate::model::ordering::OrderingKey;
    use crate::model::patch::FileId;
    use crate::model::types::{EpochId, GitOid, WorkspaceId};

    fn epoch() -> EpochId {
        EpochId::new(&"e".repeat(40)).unwrap()
    }

    fn oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn ord(ws: &str) -> OrderingKey {
        OrderingKey::new(
            epoch(),
            WorkspaceId::new(ws).unwrap(),
            1,
            1_700_000_000_000,
        )
    }

    fn side(ws: &str, content: GitOid) -> ConflictSide {
        ConflictSide::new(ws.to_owned(), content, ord(ws))
    }

    // -----------------------------------------------------------------------
    // materialize
    // -----------------------------------------------------------------------

    #[test]
    fn materialize_empty_tree_returns_empty_output() {
        let tree = ConflictTree::new(epoch());
        let out = materialize(&tree).unwrap();
        assert!(out.is_empty(), "empty tree should produce empty output");
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn materialize_preserves_clean_entries() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("src/lib.rs"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );
        tree.clean.insert(
            PathBuf::from("README.md"),
            MaterializedEntry::new(EntryMode::Blob, oid('b')),
        );

        let out = materialize(&tree).unwrap();
        assert_eq!(out.len(), 2);

        let lib_entry = out.entries.get(&PathBuf::from("src/lib.rs")).unwrap();
        match lib_entry {
            FinalEntry::Clean { mode, oid: o } => {
                assert_eq!(*mode, EntryMode::Blob);
                assert_eq!(*o, oid('a'));
            }
            FinalEntry::Rendered { .. } => panic!("clean entry should not be rendered"),
        }

        let readme_entry = out.entries.get(&PathBuf::from("README.md")).unwrap();
        match readme_entry {
            FinalEntry::Clean { mode, oid: o } => {
                assert_eq!(*mode, EntryMode::Blob);
                assert_eq!(*o, oid('b'));
            }
            FinalEntry::Rendered { .. } => panic!("clean entry should not be rendered"),
        }
    }

    #[test]
    fn materialize_preserves_executable_bit() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("scripts/build.sh"),
            MaterializedEntry::new(EntryMode::BlobExecutable, oid('a')),
        );
        let out = materialize(&tree).unwrap();
        let entry = out.entries.get(&PathBuf::from("scripts/build.sh")).unwrap();
        match entry {
            FinalEntry::Clean { mode, .. } => assert_eq!(*mode, EntryMode::BlobExecutable),
            FinalEntry::Rendered { .. } => panic!("expected Clean"),
        }
    }

    #[test]
    fn materialize_preserves_symlink_mode() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("link"),
            MaterializedEntry::new(EntryMode::Link, oid('a')),
        );
        let out = materialize(&tree).unwrap();
        let entry = out.entries.get(&PathBuf::from("link")).unwrap();
        match entry {
            FinalEntry::Clean { mode, .. } => assert_eq!(*mode, EntryMode::Link),
            FinalEntry::Rendered { .. } => panic!("expected Clean"),
        }
    }

    #[test]
    fn materialize_renders_content_conflict_to_markers() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            Conflict::Content {
                path: PathBuf::from("src/battle.rs"),
                file_id: FileId::new(1),
                base: Some(oid('0')),
                sides: vec![side("alice", oid('a')), side("bob", oid('b'))],
                atoms: vec![],
            },
        );

        let out = materialize(&tree).unwrap();
        let entry = out.entries.get(&PathBuf::from("src/battle.rs")).unwrap();
        match entry {
            FinalEntry::Rendered { mode, content } => {
                assert_eq!(*mode, EntryMode::Blob);
                let text = std::str::from_utf8(content).unwrap();
                assert!(text.contains("<<<<<<< epoch (current)"));
                assert!(text.contains("||||||| base"));
                assert!(text.contains("======="));
                // First-side workspace name appears in the theirs marker.
                assert!(text.contains(">>>>>>> alice (workspace changes)"));
                // All side workspace names recorded.
                assert!(text.contains("# side: alice"));
                assert!(text.contains("# side: bob"));
            }
            FinalEntry::Clean { .. } => panic!("conflict should be rendered"),
        }
    }

    #[test]
    fn materialize_renders_add_add_conflict() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/new.rs"),
                sides: vec![side("alice", oid('a')), side("bob", oid('b'))],
            },
        );

        let out = materialize(&tree).unwrap();
        let entry = out.entries.get(&PathBuf::from("src/new.rs")).unwrap();
        match entry {
            FinalEntry::Rendered { content, .. } => {
                let text = std::str::from_utf8(content).unwrap();
                assert!(text.contains("<<<<<<< epoch (current)"));
                assert!(text.contains(">>>>>>> alice (workspace changes)"));
                assert!(text.contains("# side: alice"));
                assert!(text.contains("# side: bob"));
                assert!(text.contains("(no base)"));
            }
            FinalEntry::Clean { .. } => panic!("conflict should be rendered"),
        }
    }

    #[test]
    fn materialize_renders_modify_delete_conflict() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/gone.rs"),
            Conflict::ModifyDelete {
                path: PathBuf::from("src/gone.rs"),
                file_id: FileId::new(2),
                modifier: side("alice", oid('a')),
                deleter: side("bob", oid('b')),
                modified_content: oid('a'),
            },
        );
        let out = materialize(&tree).unwrap();
        let entry = out.entries.get(&PathBuf::from("src/gone.rs")).unwrap();
        match entry {
            FinalEntry::Rendered { content, .. } => {
                let text = std::str::from_utf8(content).unwrap();
                assert!(text.contains("<<<<<<< epoch (current)"));
                assert!(text.contains(">>>>>>> bob (workspace changes)"));
                assert!(text.contains("modifier: alice"));
                assert!(text.contains("deleter:  bob"));
            }
            FinalEntry::Clean { .. } => panic!("conflict should be rendered"),
        }
    }

    #[test]
    fn materialize_rejects_divergent_rename() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/util.rs"),
            Conflict::DivergentRename {
                file_id: FileId::new(3),
                original: PathBuf::from("src/util.rs"),
                destinations: vec![
                    (PathBuf::from("src/helpers.rs"), side("alice", oid('a'))),
                    (PathBuf::from("src/common.rs"), side("bob", oid('b'))),
                ],
            },
        );
        let err = materialize(&tree).unwrap_err();
        match err {
            MaterializeError::UnsupportedDivergentRename { path } => {
                assert_eq!(path, PathBuf::from("src/util.rs"));
            }
        }
    }

    #[test]
    fn materialize_mixed_clean_and_conflicts() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("src/lib.rs"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            Conflict::Content {
                path: PathBuf::from("src/battle.rs"),
                file_id: FileId::new(1),
                base: Some(oid('0')),
                sides: vec![side("alice", oid('a')), side("bob", oid('b'))],
                atoms: vec![ConflictAtom::line_overlap(
                    10,
                    20,
                    vec![],
                    "overlap",
                )],
            },
        );

        let out = materialize(&tree).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out.entries.get(&PathBuf::from("src/lib.rs")),
            Some(FinalEntry::Clean { .. })
        ));
        assert!(matches!(
            out.entries.get(&PathBuf::from("src/battle.rs")),
            Some(FinalEntry::Rendered { .. })
        ));

        // rendered_entries iterator only yields the conflict.
        let rendered: Vec<_> = out.rendered_entries().collect();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].0, &PathBuf::from("src/battle.rs"));
    }

    #[test]
    fn marker_block_uses_expected_labels() {
        // Regex-style check: every rendered conflict MUST contain
        //   <<<<<<< epoch (current)    and    >>>>>>> <ws-name> (workspace changes)
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("a"),
            Conflict::Content {
                path: PathBuf::from("a"),
                file_id: FileId::new(1),
                base: None,
                sides: vec![side("alpha-ws", oid('a')), side("beta-ws", oid('b'))],
                atoms: vec![],
            },
        );
        let out = materialize(&tree).unwrap();
        let entry = out.entries.get(&PathBuf::from("a")).unwrap();
        let content = match entry {
            FinalEntry::Rendered { content, .. } => content,
            FinalEntry::Clean { .. } => panic!("expected rendered"),
        };
        let text = std::str::from_utf8(content).unwrap();
        // Exact label — these strings are the contract with
        // `maw ws resolve` and the marker-gate scanner.
        assert!(
            text.contains("<<<<<<< epoch (current)"),
            "missing OURS marker; got:\n{text}"
        );
        assert!(
            text.contains(">>>>>>> alpha-ws (workspace changes)"),
            "missing THEIRS marker with ws name; got:\n{text}"
        );
    }

    // -----------------------------------------------------------------------
    // Sidecar paths
    // -----------------------------------------------------------------------

    #[test]
    fn sidecar_dir_derives_from_workspace_name() {
        // /tmp/repo/ws/foo -> /tmp/repo/.manifold/artifacts/ws/foo
        let ws = PathBuf::from("/tmp/repo/ws/foo");
        let got = sidecar_dir_for(&ws);
        assert_eq!(
            got,
            PathBuf::from("/tmp/repo/.manifold/artifacts/ws/foo")
        );
    }

    // -----------------------------------------------------------------------
    // Sidecar writers
    // -----------------------------------------------------------------------

    /// Build a workspace-shaped temp dir: `<tmp>/ws/<name>/`.
    /// Returns the `ws_path` we hand to sidecar writers.
    fn mk_ws_tree(name: &str) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let ws_path = td.path().join("ws").join(name);
        std::fs::create_dir_all(&ws_path).unwrap();
        (td, ws_path)
    }

    #[test]
    fn legacy_sidecar_round_trips_through_rebase_conflicts_schema() {
        let (_td, ws_path) = mk_ws_tree("foo");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            Conflict::Content {
                path: PathBuf::from("src/battle.rs"),
                file_id: FileId::new(1),
                base: Some(oid('0')),
                sides: vec![side("alice", oid('a')), side("bob", oid('b'))],
                atoms: vec![],
            },
        );
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/new.rs"),
                sides: vec![side("alice", oid('c')), side("bob", oid('d'))],
            },
        );
        tree.conflicts.insert(
            PathBuf::from("src/gone.rs"),
            Conflict::ModifyDelete {
                path: PathBuf::from("src/gone.rs"),
                file_id: FileId::new(2),
                modifier: side("alice", oid('e')),
                deleter: side("bob", oid('f')),
                modified_content: oid('e'),
            },
        );

        write_legacy_sidecar(&ws_path, &tree, &oid('1'), &oid('2')).unwrap();

        // Read back & parse as our duplicate schema (structural twin of
        // maw-cli's RebaseConflicts — same field names, same JSON shape).
        let path = legacy_sidecar_path(&ws_path);
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: LegacyRebaseConflicts = serde_json::from_str(&text).unwrap();

        assert_eq!(parsed.rebase_from, oid('1').to_string());
        assert_eq!(parsed.rebase_to, oid('2').to_string());
        assert_eq!(parsed.conflicts.len(), 3);

        // Sort by path for stable assertions (BTreeMap iteration is already
        // sorted but the Vec we built from it is too).
        let by_path: BTreeMap<_, _> = parsed
            .conflicts
            .iter()
            .map(|c| (c.path.clone(), c))
            .collect();

        let battle = by_path.get("src/battle.rs").unwrap();
        assert_eq!(battle.base.as_deref(), Some(&*format!("blob:{}", oid('0'))));
        assert_eq!(battle.ours.as_deref(), Some(&*format!("blob:{}", oid('a'))));
        assert_eq!(battle.theirs.as_deref(), Some(&*format!("blob:{}", oid('b'))));

        let new = by_path.get("src/new.rs").unwrap();
        assert_eq!(new.base, None);
        assert_eq!(new.ours.as_deref(), Some(&*format!("blob:{}", oid('c'))));
        assert_eq!(new.theirs.as_deref(), Some(&*format!("blob:{}", oid('d'))));

        let gone = by_path.get("src/gone.rs").unwrap();
        assert_eq!(gone.base, None);
        assert_eq!(gone.ours.as_deref(), Some(&*format!("blob:{}", oid('e'))));
        assert_eq!(gone.theirs.as_deref(), Some(&*format!("blob:{}", oid('f'))));
    }

    #[test]
    fn legacy_sidecar_skips_divergent_rename() {
        let (_td, ws_path) = mk_ws_tree("dr");
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("a.rs"),
            Conflict::DivergentRename {
                file_id: FileId::new(1),
                original: PathBuf::from("a.rs"),
                destinations: vec![
                    (PathBuf::from("b.rs"), side("alice", oid('a'))),
                    (PathBuf::from("c.rs"), side("bob", oid('b'))),
                ],
            },
        );
        // Should succeed (we just skip DRs in the legacy sidecar).
        write_legacy_sidecar(&ws_path, &tree, &oid('1'), &oid('2')).unwrap();

        let text = std::fs::read_to_string(legacy_sidecar_path(&ws_path)).unwrap();
        let parsed: LegacyRebaseConflicts = serde_json::from_str(&text).unwrap();
        assert!(parsed.conflicts.is_empty());
    }

    #[test]
    fn structured_sidecar_round_trips_as_conflict_tree() {
        let (_td, ws_path) = mk_ws_tree("s");

        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("src/lib.rs"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/new.rs"),
                sides: vec![side("ws-1", oid('c')), side("ws-2", oid('d'))],
            },
        );

        write_structured_sidecar(&ws_path, &tree).unwrap();

        let text = std::fs::read_to_string(structured_sidecar_path(&ws_path)).unwrap();
        let parsed: ConflictTree = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, tree);
    }
}
