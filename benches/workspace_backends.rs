//! Workspace backend benchmarks.
//!
//! Measures performance of workspace create, snapshot, and N-way merge
//! as described in design doc §1.1 and §7.5.
//!
//! # Running
//!
//! ```bash
//! cargo bench --bench workspace_backends
//! # With a custom filter:
//! cargo bench --bench workspace_backends -- create
//! cargo bench --bench workspace_backends -- snapshot
//! cargo bench --bench workspace_backends -- merge
//! ```
//!
//! # Performance targets (design doc §1.1)
//!
//! - Workspace create < 100ms for 30k files (git-worktree)
//! - Workspace create < 1s for 1M files (CoW-backed)
//! - Snapshot cost proportional to changed files, not repo size
//! - N-way merge cost proportional to touched files + conflict set
//!
//! # Report
//!
//! HTML report is generated in `target/criterion/` by criterion when
//! `--features html_reports` is active (enabled by default via Cargo.toml).
//!
//! JSON data is also emitted per group in `target/criterion/<group>/<bench>/`.
//! Summary JSON is written by the `bench_summary_json` helper at the end.

use std::path::{Path, PathBuf};
use std::process::Command;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use maw::backend::{WorkspaceBackend, git::GitWorktreeBackend};
use maw::backend::platform::{
    PlatformCapabilities, auto_select_backend, detect_or_load, estimate_repo_file_count,
};
use maw::merge::partition::partition_by_path;
use maw::merge::types::{ChangeKind, FileChange, PatchSet};
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
    let _caps = detect_or_load(dir.path());
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
// Benchmark: snapshot cost vs repo size and change count
//
// Validates §1.1: "Snapshot cost proportional to changed files, not repo size."
// Strategy: hold change count fixed and vary repo size → times must be similar.
// ---------------------------------------------------------------------------

/// Benchmark `backend.snapshot()` across change counts and repo sizes.
///
/// The key invariant under test: snapshot time should track `changed_files`,
/// not `repo_files`.  We benchmark (`repo_size` × changes) pairs.
fn bench_snapshot_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/git-worktree");

    // (repo_files, changed_files) pairs.
    // Same change counts across two repo sizes → times should cluster by changes.
    let cases: &[(usize, usize)] = &[
        (100, 1),
        (100, 10),
        (100, 50),
        (500, 1),
        (500, 10),
        (500, 50),
    ];

    for &(repo_n, changed_n) in cases {
        let (_guard, root, oid) = make_temp_repo(repo_n);
        let epoch = EpochId::new(&oid).unwrap();
        let backend = GitWorktreeBackend::new(root.clone());

        // Create one workspace, modify `changed_n` files in it, then snapshot repeatedly.
        let ws_name = format!("snap-{repo_n}-{changed_n}");
        let ws_id = WorkspaceId::new(&ws_name).unwrap();
        let ws_info = backend.create(&ws_id, &epoch).expect("create snapshot workspace");
        let ws_path = &ws_info.path;

        // Modify `changed_n` files (touch files that already exist in the workspace).
        let changed_n_actual = changed_n.min(repo_n);
        for i in 0..changed_n_actual {
            let chunk = 100.max(repo_n / 10);
            let sub = format!("src/part{}", i / chunk);
            let path = ws_path.join(&sub).join(format!("file{i}.txt"));
            // Write new content to trigger a modification.
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, format!("modified bench file {i}\n"));
        }

        let label = format!("repo{repo_n}_changed{changed_n}");
        group.throughput(Throughput::Elements(changed_n_actual as u64));
        group.bench_with_input(BenchmarkId::new("snapshot", label), &changed_n, |b, _| {
            b.iter(|| {
                let _ = backend.snapshot(&ws_id);
            });
        });

        cleanup_workspace(&root, &ws_name);
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: N-way merge partition — cost vs workspace count and touched files
//
// Validates §1.1: "N-way merge cost proportional to touched files + conflict set."
// Strategy 1: vary workspace count but keep total touched files constant → flat.
// Strategy 2: vary total touched files (across workspaces) → linear growth.
// ---------------------------------------------------------------------------

/// Build a synthetic `PatchSet` for benchmarking `partition_by_path`.
///
/// Each workspace gets `files_per_ws` unique non-overlapping modified files,
/// named `ws<ws_idx>_file<file_idx>.txt` to guarantee no conflicts.
fn make_patch_set(ws_idx: usize, files_per_ws: usize, epoch: &EpochId) -> PatchSet {
    let ws_id = WorkspaceId::new(&format!("bench-ws-{ws_idx}")).unwrap();
    let changes: Vec<FileChange> = (0..files_per_ws)
        .map(|fi| {
            FileChange::new(
                PathBuf::from(format!("src/ws{ws_idx}_file{fi}.txt")),
                ChangeKind::Modified,
                Some(format!("content ws{ws_idx} file{fi}").into_bytes()),
            )
        })
        .collect();
    PatchSet::new(ws_id, epoch.clone(), changes)
}

/// Benchmark: fixed total touched files, varying workspace count.
///
/// Total files = 100 (constant). As workspace count grows, `files_per_ws` shrinks.
/// `partition_by_path` time should stay roughly constant.
fn bench_merge_partition_fixed_total(c: &mut Criterion) {
    // We need any epoch OID — use a fake 40-char hex string.
    let epoch = EpochId::new(&"a".repeat(40)).unwrap();

    let mut group = c.benchmark_group("merge/partition_fixed_total");
    let total_files = 100usize;

    // workspace counts: 2, 5, 10, 20
    for &ws_count in &[2usize, 5, 10, 20] {
        let files_per_ws = (total_files / ws_count).max(1);
        let patch_sets: Vec<PatchSet> = (0..ws_count)
            .map(|i| make_patch_set(i, files_per_ws, &epoch))
            .collect();

        group.throughput(Throughput::Elements(total_files as u64));
        group.bench_with_input(
            BenchmarkId::new("workspaces", ws_count),
            &ws_count,
            |b, _| {
                b.iter(|| {
                    let _ = partition_by_path(&patch_sets);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: fixed workspace count (5), varying total touched files.
///
/// Total files grows: 10, 50, 100, 500, 1000.
/// `partition_by_path` time should grow roughly linearly with total files.
fn bench_merge_partition_scaling(c: &mut Criterion) {
    let epoch = EpochId::new(&"b".repeat(40)).unwrap();

    let mut group = c.benchmark_group("merge/partition_scaling");
    let ws_count = 5usize;

    for &total_files in &[10usize, 50, 100, 500, 1_000] {
        let files_per_ws = total_files / ws_count;
        let patch_sets: Vec<PatchSet> = (0..ws_count)
            .map(|i| make_patch_set(i, files_per_ws, &epoch))
            .collect();

        group.throughput(Throughput::Elements(total_files as u64));
        group.bench_with_input(
            BenchmarkId::new("total_files", total_files),
            &total_files,
            |b, _| {
                b.iter(|| {
                    let _ = partition_by_path(&patch_sets);
                });
            },
        );
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
    bench_snapshot_scaling,
    bench_merge_partition_fixed_total,
    bench_merge_partition_scaling,
);
criterion_main!(benches);
