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

use maw_git::GitRepo;
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
    /// Reading a side's blob via [`GitRepo::read_blob`] failed. Includes the
    /// path the blob belongs to, the offending OID, and a stringified error
    /// from `maw-git`.
    ReadBlob {
        /// Path the blob was being read for (conflicted file path).
        path: PathBuf,
        /// The blob OID that could not be read.
        oid: GitOid,
        /// Underlying error message from `maw-git`.
        reason: String,
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
            Self::ReadBlob { path, oid, reason } => write!(
                f,
                "materialize: failed to read blob {oid} for conflict at {}: {reason}",
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

/// Build the opening marker line for a given side, e.g.
/// `"<<<<<<< epoch (current)"` for the first (epoch) side, or
/// `"<<<<<<< alice"` for an arbitrary side name. We preserve the legacy
/// "(current)" suffix when the side is named `"epoch"` so that downstream
/// consumers that special-case that label (the marker gate, `maw ws resolve`,
/// the stdout banner in `sync/rebase.rs`) keep seeing what they expect.
fn marker_open(ws_name: &str) -> String {
    if ws_name == "epoch" {
        "<<<<<<< epoch (current)".to_owned()
    } else {
        format!("<<<<<<< {ws_name}")
    }
}

/// Build the trailing marker line for a given side. The `"(workspace changes)"`
/// suffix matches the banner printed by `sync/rebase.rs` and is recognised by
/// the resolver's label parser (which strips the parenthesised tail).
fn marker_close(ws_name: &str) -> String {
    if ws_name == "epoch" {
        ">>>>>>> epoch".to_owned()
    } else {
        format!(">>>>>>> {ws_name} (workspace changes)")
    }
}

const MARKER_BASE: &str = "||||||| base";
const MARKER_SEP: &str = "=======";

// ---------------------------------------------------------------------------
// Tool-authored placeholder byte prefixes (bn-28d1)
// ---------------------------------------------------------------------------

/// Byte prefixes that `maw` itself writes at the **start** of a rendered
/// conflict blob.
///
/// These are written exclusively by this module (`materialize.rs`) when
/// projecting an unresolved [`Conflict`](crate::model::conflict::Conflict) into
/// a committable blob:
///
/// * `# structured conflict at <path>\n` — first line of the text-conflict
///   stub produced by `render_text_content_conflict`.
/// * `# BINARY CONFLICT at <path> — …\n` — first line of the binary-conflict
///   stub produced by `render_binary_content_conflict`.
///
/// Legitimate source code never starts with these exact byte sequences. The
/// merge gate cross-checks HEAD-tree blobs against this list as a
/// tamper-resistance tripwire: if the structured sidecar has been deleted or
/// emptied but a placeholder blob still sits in HEAD, the gate refuses the
/// merge instead of silently committing placeholder-markered blobs into the
/// default branch.
///
/// **Important**: this list is intentionally small and prefix-only. Do NOT
/// add generic marker patterns like `<<<<<<<` — that's exactly the false
/// positive that bn-m6ad fixed. If materialize grows a new placeholder
/// variant, update this list to match.
pub const TOOL_PLACEHOLDER_PREFIXES: &[&[u8]] =
    &[b"# structured conflict at ", b"# BINARY CONFLICT at "];

/// Return `true` if `content` starts with any byte sequence in
/// [`TOOL_PLACEHOLDER_PREFIXES`].
///
/// Only the **first bytes** of the blob are examined — a file that happens to
/// contain one of these sequences later in its body (e.g. a test fixture
/// describing the placeholder format) is NOT flagged. Callers may therefore
/// safely pass either the full blob or just a short prefix slice.
#[must_use]
pub fn is_tool_placeholder_blob(content: &[u8]) -> bool {
    TOOL_PLACEHOLDER_PREFIXES
        .iter()
        .any(|p| content.starts_with(p))
}

/// Best-effort "is this blob text?" heuristic. Used to decide whether we can
/// safely inline content inside conflict markers. Mirrors git's approach
/// (presence of a NUL byte is a strong binary signal); we additionally treat
/// invalid UTF-8 as binary because our marker template is UTF-8 text and
/// we don't want to splice arbitrary bytes into a supposedly textual file.
fn looks_text(bytes: &[u8]) -> bool {
    if bytes.contains(&0u8) {
        return false;
    }
    std::str::from_utf8(bytes).is_ok()
}

/// Ensure `bytes` end with a newline. A missing trailing newline causes the
/// next marker line to be appended to the last content line — unparseable by
/// both the stdlib merge tooling and `maw ws resolve`.
fn push_with_trailing_newline(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
    if !bytes.ends_with(b"\n") {
        out.push(b'\n');
    }
}

/// Read a blob for a conflict side, wrapping any `maw-git` failure in
/// [`MaterializeError::ReadBlob`] so the caller doesn't need to know about
/// the `maw-git` error surface.
fn read_side_blob(
    repo: &dyn GitRepo,
    path: &Path,
    oid: &GitOid,
) -> Result<Vec<u8>, MaterializeError> {
    let git_oid: maw_git::GitOid =
        oid.as_str()
            .parse()
            .map_err(|e: maw_git::OidParseError| MaterializeError::ReadBlob {
                path: path.to_path_buf(),
                oid: oid.clone(),
                reason: e.to_string(),
            })?;
    repo.read_blob(git_oid)
        .map_err(|e| MaterializeError::ReadBlob {
            path: path.to_path_buf(),
            oid: oid.clone(),
            reason: e.to_string(),
        })
}

/// Render a [`Conflict::Content`] atom collection as a diff3-style markers
/// blob, reading each side's blob bytes via `repo.read_blob`.
///
/// Marker block shape (N-side, N ≥ 2):
///
/// ```text
/// <<<<<<< <first-side-ws>
/// <bytes-of-first-side-blob>
/// ||||||| base
/// <bytes-of-base-blob>           (empty if `base` is `None`)
/// =======
/// <bytes-of-last-side-blob>
/// >>>>>>> <last-side-ws>
/// ```
///
/// With more than two sides, each intermediate side is emitted between its
/// own `=======` pair so that `maw ws resolve --keep <name>` can still find
/// a marker block per workspace.
///
/// If *any* side or the base looks binary (NUL byte or invalid UTF-8), we
/// fall through to [`render_binary_content_conflict`] — splicing raw binary
/// between marker lines would produce an unparseable blob, break the marker
/// gate, and corrupt binary files (bn-ad5z).
fn render_content_conflict(
    repo: &dyn GitRepo,
    path: &Path,
    base: Option<&GitOid>,
    sides: &[ConflictSide],
    atoms: &[crate::model::conflict::ConflictAtom],
) -> Result<Vec<u8>, MaterializeError> {
    // Load base + sides. We tolerate missing base (pure add/edit).
    let base_bytes: Option<Vec<u8>> = match base {
        Some(b) => Some(read_side_blob(repo, path, b)?),
        None => None,
    };
    let mut side_bytes: Vec<Vec<u8>> = Vec::with_capacity(sides.len());
    for s in sides {
        side_bytes.push(read_side_blob(repo, path, &s.content)?);
    }

    // If any side or the base is binary, switch to a binary-safe rendering.
    // The caller still carries the structured conflict, so resolving through
    // `maw ws resolve` or the structured sidecar stays possible.
    let any_binary = base_bytes.as_deref().is_some_and(|b| !looks_text(b))
        || side_bytes.iter().any(|b| !looks_text(b));
    if any_binary {
        return Ok(render_binary_content_conflict(path, sides, &side_bytes));
    }

    let mut out: Vec<u8> = Vec::new();

    // Informational header — `<<<<<<<` scanner and marker-gate only look at
    // marker lines, so comment lines here are ignored by both.
    out.extend_from_slice(format!("# structured conflict at {}\n", path.display()).as_bytes());
    if !atoms.is_empty() {
        out.extend_from_slice(b"# atoms:\n");
        for atom in atoms {
            out.extend_from_slice(format!("#   - {}\n", atom.summary()).as_bytes());
        }
    }
    out.push(b'\n');

    // First side — always carries the full header-to-base triplet.
    let first = &sides[0];
    out.extend_from_slice(marker_open(&first.workspace).as_bytes());
    out.push(b'\n');
    push_with_trailing_newline(&mut out, &side_bytes[0]);
    out.extend_from_slice(MARKER_BASE.as_bytes());
    out.push(b'\n');
    if let Some(b) = &base_bytes {
        push_with_trailing_newline(&mut out, b);
    }
    // (no base: emit nothing between the `|||||||` and `=======` lines)

    // Intermediate sides (sides[1 .. N-1]) get their own marker block so that
    // multi-way conflicts still have one labelled block per workspace.
    for (i, side) in sides
        .iter()
        .enumerate()
        .skip(1)
        .take(sides.len().saturating_sub(2))
    {
        out.extend_from_slice(MARKER_SEP.as_bytes());
        out.push(b'\n');
        push_with_trailing_newline(&mut out, &side_bytes[i]);
        out.extend_from_slice(marker_close(&side.workspace).as_bytes());
        out.push(b'\n');
        // Re-open a new block for the subsequent side so the structure stays
        // `<<<<<<< / ||||||| base / (empty) / ======= / ... / >>>>>>>`.
        out.extend_from_slice(marker_open(&first.workspace).as_bytes());
        out.push(b'\n');
        push_with_trailing_newline(&mut out, &side_bytes[0]);
        out.extend_from_slice(MARKER_BASE.as_bytes());
        out.push(b'\n');
        if let Some(b) = &base_bytes {
            push_with_trailing_newline(&mut out, b);
        }
    }

    // Final side — closing marker uses that side's workspace name. This is
    // the crux of bn-324m: the old code emitted the *first* side's name here,
    // so two-side conflicts appeared as epoch vs epoch.
    let last_idx = sides.len() - 1;
    let last = &sides[last_idx];
    out.extend_from_slice(MARKER_SEP.as_bytes());
    out.push(b'\n');
    push_with_trailing_newline(&mut out, &side_bytes[last_idx]);
    out.extend_from_slice(marker_close(&last.workspace).as_bytes());
    out.push(b'\n');

    Ok(out)
}

/// Binary-safe rendering for [`Conflict::Content`] when at least one side is
/// not valid UTF-8 / contains NUL bytes.
///
/// The diff3 marker format is inherently textual: splicing raw binary between
/// marker lines would (a) embed NULs that confuse the marker gate and editors,
/// (b) produce a blob that's neither valid text nor valid binary, and (c)
/// silently corrupt the file. We instead commit the bytes of the **first**
/// side verbatim and record a prefixed binary-conflict note via a
/// distinguished marker header.
///
/// The `<<<<<<<` line still fires the marker gate, so `maw ws sync` still
/// sees the path as unresolved and `maw ws resolve --keep <name>` still
/// matches against the listed side labels. Addresses bn-ad5z.
fn render_binary_content_conflict(
    path: &Path,
    sides: &[ConflictSide],
    side_bytes: &[Vec<u8>],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(
        format!(
            "# BINARY CONFLICT at {} — inlined markers would corrupt the file.\n",
            path.display()
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"# Pick a side with: maw ws resolve <workspace> --keep <side-name>\n");
    for side in sides {
        out.extend_from_slice(
            format!("# side: {}  @  {}\n", side.workspace, side.content).as_bytes(),
        );
    }
    out.push(b'\n');

    // Marker block: keep the diff3 shape so the gate and resolver recognise
    // it, but emit placeholder text instead of actual bytes for the side-side
    // segments. The chosen side's bytes are emitted in a separate, unmarked
    // section at the end so tools like `file` / `diff` still see *something*.
    let first = &sides[0];
    let last = sides.last().unwrap_or(first);
    out.extend_from_slice(marker_open(&first.workspace).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"(binary content -- bytes not inlined)\n");
    out.extend_from_slice(MARKER_BASE.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(MARKER_SEP.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"(binary content -- bytes not inlined)\n");
    out.extend_from_slice(marker_close(&last.workspace).as_bytes());
    out.push(b'\n');

    // Suffix: raw bytes of the first side (epoch, in the rebase flow) so
    // downstream automation that opens the file sees a plausible payload.
    // Tagged with a comment line so it's obvious the bytes are one side's
    // content, not a merge.
    out.extend_from_slice(
        format!(
            "\n# ----- verbatim bytes of side `{}` below (chosen arbitrarily) -----\n",
            first.workspace
        )
        .as_bytes(),
    );
    if let Some(bytes) = side_bytes.first() {
        out.extend_from_slice(bytes);
    }
    out
}

/// Render an [`Conflict::AddAdd`] conflict as a marker block.
///
/// No base (by definition). Each side's bytes are inlined between marker
/// lines. Binary sides fall through to [`render_binary_content_conflict`]
/// (same rationale as `render_content_conflict`).
fn render_add_add_conflict(
    repo: &dyn GitRepo,
    path: &Path,
    sides: &[ConflictSide],
) -> Result<Vec<u8>, MaterializeError> {
    let mut side_bytes: Vec<Vec<u8>> = Vec::with_capacity(sides.len());
    for s in sides {
        side_bytes.push(read_side_blob(repo, path, &s.content)?);
    }
    if side_bytes.iter().any(|b| !looks_text(b)) {
        return Ok(render_binary_content_conflict(path, sides, &side_bytes));
    }

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(format!("# add/add conflict at {}\n\n", path.display()).as_bytes());

    let first = &sides[0];
    out.extend_from_slice(marker_open(&first.workspace).as_bytes());
    out.push(b'\n');
    push_with_trailing_newline(&mut out, &side_bytes[0]);
    out.extend_from_slice(MARKER_BASE.as_bytes());
    out.push(b'\n');
    // (no base — empty segment)

    for (i, side) in sides
        .iter()
        .enumerate()
        .skip(1)
        .take(sides.len().saturating_sub(2))
    {
        out.extend_from_slice(MARKER_SEP.as_bytes());
        out.push(b'\n');
        push_with_trailing_newline(&mut out, &side_bytes[i]);
        out.extend_from_slice(marker_close(&side.workspace).as_bytes());
        out.push(b'\n');
        out.extend_from_slice(marker_open(&first.workspace).as_bytes());
        out.push(b'\n');
        push_with_trailing_newline(&mut out, &side_bytes[0]);
        out.extend_from_slice(MARKER_BASE.as_bytes());
        out.push(b'\n');
    }

    let last_idx = sides.len() - 1;
    let last = &sides[last_idx];
    out.extend_from_slice(MARKER_SEP.as_bytes());
    out.push(b'\n');
    push_with_trailing_newline(&mut out, &side_bytes[last_idx]);
    out.extend_from_slice(marker_close(&last.workspace).as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// Render a [`Conflict::ModifyDelete`] conflict as a marker block.
///
/// The `modifier` side contributes bytes; the `deleter` side contributes an
/// explicit `(deleted)` sentinel. Binary modifier content falls through to
/// a binary-safe variant that keeps the marker structure but refuses to
/// inline the bytes.
fn render_modify_delete_conflict(
    repo: &dyn GitRepo,
    path: &Path,
    modifier: &ConflictSide,
    deleter: &ConflictSide,
    modified_content: &GitOid,
) -> Result<Vec<u8>, MaterializeError> {
    let modifier_bytes = read_side_blob(repo, path, modified_content)?;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(format!("# modify/delete conflict at {}\n", path.display()).as_bytes());
    out.extend_from_slice(
        format!(
            "# modifier: {} @ {}\n",
            modifier.workspace, modifier.content
        )
        .as_bytes(),
    );
    out.extend_from_slice(
        format!("# deleter:  {} @ {}\n", deleter.workspace, deleter.content).as_bytes(),
    );
    out.push(b'\n');

    out.extend_from_slice(marker_open(&modifier.workspace).as_bytes());
    out.push(b'\n');
    if looks_text(&modifier_bytes) {
        push_with_trailing_newline(&mut out, &modifier_bytes);
    } else {
        out.extend_from_slice(b"(binary modifier content -- bytes not inlined)\n");
    }
    out.extend_from_slice(MARKER_BASE.as_bytes());
    out.push(b'\n');
    // No separate base content — the modifier's blob is effectively our
    // "what was there before the delete" anchor.
    out.extend_from_slice(MARKER_SEP.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(format!("(deleted by {})\n", deleter.workspace).as_bytes());
    out.extend_from_slice(marker_close(&deleter.workspace).as_bytes());
    out.push(b'\n');

    // Preserve the verbatim binary modifier bytes so automation opening the
    // file sees the real content (with the marker gate still firing above).
    if !looks_text(&modifier_bytes) {
        out.extend_from_slice(
            format!(
                "\n# ----- verbatim bytes of modifier `{}` below -----\n",
                modifier.workspace
            )
            .as_bytes(),
        );
        out.extend_from_slice(&modifier_bytes);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// materialize
// ---------------------------------------------------------------------------

/// Project a [`ConflictTree`] into a [`MaterializedOutput`].
///
/// For every `(path, MaterializedEntry)` in `tree.clean` we emit a
/// [`FinalEntry::Clean`]; for every `(path, Conflict)` in `tree.conflicts` we
/// render the conflict to marker bytes — reading each side's blob via
/// `repo.read_blob` so the diff3 block actually contains the side contents —
/// and emit a [`FinalEntry::Rendered`].
///
/// # Why `repo` is threaded in here
///
/// Prior to bn-324m, `materialize` had no `GitRepo` handle and emitted
/// placeholder strings like `(epoch content not inlined in V1 structured
/// render)` inside the marker block. That made `maw ws resolve --keep` and
/// any agent reviewing the conflict useless (no actual bytes to pick from)
/// and also surfaced the "epoch vs epoch" label bug where the closing
/// marker always named the first side. Reading blobs here resolves both.
///
/// # Errors
///
/// * [`MaterializeError::UnsupportedDivergentRename`] — V1 cannot project
///   a [`Conflict::DivergentRename`] to a single path. Phase 5 must resolve
///   such conflicts before materialization.
/// * [`MaterializeError::ReadBlob`] — a side's blob could not be read from
///   the object store. Usually indicates a corrupted `ConflictTree` where a
///   `ConflictSide::content` references an OID the repo doesn't have.
pub fn materialize(
    tree: &ConflictTree,
    repo: &dyn GitRepo,
) -> Result<MaterializedOutput, MaterializeError> {
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
            } => render_content_conflict(repo, path, base.as_ref(), sides, atoms)?,
            Conflict::AddAdd { sides, .. } => render_add_add_conflict(repo, path, sides)?,
            Conflict::ModifyDelete {
                modifier,
                deleter,
                modified_content,
                ..
            } => render_modify_delete_conflict(repo, path, modifier, deleter, modified_content)?,
            Conflict::DivergentRename { .. } => {
                return Err(MaterializeError::UnsupportedDivergentRename { path: path.clone() });
            }
        };

        entries.insert(
            path.clone(),
            FinalEntry::Rendered {
                // TODO(follow-up bone): propagate per-side mode through the
                // conflict so we don't lose executable-bit / symlink-ness at
                // the marker-render step. Today, a conflicted symlink still
                // comes out as a regular blob (bn-3gbi); we at least no
                // longer stuff the target path into a text-markers file —
                // the `looks_text` heuristic detects short, no-newline symlink
                // targets as text OK, but the resulting file is a marker
                // block whose `--keep` can pick either side's bytes.
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
            Conflict::Content { base, sides, .. } => {
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
    use std::process::Command;
    use tempfile::TempDir;

    fn epoch() -> EpochId {
        EpochId::new(&"e".repeat(40)).unwrap()
    }

    fn oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn ord(ws: &str) -> OrderingKey {
        OrderingKey::new(epoch(), WorkspaceId::new(ws).unwrap(), 1, 1_700_000_000_000)
    }

    fn side(ws: &str, content: GitOid) -> ConflictSide {
        ConflictSide::new(ws.to_owned(), content, ord(ws))
    }

    /// Minimal test fixture: a real git repo in a tempdir, with a
    /// [`maw_git::GixRepo`] opened on top of it and a helper to write blobs
    /// and hand back both `maw-core` and `maw-git` OIDs.
    ///
    /// We go via a real repo (rather than a mock) because the object-safe
    /// [`GitRepo`] trait is large and the only method we actually exercise
    /// here is `read_blob` / `write_blob` — trivially cheap through `gix`.
    struct Fx {
        _dir: TempDir,
        repo: Box<dyn GitRepo>,
    }

    impl Fx {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let status = Command::new("git")
                .args(["init", "--initial-branch=main", "-q"])
                .current_dir(dir.path())
                .status()
                .expect("git init");
            assert!(status.success(), "git init failed");
            let repo: Box<dyn GitRepo> =
                Box::new(maw_git::GixRepo::open(dir.path()).expect("open"));
            Self { _dir: dir, repo }
        }

        /// Write `bytes` as a blob and return its OID as a `maw-core` `GitOid`.
        fn blob(&self, bytes: &[u8]) -> GitOid {
            let git_oid = self.repo.write_blob(bytes).expect("write_blob");
            GitOid::new(&git_oid.to_string()).expect("valid 40-char hex")
        }
    }

    // -----------------------------------------------------------------------
    // materialize
    // -----------------------------------------------------------------------

    #[test]
    fn materialize_empty_tree_returns_empty_output() {
        let fx = Fx::new();
        let tree = ConflictTree::new(epoch());
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        assert!(out.is_empty(), "empty tree should produce empty output");
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn materialize_preserves_clean_entries() {
        let fx = Fx::new();
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("src/lib.rs"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );
        tree.clean.insert(
            PathBuf::from("README.md"),
            MaterializedEntry::new(EntryMode::Blob, oid('b')),
        );

        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
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
        let fx = Fx::new();
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("scripts/build.sh"),
            MaterializedEntry::new(EntryMode::BlobExecutable, oid('a')),
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let entry = out.entries.get(&PathBuf::from("scripts/build.sh")).unwrap();
        match entry {
            FinalEntry::Clean { mode, .. } => assert_eq!(*mode, EntryMode::BlobExecutable),
            FinalEntry::Rendered { .. } => panic!("expected Clean"),
        }
    }

    #[test]
    fn materialize_preserves_symlink_mode() {
        let fx = Fx::new();
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("link"),
            MaterializedEntry::new(EntryMode::Link, oid('a')),
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let entry = out.entries.get(&PathBuf::from("link")).unwrap();
        match entry {
            FinalEntry::Clean { mode, .. } => assert_eq!(*mode, EntryMode::Link),
            FinalEntry::Rendered { .. } => panic!("expected Clean"),
        }
    }

    /// bn-324m regression: rendered content conflicts must inline the bytes of
    /// each side between marker lines, *not* a placeholder string.
    #[test]
    fn materialize_content_conflict_inlines_both_sides_as_bytes() {
        let fx = Fx::new();
        let epoch_oid = fx.blob(b"epoch version\n");
        let ws_oid = fx.blob(b"workspace version\n");
        let base_oid = fx.blob(b"base version\n");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            Conflict::Content {
                path: PathBuf::from("src/battle.rs"),
                file_id: FileId::new(1),
                base: Some(base_oid),
                sides: vec![side("epoch", epoch_oid), side("feature", ws_oid)],
                atoms: vec![],
            },
        );

        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let entry = out.entries.get(&PathBuf::from("src/battle.rs")).unwrap();
        let content = match entry {
            FinalEntry::Rendered { mode, content } => {
                assert_eq!(*mode, EntryMode::Blob);
                content.clone()
            }
            FinalEntry::Clean { .. } => panic!("conflict should be rendered"),
        };
        let text = std::str::from_utf8(&content).unwrap();
        // Real content — not placeholder — between markers.
        assert!(
            text.contains("epoch version"),
            "epoch side bytes should be inlined; got:\n{text}"
        );
        assert!(
            text.contains("workspace version"),
            "workspace side bytes should be inlined; got:\n{text}"
        );
        assert!(
            text.contains("base version"),
            "base bytes should be inlined between ||||||| and =======; got:\n{text}"
        );
        // Negative: no placeholder from the old implementation.
        assert!(
            !text.contains("content not inlined"),
            "old placeholder string must not appear; got:\n{text}"
        );
    }

    /// bn-324m regression: the closing marker must be labelled with the
    /// *second* side's workspace, not the first. Previously both ends of the
    /// marker block said `epoch`.
    #[test]
    fn materialize_content_conflict_labels_second_side_with_ws_name() {
        let fx = Fx::new();
        let epoch_oid = fx.blob(b"epoch\n");
        let ws_oid = fx.blob(b"workspace\n");
        let base_oid = fx.blob(b"base\n");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            Conflict::Content {
                path: PathBuf::from("src/battle.rs"),
                file_id: FileId::new(1),
                base: Some(base_oid),
                sides: vec![side("epoch", epoch_oid), side("feature", ws_oid)],
                atoms: vec![],
            },
        );

        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let content = match out.entries.get(&PathBuf::from("src/battle.rs")).unwrap() {
            FinalEntry::Rendered { content, .. } => content.clone(),
            FinalEntry::Clean { .. } => panic!("expected rendered"),
        };
        let text = std::str::from_utf8(&content).unwrap();
        assert!(
            text.contains("<<<<<<< epoch (current)"),
            "first side label missing; got:\n{text}"
        );
        assert!(
            text.contains(">>>>>>> feature (workspace changes)"),
            "closing label must use second side's name (`feature`), not `epoch`; got:\n{text}"
        );
        assert!(
            !text.contains(">>>>>>> epoch (workspace changes)"),
            "closing label must not say `epoch`; got:\n{text}"
        );
    }

    /// bn-ad5z: a content conflict whose blobs are binary must not produce a
    /// marker block with NUL bytes spliced between the marker lines.
    #[test]
    fn materialize_binary_conflict_does_not_produce_text_markers() {
        let fx = Fx::new();
        // Blob containing a NUL byte — typical of images / compiled output.
        let binary = b"PNG\x00\x01\x02\x03IHDR\x00\x00".to_vec();
        let epoch_oid = fx.blob(&binary);
        // Second side binary too.
        let mut ws_bytes = binary.clone();
        ws_bytes.push(0xffu8);
        let ws_oid = fx.blob(&ws_bytes);

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("assets/logo.png"),
            Conflict::Content {
                path: PathBuf::from("assets/logo.png"),
                file_id: FileId::new(7),
                base: None,
                sides: vec![side("epoch", epoch_oid), side("feature", ws_oid)],
                atoms: vec![],
            },
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let content = match out.entries.get(&PathBuf::from("assets/logo.png")).unwrap() {
            FinalEntry::Rendered { content, .. } => content.clone(),
            FinalEntry::Clean { .. } => panic!("expected rendered"),
        };
        // Marker gate still fires (the path is unresolved) ...
        assert!(
            content.windows(7).any(|w| w == b"<<<<<<<"),
            "marker gate must still find <<<<<<< for binary conflicts"
        );
        // ... and we emit a BINARY CONFLICT banner instead of pretending
        // the bytes are text between the markers.
        let head = std::str::from_utf8(
            &content[..content
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(content.len())],
        )
        .unwrap_or("");
        assert!(
            head.contains("BINARY CONFLICT"),
            "binary-conflict banner expected in header; got head:\n{head}"
        );
    }

    /// bn-3gbi (partial): a symlink-like short payload (no newlines) still
    /// goes through the text path cleanly today — we don't yet carry per-side
    /// mode into `Conflict`, so a conflicted symlink comes out as a regular
    /// blob whose content is the target path. The key property we assert
    /// here is that we no longer silently overwrite the file with
    /// placeholder text: both sides' target paths must be present.
    #[test]
    fn materialize_symlink_conflict_is_handled_explicitly() {
        let fx = Fx::new();
        // Symlink targets look like short text paths; the `looks_text`
        // heuristic treats them as text and we render markers around them.
        let epoch_oid = fx.blob(b"../upstream/target");
        let ws_oid = fx.blob(b"../downstream/target");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("config.lnk"),
            Conflict::Content {
                path: PathBuf::from("config.lnk"),
                file_id: FileId::new(9),
                base: None,
                sides: vec![side("epoch", epoch_oid), side("feature", ws_oid)],
                atoms: vec![],
            },
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let content = match out.entries.get(&PathBuf::from("config.lnk")).unwrap() {
            FinalEntry::Rendered { content, .. } => content.clone(),
            FinalEntry::Clean { .. } => panic!("expected rendered"),
        };
        let text = std::str::from_utf8(&content).unwrap();
        // Both target paths must be present — we don't drop either side.
        assert!(
            text.contains("../upstream/target"),
            "epoch symlink target must be inlined; got:\n{text}"
        );
        assert!(
            text.contains("../downstream/target"),
            "workspace symlink target must be inlined; got:\n{text}"
        );
        // Closing marker still labelled correctly (bn-324m guarantee).
        assert!(
            text.contains(">>>>>>> feature (workspace changes)"),
            "closing marker must name the second side; got:\n{text}"
        );
    }

    /// Base content must round-trip into the `||||||| base` segment.
    #[test]
    fn materialize_preserves_base_content_when_present() {
        let fx = Fx::new();
        let epoch_oid = fx.blob(b"epoch line 1\nepoch line 2\n");
        let ws_oid = fx.blob(b"ws line 1\nws line 2\n");
        let base_oid = fx.blob(b"base line 1\nbase line 2\n");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/lib.rs"),
            Conflict::Content {
                path: PathBuf::from("src/lib.rs"),
                file_id: FileId::new(1),
                base: Some(base_oid),
                sides: vec![side("epoch", epoch_oid), side("feature", ws_oid)],
                atoms: vec![],
            },
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let content = match out.entries.get(&PathBuf::from("src/lib.rs")).unwrap() {
            FinalEntry::Rendered { content, .. } => content.clone(),
            FinalEntry::Clean { .. } => panic!("expected rendered"),
        };
        let text = std::str::from_utf8(&content).unwrap();
        // Check order: <<<<<<< then ||||||| base then ======= then >>>>>>>.
        let i_open = text.find("<<<<<<<").expect("open marker");
        let i_base = text.find("||||||| base").expect("base marker");
        let i_sep = text.find("=======").expect("sep marker");
        let i_close = text.find(">>>>>>>").expect("close marker");
        assert!(i_open < i_base && i_base < i_sep && i_sep < i_close);
        // Base content sits between ||||||| and =======.
        let base_segment = &text[i_base..i_sep];
        assert!(
            base_segment.contains("base line 1"),
            "base bytes must be in the ||||||| segment; got segment:\n{base_segment}"
        );
    }

    #[test]
    fn materialize_renders_add_add_conflict() {
        let fx = Fx::new();
        let a_oid = fx.blob(b"alice content\n");
        let b_oid = fx.blob(b"bob content\n");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/new.rs"),
                sides: vec![side("alice", a_oid), side("bob", b_oid)],
            },
        );

        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let entry = out.entries.get(&PathBuf::from("src/new.rs")).unwrap();
        match entry {
            FinalEntry::Rendered { content, .. } => {
                let text = std::str::from_utf8(content).unwrap();
                assert!(text.contains("<<<<<<< alice"));
                assert!(text.contains(">>>>>>> bob (workspace changes)"));
                assert!(text.contains("alice content"));
                assert!(text.contains("bob content"));
            }
            FinalEntry::Clean { .. } => panic!("conflict should be rendered"),
        }
    }

    #[test]
    fn materialize_renders_modify_delete_conflict() {
        let fx = Fx::new();
        let modifier_oid = fx.blob(b"kept by alice\n");

        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/gone.rs"),
            Conflict::ModifyDelete {
                path: PathBuf::from("src/gone.rs"),
                file_id: FileId::new(2),
                modifier: side("alice", modifier_oid.clone()),
                deleter: side("bob", oid('b')),
                modified_content: modifier_oid,
            },
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let entry = out.entries.get(&PathBuf::from("src/gone.rs")).unwrap();
        match entry {
            FinalEntry::Rendered { content, .. } => {
                let text = std::str::from_utf8(content).unwrap();
                assert!(text.contains("<<<<<<< alice"));
                assert!(text.contains(">>>>>>> bob (workspace changes)"));
                assert!(text.contains("modifier: alice"));
                assert!(text.contains("deleter:  bob"));
                assert!(text.contains("kept by alice"));
            }
            FinalEntry::Clean { .. } => panic!("conflict should be rendered"),
        }
    }

    #[test]
    fn materialize_rejects_divergent_rename() {
        let fx = Fx::new();
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
        let err = materialize(&tree, fx.repo.as_ref()).unwrap_err();
        match err {
            MaterializeError::UnsupportedDivergentRename { path } => {
                assert_eq!(path, PathBuf::from("src/util.rs"));
            }
            other => panic!("expected UnsupportedDivergentRename, got {other:?}"),
        }
    }

    #[test]
    fn materialize_mixed_clean_and_conflicts() {
        let fx = Fx::new();
        let a_oid = fx.blob(b"a version\n");
        let b_oid = fx.blob(b"b version\n");
        let base_oid = fx.blob(b"base\n");

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
                base: Some(base_oid),
                sides: vec![side("alice", a_oid), side("bob", b_oid)],
                atoms: vec![ConflictAtom::line_overlap(10, 20, vec![], "overlap")],
            },
        );

        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
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
        let fx = Fx::new();
        let alpha_oid = fx.blob(b"alpha content\n");
        let beta_oid = fx.blob(b"beta content\n");

        // Regex-style check: every rendered conflict MUST contain
        //   <<<<<<< <first-ws>    and    >>>>>>> <last-ws> (workspace changes)
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("a"),
            Conflict::Content {
                path: PathBuf::from("a"),
                file_id: FileId::new(1),
                base: None,
                sides: vec![side("alpha-ws", alpha_oid), side("beta-ws", beta_oid)],
                atoms: vec![],
            },
        );
        let out = materialize(&tree, fx.repo.as_ref()).unwrap();
        let entry = out.entries.get(&PathBuf::from("a")).unwrap();
        let content = match entry {
            FinalEntry::Rendered { content, .. } => content,
            FinalEntry::Clean { .. } => panic!("expected rendered"),
        };
        let text = std::str::from_utf8(content).unwrap();
        // Exact label — these strings are the contract with
        // `maw ws resolve` and the marker-gate scanner.
        assert!(
            text.contains("<<<<<<< alpha-ws"),
            "missing OURS marker; got:\n{text}"
        );
        assert!(
            text.contains(">>>>>>> beta-ws (workspace changes)"),
            "closing marker must be labelled with the second side; got:\n{text}"
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
        assert_eq!(got, PathBuf::from("/tmp/repo/.manifold/artifacts/ws/foo"));
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
        assert_eq!(
            battle.theirs.as_deref(),
            Some(&*format!("blob:{}", oid('b')))
        );

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
