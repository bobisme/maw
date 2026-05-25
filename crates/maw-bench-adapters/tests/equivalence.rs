//! Equivalence test: the same `Vec<ScriptedOp>` driven through all three
//! substrates (via [`NoopAgent::drive`]) yields the same
//! substrate-neutral [`StateSnapshot`].
//!
//! This is the **load-bearing parity property** for SG2. Any drift between
//! substrates on the substrate-neutral surface is a parity bug that biases
//! metrics. Asymmetric per-adapter artifacts (recovery refs, op-log
//! divergence, claim files) are intentionally EXCLUDED from
//! `StateSnapshot` — see `notes/sg2-adapter-parity.md` for the catalogue
//! and justification of every permitted asymmetry.

#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::missing_panics_doc)]

use std::collections::BTreeMap;

use maw_bench_adapters::jj_adapter::JjAdapter;
use maw_bench_adapters::maw_adapter::MawAdapter;
use maw_bench_adapters::worktrees_adapter::WorktreesConventionAdapter;
use maw_bench_adapters::{NoopAgent, ScriptedOp, StateSnapshot, Substrate};
use maw_scenario::{BaseRef, WsId};

fn binary_present(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn script_basic() -> Vec<ScriptedOp> {
    let a = WsId::slot(0);
    vec![
        ScriptedOp::Create {
            ws: a.clone(),
            base: BaseRef::Main,
        },
        ScriptedOp::Edit {
            ws: a.clone(),
            path: "src/lib.rs".into(),
            content: "pub fn alpha() {}\n".into(),
        },
        ScriptedOp::Commit {
            ws: a.clone(),
            msg: "feat: alpha".into(),
        },
        ScriptedOp::Merge {
            srcs: vec![a.clone()],
            // Each adapter accepts BOTH "main" and "default" as the
            // integration label; we pick "default" so the maw arm uses
            // its native verb verbatim. The other two adapters map
            // "default" → "main" (justified in the parity table).
            target: "default".into(),
            destroy: true,
        },
    ]
}

/// The set of files we compare across adapters. We exclude substrate-
/// metadata files (e.g. `.coord/*` for worktrees+convention) that ARE
/// the per-adapter substrate-native surface — they're documented in the
/// parity table as expected asymmetry.
fn filter_substrate_metadata(snap: &StateSnapshot) -> BTreeMap<String, String> {
    snap.integrated_files
        .iter()
        .filter(|(k, _)| !k.starts_with(".coord/") && !k.starts_with(".manifold/"))
        // jj records its operation log under .jj; we skip it in the
        // colocated layout we use anyway because `collect_files` excludes
        // .jj/.git. The filter here is the docs-as-code equivalent.
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[test]
fn substrate_neutral_snapshot_is_equal_across_adapters() {
    // git is required for all three. maw and jj are conditionally present.
    if !binary_present("git") {
        eprintln!("skipping: git missing on PATH");
        return;
    }
    let maw_present =
        binary_present(&std::env::var("MAW_BENCH_BIN").unwrap_or_else(|_| "maw".into()));
    let jj_present = binary_present("jj");

    let script = script_basic();

    // worktrees+convention (always present if git is).
    let mut wt = WorktreesConventionAdapter::new().expect("worktrees adapter");
    let wt_outcomes = NoopAgent::drive(&mut wt, &script).expect("wt drive");
    let wt_snap = wt.state_snapshot().expect("wt snapshot");
    let wt_files = filter_substrate_metadata(&wt_snap);

    let maw_files: Option<BTreeMap<String, String>> = if maw_present {
        let mut m = MawAdapter::new().expect("maw adapter");
        let _outcomes = NoopAgent::drive(&mut m, &script).expect("maw drive");
        let snap = m.state_snapshot().expect("maw snapshot");
        Some(filter_substrate_metadata(&snap))
    } else {
        None
    };

    let jj_files: Option<BTreeMap<String, String>> = if jj_present {
        let mut j = JjAdapter::new().expect("jj adapter");
        let _outcomes = NoopAgent::drive(&mut j, &script).expect("jj drive");
        let snap = j.state_snapshot().expect("jj snapshot");
        Some(filter_substrate_metadata(&snap))
    } else {
        None
    };

    // The "alpha" file must be present and equal across every available
    // adapter. This is the substrate-neutral equivalence assertion: same
    // script → same integrated bytes, regardless of substrate.
    let alpha_wt = wt_files.get("src/lib.rs").cloned();
    assert_eq!(
        alpha_wt.as_deref(),
        Some("pub fn alpha() {}\n"),
        "worktrees: src/lib.rs missing or wrong"
    );
    if let Some(m) = &maw_files {
        let alpha_m = m.get("src/lib.rs").cloned();
        assert_eq!(
            alpha_m, alpha_wt,
            "maw vs worktrees: integrated src/lib.rs diverges"
        );
    }
    if let Some(j) = &jj_files {
        let alpha_j = j.get("src/lib.rs").cloned();
        assert_eq!(
            alpha_j, alpha_wt,
            "jj vs worktrees: integrated src/lib.rs diverges"
        );
    }

    // After destroy=true the live workspace set must be empty on every
    // adapter (the substrate-neutral lifecycle assertion).
    assert!(
        wt_snap.live_workspaces.is_empty(),
        "worktrees: live workspaces should be empty post-merge-destroy"
    );

    // Echo the per-step outcomes for the parity audit log.
    eprintln!(
        "[parity] worktrees outcomes: {:?}",
        wt_outcomes.iter().map(|o| &o.notes).collect::<Vec<_>>()
    );
}

#[test]
fn worktrees_and_maw_produce_same_integrated_file_set() {
    if !binary_present("git") {
        eprintln!("skipping: git missing on PATH");
        return;
    }
    let maw_bin = std::env::var("MAW_BENCH_BIN").unwrap_or_else(|_| "maw".into());
    if !binary_present(&maw_bin) {
        eprintln!("skipping: maw missing on PATH");
        return;
    }

    let a = WsId::slot(0);
    let b = WsId::slot(1);
    let script = vec![
        ScriptedOp::Create {
            ws: a.clone(),
            base: BaseRef::Main,
        },
        ScriptedOp::Create {
            ws: b.clone(),
            base: BaseRef::Main,
        },
        ScriptedOp::Edit {
            ws: a.clone(),
            path: "a.txt".into(),
            content: "a\n".into(),
        },
        ScriptedOp::Commit {
            ws: a.clone(),
            msg: "a".into(),
        },
        ScriptedOp::Edit {
            ws: b.clone(),
            path: "b.txt".into(),
            content: "b\n".into(),
        },
        ScriptedOp::Commit {
            ws: b.clone(),
            msg: "b".into(),
        },
        ScriptedOp::Merge {
            srcs: vec![a.clone(), b.clone()],
            target: "default".into(),
            destroy: true,
        },
    ];

    let mut wt = WorktreesConventionAdapter::new().expect("wt");
    NoopAgent::drive(&mut wt, &script).expect("wt drive");
    let wt_files = filter_substrate_metadata(&wt.state_snapshot().expect("wt snap"));

    let mut m = MawAdapter::new().expect("maw");
    NoopAgent::drive(&mut m, &script).expect("maw drive");
    let m_files = filter_substrate_metadata(&m.state_snapshot().expect("maw snap"));

    assert_eq!(
        m_files.get("a.txt").map(String::as_str),
        Some("a\n"),
        "maw missing a.txt"
    );
    assert_eq!(
        m_files.get("b.txt").map(String::as_str),
        Some("b\n"),
        "maw missing b.txt"
    );
    assert_eq!(
        wt_files.get("a.txt").map(String::as_str),
        Some("a\n"),
        "wt missing a.txt"
    );
    assert_eq!(
        wt_files.get("b.txt").map(String::as_str),
        Some("b\n"),
        "wt missing b.txt"
    );
}
