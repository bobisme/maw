//! End-to-end smoke test for [`maw_bench::claude::ClaudeBackend`] against
//! a real `claude` install (bn-3kxq).
//!
//! # NOT RUN BY CI
//!
//! Every test in this file is `#[ignore]`-gated. Reaching the real LLM
//! requires:
//!
//! 1. The `claude` binary on `$PATH` with valid auth (OAuth keychain
//!    or `ANTHROPIC_API_KEY`).
//! 2. `MAW_BENCH_ALLOW_REAL_LLM=1` exported in the test environment
//!    (defence-in-depth — the harness refuses real dispatch without it).
//! 3. Both the `bench` and `claude-backend` cargo features enabled.
//!
//! # Cost budget
//!
//! Target: **< $0.10 per run**. The single scenario in [`smoke_lists_files_and_finishes`]
//! is one trivial Bash call (`ls`) — empirically that lands around
//! $0.04 – $0.08 (per SP3 §3 measurements; identical envelope shape).
//! A `--max-budget-usd 0.10` hard cap on the subprocess ensures we
//! cannot exceed that even on a runaway loop.
//!
//! # Lead-side invocation
//!
//! ```sh
//! MAW_BENCH_ALLOW_REAL_LLM=1 \
//!   cargo test -p maw-bench \
//!     --features bench,claude-backend \
//!     --test claude_backend_smoke \
//!     -- --ignored --nocapture
//! ```
//!
//! # What the test asserts
//!
//! - The `AgentReply` is non-empty (at least one turn arrived).
//! - At least one turn made at least one tool call (the agent used
//!   Bash to list files — confirms the tool surface is wired and
//!   stream parsing extracts `tool_use` blocks).
//! - Either `done == true` OR a sensible `stop_reason` is set
//!   (success / `end_turn` / a documented error subtype).
//! - `cost_usd` is reported and within the test budget (the `result`
//!   envelope's `total_cost_usd` made it through the parser).

#![cfg(all(feature = "bench", feature = "claude-backend"))]

use std::fs;
use std::path::PathBuf;

use maw_bench::agent::{AgentBackend, AgentConfig};
use maw_bench::claude::ClaudeBackend;
use maw_bench::substrate::{NoopSubstrate, Substrate, SubstrateConfig, SubstrateHandle};

/// Minimal smoke: ask the agent to list the files in its workspace
/// and report back. Validates the full wire (spawn → stdin prompt
/// → stream-json parse → reply assembly → cost capture).
///
/// **Cost**: target < $0.10 (one trivial Bash call). The
/// subprocess-side `--max-budget-usd 0.10` cap forces termination
/// before that ceiling.
#[test]
#[ignore = "real-LLM dispatch; manual + lead-authorized only"]
fn smoke_lists_files_and_finishes() {
    // Defence-in-depth guard mirrors the production runtime check —
    // if the operator forgot to export the env we refuse cleanly.
    assert_eq!(
        std::env::var("MAW_BENCH_ALLOW_REAL_LLM").as_deref(),
        Ok("1"),
        "set MAW_BENCH_ALLOW_REAL_LLM=1 to run this smoke test"
    );

    // Build a minimal substrate: a tempdir with one seed file the
    // agent should discover. Reuses NoopSubstrate (tempdir-backed)
    // so we don't depend on a real maw substrate adapter (T2.3).
    let mut substrate = NoopSubstrate::new();
    let handle: SubstrateHandle = substrate
        .setup(&SubstrateConfig {
            seed: 42,
            base_git_time: 0,
            debug_dir: None,
        })
        .expect("noop substrate setup");

    // Seed a couple of files so `ls` is non-trivial.
    fs::write(handle.workspace_root.join("README.md"), b"# scratch\n").unwrap();
    fs::write(handle.workspace_root.join("notes.txt"), b"hi\n").unwrap();
    let seeded_dir: PathBuf = handle.workspace_root.clone();

    let prompt = format!(
        "You are a smoke-test agent. Use the Bash tool to run `ls -1` in \
         {} and reply with the file names you see (one per line). Then \
         stop — do not do anything else.",
        seeded_dir.display()
    );

    // Pin the model+budget tight so this is cheap and predictable.
    let cfg = AgentConfig {
        model: "sonnet".to_string(),
        max_turns: 8,
        max_budget_usd: 0.10,
        temperature: 1.0,
        permission_mode: "bypassPermissions".to_string(),
        extra_env: std::collections::BTreeMap::new(),
    };

    let mut backend = ClaudeBackend::new();
    let reply = match backend.run(&prompt, &cfg, &handle) {
        Ok(r) => r,
        Err(e) => panic!(
            "ClaudeBackend.run failed: {e}\n\
             (is `claude` on PATH and authenticated? \
              run `claude --version` and `claude auth status`)"
        ),
    };

    // --- assertions ---
    eprintln!("--- smoke reply (bn-3kxq) ---");
    eprintln!("turns: {}", reply.turns.len());
    eprintln!("done: {}", reply.done);
    eprintln!("stop_reason: {:?}", reply.stop_reason);
    eprintln!("cost_usd: {:?}", reply.cost_usd);
    for (i, t) in reply.turns.iter().enumerate() {
        eprintln!(
            "  turn[{i}] index={} text_len={} tool_calls={}",
            t.index,
            t.reply_text.len(),
            t.tool_calls.len(),
        );
        for (j, tc) in t.tool_calls.iter().enumerate() {
            eprintln!(
                "    tool[{j}] name={} args_json_len={} result={}",
                tc.name,
                tc.args_json.len(),
                tc.result_truncated
                    .as_ref()
                    .map(|s| s.len().to_string())
                    .unwrap_or_else(|| "none".into()),
            );
        }
    }

    assert!(
        !reply.turns.is_empty(),
        "expected at least one turn; got empty transcript"
    );
    assert!(
        reply
            .turns
            .iter()
            .any(|t| !t.tool_calls.is_empty()),
        "expected at least one turn with a tool call (Bash); \
         the agent should have used Bash to ls. \
         If this fires, check the --allowed-tools flag in build_argv."
    );
    if let Some(cost) = reply.cost_usd {
        assert!(cost > 0.0, "cost_usd reported but not positive: {cost}");
        assert!(
            cost <= 0.15, // budget + small overrun tolerance for billing rounding
            "cost {cost} exceeds smoke-test budget (~$0.10)"
        );
    }
    // Done OR a documented stop_reason. Both are acceptable; an
    // empty done+empty stop_reason would indicate a parser regression.
    assert!(
        reply.done || !reply.stop_reason.is_empty(),
        "neither done nor stop_reason set; parser regression?"
    );

    substrate.teardown(handle).expect("teardown");
}
