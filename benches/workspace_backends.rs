//! Workspace backend benchmarks.
//!
//! Measures performance of workspace create, disk usage, and backend
//! auto-selection as described in design doc §1.1 and §7.5.
//!
//! # Running
//!
//! ```bash
//! cargo bench --bench workspace_backends
//! # With a custom filter:
//! cargo bench --bench workspace_backends -- create
//! ```
//!
//! # Performance targets (design doc §1.1)
//!
//! - Workspace create < 100ms for 30k files (git-worktree)
//! - Workspace create < 1s for 1M files (CoW-backed)
//!
//! # Report
//!
//! HTML report is generated in `target/criterion/` by criterion when
//! `--features html_reports` is active (enabled by default via Cargo.toml).

use std::path::{Path, PathBuf};
use std::process::Command;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use maw::backend::{WorkspaceBackend, git::GitWorktreeBackend};
use maw::backend::platform::{
    PlatformCapabilities, auto_select_backend, detect_or_load, estimate_repo_file_count,
};
use maw::model::types::{EpochId, WorkspaceId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary git repository with `n` dummy files.
///
/// Returns the path of the repository and the HEAD OID.
fn make_temp_repo(n: usize) -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_owned();

    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&root)
            .status()
            .expect("git")
    };

    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "bench@manifold"]);
    git(&["config", "user.name", "bench"]);

    // Create ws/ so git-worktree works correctly.
    std::fs::create_dir_all(root.join("ws")).expect("mkdir ws");

    // Generate `n` files spread across a shallow tree for speed.
    let chunk = 100.max(n / 10);
    for i in 0..n {
        let sub = format!("src/part{}", i / chunk);
        std::fs::create_dir_all(root.join(&sub)).ok();
        let path = root.join(sub).join(format!("file{i}.txt"));
        std::fs::write(path, format!("bench file {i}\n")).expect("write file");
    }

    git(&["add", "."]);
    git(&["commit", "-m", "bench: initial"]);

    let oid_out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .expect("rev-parse HEAD");
    let oid = String::from_utf8_lossy(&oid_out.stdout).trim().to_owned();

    (dir, root, oid)
}

/// Destroy a workspace using the git worktree backend.
fn cleanup_workspace(root: &Path, name: &str) {
    let backend = GitWorktreeBackend::new(root.to_owned());
    let ws_id = WorkspaceId::new(name).unwrap();
    let _ = backend.destroy(&ws_id);
}

// ---------------------------------------------------------------------------
// Benchmark: workspace create time
// ---------------------------------------------------------------------------

/// Benchmark git-worktree workspace creation across repo sizes.
fn bench_create_git_worktree(c: &mut Criterion) {
    let mut group = c.benchmark_group("create/git-worktree");

    // Repo sizes to benchmark (bounded to keep CI fast).
    // For full §1.1 validation, include 30_000 or higher.
    let sizes: &[usize] = &[100, 500, 1_000];

    for &n in sizes {
        let (_guard, root, oid) = make_temp_repo(n);
        let epoch = EpochId::new(&oid).unwrap();
        let backend = GitWorktreeBackend::new(root.clone());

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("files", n), &n, |b, _| {
            let mut counter = 0_u64;
            b.iter(|| {
                let name = format!("bench-ws-{counter}");
                counter += 1;
                let ws_id = WorkspaceId::new(&name).unwrap();
                let _ = backend.create(&ws_id, &epoch);
                // Destroy after each create to avoid workspace accumulation.
                cleanup_workspace(&root, &name);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: disk usage (cold workspace vs base repo)
// ---------------------------------------------------------------------------

/// Measure workspace disk footprint (created once, measured once — not a
/// timing benchmark).  Uses `du -sb` for portability.
fn bench_disk_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_usage/git-worktree");

    let sizes: &[usize] = &[100, 500];

    for &n in sizes {
        let (_guard, root, oid) = make_temp_repo(n);
        let epoch = EpochId::new(&oid).unwrap();
        let backend = GitWorktreeBackend::new(root.clone());

        // Create one workspace and leave it for measurement.
        let name = format!("du-ws-{n}");
        let ws_id = WorkspaceId::new(&name).unwrap();
        let info = backend.create(&ws_id, &epoch).expect("create workspace");

        // Use criterion to record the du measurement (not timing — just to
        // surface the number in the report).
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("bytes", n), &n, |b, _| {
            b.iter(|| {
                // Measure workspace dir size in bytes.
                du_bytes(&info.path)
            });
        });

        cleanup_workspace(&root, &name);
    }

    group.finish();
}

/// Return approximate on-disk size of a directory using `du -sb`.
fn du_bytes(path: &Path) -> u64 {
    let output = Command::new("du")
        .args(["-sb", path.to_str().unwrap_or(".")])
        .output();
    output.ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
    }).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Benchmark: auto-selection logic (micro — no I/O)
// ---------------------------------------------------------------------------

/// Micro-benchmark: auto-selection decision (pure function, no I/O).
///
/// Validates that the selection logic itself is O(1) and negligible overhead.
fn bench_auto_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_select");

    let caps_none = PlatformCapabilities::default();
    let caps_reflink = PlatformCapabilities {
        reflink_supported: true,
        ..PlatformCapabilities::default()
    };
    let caps_all = PlatformCapabilities {
        reflink_supported: true,
        overlay_userns_supported: true,
        fuse_overlayfs_available: true,
        kernel_major: Some(6),
        kernel_minor: Some(8),
        ..PlatformCapabilities::default()
    };

    let cases: &[(&str, usize, &PlatformCapabilities)] = &[
        ("small_no_cow",    1_000, &caps_none),
        ("medium_reflink",  50_000, &caps_reflink),
        ("large_overlay",   150_000, &caps_all),
    ];

    for (label, size, caps) in cases {
        group.bench_with_input(BenchmarkId::new("backend", label), label, |b, _| {
            b.iter(|| auto_select_backend(*size, caps));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: platform detection (with cache)
// ---------------------------------------------------------------------------

/// Benchmark the platform capability cache read (common hot path).
fn bench_platform_detect_cached(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");

    // Warm the cache.
    let caps = detect_or_load(dir.path());
    // Verify it's cached.
    assert!(
        maw::backend::platform::load_cached(dir.path()).is_some(),
        "cache should exist after detect_or_load"
    );

    c.bench_function("platform/detect_cached", |b| {
        b.iter(|| detect_or_load(dir.path()));
    });
}

// ---------------------------------------------------------------------------
// Benchmark: file count estimation
// ---------------------------------------------------------------------------

/// Benchmark `estimate_repo_file_count` on repos of different sizes.
fn bench_estimate_file_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("file_count");

    let sizes: &[usize] = &[100, 500];

    for &n in sizes {
        let (_guard, root, _oid) = make_temp_repo(n);

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("files", n), &n, |b, _| {
            b.iter(|| estimate_repo_file_count(&root));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_create_git_worktree,
    bench_disk_usage,
    bench_auto_select,
    bench_platform_detect_cached,
    bench_estimate_file_count,
);
criterion_main!(benches);
