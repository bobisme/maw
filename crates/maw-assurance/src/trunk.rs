//! Dirty-trunk tier of the in-proc SG1 driver (bn-1h9ue).
//!
//! The in-proc driver ([`crate::in_proc`]) models workspaces and merges at the
//! ref-shape level. That model had no default worktree, so the v1.0 soak never
//! replayed an uncommitted trunk edit across a merge — this cycle's largest
//! defect source (bn-3fcbu exec bits, bn-2ygs0 symlink type changes, bn-3jqfk
//! symlink/directory capture, bn-15fzo/bn-1bkr0 checkout intent). This module
//! supplies what the driver needs to close that gap:
//!
//! - [`TrunkUpdater`] — the seam through which the driver runs the
//!   **production** target update (`maw_cli::workspace::update_default_workspace`,
//!   the snapshot → checkout → replay that `maw ws merge` and merge crash
//!   recovery run) after every modelled merge. `maw-assurance` cannot depend
//!   on `maw-cli` (package cycle through `maw`), so the implementation lives in
//!   the `sg1_dst` test binary, which self-execs a helper test
//!   ([`SelfExecUpdater`] + [`run_trunk_update_helper`]). A real process per
//!   update also gives the crash windows REAL `abort()` semantics and captures
//!   the update's stdout/stderr, which the displacement oracle needs.
//! - [`apply_edit`] — applies a [`FileEdit`] of any [`EditKind`] to a
//!   directory (the default worktree, or an in-proc workspace directory).
//! - [`capture_worktree`] / [`capture_tree`] — the full entry maps (type,
//!   bytes / link target, exec bit) the replay model compares.
//! - [`judge_replay`] — the **TrunkReplayFaithfulness** reference model: given
//!   the anchor tree, the user's pre-merge worktree and the merged tree, what
//!   must be on disk after the update. It is the only oracle that sees exec
//!   bits (bn-3fcbu) and which side of a symlink type conflict was kept
//!   (bn-2ygs0); `TrunkDirtyPreservation` / `TrunkDirtyDisplacement` only ask
//!   whether the user's bytes survived.
//!
//! # Modelling gaps (documented, deliberate)
//!
//! - The merge itself is the in-proc ref-shape merge (main's tree becomes the
//!   last source's tree); only the TARGET UPDATE is production code. FF-absorb,
//!   sibling auto-rebase and the merge FSM stay with the production-code DST
//!   tier (`tests/dst_production_tier.rs`).
//! - The default worktree uses the legacy v2 shape (`<root>/ws/default`, a
//!   linked worktree of a bare root), because the in-proc workspaces live
//!   under `<root>/ws/`. `update_default_workspace` takes explicit paths, so
//!   the code under test is layout-independent; the consolidated layout is the
//!   production tier's.
//! - Crash recovery is modelled as "the next merge first re-runs the
//!   interrupted target update" — what `maw ws merge`'s auto-recovery does
//!   (`recover.rs`: anchor at the merged commit once the workspace epoch ref
//!   names it, else at the crashed merge's `epoch_before`). A crash in the
//!   plan's LAST merge is recovered by an end-of-drive drain
//!   (`InProcDriver::drain_trunk`, bn-3adck) — before it, ~16% of 64-step soak
//!   seeds ended with a pending update nobody recovered or judged.
//! - Five crash windows are inside the production update
//!   ([`crate::scenario::TARGET_UPDATE_WINDOWS`]):
//!   `FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT` (the handled snapshot-failed
//!   fallback), `FP_CLEANUP_FALLBACK_AFTER_CHECKOUT` (abort inside that
//!   fallback after its force checkout; armed together with the failing
//!   snapshot), `FP_SNAPSHOT_AFTER_CLEAN` (abort after the snapshot, the
//!   checkout intent and the clean, before the checkout),
//!   `FP_CLEANUP_AFTER_DEFAULT_CHECKOUT` (abort between checkout and replay)
//!   and `FP_CLEANUP_REPLAY_BEFORE_APPLY` (abort inside the replay, after the
//!   replay commit is pinned at `refs/manifold/replay/<ws>`, before the
//!   apply). Every other fault is modelled as "died before the update
//!   began". A completed update (live or recovery) must leave no replay pin.
//!
//! # Oracle blind spots (documented; bn-3adck sweep, narrowed by bn-36chi)
//!
//! What a clean seed does NOT prove, so coverage is not overstated:
//!
//! - **A PARTIAL replay** (a crash after the replay's first worktree write,
//!   before its last): no failpoint exists there (`FP_CLEANUP_REPLAY_BEFORE_APPLY`
//!   is before the apply), and adding one is a production change. The
//!   resume's residual-pinning of a partial replay is not exercised. Not
//!   closed.
//! - **file<->directory groups BOTH sides touched** (a directory collision):
//!   the replay settles them path by path (text merges, type conflicts,
//!   partial keeps) and the model does not predict that; left to the byte
//!   oracles. Groups only ONE side touched ARE judged (bn-36chi).
//! - **Both sides changed a regular file's bytes and the reference diff3
//!   conflicts, but the replay merged cleanly** (gix-merge accepts abutting
//!   hunks git's xdiff refuses): the model then only checks that no line
//!   either side added is missing, not the exact bytes. A clean reference
//!   merge IS checked byte-for-byte (bn-36chi), including a user file that
//!   still carries an earlier conflict's markers.
//! - **A merge that changes ONLY the exec bit of a file the user turned into
//!   a symlink**: the replay's overlap test compares content, not modes, so
//!   it keeps the user's link without a report. Accepted (nothing is lost;
//!   see `judge_replay`) — noted to the lead as a contract deviation from
//!   bn-2ygs0's "type conflicts are reported".
//! - **Whole-snapshot notices** (`replay_snapshot failed`, a stale intent's
//!   `pre-merge edits pinned at`): acknowledge every user-changed path. The
//!   resume's residual notice (`held changes beyond the interrupted
//!   update`) acknowledges only entries edited SINCE the crash (bn-3adck).
//! - **"Reported"** (`oracle_escape::classify_report`): some ONE report
//!   section (a run of lines opened by a `WARNING:` line) names the path as a
//!   token AND carries a recovery handle (bn-36chi; it used to be "the path
//!   anywhere and a handle anywhere"). Within a section the handle is still
//!   not tied to the path's own line: the conflict list and its `maw ws
//!   resolve` commands are separate paragraphs of one section.
//! - Byte oracles record only UTF-8 file content and symlink targets; mode
//!   changes and non-UTF-8 content are judged by the replay model alone.
//!
//! Closed by bn-36chi (were blind spots): the exec bit of a path absent (or a
//! symlink) at the anchor (the user's mode wins); recoveries of a TAINTED
//! pending update (judged against the pre-crash capture when maw resumes a
//! checkout intent, else against the worktree at recovery time).
#![cfg(feature = "oracles")]
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::too_many_lines)]
// `judge_replay` names the four sides of a path b/u/m/d, as in its table.
#![allow(clippy::many_single_char_names)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::scenario::{EditKind, FileEdit};

// ---------------------------------------------------------------------------
// The production target-update seam
// ---------------------------------------------------------------------------

/// One run of the production target update, as the driver requests it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrunkUpdateRequest {
    /// The default worktree (`<root>/ws/default`).
    pub default_ws_path: PathBuf,
    /// The repo root (refs, manifold dir).
    pub repo_root: PathBuf,
    /// Branch the default worktree follows (`main`).
    pub branch: String,
    /// The epoch the default worktree was on before the merge.
    pub epoch_before: String,
    /// The merged commit.
    pub epoch_after: String,
    /// Merge source workspace names (for the replay's conflict labels).
    pub sources: Vec<String>,
    /// `MAW_FP` spec armed for this run (`None` = no fault).
    pub maw_fp: Option<String>,
}

/// What one run of the target update did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrunkUpdateOutcome {
    /// Combined stdout + stderr (what the user would have seen).
    pub output: String,
    /// `true` iff the process died without reporting a result (a real
    /// `abort()` at a crash window, or any other abnormal death).
    pub crashed: bool,
    /// `Some(err)` iff the update returned an error. Like a crash, the merge
    /// command would exit non-zero with its journal left for recovery.
    pub error: Option<String>,
}

impl TrunkUpdateOutcome {
    /// The update ran to completion (returned `Ok`).
    #[must_use]
    pub const fn completed(&self) -> bool {
        !self.crashed && self.error.is_none()
    }
}

/// Runs the production target update. See the module docs.
pub trait TrunkUpdater: Send + Sync {
    /// Run one target update. `Err` = the harness could not run it at all
    /// (spawn failure etc.) — a harness error, never a verdict.
    fn update(&self, req: &TrunkUpdateRequest) -> std::io::Result<TrunkUpdateOutcome>;
}

static UPDATER: OnceLock<Arc<dyn TrunkUpdater>> = OnceLock::new();

/// Install the process-wide target updater. Every [`crate::in_proc::InProcDriver`]
/// constructed afterwards runs the dirty-trunk tier. Returns `false` if one
/// was already installed (the first one stays).
pub fn install_trunk_updater(updater: Arc<dyn TrunkUpdater>) -> bool {
    UPDATER.set(updater).is_ok()
}

/// The installed updater, if any.
#[must_use]
pub fn installed_trunk_updater() -> Option<Arc<dyn TrunkUpdater>> {
    UPDATER.get().cloned()
}

/// Env var carrying the JSON [`TrunkUpdateRequest`] to the helper process.
pub const REQUEST_ENV: &str = "SG1_TRUNK_UPDATE_REQUEST";
/// Result line the helper prints last on stdout.
const RESULT_MARKER: &str = "[sg1-trunk-update] result=";

/// A [`TrunkUpdater`] that re-executes a libtest binary, running exactly one
/// (ignored) helper test that calls [`run_trunk_update_helper`].
#[derive(Clone, Debug)]
pub struct SelfExecUpdater {
    /// The test binary (normally `std::env::current_exe()`).
    pub exe: PathBuf,
    /// The helper test's name (run with `--exact --ignored`).
    pub test_name: String,
}

impl TrunkUpdater for SelfExecUpdater {
    fn update(&self, req: &TrunkUpdateRequest) -> std::io::Result<TrunkUpdateOutcome> {
        let json = serde_json::to_string(req).map_err(std::io::Error::other)?;
        let mut cmd = Command::new(&self.exe);
        cmd.args([
            self.test_name.as_str(),
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env(REQUEST_ENV, json)
        .env("RUST_BACKTRACE", "0")
        .env_remove("MAW_FP")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null());
        if let Some(spec) = &req.maw_fp {
            cmd.env("MAW_FP", spec);
        }
        let out = match cmd.output() {
            Ok(out) => out,
            Err(err) => {
                crate::infra::raise_if_infra_io(&err, "spawn trunk-update helper");
                return Err(err);
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        crate::infra::raise_if_infra_text(&stderr, "trunk-update helper");
        let result = stdout
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(RESULT_MARKER))
            .map(str::to_owned);
        // Strip the libtest chatter and our marker from what the "user" saw.
        let mut output = String::new();
        for line in stdout.lines() {
            if line.starts_with(RESULT_MARKER)
                || line.starts_with("running ")
                || line.starts_with("test result:")
                || line.trim() == "."
                || line.trim().is_empty() && output.is_empty()
            {
                continue;
            }
            output.push_str(line);
            output.push('\n');
        }
        output.push_str(&stderr);
        let (crashed, error) = match result.as_deref() {
            Some("ok") => (false, None),
            Some(other) => (
                false,
                Some(other.strip_prefix("err ").unwrap_or(other).to_owned()),
            ),
            None => (true, None),
        };
        // A crash is ONLY the requested one: an armed `abort` failpoint that
        // killed the process with SIGABRT. Any other death without a result —
        // the helper not built into this binary (libtest exits 0 with no
        // matching test), a panic in the production update (exit 101), a
        // spawn/parse failure — is a harness error even while an `abort` is
        // armed (bn-3adck: a panic before the armed site used to read as the
        // requested crash, so the next merge "recovered" it and a production
        // panic never surfaced).
        if result.is_none()
            && !(req.maw_fp.as_deref().is_some_and(|s| s.contains("=abort"))
                && killed_by_sigabrt(out.status))
        {
            return Err(std::io::Error::other(format!(
                "trunk-update helper died unexpectedly (status {}): {}",
                out.status,
                output.trim()
            )));
        }
        Ok(TrunkUpdateOutcome {
            output,
            crashed,
            error,
        })
    }
}

/// Whether the process was killed by SIGABRT (what `std::process::abort`, and
/// so an armed `abort` failpoint, does).
#[cfg(unix)]
fn killed_by_sigabrt(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    const SIGABRT: i32 = 6;
    status.signal() == Some(SIGABRT)
}

/// Helper-process side of [`SelfExecUpdater`]. A no-op unless [`REQUEST_ENV`]
/// is set (so an ordinary `cargo test -- --ignored` run passes through it).
/// Arms `MAW_FP` via `maw_core::failpoints::init_from_env`, runs `update`,
/// and prints the result marker. An armed `abort` failpoint kills the process
/// before the marker — which the parent reads as a crash.
pub fn run_trunk_update_helper(update: impl FnOnce(&TrunkUpdateRequest) -> Result<(), String>) {
    let Ok(raw) = std::env::var(REQUEST_ENV) else {
        return;
    };
    let req: TrunkUpdateRequest =
        serde_json::from_str(&raw).expect("SG1_TRUNK_UPDATE_REQUEST is a TrunkUpdateRequest");
    maw_core::failpoints::init_from_env();
    let result = update(&req);
    let mut stdout = std::io::stdout();
    match result {
        Ok(()) => {
            let _ = writeln!(stdout, "{RESULT_MARKER}ok");
        }
        Err(e) => {
            let one_line = e.replace('\n', " | ");
            let _ = writeln!(stdout, "{RESULT_MARKER}err {one_line}");
        }
    }
    let _ = stdout.flush();
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/// Remove whatever is at `abs` (file, symlink or directory). Absent is fine.
fn remove_entry(abs: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(abs) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(abs),
        Ok(_) => std::fs::remove_file(abs),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Make every ancestor of `rel` (inside `dir`) a real directory, replacing a
/// file or symlink that is in the way.
fn ensure_parent_dirs(dir: &Path, rel: &Path) -> std::io::Result<()> {
    let mut cur = dir.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                std::fs::remove_file(&cur)?;
                std::fs::create_dir(&cur)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&cur)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_exec(abs: &Path, exec: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(abs)?.permissions().mode();
    let new = if exec {
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    std::fs::set_permissions(abs, std::fs::Permissions::from_mode(new))
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

/// Apply one edit inside `dir`. Every kind replaces whatever entry is at the
/// path (see [`EditKind`]).
pub fn apply_edit(dir: &Path, edit: &FileEdit) -> std::io::Result<()> {
    let rel = Path::new(&edit.path);
    let abs = dir.join(rel);
    ensure_parent_dirs(dir, rel)?;
    match edit.kind {
        EditKind::Write => {
            remove_entry(&abs)?;
            std::fs::write(&abs, &edit.content)
        }
        EditKind::ExecFlip => {
            let meta = std::fs::symlink_metadata(&abs).ok();
            match meta {
                Some(m) if m.is_file() => set_exec(&abs, !is_exec(&m)),
                _ => {
                    remove_entry(&abs)?;
                    std::fs::write(&abs, &edit.content)?;
                    set_exec(&abs, true)
                }
            }
        }
        EditKind::Delete => remove_entry(&abs),
        EditKind::Symlink => {
            remove_entry(&abs)?;
            std::os::unix::fs::symlink(&edit.content, &abs)
        }
        EditKind::Dir => {
            remove_entry(&abs)?;
            std::fs::create_dir(&abs)?;
            std::fs::write(abs.join(DIR_INNER), &edit.content)
        }
    }
}

/// The file an [`EditKind::Dir`] edit creates inside its directory.
pub const DIR_INNER: &str = "inner.txt";

// ---------------------------------------------------------------------------
// Entry maps
// ---------------------------------------------------------------------------

/// One worktree / tree entry, as far as the replay model cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrunkEntry {
    /// A regular file.
    File {
        /// Its bytes.
        bytes: Vec<u8>,
        /// Whether it is executable.
        exec: bool,
    },
    /// A symbolic link.
    Symlink(Vec<u8>),
}

impl TrunkEntry {
    const fn is_symlink(&self) -> bool {
        matches!(self, Self::Symlink(_))
    }

    fn describe(e: Option<&Self>) -> String {
        match e {
            None => "absent".to_owned(),
            Some(Self::File { bytes, exec }) => format!(
                "{}file {:?}",
                if *exec { "exec " } else { "" },
                String::from_utf8_lossy(&bytes[..bytes.len().min(48)])
            ),
            Some(Self::Symlink(t)) => format!("symlink -> {}", String::from_utf8_lossy(t)),
        }
    }
}

/// Path → entry.
pub type EntryMap = BTreeMap<String, TrunkEntry>;

/// Every file / symlink under `dir` (never following links), skipping the
/// top-level `.git`. Paths are `/`-separated and relative.
pub fn capture_worktree(dir: &Path) -> std::io::Result<EntryMap> {
    let mut out = EntryMap::new();
    capture_dir(dir, "", &mut out)?;
    Ok(out)
}

fn capture_dir(abs: &Path, prefix: &str, out: &mut EntryMap) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(abs)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if prefix.is_empty() && name == ".git" {
            continue;
        }
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let p = entry.path();
        let meta = std::fs::symlink_metadata(&p)?;
        if meta.file_type().is_symlink() {
            let t = std::fs::read_link(&p)?;
            out.insert(
                rel,
                TrunkEntry::Symlink(t.as_os_str().as_encoded_bytes().to_vec()),
            );
        } else if meta.is_dir() {
            capture_dir(&p, &rel, out)?;
        } else if meta.is_file() {
            out.insert(
                rel,
                TrunkEntry::File {
                    bytes: std::fs::read(&p)?,
                    exec: is_exec(&meta),
                },
            );
        }
    }
    Ok(())
}

/// Every blob / symlink of `commit`'s tree (recursively).
pub fn capture_tree(root: &Path, commit: &str) -> std::io::Result<EntryMap> {
    let out = Command::new("git")
        .args(["ls-tree", "-r", "-z", commit])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git ls-tree -r {commit}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut listed: Vec<(String, String, String)> = Vec::new(); // (mode, oid, path)
    for rec in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        let mut it = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(oid)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if kind != "blob" {
            continue; // gitlinks never occur in the in-proc model
        }
        listed.push((mode.to_owned(), oid.to_owned(), path.to_owned()));
    }
    if listed.is_empty() {
        return Ok(EntryMap::new());
    }
    // One `cat-file --batch` for every blob.
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("cat-file: no stdin"))?;
        let mut req = String::new();
        for (_, oid, _) in &listed {
            req.push_str(oid);
            req.push('\n');
        }
        stdin.write_all(req.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git cat-file --batch: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut buf: &[u8] = &out.stdout;
    let mut map = EntryMap::new();
    for (mode, _oid, path) in listed {
        let nl = buf
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| std::io::Error::other("cat-file: truncated header"))?;
        let header = String::from_utf8_lossy(&buf[..nl]).into_owned();
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other(format!("cat-file: bad header {header:?}")))?;
        let body = buf
            .get(nl + 1..nl + 1 + size)
            .ok_or_else(|| std::io::Error::other("cat-file: truncated body"))?
            .to_vec();
        buf = buf.get(nl + 2 + size..).unwrap_or_default();
        let entry = match mode.as_str() {
            "120000" => TrunkEntry::Symlink(body),
            "100755" => TrunkEntry::File {
                bytes: body,
                exec: true,
            },
            _ => TrunkEntry::File {
                bytes: body,
                exec: false,
            },
        };
        map.insert(path, entry);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// TrunkReplayFaithfulness — the reference model
// ---------------------------------------------------------------------------

/// One path the update left in a state the reference model forbids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayMismatch {
    /// The trunk path.
    pub path: String,
    /// What the model expected vs. what is on disk.
    pub detail: String,
}

/// Verdicts of one [`judge_replay`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayJudgement {
    /// Paths the model rendered a verdict on.
    pub judged: u64,
    /// Forbidden outcomes.
    pub mismatches: Vec<ReplayMismatch>,
}

/// Whether `bytes` carry both a start and an end diff3 marker line.
fn has_markers(bytes: &[u8]) -> bool {
    crate::oracle_a::is_conflict_marker_blob(bytes)
}

/// Paths involved in a file<->directory relation in any of `maps`: a path
/// that is also a directory in some map, and everything under such a path.
/// The reference model judges them per group ([`judge_df_groups`]), not with
/// the per-path table.
fn df_involved(maps: &[&EntryMap]) -> BTreeSet<String> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for m in maps {
        for p in m.keys() {
            let mut cur = p.as_str();
            while let Some((parent, _)) = cur.rsplit_once('/') {
                dirs.insert(parent.to_owned());
                cur = parent;
            }
        }
    }
    let mut involved = BTreeSet::new();
    for m in maps {
        for p in m.keys() {
            if dirs.contains(p) {
                involved.insert(p.clone());
            }
        }
    }
    let roots: Vec<String> = involved.iter().cloned().collect();
    for m in maps {
        for p in m.keys() {
            if roots.iter().any(|r| {
                p.len() > r.len() && p.starts_with(r.as_str()) && p.as_bytes()[r.len()] == b'/'
            }) {
                involved.insert(p.clone());
            }
        }
    }
    involved
}

/// **TrunkReplayFaithfulness** (bn-1h9ue): judge one completed target update.
///
/// `base` is the anchor tree the update replayed the user's edits against,
/// `user` the default worktree right before the update (for a recovered
/// update: before the crashed attempt), `merged` the merged tree, `disk` the
/// worktree after the update and `output` the update's combined output. Per
/// path (union of the four maps; file<->directory paths by group):
///
/// | user vs base | merged vs base | must be on disk |
/// |---|---|---|
/// | unchanged | any | the merged entry — type, bytes and exec bit |
/// | changed | unchanged | the user's entry |
/// | changed | changed, equal to user | that entry |
/// | changed | deleted | the user's entry (kept local) |
/// | deleted | changed | the merged entry, or nothing |
/// | changed | changed, a symlink on either side | the MERGED entry, and the output names the path with a way back (bn-2ygs0 type conflict) |
/// | changed (symlink) | only the exec bit changed | the user's link (not an overlap for the replay) |
/// | changed | changed, both regular files | a regular file; bytes: the side that changed them; both changed ⇒ the clean `git merge-file` result, or reported markers (bn-36chi); exec bit: the user's if the user changed it (or the anchor had no regular file), else the merged one (bn-3fcbu) |
///
/// File<->directory paths are judged per group (see `judge_df_groups`). A
/// whole-snapshot notice ("replay_snapshot failed") acknowledges every
/// user-changed path, which is then not judged.
#[must_use]
pub fn judge_replay(
    base: &EntryMap,
    user: &EntryMap,
    merged: &EntryMap,
    disk: &EntryMap,
    output: &str,
) -> ReplayJudgement {
    let whole_notice = crate::oracle_escape::has_whole_snapshot_notice(output);
    let skip = df_involved(&[base, user, merged, disk]);
    let mut paths: BTreeSet<&String> = BTreeSet::new();
    for m in [base, user, merged, disk] {
        paths.extend(m.keys());
    }
    let mut j = ReplayJudgement::default();
    judge_df_groups(&skip, [base, user, merged, disk], output, &mut j);
    for p in paths {
        if skip.contains(p) {
            continue;
        }
        let (b, u, m, d) = (base.get(p), user.get(p), merged.get(p), disk.get(p));
        let mut fail = |why: &str, expected: String| {
            j.mismatches.push(ReplayMismatch {
                path: p.clone(),
                detail: format!(
                    "{why}: expected {expected}, on disk {} (base {}, user {}, merged {})",
                    TrunkEntry::describe(d),
                    TrunkEntry::describe(b),
                    TrunkEntry::describe(u),
                    TrunkEntry::describe(m),
                ),
            });
        };
        if u == b {
            j.judged += 1;
            if d != m {
                fail(
                    "a path the user did not touch is not the merged entry",
                    TrunkEntry::describe(m),
                );
            }
            continue;
        }
        if whole_notice {
            continue;
        }
        j.judged += 1;
        let reported = || {
            crate::oracle_escape::report_for(output, p) == crate::oracle_escape::Report::Displaced
        };
        // Both sides changed and the output reported the path with a way
        // back while the MERGED side is on disk: the documented outcome of
        // the snapshot-failed fallback ("could not be replayed ... restore
        // yours: ...") and of every conflict that keeps the merged side.
        if m != b && u != m && d == m && reported() {
            continue;
        }
        if m == b || u == m || m.is_none() {
            if d != u {
                fail(
                    "the user's uncommitted entry was not kept",
                    TrunkEntry::describe(u),
                );
            }
            continue;
        }
        let (Some(ue), Some(me)) = (u, m) else {
            // The user deleted a path the merge changed: merged, nothing, or
            // a reported delete/modify conflict (markers).
            let reported_markers = matches!(d, Some(TrunkEntry::File { bytes, .. }) if has_markers(bytes))
                && reported();
            if d.is_some() && d != m && !reported_markers {
                fail(
                    "user-deleted, merge-changed path is neither merged nor absent",
                    TrunkEntry::describe(m),
                );
            }
            continue;
        };
        // The merge changed only the executable bit of a regular file the
        // user turned into a symlink. The replay's overlap test compares
        // committed CONTENT, not modes (`committed_content_changed`), so the
        // path is not an overlap: the user's link is restored and nothing is
        // reported. Nothing is lost (the mode change is committed, and a
        // mode has no meaning on a link), so the model accepts the user's
        // entry here — and requires it.
        if let (
            TrunkEntry::Symlink(_),
            TrunkEntry::File { bytes: mb, .. },
            Some(TrunkEntry::File { bytes: bb, .. }),
        ) = (ue, me, b)
            && mb == bb
        {
            if d != u {
                fail(
                    "the user's symlink over a mode-only merge change was not kept",
                    TrunkEntry::describe(u),
                );
            }
            continue;
        }
        if ue.is_symlink() || me.is_symlink() {
            // bn-2ygs0: a symlink and a file (or two targets) cannot merge;
            // the merged side is kept on disk and the conflict is reported.
            if d != m {
                fail(
                    "symlink type conflict did not keep the merged side",
                    TrunkEntry::describe(m),
                );
            } else if crate::oracle_escape::report_for(output, p)
                != crate::oracle_escape::Report::Displaced
            {
                fail(
                    "symlink type conflict was not reported with a way back",
                    "a conflict report naming the path".to_owned(),
                );
            }
            continue;
        }
        let (
            TrunkEntry::File {
                bytes: ub,
                exec: ux,
            },
            TrunkEntry::File {
                bytes: mb,
                exec: mx,
            },
        ) = (ue, me)
        else {
            continue;
        };
        let Some(TrunkEntry::File {
            bytes: db,
            exec: dx,
        }) = d
        else {
            fail(
                "both sides edited a regular file but no regular file is on disk",
                "a regular file".to_owned(),
            );
            continue;
        };
        let base_file = match b {
            Some(TrunkEntry::File { bytes, exec }) => Some((bytes, *exec)),
            _ => None,
        };
        // Exec bit (bn-3fcbu): the user's if the user changed it, else merged.
        // With no regular file at the anchor (absent, or a symlink) the
        // user's side is an addition / type change, and the replay's mode
        // reconciliation lets the user's own mode win (bn-36chi).
        let want = match base_file {
            Some((_, bx)) if *ux == bx => *mx,
            _ => *ux,
        };
        if *dx != want {
            fail(
                "executable bit is not the 3-way result",
                format!("exec={want}"),
            );
            continue;
        }
        // Bytes: the side that changed them.
        let want_bytes = match base_file {
            Some((bb, _)) if bb == ub => Some(mb),
            Some((bb, _)) if bb == mb => Some(ub),
            _ if ub == mb => Some(ub),
            _ => None,
        };
        let reported = || {
            crate::oracle_escape::report_for(output, p) == crate::oracle_escape::Report::Displaced
        };
        if let Some(w) = want_bytes {
            if db != w {
                fail(
                    "file bytes are not the side that changed them",
                    format!("{:?}", String::from_utf8_lossy(&w[..w.len().min(48)])),
                );
            }
            continue;
        }
        // Both sides changed the bytes differently (bn-36chi): judge against
        // an independent diff3 — `git merge-file` over the same three inputs
        // the replay merges (an absent anchor is empty; a symlink anchor
        // contributes its target, as `read_file_at_commit` does). A clean
        // reference merge must be on disk exactly (or, if the replay routed
        // it through markers — the post-merge sanity check — reported); a
        // conflicting one must leave reported markers. This also judges a
        // user file that already carried markers from an earlier conflict,
        // which the byte oracles cannot.
        let bb: &[u8] = match b {
            Some(TrunkEntry::File { bytes, .. } | TrunkEntry::Symlink(bytes)) => bytes,
            None => &[],
        };
        match reference_merge(bb, mb, ub) {
            Ok(Some(clean)) => {
                if *db != clean && !(has_markers(db) && reported()) {
                    fail(
                        "file bytes are not the clean 3-way merge",
                        format!(
                            "{:?}",
                            String::from_utf8_lossy(&clean[..clean.len().min(48)])
                        ),
                    );
                }
            }
            // The reference conflicts. The replay's gix-merge may still merge
            // cleanly where git's xdiff calls ABUTTING hunks a conflict (seen
            // with a user file that carries an earlier conflict's markers
            // right next to the merged change). So: NEW markers (more
            // marker blocks than the user's own content had) must be
            // reported; no new markers = the replay merged cleanly, and then
            // no line either side added may be missing.
            Ok(None) => {
                if marker_blocks(db) > marker_blocks(ub) {
                    if !reported() {
                        fail(
                            "conflict markers were written but the conflict was not reported",
                            "a conflict report naming the path".to_owned(),
                        );
                    }
                } else if let Some(line) =
                    dropped_side_line(bb, mb, db).or_else(|| dropped_side_line(bb, ub, db))
                {
                    fail(
                        "a clean replay dropped a line one side added",
                        format!("a file containing {:?}", String::from_utf8_lossy(line)),
                    );
                }
            }
            Err(e) => fail(
                "reference 3-way merge failed (harness)",
                format!("git merge-file: {e}"),
            ),
        }
    }
    j
}

/// Number of diff3 conflict blocks (`<<<<<<< ` lines) in `bytes`.
fn marker_blocks(bytes: &[u8]) -> usize {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| l.starts_with(b"<<<<<<< ") || *l == b"<<<<<<<")
        .count()
}

/// A line `side` added relative to `base` (multiset difference) that `disk`
/// does not hold as often, if any.
fn dropped_side_line<'a>(base: &[u8], side: &'a [u8], disk: &[u8]) -> Option<&'a [u8]> {
    let count = |bytes: &'a [u8]| {
        let mut m: BTreeMap<&'a [u8], usize> = BTreeMap::new();
        for l in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            *m.entry(l).or_default() += 1;
        }
        m
    };
    let (b, d) = (count_owned(base), count_owned(disk));
    count(side).into_iter().find_map(|(line, n)| {
        let added = n.saturating_sub(b.get(line).copied().unwrap_or(0));
        (added > 0 && d.get(line).copied().unwrap_or(0) < added).then_some(line)
    })
}

fn count_owned(bytes: &[u8]) -> BTreeMap<Vec<u8>, usize> {
    let mut m: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for l in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        *m.entry(l.to_vec()).or_default() += 1;
    }
    m
}

/// `git merge-file -p` over (anchor, merged, user): `Some(bytes)` when the
/// reference diff3 merges cleanly, `None` when it conflicts. The replay's
/// own merge is gix-merge's port of the same xdiff algorithm.
fn reference_merge(base: &[u8], merged: &[u8], user: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
    let dir = tempfile::TempDir::new()?;
    let write = |name: &str, bytes: &[u8]| -> std::io::Result<PathBuf> {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes)?;
        Ok(p)
    };
    let (m, b, u) = (
        write("merged", merged)?,
        write("base", base)?,
        write("user", user)?,
    );
    let out = Command::new("git")
        .args(["merge-file", "-p", "--diff3"])
        .arg(&m)
        .arg(&b)
        .arg(&u)
        .stdin(Stdio::null())
        .output()?;
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(c) if c > 0 && c < 128 => Ok(None),
        _ => Err(std::io::Error::other(format!(
            "exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// bn-36chi: judge the file<->directory paths ([`df_involved`]) group by
/// group (a group = one path that is a file on some side and a directory on
/// another, plus everything under it). When only ONE side touched a group
/// the outcome is determined: the user did not touch it ⇒ the merged
/// entries, exactly; only the user touched it ⇒ the user's entries exactly,
/// or the merged (= anchor) entries with a report (the replay's conservative
/// `directory_change`). A group BOTH sides touched is a directory collision
/// the replay settles path by path; it is left to the byte oracles
/// (documented blind spot).
fn judge_df_groups(
    involved: &BTreeSet<String>,
    [base, user, merged, disk]: [&EntryMap; 4],
    output: &str,
    j: &mut ReplayJudgement,
) {
    let whole_notice = crate::oracle_escape::has_whole_snapshot_notice(output);
    let mut groups: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for p in involved {
        let root = involved
            .iter()
            .filter(|r| {
                p == *r
                    || (p.len() > r.len()
                        && p.starts_with(r.as_str())
                        && p.as_bytes()[r.len()] == b'/')
            })
            .min_by_key(|r| r.len())
            .map_or(p.as_str(), String::as_str);
        groups.entry(root).or_default().push(p);
    }
    for members in groups.values() {
        let touched = |side: &EntryMap| members.iter().any(|p| side.get(*p) != base.get(*p));
        let all_are = |want: &EntryMap| members.iter().all(|p| disk.get(*p) == want.get(*p));
        let (by_user, by_merge) = (touched(user), touched(merged));
        // Both sides touched the group: a directory collision the replay
        // settles path by path (text merges, type conflicts, partial keeps)
        // — not modelled; left to the byte oracles (documented blind spot).
        if by_user && (by_merge || whole_notice) {
            continue;
        }
        j.judged += members.len() as u64;
        let (ok, want, why) = if by_user {
            // The replay reports a user file<->directory swap that collides
            // with the MERGED tree's shape as a `directory_change` and keeps
            // the merged side — even when the merge did not touch the group
            // (it compares shapes against the merged tree, not changes):
            // conservative, reported, nothing lost. Accepted outcomes: the
            // whole group is the user's,
            // or the whole group is merged AND some member is reported with
            // a way back. Never a mix. (Only reached when the merge left the
            // group alone, so "merged" is also the anchor's shape.)
            let reported = members.iter().any(|p| {
                crate::oracle_escape::report_for(output, p)
                    == crate::oracle_escape::Report::Displaced
            });
            (
                all_are(user) || (all_are(merged) && reported),
                user,
                "the user's file<->directory change (over a merge that left the group alone) \
                 was neither kept nor reported",
            )
        } else {
            (
                all_are(merged),
                merged,
                "a file<->directory path the user did not touch is not the merged entry",
            )
        };
        if ok {
            continue;
        }
        let p = members
            .iter()
            .find(|p| disk.get(**p) != want.get(**p))
            .unwrap_or(&members[0]);
        j.mismatches.push(ReplayMismatch {
            path: (*p).clone(),
            detail: format!(
                "{why}: expected {}, on disk {} (base {}, user {}, merged {})",
                TrunkEntry::describe(want.get(*p)),
                TrunkEntry::describe(disk.get(*p)),
                TrunkEntry::describe(base.get(*p)),
                TrunkEntry::describe(user.get(*p)),
                TrunkEntry::describe(merged.get(*p)),
            ),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> TrunkEntry {
        TrunkEntry::File {
            bytes: s.as_bytes().to_vec(),
            exec: false,
        }
    }
    fn x(s: &str) -> TrunkEntry {
        TrunkEntry::File {
            bytes: s.as_bytes().to_vec(),
            exec: true,
        }
    }
    fn l(s: &str) -> TrunkEntry {
        TrunkEntry::Symlink(s.as_bytes().to_vec())
    }
    fn map(entries: &[(&str, TrunkEntry)]) -> EntryMap {
        entries
            .iter()
            .map(|(p, e)| ((*p).to_owned(), e.clone()))
            .collect()
    }

    #[test]
    fn untouched_path_must_be_merged() {
        let b = map(&[("a", f("1"))]);
        let m = map(&[("a", f("2"))]);
        let ok = judge_replay(&b, &b, &m, &m, "");
        assert!(ok.mismatches.is_empty());
        assert_eq!(ok.judged, 1);
        let bad = judge_replay(&b, &b, &m, &b, "");
        assert_eq!(bad.mismatches.len(), 1, "{bad:?}");
    }

    #[test]
    fn merged_exec_flip_survives_user_content_edit() {
        // bn-3fcbu: user edits content, merge only flips +x.
        let b = map(&[("a", f("1"))]);
        let u = map(&[("a", f("user"))]);
        let m = map(&[("a", x("1"))]);
        let good = map(&[("a", x("user"))]);
        assert!(judge_replay(&b, &u, &m, &good, "").mismatches.is_empty());
        let reverted = map(&[("a", f("user"))]);
        let v = judge_replay(&b, &u, &m, &reverted, "");
        assert_eq!(v.mismatches.len(), 1, "{v:?}");
        assert!(v.mismatches[0].detail.contains("executable bit"), "{v:?}");
    }

    #[test]
    fn user_exec_flip_wins() {
        let b = map(&[("a", f("1"))]);
        let u = map(&[("a", x("1"))]);
        let m = map(&[("a", f("2"))]);
        assert!(
            judge_replay(&b, &u, &m, &map(&[("a", x("2"))]), "")
                .mismatches
                .is_empty()
        );
        assert_eq!(
            judge_replay(&b, &u, &m, &map(&[("a", f("2"))]), "")
                .mismatches
                .len(),
            1
        );
    }

    #[test]
    fn symlink_type_conflict_keeps_merged_and_reports() {
        // bn-2ygs0: merge turned link into a file, user retargeted it.
        let b = map(&[("link", l("a"))]);
        let u = map(&[("link", l("b"))]);
        let m = map(&[("link", f("now a file"))]);
        let report = "  type conflict: link\n    restore yours: maw ws recover --ref R --restore-file link\n";
        assert!(judge_replay(&b, &u, &m, &m, report).mismatches.is_empty());
        // Silent: merged kept but not reported.
        assert_eq!(judge_replay(&b, &u, &m, &m, "").mismatches.len(), 1);
        // Wrong side kept (the user's) even though reported.
        assert_eq!(judge_replay(&b, &u, &m, &u, report).mismatches.len(), 1);
    }

    #[test]
    fn user_symlink_on_merge_untouched_path_must_survive() {
        // bn-3jqfk: a retargeted link the merge did not touch.
        let b = map(&[("link", l("a")), ("o", f("1"))]);
        let u = map(&[("link", l("b")), ("o", f("1"))]);
        let m = map(&[("link", l("a")), ("o", f("2"))]);
        let good = map(&[("link", l("b")), ("o", f("2"))]);
        assert!(judge_replay(&b, &u, &m, &good, "").mismatches.is_empty());
        let followed = map(&[("link", f("contents of a")), ("o", f("2"))]);
        assert_eq!(judge_replay(&b, &u, &m, &followed, "").mismatches.len(), 1);
    }

    #[test]
    fn unreported_markers_fail() {
        let b = map(&[("a", f("base\n"))]);
        let u = map(&[("a", f("user\n"))]);
        let m = map(&[("a", f("merged\n"))]);
        let marked = map(&[(
            "a",
            f("<<<<<<< ws\nmerged\n=======\nuser\n>>>>>>> default\n"),
        )]);
        assert_eq!(judge_replay(&b, &u, &m, &marked, "").mismatches.len(), 1);
        let rep = "    [             content] a\n    maw ws resolve default --keep default\n";
        assert!(judge_replay(&b, &u, &m, &marked, rep).mismatches.is_empty());
    }

    #[test]
    fn whole_snapshot_notice_acknowledges_user_paths() {
        let b = map(&[("a", f("1"))]);
        let u = map(&[("a", f("u"))]);
        let m = map(&[("a", l("t"))]);
        let out = "WARNING: replay_snapshot failed: boom\n";
        assert!(judge_replay(&b, &u, &m, &m, out).mismatches.is_empty());
    }

    /// A group BOTH sides touched stays unjudged (documented blind spot).
    #[test]
    fn file_dir_paths_are_left_to_the_byte_oracles() {
        let b = map(&[("d", f("1"))]);
        let u = map(&[("d/inner.txt", f("u"))]);
        let m = map(&[("d", f("2"))]);
        let j = judge_replay(&b, &u, &m, &m, "");
        assert!(j.mismatches.is_empty());
        assert_eq!(j.judged, 0);
    }

    /// bn-36chi: both sides changed the bytes. A clean diff3 must be on disk
    /// exactly; the user's hunks silently dropped is a mismatch.
    #[test]
    fn both_sides_changed_bytes_judged_against_diff3() {
        let b = map(&[("a", f("1\n2\n3\n4\n5\n"))]);
        let u = map(&[("a", f("1\nU\n3\n4\n5\n"))]);
        let m = map(&[("a", f("1\n2\n3\n4\nM\n"))]);
        let good = map(&[("a", f("1\nU\n3\n4\nM\n"))]);
        let j = judge_replay(&b, &u, &m, &good, "");
        assert!(j.mismatches.is_empty(), "{j:?}");
        assert_eq!(j.judged, 1);
        // The user's hunk dropped (merged bytes kept, nothing reported).
        let v = judge_replay(&b, &u, &m, &m, "");
        assert_eq!(v.mismatches.len(), 1, "{v:?}");
        assert!(
            v.mismatches[0].detail.contains("clean 3-way merge"),
            "{v:?}"
        );
    }

    /// bn-36chi: a user file that already carries an earlier conflict's
    /// markers is judged too (the byte oracles skip it).
    #[test]
    fn user_file_with_old_markers_is_judged() {
        let old = "<<<<<<< ws\nx\n||||||| base\n=======\ny\n>>>>>>> default\n";
        let b = map(&[("a", f(&format!("top\n{old}mid\nend\n")))]);
        let u = map(&[("a", f(&format!("TOP\n{old}mid\nend\n")))]);
        let m = map(&[("a", f(&format!("top\n{old}mid\nEND\n")))]);
        let good = map(&[("a", f(&format!("TOP\n{old}mid\nEND\n")))]);
        assert!(judge_replay(&b, &u, &m, &good, "").mismatches.is_empty());
        // The replay kept the user's version verbatim, dropping the merge.
        assert_eq!(judge_replay(&b, &u, &m, &u, "").mismatches.len(), 1);
    }

    /// bn-36chi: git's diff3 calls abutting hunks a conflict where the
    /// replay's gix-merge may merge cleanly. No new markers: every line
    /// either side added must still be there.
    #[test]
    fn reference_conflict_with_clean_replay_keeps_both_sides_lines() {
        let b = map(&[("a", f("1\n2\n"))]);
        let u = map(&[("a", f("1\n2\nU\n"))]);
        let m = map(&[("a", f("1\nM\n"))]);
        assert!(
            reference_merge(b"1\n2\n", b"1\nM\n", b"1\n2\nU\n")
                .unwrap()
                .is_none()
        );
        let clean = map(&[("a", f("1\nM\nU\n"))]);
        assert!(judge_replay(&b, &u, &m, &clean, "").mismatches.is_empty());
        let dropped = map(&[("a", f("1\nM\n"))]);
        let v = judge_replay(&b, &u, &m, &dropped, "");
        assert_eq!(v.mismatches.len(), 1, "{v:?}");
        assert!(v.mismatches[0].detail.contains("dropped a line"), "{v:?}");
    }

    /// bn-36chi: with no regular file at the anchor, the user's exec bit
    /// wins (it was not judged at all before).
    #[test]
    fn exec_bit_judged_when_the_anchor_had_no_file() {
        let b = EntryMap::new();
        let u = map(&[("a", x("same\n"))]);
        let m = map(&[("a", f("same\n"))]);
        assert!(
            judge_replay(&b, &u, &m, &map(&[("a", x("same\n"))]), "")
                .mismatches
                .is_empty()
        );
        let v = judge_replay(&b, &u, &m, &map(&[("a", f("same\n"))]), "");
        assert_eq!(v.mismatches.len(), 1, "{v:?}");
        assert!(v.mismatches[0].detail.contains("executable bit"), "{v:?}");
    }

    /// bn-36chi: a merge that only flipped +x on a file the user turned
    /// into a symlink is not an overlap: the user's link must stay.
    #[test]
    fn user_symlink_over_mode_only_merge_change_is_kept() {
        let b = map(&[("a", f("1"))]);
        let u = map(&[("a", l("t"))]);
        let m = map(&[("a", x("1"))]);
        assert!(judge_replay(&b, &u, &m, &u, "").mismatches.is_empty());
        assert_eq!(judge_replay(&b, &u, &m, &m, "").mismatches.len(), 1);
    }

    /// bn-36chi: file<->directory groups one side touched are judged.
    #[test]
    fn single_sided_file_dir_groups_are_judged() {
        // Only the merge turned file `d` into a directory.
        let b = map(&[("d", f("1")), ("o", f("o"))]);
        let m = map(&[("d/inner.txt", f("2")), ("o", f("o"))]);
        let j = judge_replay(&b, &b, &m, &m, "");
        assert!(j.mismatches.is_empty(), "{j:?}");
        assert_eq!(j.judged, 3);
        // The stale file left behind: a mismatch.
        assert_eq!(judge_replay(&b, &b, &m, &b, "").mismatches.len(), 1);
        // Only the user turned `d` into a directory: kept, or merged + reported.
        let u = map(&[("d/inner.txt", f("u")), ("o", f("o"))]);
        assert!(judge_replay(&b, &u, &b, &u, "").mismatches.is_empty());
        let report = "  WARNING: 1 path(s) in 'default' have a type conflict\n    d\n      inspect yours: maw ws recover --ref R --show d\n";
        assert!(judge_replay(&b, &u, &b, &b, report).mismatches.is_empty());
        assert_eq!(judge_replay(&b, &u, &b, &b, "").mismatches.len(), 1);
    }

    /// bn-3adck: only a SIGABRT death while an `abort` failpoint is armed is
    /// the requested crash. A panic (exit 101) or a clean exit without a
    /// result marker (the helper test missing from the binary) is a harness
    /// error even with the abort armed.
    #[cfg(unix)]
    #[test]
    fn self_exec_crash_is_only_a_requested_sigabrt() {
        let t = tempfile::TempDir::new().unwrap();
        // `/bin/sh <script> <libtest args...>`: the script stands in for the
        // test binary. Run through `sh` rather than exec'd directly — a
        // freshly written executable can hit ETXTBSY while a concurrent test
        // forks with its write fd still open.
        let helper = |name: &str, body: &str| {
            let p = t.path().join(name);
            std::fs::write(&p, format!("{body}\n")).unwrap();
            SelfExecUpdater {
                exe: PathBuf::from("/bin/sh"),
                test_name: p.to_string_lossy().into_owned(),
            }
        };
        let req = |fp: Option<&str>| TrunkUpdateRequest {
            default_ws_path: t.path().to_path_buf(),
            repo_root: t.path().to_path_buf(),
            branch: "main".to_owned(),
            epoch_before: "a".to_owned(),
            epoch_after: "b".to_owned(),
            sources: vec!["ws-1".to_owned()],
            maw_fp: fp.map(str::to_owned),
        };
        let abort = Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort");

        // The requested crash: SIGABRT with the abort armed.
        let sigabrt = helper("abrt.sh", "echo 'some progress'\nkill -ABRT $$");
        let out = sigabrt.update(&req(abort)).expect("a requested crash");
        assert!(out.crashed && out.error.is_none(), "{out:?}");
        // The same death with no abort armed is not a crash.
        assert!(sigabrt.update(&req(None)).is_err());

        // A panic in the production update (libtest exit 101) is never the
        // requested crash.
        let panic = helper(
            "panic.sh",
            "echo \"thread 'x' panicked at boom\" >&2\nexit 101",
        );
        assert!(panic.update(&req(abort)).is_err(), "panic read as crash");
        assert!(panic.update(&req(None)).is_err());

        // libtest found no helper test: exit 0, no marker.
        let missing = helper("missing.sh", "echo 'running 0 tests'\nexit 0");
        assert!(
            missing.update(&req(abort)).is_err(),
            "missing helper read as crash"
        );

        // A reported result is always honoured.
        let ok = helper("ok.sh", "echo '[sg1-trunk-update] result=ok'");
        assert!(ok.update(&req(abort)).unwrap().completed());
        let err = helper("err.sh", "echo '[sg1-trunk-update] result=err boom'");
        let e = err.update(&req(None)).unwrap();
        assert_eq!(e.error.as_deref(), Some("boom"));
    }

    #[test]
    fn apply_edit_kinds_roundtrip() {
        let t = tempfile::TempDir::new().unwrap();
        let d = t.path();
        let e = |path: &str, content: &str, kind| FileEdit {
            path: path.into(),
            content: content.into(),
            kind,
        };
        apply_edit(d, &e("s/a", "x", EditKind::Write)).unwrap();
        apply_edit(d, &e("s/a", "", EditKind::ExecFlip)).unwrap();
        let m = capture_worktree(d).unwrap();
        assert_eq!(m.get("s/a"), Some(&x("x")));
        apply_edit(d, &e("s/a", "b", EditKind::Symlink)).unwrap();
        assert_eq!(capture_worktree(d).unwrap().get("s/a"), Some(&l("b")));
        apply_edit(d, &e("s/a", "in", EditKind::Dir)).unwrap();
        assert_eq!(
            capture_worktree(d).unwrap().get("s/a/inner.txt"),
            Some(&f("in"))
        );
        apply_edit(d, &e("s/a/inner.txt/deeper", "z", EditKind::Write)).unwrap();
        assert_eq!(
            capture_worktree(d).unwrap().get("s/a/inner.txt/deeper"),
            Some(&f("z"))
        );
        apply_edit(d, &e("s/a", "", EditKind::Delete)).unwrap();
        assert!(capture_worktree(d).unwrap().is_empty());
    }
}
