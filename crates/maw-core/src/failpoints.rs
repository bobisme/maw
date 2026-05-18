//! Feature-gated failpoint injection for DST.
//!
//! Compile with `--features failpoints` to enable injection.
//! Without the feature, the `fp!()` macro expands to nothing.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Actions a failpoint can take when triggered.
#[derive(Clone, Debug)]
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
    let registry = REGISTRY.lock().expect("operation should succeed");
    match registry.get(name) {
        None | Some(FailpointAction::Off) => Ok(()),
        Some(FailpointAction::Error(msg)) => Err(msg.clone()),
        Some(FailpointAction::Panic(msg)) => panic!("failpoint {name}: {msg}"),
        Some(FailpointAction::Abort) => std::process::abort(),
        Some(FailpointAction::Sleep(d)) => {
            let d = *d;
            drop(registry); // release lock before sleeping
            std::thread::sleep(d);
            Ok(())
        }
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
    "FP_BUILD_AFTER_MERGE_COMPUTE",
    "FP_BUILD_AFTER_WORKTREE_ADD",
    "FP_BUILD_BEFORE_MERGE_COMPUTE",
    "FP_BUILD_BEFORE_WORKTREE_ADD",
    "FP_CAPTURE_BEFORE_PIN",
    "FP_CLEANUP_AFTER_CAPTURE",
    "FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT",
    "FP_COMMIT_AFTER_EPOCH_CAS",
    "FP_COMMIT_BEFORE_BRANCH_CAS",
    "FP_COMMIT_BETWEEN_CAS_OPS",
    "FP_DESTROY_AFTER_DELETE",
    "FP_DESTROY_AFTER_RECORD",
    "FP_DESTROY_AFTER_STATUS",
    "FP_DESTROY_BEFORE_CAPTURE",
    "FP_DESTROY_BEFORE_DELETE",
    "FP_PREPARE_AFTER_STATE_WRITE",
    "FP_PREPARE_BEFORE_STATE_WRITE",
    "FP_RECOVER_BEFORE_RESTORE",
    "FP_RECOVER_BEFORE_SEARCH",
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
#[cfg(not(feature = "failpoints"))]
#[inline]
pub fn init_from_env() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// check returns Ok when no failpoint is set.
    #[test]
    fn check_noop_when_not_set() {
        clear_all();
        assert!(check("FP_TEST_NOOP").is_ok());
    }

    /// check returns error when failpoint is set to Error.
    #[test]
    fn check_returns_error_when_set() {
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
        set("FP_A", FailpointAction::Error("a".into()));
        set("FP_B", FailpointAction::Error("b".into()));
        clear_all();
        assert!(check("FP_A").is_ok());
        assert!(check("FP_B").is_ok());
    }

    /// Off action behaves like no failpoint set.
    #[test]
    fn check_off_action_is_noop() {
        clear_all();
        set("FP_OFF", FailpointAction::Off);
        assert!(check("FP_OFF").is_ok());
        clear("FP_OFF");
    }

    /// Sleep action returns Ok after sleeping.
    #[test]
    fn check_sleep_returns_ok() {
        clear_all();
        set("FP_SLEEP", FailpointAction::Sleep(Duration::from_millis(1)));
        assert!(check("FP_SLEEP").is_ok());
        clear("FP_SLEEP");
    }

    /// clear removes a single failpoint without affecting others.
    #[test]
    fn clear_single_failpoint() {
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

        /// Trailing-`*` glob expands against the canonical table; the two
        /// real `FP_CLEANUP_*` sites must both appear with the same action.
        #[test]
        fn glob_expands_against_known() {
            let v = parse_env_spec("FP_CLEANUP_*=sleep:5000");
            let mut names: Vec<_> = v.iter().map(|(n, _)| n.clone()).collect();
            names.sort();
            assert_eq!(
                names,
                vec![
                    "FP_CLEANUP_AFTER_CAPTURE".to_string(),
                    "FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT".to_string(),
                ]
            );
            for (_, a) in &v {
                assert_eq!(a.clone_dur(), Some(Duration::from_millis(5000)));
            }
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
                    FailpointAction::Sleep(d) => Some(*d),
                    _ => None,
                }
            }
        }
    }
}
