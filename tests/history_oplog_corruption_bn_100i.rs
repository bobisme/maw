//! Regression coverage for operation-log corruption in `maw ws history`.

mod manifold_common;

use std::io::Write;
use std::process::{Command, Stdio};

use manifold_common::TestRepo;

#[test]
fn history_reports_corrupt_oplog_instead_of_falling_back_to_git() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "alice"]);

    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .take()
        .expect("hash-object stdin")
        .write_all(b"this is not an operation")
        .expect("write corrupt operation blob");
    let output = child.wait_with_output().expect("wait for hash-object");
    assert!(output.status.success(), "git hash-object failed");
    let corrupt_oid = String::from_utf8(output.stdout)
        .expect("hash-object output is UTF-8")
        .trim()
        .to_owned();

    repo.git(&["update-ref", "refs/manifold/head/alice", &corrupt_oid]);

    let stderr = repo.maw_fails(&["ws", "history", "alice"]);
    assert!(
        stderr.contains("Failed to read operation log for workspace 'alice'"),
        "history should surface operation-log damage, got:\n{stderr}"
    );
    assert!(
        stderr.contains("git cat-file -p"),
        "history should include the actionable inspection command, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("showing git commit history"),
        "history must not disguise a corrupt operation log as an absent one"
    );
}
