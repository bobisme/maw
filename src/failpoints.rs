//! Failpoint injection — re-exported from maw-core.
pub use maw_core::failpoints::*;

/// Failpoint injection point — delegates to maw-core's failpoints::check.
///
/// With `failpoints` feature: checks the registry and may return `Err` or panic.
/// Without `failpoints` feature: compiles to nothing (zero overhead).
///
/// Usage: `fp!("FP_COMMIT_AFTER_EPOCH_CAS")?;`
#[cfg(feature = "failpoints")]
#[macro_export]
macro_rules! fp {
    ($name:expr) => {
        maw_core::failpoints::check($name)
            .map_err(|msg| anyhow::anyhow!("failpoint {}: {}", $name, msg))
    };
}

#[cfg(not(feature = "failpoints"))]
#[macro_export]
macro_rules! fp {
    ($name:expr) => {
        Ok::<(), anyhow::Error>(())
    };
}
