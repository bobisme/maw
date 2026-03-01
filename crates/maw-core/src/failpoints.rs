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
    REGISTRY.lock().unwrap().insert(name, action);
}

/// Clear a specific failpoint.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned.
pub fn clear(name: &'static str) {
    REGISTRY.lock().unwrap().remove(name);
}

/// Clear all failpoints.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned.
pub fn clear_all() {
    REGISTRY.lock().unwrap().clear();
}

/// Check if a failpoint is set and execute its action.
/// Returns `Ok(())` if no failpoint or `Off`, `Err` if `Error` action.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned, or if the
/// failpoint action is `Panic`.
pub fn check(name: &str) -> Result<(), String> {
    let registry = REGISTRY.lock().unwrap();
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
        let err = result.unwrap_err();
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
        set(
            "FP_SLEEP",
            FailpointAction::Sleep(Duration::from_millis(1)),
        );
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
}
