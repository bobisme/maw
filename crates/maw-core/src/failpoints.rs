//! Feature-gated failpoint injection for DST.
//!
//! Compile with `--features failpoints` to enable injection.
//! Without the feature, the `fp!()` macro expands to nothing.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// A deterministic interleaving hook (bn-2byw): a closure run when hit.
///
/// Unlike the env-expressible actions, a callback can only be armed
/// **in-process** (via [`set_callback`]) — it is the primitive that lets a
/// single-threaded test drive a concurrent-mutation interleaving (e.g. move
/// HEAD between a rebase walk and the `set_head` CAS re-read) deterministically,
/// with no real threads. See `notes/sg1-race-feasibility-spike-bn-3ny7.md`
/// (outcome C).
pub type FailpointCallback = Arc<dyn Fn() + Send + Sync>;

/// Actions a failpoint can take when triggered.
#[derive(Clone)]
pub enum FailpointAction {
    /// No-op (default).
    Off,
    /// Return an error with the given message.
    Error(String),
    /// Panic with the given message.
    Panic(String),
    /// Abort the process.
    Abort,
    /// Sleep for the given duration.
    Sleep(Duration),
    /// Run a registered callback, then continue (returns `Ok`). In-process
    /// only — not expressible via `MAW_FP`. The deterministic interleaving
    /// primitive (bn-2byw); arm it with [`set_callback`].
    Callback(FailpointCallback),
    /// Overwrite the file at the given path with [`CORRUPT_BYTES`], then
    /// continue (returns `Ok`).
    ///
    /// The **partial-materialization** fault primitive (bn-3gba). Unlike the
    /// crash-shaped actions, this one leaves the process running with a
    /// *silently wrong worktree* — the bn-p3m9 signature (HEAD and index
    /// correct, one file carrying foreign bytes). It is env-expressible
    /// (`MAW_FP=FP_CREATE_AFTER_MATERIALIZE=corrupt:/abs/path`) so the faithful
    /// subprocess tier can inject it into the real `maw` binary; the caller
    /// knows the workspace path before the op runs, so an absolute path is
    /// always available.
    ///
    /// A write failure is ignored: an injector must never crash the process it
    /// is only supposed to perturb.
    Corrupt(std::path::PathBuf),
    /// Write the given marker file (containing this process's pid), then
    /// BLOCK at the site until killed.
    ///
    /// The **blocking kill boundary** (bn-1jfui). A harness that wants to
    /// SIGKILL the process exactly AT a failpoint arms `hang:<abs-marker>`,
    /// waits for the marker to appear, then kills the process group. An
    /// `error` bridge cannot do that: at the commit-phase sites the process
    /// exits and clears its journal within milliseconds, so a poller misses
    /// the window, and at `FP_PREPARE_BEFORE_STATE_WRITE` there is no journal
    /// to observe at all.
    ///
    /// Bounded: after [`HANG_LIMIT`] the process aborts. It never continues
    /// past the site, so a harness that fails to deliver the kill still sees a
    /// crash, never a silently completed operation.
    Hang(std::path::PathBuf),
}

/// How long a [`FailpointAction::Hang`] site blocks before it aborts.
pub const HANG_LIMIT: Duration = Duration::from_secs(120);

/// Bytes written by [`FailpointAction::Corrupt`].
///
/// Deliberately recognisable so a test (or a confused human) can tell an
/// injected corruption from real content at a glance.
pub const CORRUPT_BYTES: &[u8] = b"FP_CORRUPT: injected stale bytes (bn-3gba)\n";

// Manual `Debug` (the `Callback` closure is not `Debug`).
impl std::fmt::Debug for FailpointAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "Off"),
            Self::Error(m) => f.debug_tuple("Error").field(m).finish(),
            Self::Panic(m) => f.debug_tuple("Panic").field(m).finish(),
            Self::Abort => write!(f, "Abort"),
            Self::Sleep(d) => f.debug_tuple("Sleep").field(d).finish(),
            Self::Callback(_) => write!(f, "Callback(<fn>)"),
            Self::Corrupt(p) => f.debug_tuple("Corrupt").field(p).finish(),
            Self::Hang(p) => f.debug_tuple("Hang").field(p).finish(),
        }
    }
}

/// Thread-safe global registry of active failpoints.
static REGISTRY: LazyLock<Mutex<HashMap<&'static str, FailpointAction>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Set a failpoint action.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned.
pub fn set(name: &'static str, action: FailpointAction) {
    REGISTRY
        .lock()
        .expect("operation should succeed")
        .insert(name, action);
}

/// Arm a [`FailpointAction::Callback`] from any closure.
///
/// Convenience over `set(name, FailpointAction::Callback(Arc::new(f)))`. The
/// callback runs (in-process) every time `check(name)` is hit until cleared.
/// This is the deterministic interleaving primitive (bn-2byw): a test arms a
/// closure that mutates state (e.g. moves HEAD) at a precise point inside
/// production code, with no real threads — so the resulting "race" is fully
/// replayable. See `notes/sg1-race-feasibility-spike-bn-3ny7.md`.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned.
pub fn set_callback<F: Fn() + Send + Sync + 'static>(name: &'static str, f: F) {
    set(name, FailpointAction::Callback(Arc::new(f)));
}

/// Clear a specific failpoint.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned.
pub fn clear(name: &'static str) {
    REGISTRY
        .lock()
        .expect("operation should succeed")
        .remove(name);
}

/// Clear all failpoints.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned.
pub fn clear_all() {
    REGISTRY.lock().expect("operation should succeed").clear();
}

/// Check if a failpoint is set and execute its action.
/// Returns `Ok(())` if no failpoint or `Off`, `Err` if `Error` action.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned, or if the
/// failpoint action is `Panic`.
///
/// # Errors
///
/// Returns the configured error message if the failpoint action is `Error`.
pub fn check(name: &str) -> Result<(), String> {
    // A failpoint armed for THIS thread (see `set_for_this_thread`) takes
    // precedence over the process-global registry. The action is cloned out
    // so no registry borrow/lock is held while it runs (a callback may
    // re-enter `check`).
    let action = THREAD_REGISTRY
        .with(|r| r.borrow().get(name).cloned())
        .or_else(|| {
            REGISTRY
                .lock()
                .expect("operation should succeed")
                .get(name)
                .cloned()
        });
    match action {
        None | Some(FailpointAction::Off) => Ok(()),
        Some(FailpointAction::Error(msg)) => Err(msg),
        Some(FailpointAction::Panic(msg)) => panic!("failpoint {name}: {msg}"),
        Some(FailpointAction::Abort) => std::process::abort(),
        Some(FailpointAction::Sleep(d)) => {
            std::thread::sleep(d);
            Ok(())
        }
        Some(FailpointAction::Callback(cb)) => {
            cb();
            Ok(())
        }
        Some(FailpointAction::Corrupt(path)) => {
            // Best-effort: a failed injection must not crash the process.
            let _ = std::fs::write(&path, CORRUPT_BYTES);
            Ok(())
        }
        Some(FailpointAction::Hang(marker)) => {
            // Write-then-rename so a watcher never reads a half-written file.
            let tmp = marker.with_extension("maw-fp-tmp");
            if std::fs::write(&tmp, std::process::id().to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &marker);
            }
            let start = std::time::Instant::now();
            while start.elapsed() < HANG_LIMIT {
                std::thread::sleep(Duration::from_millis(50));
            }
            std::process::abort()
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-scoped failpoints (bn-1svi)
// ---------------------------------------------------------------------------
//
// `set()` arms a failpoint for the whole process. In-process unit tests run
// in parallel threads of one process, so a test that arms a global failpoint
// perturbs every sibling test that reaches the same site while it is armed
// (bn-1svi: `sync_fp_auto_sync_before_checkout_aborts_cleanly` made
// unrelated sync tests fail under `just sg1-faithful-test`). Tests should
// use `set_for_this_thread` instead: the failpoint fires only on the
// arming thread, and the returned guard disarms it on drop (even when the
// test panics).

std::thread_local! {
    static THREAD_REGISTRY: std::cell::RefCell<HashMap<&'static str, FailpointAction>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Arm a failpoint that fires only when `check(name)` runs on the calling
/// thread. Disarmed when the returned guard is dropped.
///
/// Use this (not [`set`]) in in-process tests, which share the process-global
/// registry with concurrently running sibling tests. The site must execute
/// on the calling thread; a site reached on another thread does not fire.
#[must_use = "the failpoint is disarmed when the guard is dropped"]
pub fn set_for_this_thread(name: &'static str, action: FailpointAction) -> ThreadFailpointGuard {
    THREAD_REGISTRY.with(|r| r.borrow_mut().insert(name, action));
    ThreadFailpointGuard {
        name,
        _not_send: std::marker::PhantomData,
    }
}

/// Guard returned by [`set_for_this_thread`]; disarms the failpoint on drop.
/// Not `Send`: it must be dropped on the thread that armed it.
#[derive(Debug)]
pub struct ThreadFailpointGuard {
    name: &'static str,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for ThreadFailpointGuard {
    fn drop(&mut self) {
        // `try_with`: the thread-local may already be torn down at thread exit.
        let _ = THREAD_REGISTRY.try_with(|r| r.borrow_mut().remove(self.name));
    }
}

// The fp! macro is defined in the main crate (maw-workspaces) to keep
// $crate resolution correct. maw-core exports the check() function and
// types that the macro delegates to.

// ---------------------------------------------------------------------------
// MAW_FP env bridge (bn-263u / SP1 bn-imw8)
// ---------------------------------------------------------------------------
//
// The in-process DST driver injects faults directly via `set()` and needs no
// env bridge. The *faithful* subprocess tier spawns the real `maw` binary and
// can only crash it deterministically if the shipped binary honours an env
// var (SP1 Finding A: a `sleep`-widened validation window can only crash the
// one widened phase; `MAW_FP` removes that race at *every* boundary).
//
// Grammar (one spec per process, set once at startup):
//
//   MAW_FP="NAME=action;NAME=action;..."
//
//   NAME    a literal failpoint name (`FP_COMMIT_BETWEEN_CAS_OPS`) or a
//           trailing-`*` prefix glob (`FP_CLEANUP_*`) expanded against the
//           canonical `KNOWN_FAILPOINTS` table at load time so `check()`
//           stays an exact-match O(1) lookup with zero added overhead.
//   action  off | error[:msg] | panic[:msg] | abort | sleep:<ms>
//           | corrupt:<absolute-path> | hang:<absolute-marker-path>
//
// Whitespace around names/actions/`;`/`=` is trimmed. Empty segments and
// segments without `=` are ignored (forgiving: a stray `;` never aborts the
// process). An unknown bare name (no `*`) is kept verbatim so explicit typos
// are still injectable for negative tests; an unknown glob simply matches
// nothing.
//
// This whole block is gated behind `#[cfg(feature = "failpoints")]`: the
// default release build links none of it (`MAW_FP` is inert, `parse_env_spec`
// does not exist), preserving the zero-overhead contract.

/// Canonical list of every real `FP_*` site compiled into maw.
///
/// Used only to expand trailing-`*` globs in a `MAW_FP` spec at load time.
/// Keep in sync with the `fp!()` / `fp_commit()` call sites under
/// `src/merge/*`, `crates/maw-cli/src/**` (destroy/recover/capture). Test-only
/// fixture names (`FP_TEST_*`, `FP_A`, …) are intentionally excluded.
#[cfg(feature = "failpoints")]
pub const KNOWN_FAILPOINTS: &[&str] = &[
    "FP_AUTO_REBASE_BEFORE_REPLAY",
    "FP_AUTO_REBASE_BEFORE_VERIFY",
    "FP_AUTO_SYNC_BEFORE_CHECKOUT",
    "FP_BUILD_AFTER_MERGE_COMPUTE",
    "FP_BUILD_AFTER_WORKTREE_ADD",
    "FP_BUILD_BEFORE_MERGE_COMPUTE",
    "FP_BUILD_BEFORE_WORKTREE_ADD",
    "FP_CAPTURE_BEFORE_PIN",
    "FP_CLEANUP_AFTER_CAPTURE",
    "FP_CLEANUP_AFTER_DEFAULT_CHECKOUT",
    "FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT",
    "FP_COMMIT_AFTER_EPOCH_CAS",
    "FP_COMMIT_BEFORE_BRANCH_CAS",
    "FP_COMMIT_BETWEEN_CAS_OPS",
    "FP_CREATE_AFTER_MATERIALIZE",
    "FP_DESTROY_AFTER_DELETE",
    "FP_DESTROY_AFTER_RECORD",
    "FP_DESTROY_AFTER_STATUS",
    "FP_DESTROY_BEFORE_CAPTURE",
    "FP_DESTROY_BEFORE_DELETE",
    "FP_FF_ABSORB_BEFORE_SIBLING_EPOCH_REF",
    "FP_FF_ABSORB_BEFORE_SIBLING_LOCK",
    "FP_FF_ABSORB_BEFORE_SIBLING_MATERIALIZE",
    "FP_FF_ABSORB_BEFORE_SIBLING_SETHEAD",
    "FP_MIGRATE_PHASE_C_AFTER_MOVE",
    "FP_MIGRATE_PHASE_D_AFTER_FLIP",
    "FP_MIGRATE_PHASE_D_AFTER_UNBARE",
    "FP_PREPARE_AFTER_STATE_WRITE",
    "FP_PREPARE_BEFORE_STATE_WRITE",
    "FP_REBASE_BEFORE_SETHEAD",
    "FP_RECOVER_BEFORE_RESTORE",
    "FP_RECOVER_BEFORE_SEARCH",
    "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT",
    "FP_VALIDATE_AFTER_CHECK",
    "FP_VALIDATE_BEFORE_CHECK",
];

/// Parse a single `action` token into a [`FailpointAction`].
///
/// Returns `None` for an unrecognised action so the caller can skip the whole
/// segment instead of mis-injecting. `error`/`panic` accept an optional
/// `:message`; `sleep` requires `:<milliseconds>`.
#[cfg(feature = "failpoints")]
fn parse_action(token: &str) -> Option<FailpointAction> {
    let token = token.trim();
    let (head, rest) = match token.split_once(':') {
        Some((h, r)) => (h.trim(), Some(r.trim())),
        None => (token, None),
    };
    match head {
        "off" => Some(FailpointAction::Off),
        "abort" => Some(FailpointAction::Abort),
        "error" => Some(FailpointAction::Error(
            rest.filter(|s| !s.is_empty())
                .unwrap_or("MAW_FP injected error")
                .to_string(),
        )),
        "panic" => Some(FailpointAction::Panic(
            rest.filter(|s| !s.is_empty())
                .unwrap_or("MAW_FP injected panic")
                .to_string(),
        )),
        "sleep" => {
            let ms: u64 = rest?.parse().ok()?;
            Some(FailpointAction::Sleep(Duration::from_millis(ms)))
        }
        // bn-3gba: `corrupt:<abs-path>` — overwrite that file with
        // `CORRUPT_BYTES` and continue. Require an absolute target because
        // failpoint call sites can run from different worktree directories;
        // accepting a relative path could corrupt the wrong checkout.
        "corrupt" => {
            let path = rest.filter(|s| !s.is_empty())?;
            let path = std::path::PathBuf::from(path);
            path.is_absolute().then_some(FailpointAction::Corrupt(path))
        }
        // bn-1jfui: `hang:<abs-marker>` — write the marker, then block until
        // killed (see `FailpointAction::Hang`). Absolute for the same reason
        // as `corrupt`.
        "hang" => {
            let path = rest.filter(|s| !s.is_empty())?;
            let path = std::path::PathBuf::from(path);
            path.is_absolute().then_some(FailpointAction::Hang(path))
        }
        _ => None,
    }
}

/// Parse a `MAW_FP` spec string into concrete `(name, action)` pairs.
///
/// Trailing-`*` names are expanded against [`KNOWN_FAILPOINTS`]. Malformed or
/// empty segments are skipped (never panics — a bad env var must not abort a
/// production process that merely happens to be a failpoints build).
///
/// This is the parser SP1 specced for bn-263u; it is the unit-tested core of
/// the env bridge.
#[cfg(feature = "failpoints")]
#[must_use]
pub fn parse_env_spec(spec: &str) -> Vec<(String, FailpointAction)> {
    let mut out = Vec::new();
    for segment in spec.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((name, action_tok)) = segment.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let Some(action) = parse_action(action_tok) else {
            continue;
        };
        if let Some(prefix) = name.strip_suffix('*') {
            // Glob: expand against the canonical table. Unknown glob -> no-op.
            for fp in KNOWN_FAILPOINTS {
                if fp.starts_with(prefix) {
                    out.push(((*fp).to_string(), action.clone()));
                }
            }
        } else {
            out.push((name.to_string(), action));
        }
    }
    out
}

/// Intern an owned failpoint name to `&'static str`.
///
/// The [`REGISTRY`] keys are `&'static str` (set sites pass string literals).
/// Env-derived names are owned `String`s, so we leak them to obtain the
/// `'static` lifetime. This is bounded: it runs **once** per process from
/// [`init_from_env`], over at most the handful of names in `MAW_FP`. It is
/// never on a hot path and never in the default (non-failpoints) build.
#[cfg(feature = "failpoints")]
fn intern(name: String) -> &'static str {
    Box::leak(name.into_boxed_str())
}

/// One-time guard so `MAW_FP` is read at most once per process.
#[cfg(feature = "failpoints")]
static ENV_LOADED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Seed the failpoint [`REGISTRY`] from the `MAW_FP` environment variable.
///
/// Idempotent and process-global: the spec is parsed and applied the **first**
/// time this is called; subsequent calls are no-ops (`OnceLock`), so it is
/// safe to call unconditionally at every `maw` entry point. Absent/empty
/// `MAW_FP` is a clean no-op.
///
/// Call this once at binary startup (e.g. from `fn main`) so the shipped
/// subprocess honours faults the faithful DST tier injects.
///
/// # Panics
///
/// Panics only if the registry mutex is poisoned (same contract as [`set`]).
#[cfg(feature = "failpoints")]
pub fn init_from_env() {
    ENV_LOADED.get_or_init(|| {
        if let Ok(spec) = std::env::var("MAW_FP") {
            for (name, action) in parse_env_spec(&spec) {
                set(intern(name), action);
            }
        }
    });
}

/// No-op `init_from_env` for the default (zero-overhead) build.
///
/// Lets call sites invoke `failpoints::init_from_env()` unconditionally
/// without a `#[cfg]` at every site; without the feature this compiles away.
///
/// NOTE (bn-2ors, same class as bn-1cww): the scoped allow below silences
/// clippy's nursery `missing_const_for_fn` on this no-feature shape only.
/// The `--features failpoints` shape (lines above) has a fallible body that
/// cannot be `const fn`, so we mustn't unify the two by making this `const`.
#[cfg(not(feature = "failpoints"))]
#[inline]
#[allow(clippy::missing_const_for_fn)]
pub fn init_from_env() {}

#[cfg(test)]
mod tests {
    use super::*;

    // bn-2017: The `REGISTRY` is a process-global; tests that mutate it via
    // `set`/`clear`/`clear_all` race when cargo's default parallel test runner
    // schedules them concurrently (e.g. one test's `clear_all` wipes another
    // test's `FP_KEEP`). Production semantics of `set`/`check`/`clear*` are
    // unchanged — we only serialize the *test-side* access via a shared
    // `Mutex` guard. The non-failpoints build is unaffected (this module is
    // `#[cfg(test)]`-only and the `failpoints` feature still gates the
    // env-bridge tests below).
    //
    // We use `Mutex<()>` with explicit poison recovery: if one test panics
    // while holding the guard, sibling tests should still run rather than
    // cascade-fail with `PoisonError`. The guard scope covers each test from
    // its first registry mutation through its last assertion.
    static TEST_REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_registry() -> std::sync::MutexGuard<'static, ()> {
        TEST_REGISTRY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// bn-1svi: a thread-scoped failpoint fires only on the arming thread,
    /// never on a concurrently running sibling, and is disarmed when its
    /// guard drops.
    #[test]
    fn thread_scoped_failpoint_fires_only_on_arming_thread() {
        // No global-registry mutation, so no `lock_registry()` needed — that
        // independence from the global registry is the point.
        let guard = set_for_this_thread(
            "FP_TEST_THREAD_SCOPED",
            FailpointAction::Error("scoped".into()),
        );
        assert_eq!(check("FP_TEST_THREAD_SCOPED"), Err("scoped".to_owned()));
        let other = std::thread::spawn(|| check("FP_TEST_THREAD_SCOPED"))
            .join()
            .expect("join");
        assert_eq!(other, Ok(()), "must not fire on another thread");
        drop(guard);
        assert_eq!(check("FP_TEST_THREAD_SCOPED"), Ok(()), "guard must disarm");
    }

    /// check returns Ok when no failpoint is set.
    #[test]
    fn check_noop_when_not_set() {
        let _g = lock_registry();
        clear_all();
        assert!(check("FP_TEST_NOOP").is_ok());
    }

    /// bn-2byw: a `Callback` action runs the registered closure (in-process)
    /// and returns Ok, so execution continues past the failpoint. This is the
    /// deterministic interleaving primitive — the closure can mutate external
    /// state captured by the test. Verify it (a) runs, (b) can mutate captured
    /// state, and (c) runs again on a second hit until cleared.
    #[test]
    fn callback_runs_and_mutates_captured_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _g = lock_registry();
        clear_all();

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_cb = Arc::clone(&hits);
        set_callback("FP_TEST_CALLBACK", move || {
            hits_in_cb.fetch_add(1, Ordering::SeqCst);
        });

        assert!(
            check("FP_TEST_CALLBACK").is_ok(),
            "callback action returns Ok so production code continues"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "callback must have run once"
        );

        // Fires every hit until cleared.
        assert!(check("FP_TEST_CALLBACK").is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        clear("FP_TEST_CALLBACK");
        assert!(check("FP_TEST_CALLBACK").is_ok());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "cleared callback must not run again"
        );
    }

    /// check returns error when failpoint is set to Error.
    #[test]
    fn check_returns_error_when_set() {
        let _g = lock_registry();
        clear_all();
        set("FP_TEST_ERROR", FailpointAction::Error("injected".into()));
        let result = check("FP_TEST_ERROR");
        assert!(result.is_err());
        let err = result.expect_err("operation should fail");
        assert!(
            err.contains("injected"),
            "expected 'injected' in error: {err}"
        );
        clear("FP_TEST_ERROR");
    }

    /// `clear_all` resets all failpoints.
    #[test]
    fn clear_all_resets() {
        let _g = lock_registry();
        set("FP_A", FailpointAction::Error("a".into()));
        set("FP_B", FailpointAction::Error("b".into()));
        clear_all();
        assert!(check("FP_A").is_ok());
        assert!(check("FP_B").is_ok());
    }

    /// Off action behaves like no failpoint set.
    #[test]
    fn check_off_action_is_noop() {
        let _g = lock_registry();
        clear_all();
        set("FP_OFF", FailpointAction::Off);
        assert!(check("FP_OFF").is_ok());
        clear("FP_OFF");
    }

    /// Sleep action returns Ok after sleeping.
    #[test]
    fn check_sleep_returns_ok() {
        let _g = lock_registry();
        clear_all();
        set("FP_SLEEP", FailpointAction::Sleep(Duration::from_millis(1)));
        assert!(check("FP_SLEEP").is_ok());
        clear("FP_SLEEP");
    }

    /// bn-3gba: an armed `Corrupt` action actually overwrites the target file
    /// with `CORRUPT_BYTES` and lets execution continue (`check` returns Ok).
    /// This is the partial-materialization fault primitive: it must perturb
    /// state WITHOUT crashing the op, so the post-materialization verify has
    /// something to catch.
    #[test]
    fn corrupt_action_overwrites_and_continues() {
        let _g = lock_registry();
        clear_all();

        let dir = std::env::temp_dir().join(format!(
            "maw-fp-corrupt-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("victim.txt");
        std::fs::write(&file, b"original content\n").expect("seed");

        set("FP_TEST_CORRUPT", FailpointAction::Corrupt(file.clone()));
        assert!(
            check("FP_TEST_CORRUPT").is_ok(),
            "Corrupt must not abort the op — it perturbs and continues"
        );
        clear("FP_TEST_CORRUPT");

        let after = std::fs::read(&file).expect("read back");
        assert_eq!(after, CORRUPT_BYTES);

        // A missing target is swallowed: an injector must never crash the
        // process it is only supposed to perturb.
        set(
            "FP_TEST_CORRUPT_MISSING",
            FailpointAction::Corrupt(dir.join("no").join("such").join("file")),
        );
        assert!(check("FP_TEST_CORRUPT_MISSING").is_ok());
        clear("FP_TEST_CORRUPT_MISSING");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// clear removes a single failpoint without affecting others.
    #[test]
    fn clear_single_failpoint() {
        let _g = lock_registry();
        clear_all();
        set("FP_KEEP", FailpointAction::Error("keep".into()));
        set("FP_REMOVE", FailpointAction::Error("remove".into()));
        clear("FP_REMOVE");
        assert!(check("FP_REMOVE").is_ok());
        assert!(check("FP_KEEP").is_err());
        clear_all();
    }

    // ---- MAW_FP env-bridge parser (bn-263u) -------------------------------
    //
    // These exercise the production `parse_env_spec` / `parse_action` and
    // only build with `--features failpoints` (same gate as the bridge).

    #[cfg(feature = "failpoints")]
    mod env_bridge {
        use super::super::{FailpointAction, parse_env_spec};
        use std::time::Duration;

        /// A bare name with a bare action parses to one pair.
        #[test]
        fn single_error_no_msg() {
            let v = parse_env_spec("FP_COMMIT_BETWEEN_CAS_OPS=error");
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].0, "FP_COMMIT_BETWEEN_CAS_OPS");
            match &v[0].1 {
                FailpointAction::Error(m) => assert_eq!(m, "MAW_FP injected error"),
                other => panic!("expected Error, got {other:?}"),
            }
        }

        /// `error:msg` carries the custom message verbatim.
        #[test]
        fn error_with_message() {
            let v = parse_env_spec("FP_VALIDATE_AFTER_CHECK=error:boom");
            assert_eq!(v.len(), 1);
            match &v[0].1 {
                FailpointAction::Error(m) => assert_eq!(m, "boom"),
                other => panic!("expected Error, got {other:?}"),
            }
        }

        /// Multiple `;`-separated segments parse independently; whitespace and
        /// empty/`=`-less segments are tolerated.
        #[test]
        fn multi_segment_with_whitespace_and_junk() {
            let v = parse_env_spec(
                "  FP_PREPARE_BEFORE_STATE_WRITE = abort ; ; junk ; \
                 FP_BUILD_AFTER_MERGE_COMPUTE=panic:p ;",
            );
            assert_eq!(v.len(), 2);
            assert_eq!(v[0].0, "FP_PREPARE_BEFORE_STATE_WRITE");
            assert!(matches!(v[0].1, FailpointAction::Abort));
            assert_eq!(v[1].0, "FP_BUILD_AFTER_MERGE_COMPUTE");
            match &v[1].1 {
                FailpointAction::Panic(m) => assert_eq!(m, "p"),
                other => panic!("expected Panic, got {other:?}"),
            }
        }

        /// `sleep:<ms>` parses to a Duration; missing/garbage ms is dropped.
        #[test]
        fn sleep_parsing() {
            let v = parse_env_spec("FP_CLEANUP_AFTER_CAPTURE=sleep:1500");
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].1.clone_dur(), Some(Duration::from_millis(1500)));

            // bad/missing ms => whole segment skipped, not a panic.
            assert!(parse_env_spec("FP_X=sleep").is_empty());
            assert!(parse_env_spec("FP_X=sleep:abc").is_empty());
        }

        /// Trailing-`*` glob expands against the canonical table; the three
        /// real `FP_CLEANUP_*` sites must all appear with the same action.
        #[test]
        fn glob_expands_against_known() {
            let v = parse_env_spec("FP_CLEANUP_*=sleep:5000");
            let mut names: Vec<_> = v.iter().map(|(n, _)| n.clone()).collect();
            names.sort();
            assert_eq!(
                names,
                vec![
                    "FP_CLEANUP_AFTER_CAPTURE".to_string(),
                    "FP_CLEANUP_AFTER_DEFAULT_CHECKOUT".to_string(),
                    "FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT".to_string(),
                ]
            );
            for (_, a) in &v {
                assert_eq!(a.clone_dur(), Some(Duration::from_secs(5)));
            }
        }

        /// bn-3gba: `corrupt:<abs-path>` parses to a `Corrupt` action holding
        /// the path verbatim (paths are absolute, so the `split_once(':')`
        /// grammar leaves them intact on unix).
        #[test]
        fn corrupt_parsing() {
            let v = parse_env_spec("FP_CREATE_AFTER_MATERIALIZE=corrupt:/tmp/ws/a/file.txt");
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].0, "FP_CREATE_AFTER_MATERIALIZE");
            match &v[0].1 {
                FailpointAction::Corrupt(p) => {
                    assert_eq!(p, std::path::Path::new("/tmp/ws/a/file.txt"));
                }
                other => panic!("expected Corrupt, got {other:?}"),
            }

            // A bare `corrupt` (no path) is dropped like any malformed segment.
            assert!(parse_env_spec("FP_X=corrupt").is_empty());
            assert!(parse_env_spec("FP_X=corrupt:").is_empty());
            assert!(
                parse_env_spec("FP_X=corrupt:relative/path.txt").is_empty(),
                "relative corruption targets must be rejected"
            );
        }

        /// bn-1jfui: `hang:<abs-marker>` parses to a `Hang` action; relative
        /// or missing markers are rejected like `corrupt`.
        #[test]
        fn hang_parsing() {
            let v = parse_env_spec("FP_COMMIT_BETWEEN_CAS_OPS=hang:/tmp/x/reached");
            assert_eq!(v.len(), 1);
            match &v[0].1 {
                FailpointAction::Hang(p) => {
                    assert_eq!(p, std::path::Path::new("/tmp/x/reached"));
                }
                other => panic!("expected Hang, got {other:?}"),
            }
            assert!(parse_env_spec("FP_X=hang").is_empty());
            assert!(parse_env_spec("FP_X=hang:").is_empty());
            assert!(parse_env_spec("FP_X=hang:rel/marker").is_empty());
        }

        /// An unknown glob matches nothing (no panic, empty result).
        #[test]
        fn unknown_glob_is_empty() {
            assert!(parse_env_spec("FP_NOPE_*=abort").is_empty());
        }

        /// An unrecognised action drops only that segment; later valid
        /// segments still parse.
        #[test]
        fn unknown_action_skipped() {
            let v = parse_env_spec("FP_A=bogus;FP_COMMIT_AFTER_EPOCH_CAS=abort");
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].0, "FP_COMMIT_AFTER_EPOCH_CAS");
        }

        /// Empty / whitespace-only spec yields no pairs and never panics.
        #[test]
        fn empty_spec() {
            assert!(parse_env_spec("").is_empty());
            assert!(parse_env_spec("   ;  ; ").is_empty());
        }

        /// `off` is a real action (used to mask a default-on failpoint).
        #[test]
        fn off_action() {
            let v = parse_env_spec("FP_COMMIT_BEFORE_BRANCH_CAS=off");
            assert_eq!(v.len(), 1);
            assert!(matches!(v[0].1, FailpointAction::Off));
        }

        // Test-only helper to introspect Sleep durations.
        impl FailpointAction {
            fn clone_dur(&self) -> Option<Duration> {
                match self {
                    Self::Sleep(d) => Some(*d),
                    _ => None,
                }
            }
        }
    }
}
