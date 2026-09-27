//! Apply a user's `--resolve` choice to a [`ConflictRecord`], producing the
//! resolved file content.
//!
//! # Invariant
//!
//! **Resolution must not drop any workspace's non-conflicting edits.**
//! Choosing a side only decides the text of the *conflicting* regions; every
//! clean edit elsewhere in the file is kept.
//!
//! # Bystanders (bn-28os)
//!
//! A conflict's `sides` are only the workspaces whose edits overlap a
//! conflicted region; other workspaces that changed the same path are
//! recorded as `bystanders`. Every resolution (file-level, atom-level,
//! `--resolve-all`) folds each bystander's content back in with diff3, so a
//! whole-file choice no longer silently discards their edits. The merge
//! engine promotes a bystander to a side when that fold would conflict.
//!
//! # Atom-level resolution (bn-34zv)
//!
//! Line atoms are resolved by re-running the same `diff3(base, p0, pk)`
//! merges the conflict was detected with and replacing each conflict block
//! of that marker output with the chosen side's section, byte-for-byte. The
//! clean (context) parts of the marker output already carry both sides'
//! non-conflicting edits, so nothing outside the conflicted regions is
//! touched. For more than two participants the per-pair results are folded
//! with diff3; they agree on every conflicted region by construction, and a
//! residual conflict is reported as an error — never resolved by dropping a
//! side.
//!
//! The previous implementation rebuilt the file from the *base*, splicing
//! atom text that had lost its trailing newline (joining lines), placing it
//! by a base line number that was wrong whenever a clean edit earlier in
//! the file changed the line count, and reverting every clean edit outside
//! the conflicted regions.

use crate::model::conflict::Region;

use super::resolve::{ConflictRecord, Diff3Outcome, ResolveError, diff3_merge_bytes};

/// Why a resolution choice could not be applied.
#[derive(Debug)]
pub enum ResolutionChoiceError {
    /// The record has no base content; atom-level resolution needs one.
    MissingBase,
    /// The record carries no atoms.
    NoAtoms,
    /// The chosen workspace is not a side of this conflict (or has no edit
    /// for the atom).
    UnknownSide {
        /// Workspace that was chosen.
        workspace: String,
        /// Workspaces that can be chosen.
        available: Vec<String>,
    },
    /// The conflict's structure no longer matches its atoms (the merge would
    /// be recomputed differently). Resolving anyway could misplace text, so
    /// the resolution is refused.
    Layout(String),
    /// Combining the chosen regions with the other clean edits in the file
    /// produced a new textual conflict. Nothing is dropped: the caller must
    /// resolve at file level or supply content.
    FoldConflict(String),
    /// diff3 itself failed.
    Engine(ResolveError),
}

impl std::fmt::Display for ResolutionChoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBase => write!(f, "atom-level resolution requires base content"),
            Self::NoAtoms => write!(f, "conflict has no atoms"),
            Self::UnknownSide {
                workspace,
                available,
            } => write!(
                f,
                "workspace '{workspace}' is not a side in this conflict (available: {})",
                available.join(", ")
            ),
            Self::Layout(detail) => write!(f, "conflict layout changed: {detail}"),
            Self::FoldConflict(detail) => write!(
                f,
                "applying the chosen sides would conflict with other clean edits to this file \
                 ({detail}); resolve at file level or use content:PATH"
            ),
            Self::Engine(e) => write!(f, "diff3 failed: {e}"),
        }
    }
}

impl std::error::Error for ResolutionChoiceError {}

impl From<ResolveError> for ResolutionChoiceError {
    fn from(value: ResolveError) -> Self {
        Self::Engine(value)
    }
}

/// One segment of `git merge-file --diff3` output.
#[derive(Debug)]
enum Segment {
    /// Clean output, copied verbatim.
    Context(Vec<u8>),
    /// A conflict block: the ours / theirs sections, exact bytes (the base
    /// section is not needed to resolve).
    Block { ours: Vec<u8>, theirs: Vec<u8> },
}

/// Is `line` a conflict marker made of `ch` repeated 7 times, followed by
/// end-of-line or a space (label)?
fn is_marker(line: &[u8], ch: u8) -> bool {
    line.len() >= 7
        && line[..7].iter().all(|b| *b == ch)
        && matches!(line.get(7), None | Some(b' ' | b'\r' | b'\n'))
}

fn check_marker_inputs(
    base: &[u8],
    sides: &[(String, &[u8])],
) -> Result<(), ResolutionChoiceError> {
    // Git uses the same delimiters as literal marker-like source lines.
    // Parsing such output could consume chosen content as a base section.
    // Keep file-level resolution available, but refuse ambiguous atom splices.
    if std::iter::once(base)
        .chain(sides.iter().map(|(_, content)| *content))
        .any(|content| {
            content
                .split_inclusive(|b| *b == b'\n')
                .any(|line| b"<|=>".iter().any(|ch| is_marker(line, *ch)))
        })
    {
        return Err(ResolutionChoiceError::Layout(
            "input contains literal conflict markers; use a file-level choice or content:PATH"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Split diff3 marker output into context and conflict blocks, keeping
/// every byte (line endings included).
///
/// `ours_full` / `theirs_full` are the complete input files: when the last
/// block runs to end-of-file, git terminates a section that lacks a final
/// newline with one of its own so the closing marker starts a line. That
/// synthetic newline is removed again here, so each section is exactly the
/// side's bytes.
fn parse_marker_output(
    output: &[u8],
    ours_full: &[u8],
    theirs_full: &[u8],
) -> Result<Vec<Segment>, ResolutionChoiceError> {
    #[derive(PartialEq, Eq)]
    enum State {
        Context,
        Ours,
        Base,
        Theirs,
    }
    let mut segments = Vec::new();
    let mut state = State::Context;
    let mut context = Vec::new();
    let mut ours = Vec::new();
    let mut theirs = Vec::new();

    for line in output.split_inclusive(|b| *b == b'\n') {
        match state {
            State::Context if is_marker(line, b'<') => {
                if !context.is_empty() {
                    segments.push(Segment::Context(std::mem::take(&mut context)));
                }
                state = State::Ours;
            }
            State::Context => context.extend_from_slice(line),
            State::Ours if is_marker(line, b'|') => state = State::Base,
            State::Ours => ours.extend_from_slice(line),
            State::Base if is_marker(line, b'=') => state = State::Theirs,
            State::Base => {}
            State::Theirs if is_marker(line, b'>') => {
                segments.push(Segment::Block {
                    ours: std::mem::take(&mut ours),
                    theirs: std::mem::take(&mut theirs),
                });
                state = State::Context;
            }
            State::Theirs => theirs.extend_from_slice(line),
        }
    }
    if state != State::Context {
        return Err(ResolutionChoiceError::Layout(
            "unterminated conflict block in diff3 output".to_owned(),
        ));
    }
    if !context.is_empty() {
        segments.push(Segment::Context(context));
    }

    // A block with nothing after it runs to EOF on every side.
    if let Some(Segment::Block { ours, theirs }) = segments.last_mut() {
        strip_synthetic_eol(ours, ours_full);
        strip_synthetic_eol(theirs, theirs_full);
    }
    Ok(segments)
}

/// Remove the line ending git appended to an EOF section whose side has no
/// trailing newline.
fn strip_synthetic_eol(section: &mut Vec<u8>, full: &[u8]) {
    if full.ends_with(b"\n") || !section.ends_with(b"\n") {
        return;
    }
    section.pop();
    if section.ends_with(b"\r") && !full.ends_with(b"\r") {
        section.pop();
    }
}

fn fold_conflict(what: &str) -> ResolutionChoiceError {
    ResolutionChoiceError::FoldConflict(what.to_owned())
}

/// Sides of `record` that carry content, in record order.
fn content_sides(record: &ConflictRecord) -> Vec<(String, &[u8])> {
    record
        .sides
        .iter()
        .filter_map(|s| {
            s.content
                .as_deref()
                .map(|c| (s.workspace_id.as_str().to_owned(), c))
        })
        .collect()
}

fn unknown_side(workspace: &str, record: &ConflictRecord) -> ResolutionChoiceError {
    ResolutionChoiceError::UnknownSide {
        workspace: workspace.to_owned(),
        available: record
            .sides
            .iter()
            .map(|s| s.workspace_id.as_str().to_owned())
            .collect(),
    }
}

/// Resolve every atom of `record` to the workspace named in `choices`
/// (one entry per atom, in atom order) and return the full file content.
///
/// # Errors
///
/// See [`ResolutionChoiceError`]. Every error is fail-closed: no content is
/// produced that could drop an edit.
pub fn resolve_record_atoms(
    record: &ConflictRecord,
    choices: &[&str],
) -> Result<Vec<u8>, ResolutionChoiceError> {
    let chosen = resolve_participant_atoms(record, choices)?;
    fold_bystanders(record, chosen)
}

/// Resolve the whole file to side `workspace` (`--resolve cf-X=<ws>`):
/// that side's full content, folded with every bystander's edits (bn-28os).
///
/// Returns `Ok(None)` when the chosen side deleted the file.
///
/// # Errors
///
/// [`ResolutionChoiceError::UnknownSide`] if `workspace` is not a side;
/// [`ResolutionChoiceError::FoldConflict`] if a bystander cannot be folded
/// in (fail-closed: nothing is dropped).
pub fn resolve_record_to_side(
    record: &ConflictRecord,
    workspace: &str,
) -> Result<Option<Vec<u8>>, ResolutionChoiceError> {
    let side = record
        .sides
        .iter()
        .find(|s| s.workspace_id.as_str() == workspace)
        .ok_or_else(|| unknown_side(workspace, record))?;
    match &side.content {
        None if record.bystanders.is_empty() => Ok(None),
        None => Err(fold_conflict(&format!(
            "{workspace} deleted the file but {} also edited it",
            bystander_names(record)
        ))),
        Some(content) => fold_bystanders(record, content.clone()).map(Some),
    }
}

fn bystander_names(record: &ConflictRecord) -> String {
    record
        .bystanders
        .iter()
        .map(|b| b.workspace_id.as_str().to_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

/// bn-28os: fold every bystander's content into `chosen` with diff3.
///
/// Bystanders' edits are disjoint from every conflicted region, so the
/// fold is clean for any participant's content (the merge engine promotes
/// a bystander to participant when it is not). A conflict here is refused,
/// never resolved by dropping the bystander.
///
/// # Errors
///
/// [`ResolutionChoiceError::FoldConflict`] / [`ResolutionChoiceError::Engine`].
pub fn fold_bystanders(
    record: &ConflictRecord,
    chosen: Vec<u8>,
) -> Result<Vec<u8>, ResolutionChoiceError> {
    if record.bystanders.is_empty() {
        return Ok(chosen);
    }
    let base = record
        .base
        .as_deref()
        .ok_or(ResolutionChoiceError::MissingBase)?;
    let mut acc = chosen;
    for bystander in &record.bystanders {
        let who = bystander.workspace_id.as_str();
        let content = bystander
            .content
            .as_deref()
            .ok_or_else(|| fold_conflict(&format!("bystander {who} deleted the file")))?;
        acc = match diff3_merge_bytes(base, &acc, content)? {
            Diff3Outcome::Clean(merged) => merged,
            Diff3Outcome::Conflict { .. } => {
                return Err(fold_conflict(&format!(
                    "{who}'s non-conflicting edit overlaps the chosen text"
                )));
            }
        };
    }
    Ok(acc)
}

/// Bystander edits that are missing from user-supplied `content`
/// (`--resolve cf-X=content:PATH`): the names of bystanders whose
/// non-conflicting edits would be lost if `content` were used as-is.
///
/// A bystander counts as present when folding it into `content` changes
/// nothing. A diff3 error counts as missing (warn rather than stay silent).
#[must_use]
pub fn bystanders_missing_from(record: &ConflictRecord, content: &[u8]) -> Vec<String> {
    let Some(base) = record.base.as_deref() else {
        return Vec::new();
    };
    record
        .bystanders
        .iter()
        .filter(|b| {
            let Some(theirs) = b.content.as_deref() else {
                return true;
            };
            !matches!(
                diff3_merge_bytes(base, content, theirs),
                Ok(Diff3Outcome::Clean(merged)) if merged == content
            )
        })
        .map(|b| b.workspace_id.as_str().to_owned())
        .collect()
}

/// [`resolve_record_atoms`] without the bystander fold.
fn resolve_participant_atoms(
    record: &ConflictRecord,
    choices: &[&str],
) -> Result<Vec<u8>, ResolutionChoiceError> {
    let base = record
        .base
        .as_deref()
        .ok_or(ResolutionChoiceError::MissingBase)?;
    if record.atoms.is_empty() {
        return Err(ResolutionChoiceError::NoAtoms);
    }
    if choices.len() != record.atoms.len() {
        return Err(ResolutionChoiceError::Layout(format!(
            "{} choices for {} atoms",
            choices.len(),
            record.atoms.len()
        )));
    }

    // A single whole-file atom: the choice is that side's whole file.
    if record.atoms.len() == 1 && record.atoms[0].base_region == Region::WholeFile {
        let sides = content_sides(record);
        return sides
            .iter()
            .find(|(w, _)| w == choices[0])
            .map(|(_, c)| c.to_vec())
            .ok_or_else(|| unknown_side(choices[0], record));
    }

    if record
        .atoms
        .iter()
        .any(|a| matches!(a.base_region, Region::Lines { .. }))
    {
        if !record
            .atoms
            .iter()
            .all(|a| matches!(a.base_region, Region::Lines { .. }))
        {
            return Err(ResolutionChoiceError::Layout(
                "mixed line and non-line atoms".to_owned(),
            ));
        }
        return resolve_line_atoms(record, base, choices);
    }

    splice_region_atoms(record, base, choices)
}

/// Line atoms (diff3-derived): resolve conflict blocks in the pairwise
/// marker outputs, then fold the per-pair results.
fn resolve_line_atoms(
    record: &ConflictRecord,
    base: &[u8],
    choices: &[&str],
) -> Result<Vec<u8>, ResolutionChoiceError> {
    let sides = content_sides(record);
    if sides.len() < 2 || sides.len() != record.sides.len() {
        return Err(ResolutionChoiceError::Layout(
            "line atoms need every side to carry content".to_owned(),
        ));
    }
    check_marker_inputs(base, &sides)?;
    // Atom edits are [p0, p1, ..., pn] in side order (kway_conflict_atoms).
    for atom in &record.atoms {
        let names: Vec<&str> = atom.edits.iter().map(|e| e.workspace.as_str()).collect();
        let expected: Vec<&str> = sides.iter().map(|(w, _)| w.as_str()).collect();
        if names != expected {
            return Err(ResolutionChoiceError::Layout(format!(
                "atom edits {names:?} do not match sides {expected:?}"
            )));
        }
    }
    let (p0_name, p0) = (&sides[0].0, sides[0].1);

    // Marker output of diff3(base, p0, pk) for every k >= 1, parsed.
    let mut pair_segments: Vec<Vec<Segment>> = Vec::with_capacity(sides.len() - 1);
    for (pk_name, pk) in &sides[1..] {
        let marker = match diff3_merge_bytes(base, p0, pk)? {
            Diff3Outcome::Conflict { marker_output } => marker_output,
            Diff3Outcome::Clean(_) => {
                return Err(ResolutionChoiceError::Layout(format!(
                    "{p0_name} and {pk_name} no longer conflict"
                )));
            }
        };
        let segs = parse_marker_output(&marker, p0, pk)?;
        let blocks = segs
            .iter()
            .filter(|s| matches!(s, Segment::Block { .. }))
            .count();
        if blocks != record.atoms.len() {
            return Err(ResolutionChoiceError::Layout(format!(
                "{blocks} conflict blocks between {p0_name} and {pk_name}, {} atoms",
                record.atoms.len()
            )));
        }
        pair_segments.push(segs);
    }

    // Exact text of block `i` as written by workspace `ws`.
    let chosen_text = |i: usize, ws: &str| -> Result<Vec<u8>, ResolutionChoiceError> {
        let (pair, take_ours) = if ws == p0_name {
            (0, true)
        } else {
            let k = sides[1..]
                .iter()
                .position(|(w, _)| w == ws)
                .ok_or_else(|| unknown_side(ws, record))?;
            (k, false)
        };
        let block = pair_segments[pair]
            .iter()
            .filter_map(|s| match s {
                Segment::Block { ours, theirs } => Some(if take_ours { ours } else { theirs }),
                Segment::Context(_) => None,
            })
            .nth(i)
            .ok_or_else(|| ResolutionChoiceError::Layout(format!("missing block {i}")))?;
        Ok(block.clone())
    };
    let texts: Vec<Vec<u8>> = choices
        .iter()
        .enumerate()
        .map(|(i, ws)| chosen_text(i, ws))
        .collect::<Result<_, _>>()?;

    // R_k: pair k's merge with every block replaced by the chosen text.
    let mut result: Option<Vec<u8>> = None;
    for segs in &pair_segments {
        let mut r = Vec::new();
        let mut block_idx = 0;
        for seg in segs {
            match seg {
                Segment::Context(bytes) => r.extend_from_slice(bytes),
                Segment::Block { .. } => {
                    r.extend_from_slice(&texts[block_idx]);
                    block_idx += 1;
                }
            }
        }
        result = Some(match result {
            None => r,
            Some(acc) if acc == r => acc,
            Some(acc) => match diff3_merge_bytes(base, &acc, &r)? {
                Diff3Outcome::Clean(merged) => merged,
                Diff3Outcome::Conflict { .. } => {
                    return Err(fold_conflict(
                        "per-workspace clean edits overlap after choosing sides",
                    ));
                }
            },
        });
    }
    result.ok_or(ResolutionChoiceError::NoAtoms)
}

/// Byte-range atoms (AST merge): splice the chosen edit text over the base
/// byte range. AST atoms carry exact byte ranges and exact edit text.
///
/// The producer checks that these atoms reconstruct every participant's
/// full file. Otherwise it emits diff3 atoms, which preserve clean edits.
fn splice_region_atoms(
    record: &ConflictRecord,
    base: &[u8],
    choices: &[&str],
) -> Result<Vec<u8>, ResolutionChoiceError> {
    let mut atoms: Vec<(usize, usize, usize)> = Vec::new();
    for (i, atom) in record.atoms.iter().enumerate() {
        let (s, e) = match &atom.base_region {
            Region::AstNode {
                start_byte,
                end_byte,
                ..
            } => (*start_byte as usize, *end_byte as usize),
            Region::WholeFile => (0, base.len()),
            Region::Lines { .. } => {
                return Err(ResolutionChoiceError::Layout(
                    "unexpected line atom".to_owned(),
                ));
            }
        };
        atoms.push((s.min(base.len()), e.min(base.len()), i));
    }
    atoms.sort_unstable();
    let mut out = Vec::with_capacity(base.len());
    let mut pos = 0usize;
    for (s, e, i) in atoms {
        if s < pos {
            return Err(ResolutionChoiceError::Layout(
                "overlapping atom regions".to_owned(),
            ));
        }
        out.extend_from_slice(&base[pos..s]);
        let edit = record.atoms[i]
            .edits
            .iter()
            .find(|ed| ed.workspace == choices[i])
            .ok_or_else(|| unknown_side(choices[i], record))?;
        out.extend_from_slice(edit.content.as_bytes());
        pos = e;
    }
    out.extend_from_slice(&base[pos..]);
    Ok(out)
}

/// AST coordinates alone cannot describe clean edits outside the atoms or
/// insertions whose reported base range actually belongs to a variant.
/// Only expose these atoms when selecting each side reconstructs its bytes.
#[cfg(feature = "ast-merge")]
pub(super) fn ast_atoms_reconstruct_sides(record: &ConflictRecord) -> bool {
    let Some(base) = record.base.as_deref() else {
        return false;
    };
    !record.atoms.is_empty()
        && record.sides.iter().all(|side| {
            let Some(content) = side.content.as_deref() else {
                return false;
            };
            let choices = vec![side.workspace_id.as_str(); record.atoms.len()];
            matches!(splice_region_atoms(record, base, &choices), Ok(bytes) if bytes == content)
        })
}

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    //! bn-3bjx: targeted tests for branches the `resolution_choice_tests`
    //! proptests never reach (deleted sides, bystander bookkeeping, the
    //! whole-file shortcut, byte-range splicing). Each was found by a
    //! surviving cargo-mutants mutant.

    use std::path::PathBuf;

    use super::*;
    use crate::merge::resolve::{ConflictReason, ConflictSide};
    use crate::merge::types::ChangeKind;
    use crate::model::conflict::{AtomEdit, ConflictAtom, ConflictReason as AtomReason};
    use crate::model::types::WorkspaceId;

    fn side(ws: &str, content: Option<&[u8]>) -> ConflictSide {
        ConflictSide {
            workspace_id: WorkspaceId::new(ws).unwrap(),
            kind: if content.is_some() {
                ChangeKind::Modified
            } else {
                ChangeKind::Deleted
            },
            content: content.map(<[u8]>::to_vec),
        }
    }

    fn atom(region: Region, edits: &[(&str, &str)]) -> ConflictAtom {
        ConflictAtom::new(
            region,
            edits
                .iter()
                .map(|(w, c)| AtomEdit::new(*w, Region::whole_file(), *c))
                .collect(),
            AtomReason::OverlappingLineEdits {
                description: "test".to_owned(),
            },
        )
    }

    fn record(
        base: Option<&[u8]>,
        sides: Vec<ConflictSide>,
        atoms: Vec<ConflictAtom>,
        bystanders: Vec<ConflictSide>,
    ) -> ConflictRecord {
        ConflictRecord {
            path: PathBuf::from("f.txt"),
            base: base.map(<[u8]>::to_vec),
            sides,
            reason: ConflictReason::Diff3Conflict,
            atoms,
            bystanders,
        }
    }

    const BASE: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\n";
    const WA: &[u8] = b"A1\nl2\nl3\nl4\nl5\nl6\nl7\n";
    const WB: &[u8] = b"B1\nl2\nl3\nl4\nl5\nl6\nl7\n";
    const WC: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nC7\n";

    #[test]
    fn display_messages() {
        assert_eq!(
            ResolutionChoiceError::MissingBase.to_string(),
            "atom-level resolution requires base content"
        );
        assert_eq!(
            ResolutionChoiceError::UnknownSide {
                workspace: "x".to_owned(),
                available: vec!["a".to_owned(), "b".to_owned()],
            }
            .to_string(),
            "workspace 'x' is not a side in this conflict (available: a, b)"
        );
        assert_eq!(
            ResolutionChoiceError::Layout("d".to_owned()).to_string(),
            "conflict layout changed: d"
        );
    }

    /// A deleting side resolves to "delete" only when no bystander edited the
    /// file; otherwise the bystander's edit would be dropped, so it refuses
    /// and names the bystander.
    #[test]
    fn resolve_to_deleting_side_respects_bystanders() {
        let no_bystanders = record(
            Some(BASE),
            vec![side("wa", None), side("wb", Some(WB))],
            vec![],
            vec![],
        );
        assert_eq!(resolve_record_to_side(&no_bystanders, "wa").unwrap(), None);

        let with_bystander = record(
            Some(BASE),
            vec![side("wa", None), side("wb", Some(WB))],
            vec![],
            vec![side("wc", Some(WC))],
        );
        match resolve_record_to_side(&with_bystander, "wa") {
            Err(ResolutionChoiceError::FoldConflict(msg)) => {
                assert_eq!(msg, "wa deleted the file but wc also edited it")
            }
            other => panic!("expected FoldConflict, got {other:?}"),
        }
        // The content side still folds the bystander in.
        assert_eq!(
            resolve_record_to_side(&with_bystander, "wb")
                .unwrap()
                .unwrap(),
            b"B1\nl2\nl3\nl4\nl5\nl6\nC7\n"
        );
    }

    #[test]
    fn bystanders_missing_from_reports_exactly_the_missing_ones() {
        let rec = record(
            Some(BASE),
            vec![side("wa", Some(WA)), side("wb", Some(WB))],
            vec![],
            vec![side("wc", Some(WC)), side("wd", None)],
        );
        // wc's edit present; wd deleted the file, so it always counts as missing.
        assert_eq!(
            bystanders_missing_from(&rec, b"A1\nl2\nl3\nl4\nl5\nl6\nC7\n"),
            vec!["wd".to_owned()]
        );
        // wc's edit absent.
        assert_eq!(
            bystanders_missing_from(&rec, WA),
            vec!["wc".to_owned(), "wd".to_owned()]
        );
        // No base: nothing can be judged.
        let no_base = record(None, rec.sides.clone(), vec![], rec.bystanders.clone());
        assert!(bystanders_missing_from(&no_base, WA).is_empty());
    }

    /// A single whole-file atom resolves to the chosen side's full bytes, not
    /// to the atom edit's (lossy, possibly summarised) text.
    #[test]
    fn single_whole_file_atom_takes_the_sides_bytes() {
        let rec = record(
            Some(BASE),
            vec![side("wa", Some(b"A\xff\n")), side("wb", Some(b"B\n"))],
            vec![atom(
                Region::whole_file(),
                &[("wa", "edit-a"), ("wb", "edit-b")],
            )],
            vec![],
        );
        assert_eq!(resolve_record_atoms(&rec, &["wa"]).unwrap(), b"A\xff\n");
        assert_eq!(resolve_record_atoms(&rec, &["wb"]).unwrap(), b"B\n");
    }

    /// Line atoms need every side's content: a deleting third side must not
    /// be silently ignored.
    #[test]
    fn line_atoms_refuse_when_a_side_deleted_the_file() {
        let rec = record(
            Some(BASE),
            vec![side("wa", Some(WA)), side("wb", Some(WB)), side("wc", None)],
            vec![atom(Region::lines(1, 2), &[("wa", "A1\n"), ("wb", "B1\n")])],
            vec![],
        );
        assert!(
            matches!(
                resolve_record_atoms(&rec, &["wa"]),
                Err(ResolutionChoiceError::Layout(_))
            ),
            "{:?}",
            resolve_record_atoms(&rec, &["wa"])
        );
    }

    fn ast(start: u32, end: u32) -> Region {
        Region::ast_node("function_item", None, start, end)
    }

    fn two_sides() -> Vec<ConflictSide> {
        vec![side("wa", Some(b"a")), side("wb", Some(b"b"))]
    }

    /// Byte-range atoms: each chosen edit replaces its base range, atoms are
    /// applied in base order whatever their record order, and text between
    /// atoms is kept.
    #[test]
    fn splice_region_atoms_places_each_choice() {
        // [0,10) "fn a() {}\n", gap "\n", [11,21) "fn b() {}\n".
        let base: &[u8] = b"fn a() {}\n\nfn b() {}\n";
        let rec = record(
            Some(base),
            two_sides(),
            vec![
                atom(
                    ast(11, 21),
                    &[("wa", "fn b() {A}\n"), ("wb", "fn b() {B}\n")],
                ),
                atom(
                    ast(0, 10),
                    &[("wa", "fn a() {A}\n"), ("wb", "fn a() {B}\n")],
                ),
            ],
            vec![],
        );
        assert_eq!(
            resolve_record_atoms(&rec, &["wb", "wa"]).unwrap(),
            b"fn a() {A}\n\nfn b() {B}\n"
        );
        assert_eq!(
            resolve_record_atoms(&rec, &["wa", "wb"]).unwrap(),
            b"fn a() {B}\n\nfn b() {A}\n"
        );

        // Adjacent ranges ([0,10), [10,20)) are not overlapping.
        let base: &[u8] = b"fn a() {}\nfn b() {}\n";
        let rec = record(
            Some(base),
            two_sides(),
            vec![
                atom(ast(0, 10), &[("wa", "A\n"), ("wb", "a\n")]),
                atom(ast(10, 20), &[("wa", "B\n"), ("wb", "b\n")]),
            ],
            vec![],
        );
        assert_eq!(
            resolve_record_atoms(&rec, &["wa", "wb"]).unwrap(),
            b"A\nb\n"
        );

        // Overlapping ranges are refused.
        let rec = record(
            Some(base),
            two_sides(),
            vec![
                atom(ast(0, 12), &[("wa", "A\n"), ("wb", "a\n")]),
                atom(ast(10, 20), &[("wa", "B\n"), ("wb", "b\n")]),
            ],
            vec![],
        );
        assert!(matches!(
            resolve_record_atoms(&rec, &["wa", "wb"]),
            Err(ResolutionChoiceError::Layout(_))
        ));
    }
}
