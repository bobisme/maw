//! Integration test: exhaustive model check of the merge protocol.
//!
//! Requires the `assurance` feature: `cargo test --test formal_model --features assurance`

#![cfg(feature = "assurance")]

use maw::assurance::model::*;
use stateright::*;

fn parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

#[test]
fn merge_model_check_two_workspaces() {
    let model = MergeModel::new(vec!["alice".into(), "bob".into()]);
    model
        .checker()
        .threads(parallelism())
        .spawn_dfs()
        .join()
        .assert_properties();
}

#[test]
fn merge_model_check_three_workspaces() {
    let model = MergeModel::new(vec!["alice".into(), "bob".into(), "carol".into()]);
    model
        .checker()
        .threads(parallelism())
        .spawn_dfs()
        .join()
        .assert_properties();
}
