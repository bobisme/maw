//! Property tests: applying a `--resolve` choice must not drop any
//! workspace's non-conflicting edits (bn-34zv atom splice, bn-28os
//! bystander workspaces).
//!
//! Scenarios are random line edits over a small base (replace / delete /
//! insert per line, optional missing final newline, optional CRLF). Two
//! workspaces always rewrite the same line differently, so every case
//! produces a real conflict through the production pipeline
//! (`partition_by_path` + `resolve_partition*`); the record is then resolved
//! with the production [`resolve_record_atoms`].
//!
//! Oracles are independent of maw's own marker parsing:
//! - two participants: resolving every atom to `p0` / `p1` must equal
//!   `git merge-file -p --ours` / `--theirs` (git's own "favor one side,
//!   keep all clean hunks" merge, byte-exact incl. EOF newline);
//! - any participant count: the result must absorb every workspace's edits
//!   (re-merging any workspace into the result with `--ours` is a no-op).

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use proptest::prelude::*;

use crate::merge::apply_resolution::{
    ResolutionChoiceError, resolve_record_atoms, resolve_record_to_side,
};
use crate::merge::partition::partition_by_path;
use crate::merge::resolve::{ConflictReason, ConflictRecord, ResolveResult, resolve_partition};
use crate::merge::types::{ChangeKind, FileChange, PatchSet};
use crate::model::conflict::Region;
use crate::model::types::{EpochId, WorkspaceId};

const PATH: &str = "f.txt";

/// Per-base-line edit.
#[derive(Clone, Debug)]
enum LineOp {
    Keep,
    Replace,
    Delete,
    InsertAfter,
}

fn arb_op() -> impl Strategy<Value = LineOp> {
    prop_oneof![
        6 => Just(LineOp::Keep),
        2 => Just(LineOp::Replace),
        1 => Just(LineOp::Delete),
        1 => Just(LineOp::InsertAfter),
    ]
}

#[derive(Clone, Debug)]
struct Scenario {
    base_lines: usize,
    crlf: bool,
    /// Per workspace: ops per base line, and whether the final newline is dropped.
    workspaces: Vec<(Vec<LineOp>, bool)>,
    /// Base line that ws-0 and ws-1 both replace (forced conflict).
    clash_line: usize,
    base_no_final_eol: bool,
}

fn arb_scenario(max_ws: usize) -> impl Strategy<Value = Scenario> {
    (3usize..=8, any::<bool>(), any::<bool>(), 2..=max_ws).prop_flat_map(
        |(n, crlf, base_no_eol, k)| {
            (
                prop::collection::vec(
                    (
                        prop::collection::vec(arb_op(), n),
                        prop::bool::weighted(0.2),
                    ),
                    k,
                ),
                0..n,
            )
                .prop_map(move |(workspaces, clash_line)| Scenario {
                    base_lines: n,
                    crlf,
                    workspaces,
                    clash_line,
                    base_no_final_eol: base_no_eol,
                })
        },
    )
}

fn render(lines: &[String], crlf: bool, no_final_eol: bool) -> Vec<u8> {
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut s = lines.join(eol);
    if !lines.is_empty() && !no_final_eol {
        s.push_str(eol);
    }
    s.into_bytes()
}

fn build(sc: &Scenario) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let base_lines: Vec<String> = (0..sc.base_lines).map(|i| format!("b{i}")).collect();
    let base = render(&base_lines, sc.crlf, sc.base_no_final_eol);
    let mut out = Vec::new();
    for (w, (ops, no_eol)) in sc.workspaces.iter().enumerate() {
        let mut lines = Vec::new();
        for (i, base_line) in base_lines.iter().enumerate() {
            let op = if w < 2 && i == sc.clash_line {
                &LineOp::Replace
            } else {
                &ops[i]
            };
            match op {
                LineOp::Keep => lines.push(base_line.clone()),
                LineOp::Replace => lines.push(format!("w{w}r{i}")),
                LineOp::Delete => {}
                LineOp::InsertAfter => {
                    lines.push(base_line.clone());
                    lines.push(format!("w{w}i{i}"));
                }
            }
        }
        let no_final = if lines.len() == base_lines.len() && lines == base_lines {
            sc.base_no_final_eol
        } else {
            *no_eol
        };
        out.push((format!("ws-{w}"), render(&lines, sc.crlf, no_final)));
    }
    (base, out)
}

fn merge(base: &[u8], variants: &[(String, Vec<u8>)], ast: bool) -> ResolveResult {
    let epoch = EpochId::new(&"a".repeat(40)).unwrap();
    let patch_sets: Vec<PatchSet> = variants
        .iter()
        .filter(|(_, c)| c.as_slice() != base)
        .map(|(w, c)| {
            PatchSet::new(
                WorkspaceId::new(w).unwrap(),
                epoch.clone(),
                vec![FileChange::new(
                    PathBuf::from(PATH),
                    ChangeKind::Modified,
                    Some(c.clone()),
                )],
            )
        })
        .collect();
    let partition = partition_by_path(&patch_sets);
    let mut bases = BTreeMap::new();
    bases.insert(PathBuf::from(PATH), base.to_vec());
    if ast {
        #[cfg(feature = "ast-merge")]
        {
            return crate::merge::resolve::resolve_partition_with_ast(
                &partition,
                &bases,
                &crate::merge::ast_merge::AstMergeConfig::default(),
            )
            .unwrap();
        }
    }
    resolve_partition(&partition, &bases).unwrap()
}

/// `git merge-file -p [--ours|--theirs] ours base theirs` — independent oracle.
fn git_merge_file(ours: &[u8], base: &[u8], theirs: &[u8], favor: Option<&str>) -> (bool, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let (o, b, t) = (
        dir.path().join("o"),
        dir.path().join("b"),
        dir.path().join("t"),
    );
    std::fs::write(&o, ours).unwrap();
    std::fs::write(&b, base).unwrap();
    std::fs::write(&t, theirs).unwrap();
    let mut cmd = Command::new("git");
    cmd.arg("merge-file").arg("-p");
    if let Some(f) = favor {
        cmd.arg(f);
    }
    let out = cmd.arg(&o).arg(&b).arg(&t).output().unwrap();
    (out.status.code() == Some(0), out.stdout)
}

fn diff3_record(result: &ResolveResult) -> Option<&ConflictRecord> {
    result
        .conflicts
        .iter()
        .find(|c| matches!(c.reason, ConflictReason::Diff3Conflict) && !c.atoms.is_empty())
}

/// Re-merging `content` into `resolved` (favoring `resolved`) is a no-op:
/// every non-conflicting edit of `content` is already in `resolved`.
fn assert_absorbs(
    base: &[u8],
    resolved: &[u8],
    content: &[u8],
    what: &str,
) -> Result<(), TestCaseError> {
    let (_, again) = git_merge_file(resolved, base, content, Some("--ours"));
    prop_assert_eq!(
        String::from_utf8_lossy(&again),
        String::from_utf8_lossy(resolved),
        "{}",
        what
    );
    Ok(())
}

fn check_scenario(sc: &Scenario, ast: bool) -> Result<(), TestCaseError> {
    let (base, variants) = build(sc);
    let result = merge(&base, &variants, ast);
    let Some(record) = diff3_record(&result) else {
        // ws-0 / ws-1 always clash, so a diff3 conflict must be reported.
        return Err(TestCaseError::fail(format!(
            "expected a diff3 conflict: {result:?}"
        )));
    };
    let participants: Vec<(String, Vec<u8>)> = record
        .sides
        .iter()
        .map(|s| {
            (
                s.workspace_id.as_str().to_owned(),
                s.content.clone().unwrap(),
            )
        })
        .collect();
    // bn-28os: every workspace that changed the file is either a side or a
    // recorded bystander — none is silently omitted.
    for (w, content) in &variants {
        if content.as_slice() == base.as_slice() {
            continue;
        }
        let side = participants.iter().any(|(p, _)| p == w);
        let bystander = record
            .bystanders
            .iter()
            .any(|b| b.workspace_id.as_str() == w);
        prop_assert!(
            side ^ bystander,
            "{} must be exactly one of side/bystander: {:?}",
            w,
            record
        );
    }
    let bystanders: Vec<(String, Vec<u8>)> = record
        .bystanders
        .iter()
        .map(|b| {
            (
                b.workspace_id.as_str().to_owned(),
                b.content.clone().unwrap(),
            )
        })
        .collect();
    let n_atoms = record.atoms.len();
    let whole_file = n_atoms == 1 && record.atoms[0].base_region == Region::WholeFile;

    for (chosen, chosen_content) in &participants {
        // --- File-level: cf-X=<chosen> -----------------------------------
        let file_level = resolve_record_to_side(record, chosen)
            .map_err(|e| TestCaseError::fail(format!("file-level {chosen}: {e}")))?
            .expect("content side");
        if bystanders.is_empty() {
            prop_assert_eq!(
                &file_level,
                chosen_content,
                "file-level must be the side's file"
            );
        }
        assert_absorbs(
            &base,
            &file_level,
            chosen_content,
            "file-level lost chosen side",
        )?;
        for (w, content) in &bystanders {
            assert_absorbs(
                &base,
                &file_level,
                content,
                &format!("file-level resolve to {chosen} dropped bystander {w}"),
            )?;
        }

        // --- Atom-level: every atom -> <chosen> ---------------------------
        let choices = vec![chosen.as_str(); n_atoms];
        let resolved = match resolve_record_atoms(record, &choices) {
            Ok(r) => r,
            // Fail-closed refusal is allowed only for k-way folds.
            Err(ResolutionChoiceError::FoldConflict(_)) if participants.len() > 2 => continue,
            Err(e) => {
                return Err(TestCaseError::fail(format!(
                    "resolve to {chosen} failed: {e}\nrecord: {record:?}"
                )));
            }
        };

        // Exact oracle for two participants and no bystanders.
        if participants.len() == 2 && bystanders.is_empty() {
            let favor = if chosen == &participants[0].0 {
                "--ours"
            } else {
                "--theirs"
            };
            let (_, expected) =
                git_merge_file(&participants[0].1, &base, &participants[1].1, Some(favor));
            prop_assert_eq!(
                String::from_utf8_lossy(&resolved),
                String::from_utf8_lossy(&expected),
                "all atoms -> {} must equal git merge-file {}",
                chosen,
                favor
            );
        }

        // Embedding: the chosen side, every bystander and (two participants,
        // line atoms) the other participant's clean edits are absorbed. With
        // three or more participants, an edit inside a region shared with a
        // third workspace is legitimately superseded by the chosen text, and
        // a single whole-file atom is a whole-file choice by design.
        assert_absorbs(
            &base,
            &resolved,
            chosen_content,
            "atom-level lost chosen side",
        )?;
        for (w, content) in &bystanders {
            assert_absorbs(
                &base,
                &resolved,
                content,
                &format!("atom resolve to {chosen} dropped bystander {w}"),
            )?;
        }
        if participants.len() == 2 && !whole_file {
            for (w, content) in &participants {
                assert_absorbs(
                    &base,
                    &resolved,
                    content,
                    &format!("atom resolve to {chosen} dropped a clean edit of {w}"),
                )?;
            }
        }
    }

    // Mixed per-atom choices (two participants): alternate sides by atom.
    if participants.len() == 2 && n_atoms >= 2 {
        let choices: Vec<&str> = (0..n_atoms)
            .map(|i| participants[i % 2].0.as_str())
            .collect();
        let resolved = resolve_record_atoms(record, &choices)
            .map_err(|e| TestCaseError::fail(format!("mixed resolve failed: {e}")))?;
        for (w, content) in participants.iter().chain(&bystanders) {
            assert_absorbs(
                &base,
                &resolved,
                content,
                &format!("mixed resolution dropped an edit of {w}"),
            )?;
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Two workspaces: resolving every atom to either side keeps the other
    /// side's clean edits and matches git's favor-side merge byte-for-byte.
    #[test]
    fn atom_resolution_two_way_matches_git_favor(sc in arb_scenario(2)) {
        check_scenario(&sc, false)?;
        check_scenario(&sc, true)?;
    }

    /// Up to four workspaces: resolution never drops an edit (or refuses).
    #[test]
    fn atom_resolution_k_way_never_drops_edits(sc in arb_scenario(4)) {
        check_scenario(&sc, false)?;
        check_scenario(&sc, true)?;
    }
}
