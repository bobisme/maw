//! Reproduces the SP3 jj opfork-wedge against the real `jj` binary so the
//! adapter's observability is permanently locked in (per the
//! `bn-mit2` HARD RULES: do NOT install workarounds that silently dodge
//! the wedge).
//!
//! Mirrors `notes/agent-benchmark-feasibility.md` §1: three concurrent
//! workspaces, repeated round-based concurrent `jj status`/`jj describe`
//! bursts. The pass condition is "we observed at least one of: the
//! 'sibling of the working copy's operation' envelope, OR a `(divergent)`
//! change-id, OR the 'Concurrent modification detected' notice — any of
//! the three wedge fingerprints SP3 §1 enumerated."
//!
//! This test is `#[ignore]`-gated because it shells out to real `jj` and
//! runs concurrent processes; run via `cargo test --features bench -- --ignored`.

#![cfg(feature = "bench")]
#![allow(clippy::needless_collect)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::single_match_else)]
#![allow(clippy::missing_panics_doc)]

use std::path::PathBuf;
use std::process::Command;

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(bin: &str, args: &[&str], cwd: &std::path::Path) -> (bool, String, String) {
    let out = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env("JJ_USER", "bench")
        .env("JJ_EMAIL", "bench@localhost")
        .output()
        .expect("spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[ignore = "requires jj on PATH; runs concurrent jj processes — see SP3 §1"]
fn jj_opfork_wedge_is_reproducible() {
    assert!(
        jj_available(),
        "jj not on PATH — install jj 0.41+ to reproduce SP3 §1 wedge"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let root: PathBuf = tmp.path().to_path_buf();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    // Colocated init + seed (mirrors SP3 §1 setup).
    run("jj", &["git", "init", "--colocate"], &repo);
    std::fs::write(repo.join("README.md"), "seed\n").unwrap();
    run("jj", &["describe", "-m", "base commit"], &repo);
    run("jj", &["new", "-m", "wip"], &repo);

    // Three concurrent workspaces.
    for name in ["alice", "bob", "carol"] {
        let dir = root.join(format!("ws-{name}"));
        let dir_s = dir.to_string_lossy().to_string();
        run("jj", &["workspace", "add", "--name", name, &dir_s], &repo);
    }

    // Concurrency driver — rounds of parallel commands that read the
    // same op-head and extend the DAG concurrently. SP3 §1 used 5
    // rounds for the trigger; we also do the heavier 8-round burst.
    use std::thread;
    let mut fingerprints: Vec<String> = Vec::new();
    for rounds in [5_usize, 8] {
        let mut handles = vec![];
        for round in 0..rounds {
            for name in ["alice", "bob", "carol"] {
                let dir = root.join(format!("ws-{name}"));
                let h = thread::spawn(move || {
                    let dir_clone = dir.clone();
                    // Heterogeneous commands to maximize op-DAG churn.
                    match name {
                        "alice" => {
                            let (_, _, e) = run("jj", &["status"], &dir_clone);
                            e
                        }
                        "bob" => {
                            let msg = format!("bob r{round}");
                            let (_, _, e) = run("jj", &["describe", "-m", &msg], &dir_clone);
                            e
                        }
                        _ => {
                            let (_, _, e) = run("jj", &["new", "-m", "wip"], &dir_clone);
                            e
                        }
                    }
                });
                handles.push(h);
            }
        }
        for h in handles {
            let stderr = h.join().expect("thread");
            if stderr.contains("sibling of the working copy")
                || stderr.contains("Concurrent modification detected")
            {
                fingerprints.push(stderr);
            }
        }
    }

    // Post-burst: list workspaces and look for `(divergent)`.
    let (_, list_out, list_err) = run("jj", &["workspace", "list"], &repo);
    if list_out.contains("(divergent)") || list_err.contains("(divergent)") {
        fingerprints.push(format!("(divergent) in workspace list: {list_out}"));
    }
    // Also probe op log for `reconcile` nodes (the heal-merge signature).
    let (_, op_out, _) = run("jj", &["op", "log", "--no-graph", "--limit", "200"], &repo);
    if op_out.contains("reconcile divergent operations") {
        fingerprints.push("op log shows reconcile divergent operations".to_string());
    }

    assert!(
        !fingerprints.is_empty(),
        "SP3 §1 wedge not reproduced — expected at least one of: \
         'sibling of the working copy's operation' / \
         'Concurrent modification detected' / \
         '(divergent)' / 'reconcile divergent operations'. \
         If this test fails on a future jj release, do NOT install a \
         workaround in the adapter — instead update SP3 + the parity \
         table; the wedge being absent IS the news."
    );

    // Print the first fingerprint to the test log so reviewers can see it.
    eprintln!(
        "[wedge fingerprint, {} occurrences] {}",
        fingerprints.len(),
        fingerprints[0]
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join(" | ")
    );
}
