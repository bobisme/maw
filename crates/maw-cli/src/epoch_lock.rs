//! Repo-level single-writer epoch lock (bn-13rc).
//!
//! maw's core use case is many agents mutating one repository concurrently.
//! Epoch mutations — `ws merge` (including FF-absorb and sibling auto-rebase),
//! `ws advance`, `epoch sync`, `gc`, and `ws destroy` — all read-modify-write
//! shared manifold state (`refs/manifold/epoch/current`, per-workspace epoch
//! refs, the branch tip, destroy/recovery records). If two maw processes
//! interleave those phases the result is undefined: a lost epoch write, a
//! sibling rebased onto a tip that no longer exists, a half-pruned record.
//!
//! This module provides ONE advisory lock for the whole repo's epoch state at
//! `<repo>/.manifold/locks/epoch.lock`, so at most one maw process is ever
//! inside an epoch mutation at a time.
//!
//! # Mechanism
//!
//! Identical to the per-workspace locks (`workspace/sync/lock.rs`,
//! `workspace/create_lock.rs`, bn-1d1g/bn-3bbc): an `fs4` advisory `flock` on a
//! create-if-missing lockfile that is *never deleted on drop*. The kernel
//! releases the lock automatically when the process dies, so it is crash-safe —
//! a `kill -9` mid-merge leaves no stuck lock, the next acquirer flocks cleanly.
//! Leaving the file in place keeps the lock identity stable across acquisitions
//! (see the per-workspace modules for the delete-on-drop race this avoids).
//!
//! Unlike the per-workspace lockfiles (whose *content* is unused), the epoch
//! lockfile also stores holder metadata (see [`HolderInfo`]) written under the
//! exclusive lock, so a contending process can report *who* holds it. Stale
//! content left by a crashed holder is harmless: contention is decided purely
//! by the live `flock`, never by the file's content.
//!
//! # Deadlock / ordering rule
//!
//! **The epoch lock is ALWAYS acquired BEFORE any per-workspace lock, never
//! after.** `ws sync --rebase` takes the epoch lock first (in [`crate::workspace::sync`])
//! and only then the per-workspace rebase lock deeper in the pipeline. `ws merge`
//! holds the epoch lock across its own sibling auto-rebase, which takes
//! per-workspace locks — again epoch-first. No code path acquires the epoch lock
//! while already holding a per-workspace lock. The best-effort
//! `auto_sync_if_stale` path takes only a *try*-lock on the per-workspace lock
//! and never the epoch lock, so it cannot deadlock or serialize read commands.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

use crate::workspace::{MawConfig, now_timestamp_iso8601};

/// Exit code returned when the epoch lock is contended and the caller cannot
/// take it (immediate `--no-wait`/config failure, or a wait that timed out).
///
/// Distinct from the generic error code (`1`) and clap's usage code (`2`) so
/// orchestration scripts can recognise "busy, retry later" and back off. `75`
/// is the conventional `EX_TEMPFAIL` from `<sysexits.h>`.
pub const EPOCH_LOCK_BUSY_EXIT_CODE: i32 = 75;

/// How often to re-attempt the lock while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Error signalling that the repo-level epoch lock could not be acquired
/// because another maw process holds it.
///
/// The human-readable contention message (with holder metadata) is printed to
/// stderr by [`EpochLock::acquire`] *before* this error is returned; `main`
/// downcasts it to exit with [`EPOCH_LOCK_BUSY_EXIT_CODE`] without printing a
/// second, generic `Error:` line. This mirrors `exec::ExitCodeError`.
#[derive(Debug)]
pub struct EpochLockBusy;

impl std::fmt::Display for EpochLockBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "epoch lock is held by another maw process")
    }
}

impl std::error::Error for EpochLockBusy {}

/// Metadata about the process holding (or last to have held) the epoch lock.
///
/// Serialised as JSON into the lockfile under the exclusive lock. Deserialised
/// by a contending process (or `maw doctor`) to report who holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolderInfo {
    /// OS process id of the holder.
    pub pid: u32,
    /// Human label for the mutation (e.g. `"ws merge"`).
    pub command: String,
    /// Orchestrator/agent identity from `$AGENT`, if set.
    #[serde(default)]
    pub agent: Option<String>,
    /// ISO-8601 UTC timestamp of when the lock was acquired.
    pub started_at: String,
}

impl HolderInfo {
    /// Build a record describing the current process acquiring `command`.
    fn current(command: &str) -> Self {
        Self {
            pid: std::process::id(),
            command: command.to_owned(),
            agent: std::env::var("AGENT").ok().filter(|s| !s.is_empty()),
            started_at: now_timestamp_iso8601(),
        }
    }

    /// One-line summary for the "waiting for epoch lock held by …" note.
    #[must_use]
    pub fn describe(&self) -> String {
        let agent = self
            .agent
            .as_deref()
            .map_or_else(String::new, |a| format!(", agent {a}"));
        format!(
            "pid {} ({}{}, started {})",
            self.pid, self.command, agent, self.started_at
        )
    }
}

/// Whether (and how long) to wait for a contended lock.
#[derive(Debug, Clone, Copy)]
pub struct WaitPolicy {
    /// If false, fail immediately on contention instead of polling.
    pub wait: bool,
    /// Maximum time to poll before giving up (ignored when `wait` is false).
    pub timeout: Duration,
}

impl WaitPolicy {
    /// Resolve the effective policy from config (`[lock]` in `.maw.toml`) with
    /// environment overrides.
    ///
    /// Precedence (later wins): config `no_wait`/`wait_seconds`, then
    /// `MAW_LOCK_NO_WAIT` (truthy = don't wait, falsy = do wait), then
    /// `MAW_LOCK_WAIT_SECS` (integer seconds). Orchestrators that prefer an
    /// immediate distinct-exit-code failure set `no_wait` (config) or
    /// `MAW_LOCK_NO_WAIT=1` (env).
    #[must_use]
    pub fn resolve(root: &Path) -> Self {
        let config = MawConfig::load(root).unwrap_or_default();
        let mut no_wait = config.lock_no_wait();
        let mut secs = config.lock_wait_seconds();

        if let Ok(raw) = std::env::var("MAW_LOCK_NO_WAIT")
            && let Some(v) = parse_bool(&raw)
        {
            no_wait = v;
        }
        if let Ok(raw) = std::env::var("MAW_LOCK_WAIT_SECS")
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            secs = v;
        }

        Self {
            wait: !no_wait,
            timeout: Duration::from_secs(secs),
        }
    }
}

/// Parse a boolean-ish env string. Returns `None` for unrecognised values so
/// the caller keeps its default.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// RAII handle for the repo-level epoch lock.
///
/// Held for the duration of the critical section. Dropping it (on success,
/// error, or panic) closes the file descriptor, which releases the advisory
/// `flock` held by the kernel.
pub struct EpochLock {
    /// The locked file. Kept alive for the full critical section — when it
    /// drops, the kernel releases the advisory lock.
    _file: File,
    /// Path retained for diagnostics / tests.
    #[allow(dead_code)]
    path: PathBuf,
}

impl EpochLock {
    /// Acquire the epoch lock for the repo at `root`, using the policy resolved
    /// from config and environment.
    ///
    /// `command` is a short label recorded as holder metadata (e.g.
    /// `"ws merge"`). On contention this waits briefly (default ~10s, printing a
    /// one-line progress note) then, if still held, prints a self-contained
    /// who-holds-it message and returns [`EpochLockBusy`].
    ///
    /// # Errors
    ///
    /// Returns [`EpochLockBusy`] if the lock is held and could not be acquired
    /// within the wait window (or immediately, under `no_wait`). Returns other
    /// errors if the lock directory or file cannot be created/opened.
    pub fn acquire(root: &Path, command: &str) -> Result<Self> {
        Self::acquire_with(root, command, WaitPolicy::resolve(root))
    }

    /// Acquire with an explicit [`WaitPolicy`] (used by tests).
    ///
    /// # Errors
    ///
    /// See [`EpochLock::acquire`].
    pub fn acquire_with(root: &Path, command: &str, policy: WaitPolicy) -> Result<Self> {
        let path = lock_path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create epoch lock dir {}", parent.display()))?;
        }

        // create-if-missing, never truncate — a stale (crashed-holder) file is
        // reused as-is; its content is overwritten only once WE hold the lock.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open epoch lockfile {}", path.display()))?;

        // Fast, uncontended path: a single non-blocking attempt.
        if try_lock(&file)? {
            write_holder(&file, command)?;
            return Ok(Self { _file: file, path });
        }

        if !policy.wait {
            print_contention(&path, ContentionReason::NoWait);
            return Err(EpochLockBusy.into());
        }

        // Contended and willing to wait: emit a one-time progress note naming
        // the current holder, then poll until the deadline.
        let holder_note = read_holder_at(&path)
            .map_or_else(|| "another maw process".to_owned(), |h| h.describe());
        eprintln!("waiting for epoch lock held by {holder_note} …");

        let deadline = Instant::now() + policy.timeout;
        loop {
            std::thread::sleep(POLL_INTERVAL);
            if try_lock(&file)? {
                write_holder(&file, command)?;
                return Ok(Self { _file: file, path });
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        print_contention(
            &path,
            ContentionReason::Timeout {
                secs: policy.timeout.as_secs(),
            },
        );
        Err(EpochLockBusy.into())
    }
}

/// Non-blocking exclusive lock attempt. `Ok(true)` = acquired, `Ok(false)` =
/// currently held by someone else, `Err` = unexpected I/O failure.
fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

/// Overwrite the lockfile with the current holder's metadata. Safe because we
/// only call this while holding the exclusive lock.
fn write_holder(file: &File, command: &str) -> io::Result<()> {
    let info = HolderInfo::current(command);
    let json = serde_json::to_vec_pretty(&info).unwrap_or_default();
    file.set_len(0)?;
    let mut handle: &File = file;
    handle.seek(SeekFrom::Start(0))?;
    handle.write_all(&json)?;
    handle.flush()?;
    Ok(())
}

/// Read holder metadata from the lockfile at `root`, if any recorded.
#[must_use]
pub fn read_holder(root: &Path) -> Option<HolderInfo> {
    read_holder_at(&lock_path(root))
}

fn read_holder_at(path: &Path) -> Option<HolderInfo> {
    let mut contents = String::new();
    File::open(path).ok()?.read_to_string(&mut contents).ok()?;
    if contents.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&contents).ok()
}

/// Current lock state, for `maw doctor`.
#[derive(Debug)]
pub struct LockStatus {
    /// True if the lockfile is currently `flock`ed by a live process.
    pub held: bool,
    /// Holder metadata recorded in the file (may be stale if `held` is false).
    pub holder: Option<HolderInfo>,
}

/// Inspect the epoch lock without taking it for the duration of a command.
///
/// `held` is decided by a momentary non-blocking `flock` attempt — the ground
/// truth — not by file content. `holder` is whatever metadata the file records,
/// which is stale (a previous crashed holder) when `held` is false.
#[must_use]
pub fn inspect(root: &Path) -> LockStatus {
    let path = lock_path(root);
    let holder = read_holder_at(&path);
    let held = is_locked(&path);
    LockStatus { held, holder }
}

/// True if some other process currently holds the advisory lock on `path`.
fn is_locked(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        // No lockfile (or unreadable) => nobody holds it.
        return false;
    };
    // `WouldBlock` => someone else holds it. Any other outcome (we acquired it,
    // or an unexpected error) means "not held by another process"; a successful
    // momentary acquire is released when `file` drops at end of scope.
    matches!(
        file.try_lock_exclusive(),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock
    )
}

/// Why a lock acquisition failed, for the contention message.
#[derive(Clone, Copy)]
enum ContentionReason {
    /// `--no-wait`/config: fail immediately.
    NoWait,
    /// The wait window elapsed with the lock still held.
    Timeout { secs: u64 },
}

/// Print the self-contained who-holds-it message to stderr.
fn print_contention(path: &Path, reason: ContentionReason) {
    eprintln!("Error: epoch lock is held by another maw process.");
    if let Some(h) = read_holder_at(path) {
        eprintln!("  pid:     {}", h.pid);
        eprintln!("  command: {}", h.command);
        if let Some(agent) = &h.agent {
            eprintln!("  agent:   {agent}");
        }
        eprintln!("  started: {}", h.started_at);
    } else {
        eprintln!("  (no holder metadata recorded)");
    }
    match reason {
        ContentionReason::NoWait => {
            eprintln!("  (--no-wait: failing immediately)");
        }
        ContentionReason::Timeout { secs } => {
            eprintln!("  (waited {secs}s, still held)");
        }
    }
    eprintln!();
    eprintln!("  To fix: wait for that process to finish, or — if it is dead — the lock");
    eprintln!("  has already auto-released; just re-run your command.");
}

/// Path of the single repo-level epoch lockfile.
fn lock_path(root: &Path) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("locks")
        .join("epoch.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn no_wait() -> WaitPolicy {
        WaitPolicy {
            wait: false,
            timeout: Duration::from_secs(0),
        }
    }

    // bn-1d22: under parallel `cargo test`, a *sibling* test's `Command::spawn`
    // dup()s every open fd — including this test's lockfile fd — across fork().
    // BSD `flock` locks live on the open file description shared by those dups,
    // so a lock this test just released can still read as *held* until the child
    // execs (the fd is `O_CLOEXEC`) a fraction of a millisecond later. The
    // perturbation is one-directional: a stray fork can only make a *free* lock
    // momentarily look held, never make a *held* lock look free. So the
    // "must fail while genuinely held" assertions stay immediate and exact, while
    // every "should now be free / should re-acquire" observation is allowed to
    // settle. `RESETTLE` bounds that settling window (a released lock reappears
    // free within one fork→exec, i.e. microseconds; the budget is generous only
    // so a starved scheduler on a loaded box cannot flake it).
    const RESETTLE: Duration = Duration::from_secs(5);

    /// A [`WaitPolicy`] that briefly waits, used for a *post-release* re-acquire
    /// that a sibling test's transient fork can momentarily block (bn-1d22).
    const fn resettle() -> WaitPolicy {
        WaitPolicy {
            wait: true,
            timeout: RESETTLE,
        }
    }

    /// Poll [`inspect`] until the lock reads as not held, or [`RESETTLE`] elapses.
    /// The returned status is asserted on by the caller (so a genuinely-stuck
    /// lock still fails the test, just after the settling budget).
    fn wait_until_not_held(root: &Path) -> LockStatus {
        let deadline = Instant::now() + RESETTLE;
        loop {
            let status = inspect(root);
            if !status.held || Instant::now() >= deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn acquire_succeeds_on_fresh_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard =
            EpochLock::acquire_with(tmp.path(), "ws merge", no_wait()).expect("fresh repo acquire");
        drop(guard);
    }

    #[test]
    fn second_acquire_is_blocked_while_held() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first =
            EpochLock::acquire_with(tmp.path(), "ws merge", no_wait()).expect("first acquire");
        let second = EpochLock::acquire_with(tmp.path(), "ws advance", no_wait());
        assert!(
            second.is_err(),
            "second concurrent epoch acquire must fail under no-wait"
        );
        let err = second.err().expect("err");
        assert!(
            err.downcast_ref::<EpochLockBusy>().is_some(),
            "contention must surface as EpochLockBusy"
        );
        drop(first);
        // After release a fresh acquire succeeds. A sibling test's fork() can
        // transiently keep our just-closed fd's description open (bn-1d22), so
        // allow the re-acquire to settle rather than demanding it no-wait.
        let third =
            EpochLock::acquire_with(tmp.path(), "gc", resettle()).expect("post-release acquire");
        drop(third);
    }

    #[test]
    fn holder_metadata_is_recorded_and_readable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = EpochLock::acquire_with(tmp.path(), "ws merge", no_wait()).expect("acquire");
        let holder = read_holder(tmp.path()).expect("holder metadata written");
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(holder.command, "ws merge");
        assert!(
            !holder.started_at.is_empty(),
            "started_at must be populated"
        );
        drop(guard);
    }

    #[test]
    fn inspect_reports_held_and_holder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let before = inspect(tmp.path());
        assert!(!before.held, "no lockfile yet => not held");

        let guard = EpochLock::acquire_with(tmp.path(), "epoch sync", no_wait()).expect("acquire");
        let during = inspect(tmp.path());
        assert!(during.held, "inspect must observe the live flock");
        let holder = during.holder.expect("holder recorded");
        assert_eq!(holder.command, "epoch sync");
        drop(guard);

        // After release the flock is gone but the (now stale) metadata remains.
        // Poll: a sibling test's fork() can briefly keep the released fd's
        // description open (bn-1d22); a held lock can never spuriously read free.
        let after = wait_until_not_held(tmp.path());
        assert!(!after.held, "released lock must read as not held");
        assert!(
            after.holder.is_some(),
            "stale content persists but is not treated as locked"
        );
    }

    #[test]
    fn stale_content_without_flock_is_not_locked() {
        // A lockfile full of holder JSON but with no live flock (previous crash)
        // must never be treated as held.
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let guard =
                EpochLock::acquire_with(tmp.path(), "ws destroy", no_wait()).expect("acquire");
            drop(guard);
        }
        assert!(
            read_holder(tmp.path()).is_some(),
            "content left behind after release"
        );
        // A sibling fork can briefly hold the released fd (bn-1d22); let it settle.
        let after = wait_until_not_held(tmp.path());
        assert!(!after.held, "stale content is not a lock");
        // And a fresh acquire still succeeds (allowed to settle for the same reason).
        let guard =
            EpochLock::acquire_with(tmp.path(), "gc", resettle()).expect("acquire over stale");
        drop(guard);
    }

    #[test]
    fn parse_bool_recognises_common_forms() {
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool(" on "), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn wait_then_succeed_after_release() {
        // Holder releases after ~150ms; a waiter with a 5s window must block
        // briefly then succeed.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let held = EpochLock::acquire_with(&root, "ws merge", no_wait()).expect("hold");

        let releaser = {
            let held = held;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                drop(held);
            })
        };

        let policy = WaitPolicy {
            wait: true,
            timeout: Duration::from_secs(5),
        };
        let start = Instant::now();
        let guard = EpochLock::acquire_with(&root, "ws advance", policy)
            .expect("waiter should acquire after release");
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "waiter must have blocked until the holder released"
        );
        drop(guard);
        releaser.join().expect("releaser join");
    }
}
