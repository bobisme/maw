//! Post-sync hook execution and result persistence (bn-1lhb).
//!
//! A `post_sync` hook is an optional per-workspace command (configured in
//! `.maw.toml`'s `[hooks]` table) that runs inside a workspace after ANY
//! successful sync/auto-rebase replay:
//!
//! * a direct `maw ws sync --rebase` replay (or FF-sync advance),
//! * a merge-triggered sibling auto-rebase, and
//! * an FF-absorb sibling replay from `reconcile_epoch_with_branch`.
//!
//! Motivation (mess field report 2, bn-1m4d): four times in one session a
//! sibling auto-rebase replayed CLEANLY textually but produced a workspace
//! that does not compile (the merged sibling reworked an API). All four were
//! plain compile errors a `cargo check` at rebase time would have caught.
//!
//! jj model: a hook failure is a SIGNAL only. It never blocks or rolls back
//! the sync/merge and never changes its exit code — the rebase itself
//! succeeded. The pass/fail is persisted per workspace under
//! `.maw/manifold/artifacts/ws/<name>/postsync.json` and surfaced by the
//! read-only discovery commands (`ws list`, `ws status`) plus the triggering
//! merge summary. Read commands NEVER re-run the hook; they only read the
//! last persisted record.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::MawConfig;

/// How many trailing lines of hook output are retained in the persisted
/// record (spec: "tail-of-output (last ~20 lines)").
const OUTPUT_TAIL_LINES: usize = 20;

/// Persisted last-known result of a workspace's post-sync hook run. Read by
/// `ws list` / `ws status`; written by [`run_post_sync_hooks`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PostSyncHookRecord {
    /// The command whose outcome this record captures — the first failing
    /// command, or the last command when all passed.
    pub command: String,
    /// Process exit code (`-1` when the process was killed or produced no
    /// code, e.g. on timeout).
    pub exit_code: i32,
    /// True when the hook was killed for exceeding `hook_timeout_seconds`.
    pub timed_out: bool,
    /// ISO-8601 UTC timestamp of the run.
    pub timestamp: String,
    /// Epoch the workspace was replayed onto when the hook ran.
    pub epoch: String,
    /// Last ~20 lines of combined stdout+stderr.
    pub output_tail: Vec<String>,
    /// maw version that produced this record.
    pub tool_version: String,
}

impl PostSyncHookRecord {
    /// A recorded hook is "failed" when it exited non-zero or timed out.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.timed_out || self.exit_code != 0
    }
}

/// Compact per-operation summary attached to `sync`/`merge` JSON output
/// (bn-1lhb; field shape kept additive for the bn-20fp merge-JSON bone).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PostSyncHookSummary {
    /// Always `true` — this summary is only produced when hooks actually ran.
    pub ran: bool,
    /// Exit code of the determining command (`-1` on timeout / no code).
    pub exit_code: Option<i32>,
    /// Whether the hook timed out.
    pub timed_out: bool,
}

impl PostSyncHookSummary {
    /// Whether this run should be surfaced as a failure.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.timed_out || self.exit_code.is_some_and(|c| c != 0)
    }
}

/// Marker read from the persisted record for `ws list` / `ws status`.
#[derive(Clone, Debug, Serialize)]
pub struct PostSyncHookInfo {
    /// True when the last recorded run failed (non-zero exit or timeout).
    pub failed: bool,
    /// Exit code of the last recorded run.
    pub exit_code: i32,
    /// Whether the last recorded run timed out.
    pub timed_out: bool,
}

/// Path to a workspace's persisted post-sync hook result.
#[must_use]
pub fn record_path(root: &Path, ws_name: &str) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("postsync.json")
}

/// Run the configured `post_sync` hooks inside `ws_path`, persist the result,
/// and return a summary.
///
/// Returns `None` when no `post_sync` hooks are configured (zero behavior
/// change — nothing is run and nothing is written). Returns `Some(summary)`
/// when hooks ran, regardless of pass/fail.
///
/// Callers MUST only invoke this after a replay actually happened; an
/// up-to-date sync must not reach here (spec item 2).
pub fn run_post_sync_hooks(
    root: &Path,
    ws_name: &str,
    ws_path: &Path,
    epoch: &str,
) -> Option<PostSyncHookSummary> {
    let config = MawConfig::load(root).ok()?;
    let commands = config.post_sync_hooks();
    if commands.is_empty() {
        return None;
    }
    // Guard against a zero timeout wedging on a fast poll loop.
    let timeout = Duration::from_secs(config.hook_timeout_seconds().max(1));

    let mut command = String::new();
    let mut output = String::new();
    let mut exit_code = 0;
    let mut timed_out = false;
    for cmd in commands {
        let run = run_one_with_timeout(cmd, ws_path, timeout);
        command.clone_from(cmd);
        output = run.output;
        exit_code = run.exit_code;
        timed_out = run.timed_out;
        // First failure determines the recorded outcome; a failed `cargo
        // check` makes any subsequent command meaningless.
        if run.timed_out || run.exit_code != 0 {
            break;
        }
    }

    let record = PostSyncHookRecord {
        command,
        exit_code,
        timed_out,
        timestamp: super::now_timestamp_iso8601_precise(),
        epoch: epoch.to_string(),
        output_tail: tail_lines(&output, OUTPUT_TAIL_LINES),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if let Err(e) = write_record(root, ws_name, &record) {
        tracing::warn!(
            workspace = %ws_name,
            error = %e,
            "failed to persist post-sync hook result"
        );
    }

    Some(PostSyncHookSummary {
        ran: true,
        exit_code: Some(exit_code),
        timed_out,
    })
}

/// Read a workspace's last persisted post-sync hook record, if any. Never
/// re-runs the hook.
#[must_use]
pub fn read_latest(root: &Path, ws_name: &str) -> Option<PostSyncHookRecord> {
    let content = std::fs::read_to_string(record_path(root, ws_name)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Read the compact marker for `ws list` / `ws status`. `None` when no hook
/// has ever run for this workspace.
#[must_use]
pub fn latest_info(root: &Path, ws_name: &str) -> Option<PostSyncHookInfo> {
    let record = read_latest(root, ws_name)?;
    Some(PostSyncHookInfo {
        failed: record.failed(),
        exit_code: record.exit_code,
        timed_out: record.timed_out,
    })
}

/// Outcome of one hook command execution.
struct HookRun {
    exit_code: i32,
    timed_out: bool,
    output: String,
}

/// Run one `sh -c <cmd>` in `cwd`, capturing combined stdout+stderr, killing
/// it (and flagging `timed_out`) if it exceeds `timeout`.
fn run_one_with_timeout(cmd: &str, cwd: &Path, timeout: Duration) -> HookRun {
    let mut command = Command::new("sh");
    command
        .args(["-c", cmd])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A hook can spawn descendants which inherit stdout/stderr. Give the hook
    // its own process group so a timeout can close the entire command tree;
    // otherwise the reader threads below block until surviving children exit.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return HookRun {
                exit_code: -1,
                timed_out: false,
                output: format!("failed to spawn post-sync hook `{cmd}`: {e}"),
            };
        }
    };

    // Drain the pipes on dedicated threads so a chatty hook can't deadlock by
    // filling the OS pipe buffer while we poll for completion.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_handle = std::thread::spawn(move || read_all_lossy(stdout));
    let err_handle = std::thread::spawn(move || read_all_lossy(stderr));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_hook_tree(&mut child);
                    let _ = child.wait();
                    timed_out = true;
                    break -1;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break -1,
        }
    };

    let mut output = out_handle.join().unwrap_or_default();
    let stderr_text = err_handle.join().unwrap_or_default();
    if !stderr_text.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&stderr_text);
    }

    HookRun {
        exit_code,
        timed_out,
        output,
    }
}

fn kill_hook_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The child is the process-group leader because `process_group(0)` was
        // set before spawn. A negative PID targets every process in the group.
        let group = format!("-{}", child.id());
        let killed = Command::new("kill")
            .args(["-KILL", "--", &group])
            .status()
            .is_ok_and(|status| status.success());
        if killed {
            return;
        }
    }
    let _ = child.kill();
}

fn read_all_lossy(reader: Option<impl Read>) -> String {
    let mut buf = Vec::new();
    if let Some(mut reader) = reader {
        let _ = reader.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn tail_lines(text: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].iter().map(|l| (*l).to_string()).collect()
}

fn write_record(root: &Path, ws_name: &str, record: &PostSyncHookRecord) -> Result<()> {
    let path = record_path(root, ws_name);
    let dir = path
        .parent()
        .with_context(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create dir {}", dir.display()))?;

    let tmp_path = dir.join(".postsync.json.tmp");
    let json = serde_json::to_string_pretty(record).context("serialize post-sync hook record")?;
    {
        let mut file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("create temp file {}", tmp_path.display()))?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("write temp file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync temp file {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lines_keeps_last_n() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(tail_lines(text, 3), vec!["c", "d", "e"]);
        assert_eq!(tail_lines(text, 10), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(tail_lines("", 5), Vec::<String>::new());
    }

    #[test]
    fn run_one_success_captures_output() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let run = run_one_with_timeout("echo hello", dir.path(), Duration::from_secs(10));
        assert_eq!(run.exit_code, 0);
        assert!(!run.timed_out);
        assert!(run.output.contains("hello"), "output: {}", run.output);
    }

    #[test]
    fn run_one_failure_reports_exit_code() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let run = run_one_with_timeout("exit 101", dir.path(), Duration::from_secs(10));
        assert_eq!(run.exit_code, 101);
        assert!(!run.timed_out);
    }

    #[test]
    fn run_one_captures_stderr() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let run = run_one_with_timeout(
            "echo oops 1>&2; exit 1",
            dir.path(),
            Duration::from_secs(10),
        );
        assert_eq!(run.exit_code, 1);
        assert!(run.output.contains("oops"), "output: {}", run.output);
    }

    #[test]
    fn run_one_timeout_is_flagged() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let run = run_one_with_timeout("sleep 5", dir.path(), Duration::from_secs(1));
        assert!(run.timed_out, "expected timeout flag");
        assert_eq!(run.exit_code, -1);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendants_that_keep_output_pipes_open() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let started = Instant::now();
        let run = run_one_with_timeout("sleep 3 & wait", dir.path(), Duration::from_millis(100));
        assert!(run.timed_out, "expected timeout flag");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout waited for a surviving descendant: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn record_roundtrips_and_marks_failure() {
        let record = PostSyncHookRecord {
            command: "cargo check".to_string(),
            exit_code: 101,
            timed_out: false,
            timestamp: "2026-07-10T00:00:00.000Z".to_string(),
            epoch: "a".repeat(40),
            output_tail: vec!["error[E0063]".to_string()],
            tool_version: "test".to_string(),
        };
        assert!(record.failed());
        let json = serde_json::to_string(&record).expect("serialize");
        let back: PostSyncHookRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.exit_code, 101);
        assert!(back.failed());
    }

    #[test]
    fn write_then_read_latest() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let root = dir.path();
        let record = PostSyncHookRecord {
            command: "cargo check".to_string(),
            exit_code: 0,
            timed_out: false,
            timestamp: "2026-07-10T00:00:00.000Z".to_string(),
            epoch: "b".repeat(40),
            output_tail: vec![],
            tool_version: "test".to_string(),
        };
        write_record(root, "alice", &record).expect("write");
        let back = read_latest(root, "alice").expect("read");
        assert_eq!(back.command, "cargo check");
        assert!(!back.failed());

        let info = latest_info(root, "alice").expect("info");
        assert!(!info.failed);
        assert_eq!(info.exit_code, 0);

        // No record for an unknown workspace.
        assert!(read_latest(root, "ghost").is_none());
        assert!(latest_info(root, "ghost").is_none());
    }
}
