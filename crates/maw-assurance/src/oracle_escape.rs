//! Oracles for the 2026-07 escape paths (bn-2bcx).
//!
//! Three targeted state-coherence / content-faithfulness oracles that close
//! the gaps the 2026-07 field-report escapes slipped through. Each maps 1:1 to
//! an escaped-bug class:
//!
//! - [`SiblingRefFaithfulness`] — the **bn-rah2** class. FF-absorb
//!   (`reconcile_epoch_with_branch`) orphaned a committed-ahead *sibling*
//!   workspace by raw-resetting its HEAD to the absorbed branch tip instead of
//!   replaying it. This oracle asserts that every **live** workspace's
//!   previously-committed content stays reachable from the union of all refs
//!   (`git rev-list --all`), *unless the just-executed op legitimately moved
//!   that workspace* — so a merge that orphans a NON-target sibling's work
//!   trips it, while a sibling's own commit/advance/replay does not.
//! - [`TrunkDirtyPreservation`] — the **bn-1xmk** class. Trunk
//!   preserve-and-replay clobbered uncommitted tracked trunk files whose
//!   committed content changed via a merge. This oracle asserts the recorded
//!   uncommitted trunk bytes survive on disk **or** are surfaced in a recovery
//!   ref, after any op.
//! - [`check_record_ref_coherence`] — the **bn-3uou** class. `maw gc` desynced
//!   recovery refs from destroy records. This oracle asserts no destroy record
//!   claims (via `snapshot_ref` / `final_head_ref`) a recovery ref that does
//!   not exist.
//!
//! # Independent-verifier carveout
//!
//! Like [`crate::oracle_b`], all git access here uses the `git` CLI (cwd = repo
//! root) rather than `gix`/`maw-git`, so the verifier does not share code paths
//! with the machinery under test. Destroy records are parsed directly off disk
//! (their JSON schema fields, not the `maw-cli` `DestroyRecord` type) to avoid a
//! `maw-assurance -> maw-cli` dependency cycle — the same "read the artifact,
//! don't call the producer" discipline the rest of the oracle stack follows.

#![cfg(feature = "oracles")]
// Harness/verifier support code (like `in_proc` / `oracle_b`'s git plumbing):
// relax a few pedantic lints that hurt readability of the CLI plumbing without
// buying defect prevention. The production crates keep the strict workspace
// lints.
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::process::Command;

use maw_core::model::layout::LayoutFlavor;

use crate::scenario::Op;

// ---------------------------------------------------------------------------
// Violation type
// ---------------------------------------------------------------------------

/// A violation of one of the bn-2bcx escape-path oracles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapeViolation {
    /// **bn-rah2** — a live workspace's previously-committed content is no
    /// longer reachable from any ref, and the just-executed op did not target
    /// that workspace (so it was orphaned as a side effect — the FF-absorb
    /// sibling-reset class).
    SiblingWorkOrphaned {
        /// The workspace whose committed content was orphaned.
        workspace: String,
        /// A blob OID that was committed by `workspace` but is now unreachable
        /// from `git rev-list --all --objects`.
        blob: String,
    },

    /// **bn-1xmk** — uncommitted trunk bytes recorded at `path` neither survive
    /// on disk in the default workspace nor are surfaced in any recovery ref.
    TrunkDirtyLost {
        /// The tracked trunk path whose uncommitted bytes were lost.
        path: String,
    },

    /// **bn-3uou** — a destroy record claims a recovery ref that does not
    /// exist. `maw gc` must keep records ↔ refs coherent.
    RecordClaimsMissingRef {
        /// The destroyed workspace the record belongs to.
        workspace: String,
        /// The destroy-record file name.
        record: String,
        /// The recovery ref the record claims but that is absent from the repo.
        claimed_ref: String,
    },

    /// The oracle's own git invocation failed, so no verdict is possible.
    /// Reported as a violation so the run stops loudly rather than silently
    /// green-lighting on broken tooling (matches `oracle_b`'s `GitError`).
    GitError {
        /// Which oracle was running.
        check: &'static str,
        /// The command that failed.
        command: String,
        /// Stderr from the command.
        stderr: String,
    },
}

impl fmt::Display for EscapeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SiblingWorkOrphaned { workspace, blob } => write!(
                f,
                "SiblingRefFaithfulness (bn-rah2): live workspace '{workspace}' committed \
                 blob {blob} that is no longer reachable from any ref — its work was \
                 orphaned by an op that did not target it (FF-absorb sibling-reset class)"
            ),
            Self::TrunkDirtyLost { path } => write!(
                f,
                "TrunkDirtyPreservation (bn-1xmk): uncommitted trunk bytes at '{path}' were \
                 lost — the file no longer holds them and no recovery ref surfaces them"
            ),
            Self::RecordClaimsMissingRef {
                workspace,
                record,
                claimed_ref,
            } => write!(
                f,
                "RecordRefCoherence (bn-3uou): destroy record {workspace}/{record} claims \
                 recovery ref '{claimed_ref}' but it does not exist"
            ),
            Self::GitError {
                check,
                command,
                stderr,
            } => write!(
                f,
                "OracleEscape {check}: git error running `{command}`: {stderr}"
            ),
        }
    }
}

impl std::error::Error for EscapeViolation {}

// ---------------------------------------------------------------------------
// SiblingRefFaithfulness (bn-rah2) — stateful, incremental
// ---------------------------------------------------------------------------

/// Tracks each live workspace's committed content (blob OID set) across steps
/// and, after every op, asserts that content stays reachable from the union of
/// all refs — unless the op legitimately moved that workspace.
///
/// The design deliberately tracks **blobs**, not commit OIDs: a legitimate
/// rebase/replay (including the bn-rah2 fix's sibling replay) preserves the
/// blobs in a new commit, so blob-reachability stays green; only an orphaning
/// reset (which leaves the blobs unreferenced) trips it. The op-targeting skip
/// lets a workspace's own commit legitimately drop content (e.g. deleting a
/// file) without a false positive.
#[derive(Debug, Default, Clone)]
pub struct SiblingRefFaithfulness {
    /// `ws name -> set of blob OIDs committed by that workspace (as of the last
    /// step it was observed live)`.
    committed_blobs: BTreeMap<String, BTreeSet<String>>,
}

impl SiblingRefFaithfulness {
    /// Fresh tracker with no recorded state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check the invariant after `op` executed against `repo_root`, then
    /// refresh the tracked per-workspace committed-blob sets from the current
    /// live workspace **worktree HEADs**. Call once per step, in order.
    ///
    /// The committed tip of a maw workspace lives at its **worktree HEAD** (a
    /// plain `git commit` in the workspace advances only the detached worktree
    /// HEAD; maw does not maintain a `refs/manifold/ws/<ws>` state ref for
    /// non-default workspaces). So this oracle enumerates worktrees via
    /// `git worktree list` and reads each one's HEAD.
    ///
    /// The reachability roots are exactly Oracle A's frontier — every extant
    /// workspace's current worktree HEAD (its "current HEAD"), plus every
    /// commit-typed manifold/branch ref (epoch, `refs/heads/*`, recovery, the
    /// per-workspace epoch/state refs). A legitimate rebase/replay (whose NEW
    /// worktree HEAD still contains the blobs) stays green; an orphaning reset
    /// (which moves the workspace's HEAD to a commit that drops the blobs,
    /// leaving them reachable from no root) turns red — the bn-rah2
    /// sibling-orphan signature. Orphaned objects are excluded from `rev-list`
    /// even before `git gc` prunes them, so the reset is detected immediately.
    pub fn check_step(&mut self, repo_root: &Path, op: &Op) -> Vec<EscapeViolation> {
        let worktrees = list_worktrees(repo_root);
        let roots = frontier_roots(repo_root, &worktrees);
        let reachable = match rev_list_objects(repo_root, &roots) {
            Ok(s) => s,
            Err(v) => return vec![v],
        };
        let targeted = op_targets(op);

        let mut violations = Vec::new();
        for (ws, blobs) in &self.committed_blobs {
            if !worktrees.contains_key(ws) {
                // Destroyed / removed workspace — its lifecycle is covered by
                // Oracle A / RecordRefCoherence, not this oracle.
                continue;
            }
            if targeted.contains(ws) {
                // The op legitimately moved this workspace (commit/advance/
                // sync/merge-source/recover) — a content change here is not an
                // orphaning side effect.
                continue;
            }
            for blob in blobs {
                if !reachable.contains(blob) {
                    violations.push(EscapeViolation::SiblingWorkOrphaned {
                        workspace: ws.clone(),
                        blob: blob.clone(),
                    });
                }
            }
        }

        // Refresh: record each live workspace's current worktree-HEAD blob set.
        let mut next: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (ws, head) in &worktrees {
            let blobs = blobs_of_ref(repo_root, head);
            if !blobs.is_empty() {
                next.insert(ws.clone(), blobs);
            }
        }
        self.committed_blobs = next;

        violations
    }
}

// ---------------------------------------------------------------------------
// TrunkDirtyPreservation (bn-1xmk) — stateful
// ---------------------------------------------------------------------------

/// Records uncommitted trunk (default-workspace) writes and asserts, after any
/// op, that the recorded bytes survive — either verbatim on disk in the default
/// worktree, or surfaced as a blob reachable from a recovery ref.
#[derive(Debug, Default, Clone)]
pub struct TrunkDirtyPreservation {
    /// `path -> expected uncommitted content`. Most-recent write per path wins.
    pending: BTreeMap<String, String>,
}

impl TrunkDirtyPreservation {
    /// Fresh tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an uncommitted trunk write the harness just performed (the bytes
    /// it wrote into the default workspace's working tree at `path`).
    pub fn record_dirty(&mut self, path: &str, content: &str) {
        self.pending.insert(path.to_owned(), content.to_owned());
    }

    /// Clear the expectation for `paths` — call when the default workspace
    /// legitimately (re)commits or overwrites those paths, so the oracle stops
    /// expecting the superseded dirty bytes.
    pub fn note_trunk_overwrite<I, S>(&mut self, paths: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for p in paths {
            self.pending.remove(p.as_ref());
        }
    }

    /// Verify every recorded uncommitted trunk write still survives.
    #[must_use]
    pub fn check(&self, repo_root: &Path) -> Vec<EscapeViolation> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let default_ws = LayoutFlavor::detect(repo_root).default_target_path(repo_root, "default");
        // The set of blob OIDs reachable from recovery refs (surfaced content).
        let recovery_blobs = recovery_reachable_blobs(repo_root);

        let mut violations = Vec::new();
        for (path, content) in &self.pending {
            // 1) Bytes survive verbatim on disk in the default worktree.
            let on_disk = std::fs::read_to_string(default_ws.join(path)).ok();
            if on_disk.as_deref() == Some(content.as_str()) {
                continue;
            }
            // 2) Or the content is surfaced as a blob reachable from a recovery
            //    ref (the "explicitly surfaced in a recovery ref" escape hatch).
            if hash_blob(repo_root, content).is_some_and(|oid| recovery_blobs.contains(&oid)) {
                continue;
            }
            violations.push(EscapeViolation::TrunkDirtyLost { path: path.clone() });
        }
        violations
    }
}

// ---------------------------------------------------------------------------
// RecordRefCoherence (bn-3uou) — stateless
// ---------------------------------------------------------------------------

/// Assert no destroy record claims a recovery ref that does not exist.
///
/// Reads the destroy-record JSON artifacts directly off disk (independent
/// verifier: it does not call `maw-cli`'s writer) and checks each record's
/// claimed recovery ref (`snapshot_ref`, else `final_head_ref`) against the
/// live ref set. Every `maw gc` must leave records ↔ refs coherent.
#[must_use]
pub fn check_record_ref_coherence(repo_root: &Path) -> Vec<EscapeViolation> {
    let refs = match all_ref_names(repo_root) {
        Ok(r) => r,
        Err(v) => return vec![v],
    };
    let destroy_dir = LayoutFlavor::detect(repo_root)
        .manifold_dir(repo_root)
        .join("destroy");
    let Ok(ws_dirs) = std::fs::read_dir(&destroy_dir) else {
        return Vec::new(); // No destroy records at all.
    };

    let mut violations = Vec::new();
    // Deterministic order: sort workspace dirs, then record files.
    let mut ws_names: Vec<String> = ws_dirs
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ws_names.sort();

    for ws in ws_names {
        let dir = destroy_dir.join(&ws);
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut record_files: Vec<String> = files
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json") && n != "latest.json")
            .collect();
        record_files.sort();

        for record in record_files {
            let Ok(body) = std::fs::read_to_string(dir.join(&record)) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            // Mirror `DestroyRecord::recovery_ref`: snapshot_ref, else
            // final_head_ref.
            let claimed = json
                .get("snapshot_ref")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    json.get("final_head_ref")
                        .and_then(serde_json::Value::as_str)
                });
            if let Some(claimed_ref) = claimed.filter(|r| !refs.contains(*r)) {
                violations.push(EscapeViolation::RecordClaimsMissingRef {
                    workspace: ws.clone(),
                    record: record.clone(),
                    claimed_ref: claimed_ref.to_owned(),
                });
            }
        }
    }
    violations
}

// ---------------------------------------------------------------------------
// git / model helpers (independent-verifier carveout: CLI, not gix)
// ---------------------------------------------------------------------------

/// Enumerate live worktrees as `basename -> HEAD OID`, via
/// `git worktree list --porcelain`. Bare entries and worktrees with no
/// resolved HEAD are skipped. The basename is the workspace name maw uses.
fn list_worktrees(repo_root: &Path) -> BTreeMap<String, String> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output();
    let Ok(out) = out else {
        return BTreeMap::new();
    };
    if !out.status.success() {
        return BTreeMap::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = BTreeMap::new();
    let mut cur_name: Option<String> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            cur_name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
        } else if let Some(oid) = line.strip_prefix("HEAD ") {
            if let Some(name) = cur_name.take() {
                map.insert(name, oid.to_owned());
            }
        } else if line == "bare" {
            cur_name = None;
        }
    }
    map
}

/// The reachability roots: every extant worktree HEAD plus every commit-typed
/// manifold/branch ref (epoch, `refs/heads/*`, recovery, per-ws epoch/state).
/// Mirrors `oracle_a::compute_frontier`. Deliberately excludes the blob-typed
/// `refs/manifold/head/<ws>` oplog refs (passing a blob to `rev-list` errors).
fn frontier_roots(repo_root: &Path, worktrees: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut roots: BTreeSet<String> = worktrees.values().cloned().collect();
    if let Ok(refs) = all_ref_names(repo_root) {
        for r in &refs {
            let is_commit_ref = r == "refs/manifold/epoch/current"
                || r.starts_with("refs/heads/")
                || r.starts_with("refs/manifold/recovery/")
                || r.starts_with("refs/manifold/epoch/ws/")
                || r.starts_with(maw_core::refs::WORKSPACE_STATE_PREFIX);
            if is_commit_ref {
                roots.insert(r.clone());
            }
        }
    }
    roots
}

/// Objects reachable from `roots` (`git rev-list --objects <roots...>`).
/// Orphaned (unreferenced) objects are excluded even before `git gc` prunes
/// them, which is what makes an orphaning reset detectable immediately.
fn rev_list_objects(
    repo_root: &Path,
    roots: &BTreeSet<String>,
) -> Result<BTreeSet<String>, EscapeViolation> {
    if roots.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut args: Vec<&str> = vec!["rev-list", "--objects"];
    args.extend(roots.iter().map(String::as_str));
    let out = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output()
        .map_err(|e| EscapeViolation::GitError {
            check: "SiblingRefFaithfulness",
            command: "git rev-list --objects".to_owned(),
            stderr: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(EscapeViolation::GitError {
            check: "SiblingRefFaithfulness",
            command: format!("git rev-list --objects [{} roots]", roots.len()),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect())
}

/// Every ref name in the repo.
fn all_ref_names(repo_root: &Path) -> Result<BTreeSet<String>, EscapeViolation> {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| EscapeViolation::GitError {
            check: "RecordRefCoherence",
            command: "git for-each-ref".to_owned(),
            stderr: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(EscapeViolation::GitError {
            check: "RecordRefCoherence",
            command: "git for-each-ref".to_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// Blob OIDs in the tree of `ref_name` (recursive). Empty if the ref does not
/// resolve or points at no commit.
fn blobs_of_ref(repo_root: &Path, ref_name: &str) -> BTreeSet<String> {
    let out = Command::new("git")
        .args(["ls-tree", "-r", ref_name])
        .current_dir(repo_root)
        .output();
    let Ok(out) = out else {
        return BTreeSet::new();
    };
    if !out.status.success() {
        return BTreeSet::new();
    }
    // Line format: "<mode> <type> <oid>\t<path>". Keep blobs only.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (meta, _path) = l.split_once('\t')?;
            let mut parts = meta.split_whitespace();
            let _mode = parts.next()?;
            let kind = parts.next()?;
            let oid = parts.next()?;
            (kind == "blob").then(|| oid.to_owned())
        })
        .collect()
}

/// The set of blob OIDs reachable from any `refs/manifold/recovery/*` ref.
fn recovery_reachable_blobs(repo_root: &Path) -> BTreeSet<String> {
    // Names of every recovery ref, then their reachable blobs.
    let Ok(refs) = all_ref_names(repo_root) else {
        return BTreeSet::new();
    };
    let mut blobs = BTreeSet::new();
    for r in refs
        .iter()
        .filter(|r| r.starts_with("refs/manifold/recovery/"))
    {
        blobs.extend(blobs_of_ref(repo_root, r));
    }
    blobs
}

/// Compute the git blob OID for `content` WITHOUT writing it (`git hash-object
/// --stdin`), so we can test membership in a reachable set.
fn hash_blob(repo_root: &Path, content: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(content.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The set of workspace names an op legitimately mutates (so a content change
/// there is not an orphaning side effect).
fn op_targets(op: &Op) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    match op {
        Op::WsCreate { ws, .. }
        | Op::EditFiles { ws, .. }
        | Op::Commit { ws, .. }
        | Op::Sync { ws }
        | Op::Advance { ws }
        | Op::Destroy { ws, .. } => {
            set.insert(ws.0.clone());
        }
        Op::Merge { srcs, .. } => {
            for s in srcs {
                set.insert(s.0.clone());
            }
        }
        Op::Recover { ws, to } => {
            set.insert(ws.0.clone());
            set.insert(to.0.clone());
        }
        // Trunk-level ops target no tracked workspace.
        Op::OutOfMawCommit { .. } | Op::DirtyTrunkWrite { .. } | Op::Gc { .. } => {}
    }
    set
}

// ---------------------------------------------------------------------------
// Tests — each plants exactly one escape-class violation (and its green twin).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    use crate::scenario::{Seeded, WsId};

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A V2-layout temp repo (no `.maw/manifold` marker) with one root commit.
    fn setup_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("README.md"), "# test\n").unwrap();
        git(root, &["add", "README.md"]);
        git(root, &["commit", "-q", "-m", "init"]);
        let oid = git(root, &["rev-parse", "HEAD"]);
        (dir, oid)
    }

    /// Build a commit containing a single unique file and return its OID.
    fn commit_unique_file(root: &Path, parent: &str, path: &str, content: &str) -> String {
        let blob = {
            use std::io::Write as _;
            use std::process::Stdio;
            let mut c = Command::new("git")
                .args(["hash-object", "-w", "--stdin"])
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(content.as_bytes())
                .unwrap();
            String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout)
                .trim()
                .to_owned()
        };
        let tree = {
            use std::io::Write as _;
            use std::process::Stdio;
            let mut c = Command::new("git")
                .args(["mktree"])
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            writeln!(c.stdin.as_mut().unwrap(), "100644 blob {blob}\t{path}").unwrap();
            String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout)
                .trim()
                .to_owned()
        };
        git(root, &["commit-tree", &tree, "-p", parent, "-m", "work"])
    }

    fn make_ws_dir(root: &Path, name: &str) {
        fs::create_dir_all(root.join("ws").join(name)).unwrap();
    }

    /// Register a live workspace as a real detached worktree at `ws/<name>`
    /// with HEAD at `commit` (the worktree HEAD is the committed tip the oracle
    /// tracks — matching how maw workspaces actually store their tip).
    fn make_ws(root: &Path, name: &str, commit: &str) {
        let path = root.join("ws").join(name);
        git(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                commit,
            ],
        );
    }

    /// Move a workspace's worktree HEAD — the orphaning / replay primitive.
    fn move_ws_tip(root: &Path, name: &str, commit: &str) {
        let path = root.join("ws").join(name);
        git(&path, &["checkout", "-q", "--detach", commit]);
    }

    fn gc_op() -> Op {
        Op::Gc {
            recovery_snapshots: false,
            older_than_days: 30,
        }
    }

    // ----- SiblingRefFaithfulness (bn-rah2) --------------------------------

    #[test]
    fn sibling_orphaned_by_non_targeting_op_trips() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        // A live sibling with committed-ahead work (unique blob) as a real
        // worktree whose HEAD is at the committed-ahead commit.
        let sib_commit = commit_unique_file(
            root,
            &root_oid,
            "sibling.txt",
            "SIBLING committed-ahead work\n",
        );
        make_ws(root, "sibling", &sib_commit);

        let mut oracle = SiblingRefFaithfulness::new();
        // Step 1: a non-targeting op records the sibling's blobs, no violation
        // (its work is reachable from its own worktree HEAD).
        let v1 = oracle.check_step(root, &gc_op());
        assert!(v1.is_empty(), "step 1 should be clean: {v1:?}");

        // Step 2: ORPHAN the sibling's work — reset its WORKTREE HEAD to the
        // root commit (which does NOT contain sibling.txt). The manifold state
        // ref still pins sib_commit, but the oracle (correctly) does not count
        // state refs as reachable roots. The op does not target 'sibling'.
        move_ws_tip(root, "sibling", &root_oid);
        let v2 = oracle.check_step(root, &gc_op());
        assert!(
            v2.iter().any(|v| matches!(
                v,
                EscapeViolation::SiblingWorkOrphaned { workspace, .. } if workspace == "sibling"
            )),
            "orphaned sibling work must trip SiblingRefFaithfulness: {v2:?}"
        );
    }

    #[test]
    fn sibling_own_commit_does_not_false_positive() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let c1 = commit_unique_file(root, &root_oid, "s.txt", "v1\n");
        make_ws(root, "sibling", &c1);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());

        // The sibling itself moves its HEAD to a new commit that drops the old
        // blob. Because the op TARGETS 'sibling', this legitimate self-move
        // must NOT trip the oracle.
        let c2 = commit_unique_file(root, &c1, "s.txt", "v2 replaces v1\n");
        move_ws_tip(root, "sibling", &c2);
        let op = Op::Commit {
            ws: WsId("sibling".to_owned()),
            msg: Seeded("bump".to_owned()),
        };
        let v = oracle.check_step(root, &op);
        assert!(
            v.is_empty(),
            "a workspace's own commit must not trip the oracle: {v:?}"
        );
    }

    #[test]
    fn sibling_replay_preserving_blobs_stays_green() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        let sib = commit_unique_file(root, &root_oid, "keep.txt", "MUST SURVIVE\n");
        make_ws(root, "sibling", &sib);

        let mut oracle = SiblingRefFaithfulness::new();
        let _ = oracle.check_step(root, &gc_op());

        // Simulate a legitimate replay: move the worktree HEAD to a NEW commit
        // (different OID) that still contains the same blob. The old sib commit
        // is now unreferenced, but its blob survives via the new HEAD — so no
        // violation, even though the op does not target 'sibling'.
        let replayed = commit_unique_file(root, &root_oid, "keep.txt", "MUST SURVIVE\n");
        move_ws_tip(root, "sibling", &replayed);
        let v = oracle.check_step(root, &gc_op());
        assert!(
            v.is_empty(),
            "a replay preserving the blobs must stay green: {v:?}"
        );
    }

    // ----- TrunkDirtyPreservation (bn-1xmk) --------------------------------

    #[test]
    fn dirty_trunk_lost_trips_and_survival_is_green() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // Default worktree in V2 layout is <root>/ws/default.
        make_ws_dir(root, "default");

        let mut oracle = TrunkDirtyPreservation::new();
        oracle.record_dirty("hot.txt", "UNCOMMITTED dirty bytes\n");

        // Not on disk, not in recovery → LOST.
        let v = oracle.check(root);
        assert!(
            v.iter().any(
                |x| matches!(x, EscapeViolation::TrunkDirtyLost { path } if path == "hot.txt")
            ),
            "missing dirty bytes must trip TrunkDirtyPreservation: {v:?}"
        );

        // Write the bytes verbatim to the default worktree → survives.
        fs::write(root.join("ws/default/hot.txt"), "UNCOMMITTED dirty bytes\n").unwrap();
        assert!(
            oracle.check(root).is_empty(),
            "dirty bytes present on disk must be green"
        );
    }

    #[test]
    fn dirty_trunk_surfaced_in_recovery_ref_is_green() {
        let (dir, root_oid) = setup_repo();
        let root = dir.path();
        make_ws_dir(root, "default");

        let mut oracle = TrunkDirtyPreservation::new();
        let content = "dirty bytes surfaced via recovery\n";
        oracle.record_dirty("hot.txt", content);

        // Bytes NOT on disk, but surfaced as a blob reachable from a recovery
        // ref: build a commit containing exactly that blob and pin it under
        // refs/manifold/recovery/.
        let rec = commit_unique_file(root, &root_oid, "hot.txt", content);
        git(
            root,
            &["update-ref", "refs/manifold/recovery/default/snap", &rec],
        );
        let v = oracle.check(root);
        assert!(
            v.is_empty(),
            "dirty bytes surfaced in a recovery ref must be green: {v:?}"
        );
    }

    // ----- RecordRefCoherence (bn-3uou) ------------------------------------

    fn write_destroy_record(root: &Path, ws: &str, filename: &str, snapshot_ref: Option<&str>) {
        let dir = root.join(".manifold").join("destroy").join(ws);
        fs::create_dir_all(&dir).unwrap();
        let snap = snapshot_ref.map_or_else(|| "null".to_owned(), |r| format!("\"{r}\""));
        let body = format!(
            r#"{{"workspace_id":"{ws}","destroyed_at":"2026-07-09T00:00:00.000Z",
                "final_head":"{oid}","final_head_ref":null,"snapshot_oid":"{oid}",
                "snapshot_ref":{snap},"capture_mode":"dirty_snapshot","dirty_files":[],
                "base_epoch":"{oid}","destroy_reason":"destroy","tool_version":"test"}}"#,
            oid = "a".repeat(40),
        );
        fs::write(dir.join(filename), body).unwrap();
    }

    #[test]
    fn record_claiming_missing_ref_trips() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // A record claims a recovery ref that does not exist.
        write_destroy_record(
            root,
            "gone",
            "20260709-000000.json",
            Some("refs/manifold/recovery/gone/missing-snap"),
        );
        let v = check_record_ref_coherence(root);
        assert!(
            v.iter().any(|x| matches!(
                x,
                EscapeViolation::RecordClaimsMissingRef { workspace, .. } if workspace == "gone"
            )),
            "a record claiming a missing recovery ref must trip: {v:?}"
        );
    }

    #[test]
    fn record_with_present_ref_is_green() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let ref_name = "refs/manifold/recovery/kept/snap";
        git(root, &["update-ref", ref_name, &oid]);
        write_destroy_record(root, "kept", "20260709-000000.json", Some(ref_name));
        let v = check_record_ref_coherence(root);
        assert!(
            v.is_empty(),
            "a record whose claimed recovery ref exists must be green: {v:?}"
        );
    }

    #[test]
    fn record_with_no_snapshot_ref_is_green() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();
        // capture_mode none / no snapshot_ref → nothing to check.
        write_destroy_record(root, "nosnap", "20260709-000000.json", None);
        assert!(
            check_record_ref_coherence(root).is_empty(),
            "a record with no claimed ref must be green"
        );
    }
}
