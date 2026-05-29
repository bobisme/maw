//! bn-1q6z PATH-shim smoke test.
//!
//! Asserts the wrapper-script PATH-shim:
//!
//! 1. Materializes and runs as a passthrough (default-disabled) when
//!    `MAW_BENCH_CHAOS_KILL_PROB` is unset — the agent sees byte-identical
//!    behaviour to invoking the real binary.
//! 2. SIGKILLs the wrapped subprocess (not the system git) when the
//!    chaos env is armed — exit status carries the kill signal.
//! 3. Materializes from a real adapter (`WorktreesConventionAdapter`)
//!    such that the wired shim dir is reachable via the adapter's
//!    `shim()` accessor and matches what
//!    `RealSubstrate::setup` would populate into
//!    `SubstrateHandle::agent_extra_env`.
//!
//! Skips gracefully when `git` is missing on the test host.

#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::process::Command;
use std::time::{Duration, Instant};

use maw_bench_adapters::shim::{ShimSet, env_keys};
use maw_bench_adapters::worktrees_adapter::WorktreesConventionAdapter;

fn git_on_path() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Default-passthrough: shim with no chaos env behaves as `git --version`.
#[test]
fn shim_passthrough_is_byte_identical_when_chaos_unset() {
    if !git_on_path() {
        eprintln!("skipping: git missing");
        return;
    }
    let shim = ShimSet::materialize_temp().expect("materialize");
    let real = Command::new("git").arg("--version").output().expect("real");
    let via_shim = Command::new(shim.dir().join("git"))
        .arg("--version")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(env_keys::SHIM_DIR, shim.dir())
        .env_remove(env_keys::CHAOS_KILL_PROB)
        .output()
        .expect("shim spawn");
    assert!(
        via_shim.status.success(),
        "shim status: {:?}",
        via_shim.status
    );
    assert_eq!(
        String::from_utf8_lossy(&real.stdout),
        String::from_utf8_lossy(&via_shim.stdout),
        "passthrough must match real git stdout byte-for-byte"
    );
}

/// Chaos armed: shim SIGKILLs the wrapped invocation. We point the
/// shim at a deterministic-slow fake "real git" (a `sleep 30`
/// shim-let) via `_MAW_SHIM_REAL_GIT` so the test is not racing the
/// system git's actual runtime. This is the cleanest assertion of
/// the shim's *mechanics*: roll fires → setsid → kill after Nms →
/// wrapped subprocess dies. The agent under real chaos sees the
/// exact same code path; what varies is which real binary is
/// wrapped.
#[test]
fn shim_kills_wrapped_invocation_when_chaos_armed() {
    let shim = ShimSet::materialize_temp().expect("materialize");

    // Build a long-running "fake real git" so the kill window is
    // deterministically inside the child's lifetime.
    let fake_dir = tempfile::tempdir().expect("fake tempdir");
    let fake_git = fake_dir.path().join("real-git");
    std::fs::write(
        &fake_git,
        "#!/usr/bin/env bash\n\
         # Fake `real git` for the shim smoke test. Sleeps 30s; the\n\
         # shim's chaos path is expected to SIGKILL us long before.\n\
         echo 'fake-git: starting'\n\
         sleep 30\n\
         echo 'fake-git: finished (chaos missed!)'\n",
    )
    .expect("write fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&fake_git).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake_git, p).unwrap();
    }

    let started = Instant::now();
    let out = Command::new(shim.dir().join("git"))
        .arg("merge")
        .arg("some-branch")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(env_keys::SHIM_DIR, shim.dir())
        .env(env_keys::REAL_GIT, &fake_git)
        .env(env_keys::CHAOS_KILL_PROB, "1.0")
        .env(env_keys::CHAOS_KILL_MS, "50")
        .output()
        .expect("shim spawn");
    let elapsed = started.elapsed();
    // The kill must arrive promptly. With kill_ms=50 the wall-time
    // budget is ~100ms; 5s is a very loose CI-jitter slack.
    assert!(
        elapsed < Duration::from_secs(5),
        "shim didn't kill within 5s; elapsed = {elapsed:?}"
    );
    assert!(
        !out.status.success(),
        "shim should have killed wrapped `git merge`; status = {:?}",
        out.status
    );
    // The wrapped fake-git starts but never finishes — its
    // "finished (chaos missed!)" sentinel must NOT appear.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("chaos missed"),
        "kill window missed the wrapped subprocess; output: {combined}"
    );
    // Sanity: the shim emitted its chaos-firing log line on stderr.
    assert!(
        combined.contains("chaos firing"),
        "expected shim chaos-firing stderr; got: {combined}"
    );
    // The wrapped subprocess started (printed "starting") before the
    // kill arrived — proves the shim actually executed the wrapped
    // binary, not just bailed early.
    assert!(
        combined.contains("fake-git: starting"),
        "wrapped subprocess didn't start; output: {combined}"
    );
}

/// Real adapter integration: the `WorktreesConventionAdapter`
/// materializes a shim in `new()` and exposes it via `shim()`. The
/// returned dir is on disk and runnable.
#[test]
fn worktrees_adapter_exposes_runnable_shim_dir() {
    if !git_on_path() {
        eprintln!("skipping: git missing");
        return;
    }
    let adapter = WorktreesConventionAdapter::new().expect("adapter");
    let shim_dir = adapter.shim().dir();
    assert!(shim_dir.exists(), "adapter shim dir missing: {shim_dir:?}");
    let git_shim = shim_dir.join("git");
    let jj_shim = shim_dir.join("jj");
    assert!(git_shim.exists(), "git shim missing under adapter");
    assert!(jj_shim.exists(), "jj shim missing under adapter");
    // Smoke: passthrough works through the adapter's shim too.
    let out = Command::new(&git_shim)
        .arg("--version")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(env_keys::SHIM_DIR, shim_dir)
        .env_remove(env_keys::CHAOS_KILL_PROB)
        .output()
        .expect("spawn adapter shim");
    assert!(out.status.success(), "adapter shim passthrough failed");
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("git version"),
        "expected real git output; got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
