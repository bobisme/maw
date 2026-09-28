//! bn-wxg28: `maw gc --recovery-snapshots` keeps recovery pins of workspaces
//! that still exist (the default workspace's dirty-trunk pins, `materialize-*`
//! pins of a live agent workspace) unless `--include-live` is passed, and any
//! risky drop (`--older-than 0`, a pin younger than a day, a live workspace's
//! pin) deletes nothing without `--force` and lists what it would drop.

mod manifold_common;

use manifold_common::TestRepo;

const OLD_TS: &str = "2020-01-01T00-00-00.000000000Z";
const NEEDLE: &str = "NEEDLE-bn-wxg28-displaced-trunk-bytes";

fn recovery_refs(repo: &TestRepo) -> Vec<String> {
    repo.git(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/manifold/recovery/",
    ])
    .lines()
    .map(str::to_owned)
    .collect()
}

/// A commit that holds `NEEDLE` (the "only copy" of displaced bytes), made in
/// a scratch workspace so trunk stays clean.
fn needle_commit(repo: &TestRepo) -> String {
    repo.create_workspace("carrier");
    repo.add_file("carrier", "displaced.txt", &format!("{NEEDLE}\n"));
    repo.git_in_workspace("carrier", &["add", "displaced.txt"]);
    repo.git_in_workspace("carrier", &["commit", "-m", "displaced bytes"]);
    repo.workspace_head("carrier")
}

fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn live_default_pin_survives_gc_and_recover_still_finds_it() {
    let repo = TestRepo::new();
    let oid = needle_commit(&repo);
    let default_pin = format!("refs/manifold/recovery/default/{OLD_TS}");
    let carrier_pin = format!("refs/manifold/recovery/carrier/materialize-{OLD_TS}");
    let gone_pin = format!("refs/manifold/recovery/gone/{OLD_TS}");
    for r in [&default_pin, &carrier_pin, &gone_pin] {
        repo.git(&["update-ref", r, &oid]);
    }

    // Default sweep: old pins of live workspaces are kept; the old pin of a
    // workspace that no longer exists is collected WITHOUT --force.
    let out = repo.maw_ok(&["gc", "--recovery-snapshots"]);
    let refs = recovery_refs(&repo);
    assert!(refs.contains(&default_pin), "{refs:?}\n{out}");
    assert!(refs.contains(&carrier_pin), "{refs:?}\n{out}");
    assert!(!refs.contains(&gone_pin), "{refs:?}\n{out}");
    assert!(
        out.contains("--include-live"),
        "output must say how to include live pins: {out}"
    );

    // `maw ws recover` still finds the kept pin's content.
    let found = repo.maw_ok(&["ws", "recover", "--search", NEEDLE]);
    assert!(found.contains("default"), "recover --search: {found}");

    // --include-live without --force: refuses, lists the refs, deletes nothing.
    let out = repo.maw_raw_exact(&["gc", "--recovery-snapshots", "--include-live"]);
    let text = combined(&out);
    assert!(!out.status.success(), "must refuse: {text}");
    assert!(text.contains(&default_pin), "{text}");
    assert!(text.contains("workspace default, LIVE workspace"), "{text}");
    assert!(
        text.contains("maw gc --recovery-snapshots --include-live --force"),
        "{text}"
    );
    assert_eq!(recovery_refs(&repo).len(), 2, "refused gc deleted a ref");

    // --dry-run previews the same drop and the --force command, exit 0.
    let out = repo.maw_ok(&["gc", "--recovery-snapshots", "--include-live", "--dry-run"]);
    assert!(out.contains(&default_pin), "{out}");
    assert!(out.contains("--include-live --force"), "{out}");
    assert_eq!(recovery_refs(&repo).len(), 2);

    // --include-live --force drops them.
    repo.maw_ok(&["gc", "--recovery-snapshots", "--include-live", "--force"]);
    assert!(recovery_refs(&repo).is_empty());
}

#[test]
fn older_than_zero_refuses_without_force() {
    let repo = TestRepo::new();
    repo.create_workspace("alice");
    repo.add_file("alice", "draft.md", "queued work\n");
    repo.maw_ok(&["ws", "destroy", "alice", "--force"]);
    let before = recovery_refs(&repo);
    assert_eq!(before.len(), 1, "destroy --force pins one snapshot");

    let out = repo.maw_raw_exact(&["gc", "--recovery-snapshots", "--older-than", "0"]);
    let text = combined(&out);
    assert!(!out.status.success(), "must refuse: {text}");
    assert!(text.contains(&before[0]), "must list the ref: {text}");
    assert!(text.contains("workspace alice"), "{text}");
    assert!(text.contains("less than 1 day"), "{text}");
    assert!(
        text.contains("maw gc --recovery-snapshots --older-than 0 --force"),
        "must print the exact next command: {text}"
    );
    assert_eq!(recovery_refs(&repo), before, "refused gc deleted a ref");

    repo.maw_ok(&["gc", "--recovery-snapshots", "--older-than", "0", "--force"]);
    assert!(recovery_refs(&repo).is_empty());
}

#[test]
fn force_and_include_live_require_recovery_snapshots() {
    let repo = TestRepo::new();
    let err = repo.maw_fails(&["gc", "--force"]);
    assert!(err.contains("--recovery-snapshots"), "{err}");
    let err = repo.maw_fails(&["gc", "--include-live"]);
    assert!(err.contains("--recovery-snapshots"), "{err}");
}
