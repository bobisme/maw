//! End-to-end regression coverage for validation pipe backpressure (bn-15tj).

mod manifold_common;

use std::path::{Path, PathBuf};

use manifold_common::TestRepo;

const VERBOSE_COMMAND: &str = "yes merge-stdout | head -c 200000; echo MERGE-STDOUT-END; \
     yes merge-stderr | head -c 200000 >&2; echo MERGE-STDERR-END >&2";

fn write_validation_config(repo: &TestRepo, command: &str, on_failure: &str) {
    std::fs::write(
        repo.root().join(".manifold/config.toml"),
        format!(
            "[repo]\nbranch = \"main\"\n\n[merge.validation]\ncommand = {command:?}\n\
             timeout_seconds = 10\non_failure = \"{on_failure}\"\n"
        ),
    )
    .expect("write validation config");
}

fn validation_artifacts(root: &Path) -> Vec<PathBuf> {
    let merge_dir = root.join(".manifold/artifacts/merge");
    std::fs::read_dir(merge_dir)
        .expect("read merge artifacts")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("validation.json"))
        .filter(|path| path.is_file())
        .collect()
}

fn only_quarantine_id(repo: &TestRepo) -> String {
    let quarantine = repo.root().join(".manifold/quarantine");
    let mut ids: Vec<String> = std::fs::read_dir(quarantine)
        .expect("read quarantine directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), 1, "expected one quarantine: {ids:?}");
    ids.pop().expect("quarantine id")
}

#[test]
fn real_merge_drains_verbose_validator_and_persists_both_streams() {
    let repo = TestRepo::new();
    write_validation_config(&repo, VERBOSE_COMMAND, "block");
    repo.create_workspace("worker");
    repo.add_file("worker", "merged.txt", "merged\n");

    let output = repo.maw_raw_exact(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "default",
        "--message",
        "merge worker",
    ]);
    assert!(
        output.status.success(),
        "merge failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let artifacts = validation_artifacts(repo.root());
    assert_eq!(
        artifacts.len(),
        1,
        "validation artifact missing: {artifacts:?}"
    );
    let artifact = std::fs::read_to_string(&artifacts[0]).expect("read validation artifact");
    assert!(artifact.contains("MERGE-STDOUT-END"));
    assert!(artifact.contains("MERGE-STDERR-END"));
    assert!(artifact.contains("\"passed\": true"));
}

#[test]
fn quarantine_promote_reuses_verbose_runner_without_pipe_stall() {
    let repo = TestRepo::new();
    let command = format!("{VERBOSE_COMMAND}; test ! -f BROKEN");
    write_validation_config(&repo, &command, "quarantine");
    repo.create_workspace("worker");
    repo.add_file("worker", "merged.txt", "merged\n");
    repo.add_file("worker", "BROKEN", "broken\n");

    let _merge = repo.maw_raw_exact(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "default",
        "--message",
        "quarantine worker",
    ]);
    let merge_id = only_quarantine_id(&repo);
    let quarantine_artifact = repo
        .root()
        .join(".manifold/quarantine")
        .join(&merge_id)
        .join("validation.json");
    let artifact =
        std::fs::read_to_string(quarantine_artifact).expect("read quarantine validation artifact");
    assert!(artifact.contains("MERGE-STDOUT-END"));
    assert!(artifact.contains("MERGE-STDERR-END"));
    assert!(artifact.contains("\"passed\": false"));

    let broken = repo
        .root()
        .join("ws")
        .join(format!("merge-quarantine-{merge_id}"))
        .join("BROKEN");
    std::fs::remove_file(broken).expect("fix quarantine");
    let promote = repo.maw_raw_exact(&["merge", "promote", &merge_id]);
    assert!(
        promote.status.success(),
        "promote failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&promote.stdout),
        String::from_utf8_lossy(&promote.stderr)
    );
    assert!(repo.file_exists("default", "merged.txt"));
}
