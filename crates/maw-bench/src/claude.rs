//! Claude Code subprocess backend — real-LLM agent driver.
//!
//! **GATED BEHIND `claude-backend`.** A vanilla `cargo check --features
//! bench` does NOT pull this module's subprocess wire and does NOT
//! require `claude` on PATH. Real benchmark sweeps opt in by enabling
//! this feature AND setting `MAW_BENCH_ALLOW_REAL_LLM=1` at runtime
//! (defence-in-depth — a compile-time feature alone cannot fire a
//! billable call).
//!
//! # Architecture (frozen via SP3 / bn-2ixm)
//!
//! Subprocess invocation pattern, not direct Anthropic API. The
//! rationale (see `notes/agent-benchmark-feasibility.md` §2) is that
//! Claude Code's actual tool surface (Bash / Read / Edit / Write /
//! Glob / Grep) drives git / maw / jj; we want to measure the
//! production agent, not a reimplemented tool loop.
//!
//! One `claude -p` invocation per scenario run (NOT per planned step).
//! The agent's task prompt = the rendered scenario; the agent makes
//! multiple turns inside one process. cwd of the spawned subprocess
//! is [`SubstrateHandle::workspace_root`].
//!
//! # Auth gotcha (SP3 §2)
//!
//! `--bare` (the obvious "isolate context" flag) breaks OAuth in this
//! build — it forces auth to `ANTHROPIC_API_KEY`/`apiKeyHelper` only
//! and refuses the OAuth/keychain session. We deliberately do NOT
//! pass `--bare`; context isolation comes from the substrate placing
//! its workspace under a clean root with no surrounding `CLAUDE.md` /
//! `AGENTS.md` / `.mcp.json`. The smoke test `tests/claude_backend_smoke.rs`
//! validates against the operator's installed `claude` (OAuth, API
//! key, whatever they configured).
//!
//! # Drift from SP3's invocation (2026-05-26, CC 2.1.150)
//!
//! - `--max-turns` is NOT a current `claude` flag. We honour
//!   [`AgentConfig::max_turns`] as a post-stream cap (count of
//!   distinct assistant message ids).
//! - `--temperature` is NOT a current `claude` flag. We document this
//!   as deferred (CC does not expose sampling-temperature on the CLI).
//! - `--cwd` is NOT a current `claude` flag. We set the subprocess
//!   cwd via [`std::process::Command::current_dir`] instead.
//! - `--add-dir` is available; we pass `workspace_root` so the agent
//!   has tool access scoped to it.
//! - SP3 used `--output-format json` (single result envelope). We use
//!   `--output-format stream-json --verbose` because the per-turn /
//!   per-tool-call detail the harness records only appears in the
//!   stream form. The final `result` event in the stream still
//!   carries `total_cost_usd` (the §6.4 manifest field).
//!
//! # Stream envelope shape (probed on CC 2.1.150)
//!
//! Each line on stdout is one JSON object with a discriminator `type`:
//!
//! - `system` — `subtype: "init"` carries `cwd`, `model`,
//!   `permissionMode`, `claude_code_version`. Other subtypes
//!   (`hook_started`, `hook_response`) are ignored.
//! - `assistant` — one model message. `message.id` is the turn key
//!   (multiple `assistant` events can share the same id when the
//!   model emits thinking + tool_use blocks separately). `message.content`
//!   is an array of blocks: `{type: "text", text: ...}`,
//!   `{type: "thinking", thinking: ...}`, or
//!   `{type: "tool_use", id, name, input}`.
//! - `user` — tool-result echo (the harness records the truncated
//!   result text on the originating tool call via `tool_use_id`).
//! - `rate_limit_event` / `stream_event` — ignored (telemetry).
//! - `result` — terminal envelope. Fields: `subtype` (`success` /
//!   `error_max_budget_usd` / `error_max_turns` / ...), `is_error`,
//!   `total_cost_usd`, `num_turns`, `stop_reason`, `result` (final
//!   text), `permission_denials`.
//!
//! Unknown event types are tolerated (the CC stream schema is allowed
//! to grow forward-compatibly). Malformed JSON lines are skipped
//! (the harness's contract is "best-effort transcript"; billing is
//! read from the terminal `result` event).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::agent::{AgentBackend, AgentConfig, AgentError, AgentReply, AgentTurn};
use crate::run::ToolCall;
use crate::substrate::SubstrateHandle;

/// Maximum characters of a tool result we record in
/// [`ToolCall::result_truncated`]. Bounds JSON record size — the
/// harness's contract is the event stream, not the data stream.
pub const TOOL_RESULT_TRUNCATE_BYTES: usize = 4096;

/// Real-LLM backend driving `claude -p --output-format stream-json`.
///
/// Holds the path to the `claude` binary (defaults to `"claude"` on
/// `$PATH`). Construction does NOT validate the binary exists; that
/// happens lazily in [`AgentBackend::run`] when the operator has
/// opted-in via `MAW_BENCH_ALLOW_REAL_LLM=1`.
pub struct ClaudeBackend {
    /// Path to the `claude` executable. Defaults to `"claude"`
    /// (resolved via `$PATH`). Override with [`ClaudeBackend::with_binary`]
    /// for test rigs that point at a fake binary.
    binary: String,
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeBackend {
    /// Construct a `ClaudeBackend` that resolves `claude` from `$PATH`.
    ///
    /// Note: unlike the T2.2 `from_env` constructor, this does NOT
    /// read `ANTHROPIC_API_KEY`. The subprocess inherits the
    /// operator's environment and uses whatever auth their `claude`
    /// install is configured for (OAuth keychain, API key, etc.).
    /// SP3 §2 documents why this matters: forcing API-key-only auth
    /// (e.g. via `--bare`) breaks the OAuth path on this host.
    #[must_use]
    pub fn new() -> Self {
        Self {
            binary: "claude".to_string(),
        }
    }

    /// Override the `claude` binary path. Used by integration tests
    /// that point at a fake binary instead of the real install.
    #[must_use]
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// Construct from env. Kept for source-compat with T2.2 callers;
    /// `ANTHROPIC_API_KEY` is no longer required (subprocess inherits
    /// whatever auth the operator's `claude` is configured for) so
    /// this just delegates to [`ClaudeBackend::new`].
    ///
    /// # Errors
    ///
    /// Never errors (kept fallible for source-compat). Callers can
    /// safely `.expect("infallible")`.
    pub fn from_env() -> Result<Self, AgentError> {
        Ok(Self::new())
    }
}

/// Build the subprocess argv for one [`ClaudeBackend::run`].
///
/// Pulled out so tests can assert the exact flag set without
/// spawning a process. The argv shape is part of the §6.4
/// reproducibility manifest — anyone re-running the benchmark by
/// hand should be able to type these same flags.
fn build_argv(config: &AgentConfig, workspace_root: &Path) -> Vec<String> {
    let argv = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // stream-json requires --verbose per CC 2.1.150's CLI.
        "--verbose".to_string(),
        "--model".to_string(),
        config.model.clone(),
        "--permission-mode".to_string(),
        config.permission_mode.clone(),
        "--allowed-tools".to_string(),
        "Bash,Read,Edit,Write,Glob,Grep".to_string(),
        // Scope tool access to the agent's substrate workspace.
        "--add-dir".to_string(),
        workspace_root.to_string_lossy().into_owned(),
        // §1.1 budget guard. Distinct from the harness's post-stream
        // max_turns cap below.
        "--max-budget-usd".to_string(),
        format!("{:.4}", config.max_budget_usd),
        // Disable session persistence so each scenario run is a clean
        // fresh-context invocation (the harness's invariant).
        "--no-session-persistence".to_string(),
    ];
    // NOTE (drift): SP3 referenced `--max-turns` and `--temperature`.
    // CC 2.1.150 exposes neither on the CLI. The harness honours
    // `config.max_turns` post-stream (see `parse_stream`). Temperature
    // is provider-default; recorded in the manifest as such.
    let _ = config.temperature; // explicitly acknowledge unused
    argv
}

/// One assistant-message-id's worth of accumulated state, used by
/// [`parse_stream`] to fold multiple `assistant` events with the same
/// `message.id` into a single [`AgentTurn`].
#[derive(Default)]
struct TurnAccumulator {
    /// Insertion order (the order the first event for this id arrived).
    seq: u32,
    /// Wall-clock at first appearance.
    ts_unix_ms: u64,
    /// Concatenated text blocks across this turn's assistant events.
    text: String,
    /// Tool calls in arrival order.
    tool_calls: Vec<ToolCall>,
    /// Mapping `tool_use_id` -> index into `tool_calls` so later
    /// `user` tool-result events can backfill `result_truncated`.
    tool_index: HashMap<String, usize>,
}

/// Result of parsing one full stream. Holds the per-turn accumulators
/// in arrival order plus terminal-envelope state.
struct ParsedStream {
    turns: Vec<AgentTurn>,
    cost_usd: Option<f64>,
    /// Stop reason from the terminal `result` event, or a synthetic
    /// reason if the stream ended without a `result` (e.g. process
    /// died). Mirrors the `claude -p` envelope's `stop_reason` /
    /// `subtype` semantics.
    stop_reason: String,
    /// True iff the terminal `result` reported `is_error: false`.
    done: bool,
}

/// Truncate a tool-result string to [`TOOL_RESULT_TRUNCATE_BYTES`]
/// bytes (UTF-8 boundary safe). Marks truncation with a trailing
/// ellipsis sentinel so analysts can tell at a glance whether the
/// record is full or clipped.
fn truncate_result(s: &str) -> String {
    if s.len() <= TOOL_RESULT_TRUNCATE_BYTES {
        return s.to_string();
    }
    // Find a UTF-8 char boundary at-or-before the byte limit.
    let mut end = TOOL_RESULT_TRUNCATE_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("…[truncated]");
    out
}

/// Pull a `String` field out of a serde JSON value defensively.
fn json_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

/// Parse the stream-json line stream into a [`ParsedStream`].
///
/// Streams from a reader so the caller can pipe stdout directly
/// without buffering the whole transcript. Defensive: skips lines
/// that aren't valid JSON or aren't an object with a `type` key
/// (the CC stream is forward-compatible so we tolerate unknown
/// events).
fn parse_stream<R: BufRead>(reader: R, max_turns: u32) -> ParsedStream {
    let mut turn_order: Vec<String> = Vec::new();
    let mut turn_map: HashMap<String, TurnAccumulator> = HashMap::new();
    let mut cost_usd: Option<f64> = None;
    let mut stop_reason: String = "stream_ended".to_string();
    let mut done = false;
    let mut subtype: Option<String> = None;
    let mut is_error: Option<bool> = None;

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = json_str(&v, "type") else {
            continue;
        };
        match kind.as_str() {
            "assistant" => handle_assistant(&v, &mut turn_order, &mut turn_map),
            "user" => handle_user_tool_result(&v, &mut turn_map),
            "result" => {
                cost_usd = v.get("total_cost_usd").and_then(Value::as_f64);
                stop_reason = json_str(&v, "stop_reason").unwrap_or_default();
                subtype = json_str(&v, "subtype");
                is_error = v.get("is_error").and_then(Value::as_bool);
            }
            // `system`, `rate_limit_event`, `stream_event`, … — ignored.
            _ => {}
        }
    }

    // Resolve done / stop_reason. If `result` set `is_error == false`
    // we trust it. If `is_error == true`, prefer the subtype as the
    // reason (e.g. `error_max_budget_usd`).
    match (is_error, subtype) {
        (Some(false), _) => {
            done = true;
        }
        (Some(true), Some(sub)) => {
            stop_reason = sub;
        }
        _ => {}
    }

    // Materialize turns in arrival order.
    let mut turns: Vec<AgentTurn> = turn_order
        .into_iter()
        .filter_map(|id| turn_map.remove(&id))
        .map(|acc| AgentTurn {
            index: acc.seq,
            ts_unix_ms: acc.ts_unix_ms,
            reply_text: acc.text,
            tool_calls: acc.tool_calls,
        })
        .collect();

    // Post-stream `max_turns` cap. CC 2.1.150 doesn't expose a
    // `--max-turns` flag, so the harness enforces the contract: if
    // the model produced more turns than allowed, truncate and mark
    // the run as not-done with `stop_reason = "max_turns"`.
    if max_turns > 0 && turns.len() > max_turns as usize {
        turns.truncate(max_turns as usize);
        done = false;
        stop_reason = "max_turns".to_string();
    }

    ParsedStream {
        turns,
        cost_usd,
        stop_reason,
        done,
    }
}

/// Fold one `assistant` envelope into the turn accumulators.
fn handle_assistant(
    v: &Value,
    turn_order: &mut Vec<String>,
    turn_map: &mut HashMap<String, TurnAccumulator>,
) {
    let Some(message) = v.get("message") else {
        return;
    };
    let Some(msg_id) = json_str(message, "id") else {
        return;
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

    let acc = turn_map.entry(msg_id.clone()).or_insert_with(|| {
        let seq = u32::try_from(turn_order.len() + 1).unwrap_or(u32::MAX);
        turn_order.push(msg_id.clone());
        TurnAccumulator {
            seq,
            ts_unix_ms: now_ms,
            ..TurnAccumulator::default()
        }
    });

    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        match json_str(block, "type").as_deref() {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    acc.text.push_str(t);
                }
            }
            // `thinking` blocks aren't part of the reply_text contract
            // (they're metacognitive; analyst can pull them from
            // the raw stream if needed). They fall through to the
            // catch-all below — kept un-arm'd to avoid `match_same_arms`.
            Some("tool_use") => {
                let name = json_str(block, "name").unwrap_or_default();
                let id = json_str(block, "id").unwrap_or_default();
                let args_json = block
                    .get("input")
                    .map(|i| serde_json::to_string(i).unwrap_or_default())
                    .unwrap_or_default();
                let call_idx = acc.tool_calls.len();
                acc.tool_calls.push(ToolCall {
                    name,
                    args_json,
                    ts_unix_ms: now_ms,
                    result_truncated: None,
                    attributed_op: None,
                    attributed_outcome: None,
                });
                if !id.is_empty() {
                    acc.tool_index.insert(id, call_idx);
                }
            }
            _ => {}
        }
    }
}

/// Fold one `user` tool-result envelope back onto the originating
/// tool call's `result_truncated` field. Best-effort: if the
/// `tool_use_id` isn't in any accumulator, we drop the result (it's
/// noise from a tool we didn't record).
fn handle_user_tool_result(v: &Value, turn_map: &mut HashMap<String, TurnAccumulator>) {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        if json_str(block, "type").as_deref() != Some("tool_result") {
            continue;
        }
        let Some(use_id) = json_str(block, "tool_use_id") else {
            continue;
        };
        // tool_result `content` can be a string OR a list of content
        // blocks (text, image, …). Handle both.
        let text = match block.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|x| x.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        // Find which turn this `tool_use_id` belongs to. Linear scan
        // is fine — turn counts are O(tens) for the SG2 budget.
        for acc in turn_map.values_mut() {
            if let Some(&idx) = acc.tool_index.get(&use_id) {
                if let Some(call) = acc.tool_calls.get_mut(idx) {
                    call.result_truncated = Some(truncate_result(&text));
                }
                return;
            }
        }
    }
}

impl AgentBackend for ClaudeBackend {
    fn run(
        &mut self,
        prompt: &str,
        config: &AgentConfig,
        handle: &SubstrateHandle,
    ) -> Result<AgentReply, AgentError> {
        // Defence-in-depth: even with the feature enabled, refuse to
        // make a real LLM call unless the operator has explicitly opted
        // in by setting an env var. Cost guard against accidental
        // `cargo test --features claude-backend`.
        if std::env::var("MAW_BENCH_ALLOW_REAL_LLM").as_deref() != Ok("1") {
            return Err(AgentError::Config(
                "real LLM dispatch is opt-in: set MAW_BENCH_ALLOW_REAL_LLM=1 \
                 (this is a deliberate guard — see crates/maw-bench/src/claude.rs \
                  for rationale)"
                    .to_string(),
            ));
        }

        let argv = build_argv(config, &handle.workspace_root);
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv)
            .current_dir(&handle.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            AgentError::Provider(format!(
                "failed to spawn `{}`: {e} (is the binary on $PATH?)",
                self.binary
            ))
        })?;

        // Pipe the prompt to the agent's stdin and close it so the
        // subprocess sees EOF and proceeds with a single user turn.
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .map_err(|e| AgentError::Provider(format!("write prompt to stdin: {e}")))?;
            // Drop on scope exit closes stdin.
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Provider("subprocess stdout pipe missing".to_string()))?;
        let parsed = parse_stream(BufReader::new(stdout), config.max_turns);

        // Drain stderr (small) — discarded but kept for error
        // diagnostics on non-zero exit.
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut stderr_buf);
        }

        let status = child
            .wait()
            .map_err(|e| AgentError::Provider(format!("wait on subprocess: {e}")))?;

        // We treat a non-zero exit WITHOUT a parsed `result` envelope
        // as a provider error (CC died before completing). If a
        // `result` envelope arrived (even with `is_error: true`), we
        // surface it as a normal AgentReply with `done = false` and
        // the appropriate `stop_reason` — the harness's verdict
        // logic decides whether that's a substrate failure or a
        // budget-cap discard.
        if !status.success() && parsed.cost_usd.is_none() && parsed.turns.is_empty() {
            return Err(AgentError::Provider(format!(
                "claude exited {:?} with no result envelope; stderr: {}",
                status.code(),
                stderr_buf.lines().take(20).collect::<Vec<_>>().join(" | ")
            )));
        }

        // Honour max_budget_usd post-stream when the envelope reports
        // a cost above the budget but the subtype didn't already
        // flag it (defensive — the CLI's `--max-budget-usd` should
        // surface `error_max_budget_usd`, but we double-check).
        let mut reply = AgentReply {
            turns: parsed.turns,
            done: parsed.done,
            cost_usd: parsed.cost_usd,
            stop_reason: parsed.stop_reason,
        };
        if let Some(c) = reply.cost_usd
            && c > config.max_budget_usd
            && reply.done
        {
            reply.done = false;
            reply.stop_reason = "max_budget".to_string();
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::substrate::{NoopSubstrate, Substrate, SubstrateConfig};
    use std::io::Cursor;
    use std::path::PathBuf;

    /// `from_env` is now infallible (subprocess inherits ambient auth).
    /// Kept fallible for source-compat with T2.2 callers.
    #[test]
    fn from_env_is_infallible() {
        let r = ClaudeBackend::from_env();
        assert!(r.is_ok());
    }

    /// Even with the feature enabled, refuses to make a real call
    /// unless `MAW_BENCH_ALLOW_REAL_LLM=1` (cost guard). The test
    /// asserts the guard ONLY when the env var happens to be unset
    /// in the test runner; if it's set (e.g. the operator is running
    /// the smoke test in the same `cargo test` invocation), we
    /// gracefully skip — touching env from a unit test would require
    /// `unsafe { set_var }` which the workspace forbids
    /// (`unsafe_code = "forbid"`). The smoke test exercises the
    /// opt-in path end-to-end.
    #[test]
    fn run_refuses_without_explicit_opt_in() {
        // Skip if the operator is mid-smoke-test (env already set).
        if std::env::var("MAW_BENCH_ALLOW_REAL_LLM").as_deref() == Ok("1") {
            eprintln!("MAW_BENCH_ALLOW_REAL_LLM=1 in env; skipping guard test");
            return;
        }
        let mut backend = ClaudeBackend::new();
        let agent_cfg = AgentConfig::default();
        let mut substrate = NoopSubstrate::new();
        let handle = substrate
            .setup(&SubstrateConfig {
                seed: 1,
                base_git_time: 0,
                debug_dir: None,
            })
            .expect("noop");
        let result = backend.run("p", &agent_cfg, &handle);
        match result {
            Err(AgentError::Config(msg)) => assert!(msg.contains("MAW_BENCH_ALLOW_REAL_LLM")),
            other => panic!("expected Config error; got {other:?}"),
        }
    }

    /// argv contains the canonical SP3 flags (with the documented
    /// drift applied for CC 2.1.150).
    #[test]
    fn build_argv_carries_canonical_flags() {
        let c = AgentConfig {
            model: "sonnet".into(),
            max_turns: 40,
            max_budget_usd: 2.00,
            temperature: 1.0,
            permission_mode: "bypassPermissions".into(),
        };
        let argv = build_argv(&c, &PathBuf::from("/tmp/ws"));
        // Anchor flags that MUST be present for the SP3 invocation
        // to be reproducible by a human typing the command.
        assert!(argv.iter().any(|a| a == "-p"));
        assert!(argv.iter().any(|a| a == "--output-format"));
        assert!(argv.iter().any(|a| a == "stream-json"));
        assert!(argv.iter().any(|a| a == "--verbose"));
        assert!(argv.iter().any(|a| a == "--model"));
        assert!(argv.iter().any(|a| a == "sonnet"));
        assert!(argv.iter().any(|a| a == "--permission-mode"));
        assert!(argv.iter().any(|a| a == "bypassPermissions"));
        assert!(argv.iter().any(|a| a == "--allowed-tools"));
        assert!(argv.iter().any(|a| a == "Bash,Read,Edit,Write,Glob,Grep"));
        assert!(argv.iter().any(|a| a == "--add-dir"));
        assert!(argv.iter().any(|a| a == "/tmp/ws"));
        assert!(argv.iter().any(|a| a == "--max-budget-usd"));
        assert!(argv.iter().any(|a| a == "--no-session-persistence"));
        // Drift: `--max-turns` and `--temperature` are deliberately
        // absent (CC 2.1.150 has neither). See module doc.
        assert!(!argv.iter().any(|a| a == "--max-turns"));
        assert!(!argv.iter().any(|a| a == "--temperature"));
    }

    /// Stream parser handles a happy-path single-turn no-tool-call
    /// stream (the simplest envelope).
    #[test]
    fn parse_stream_single_turn_text_only() {
        let stream = "{\"type\":\"system\",\"subtype\":\"init\",\"cwd\":\"/tmp\"}\n\
                      {\"type\":\"assistant\",\"message\":{\"id\":\"msg_A\",\
                          \"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
                      {\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
                          \"total_cost_usd\":0.0123,\"num_turns\":1,\"stop_reason\":\"end_turn\"}\n";
        let p = parse_stream(Cursor::new(stream), 40);
        assert_eq!(p.turns.len(), 1);
        assert_eq!(p.turns[0].index, 1);
        assert_eq!(p.turns[0].reply_text, "hello");
        assert!(p.turns[0].tool_calls.is_empty());
        assert_eq!(p.cost_usd, Some(0.0123));
        assert!(p.done);
        assert_eq!(p.stop_reason, "end_turn");
    }

    /// Stream parser fuses two `assistant` events with the same
    /// `message.id` into one turn (the thinking + tool_use case).
    #[test]
    fn parse_stream_fuses_same_message_id_into_one_turn() {
        let stream = "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_A\",\
                          \"content\":[{\"type\":\"thinking\",\"thinking\":\"plan\"}]}}\n\
                      {\"type\":\"assistant\",\"message\":{\"id\":\"msg_A\",\
                          \"content\":[{\"type\":\"tool_use\",\"id\":\"tu_1\",\
                              \"name\":\"Bash\",\"input\":{\"command\":\"echo hi\"}}]}}\n\
                      {\"type\":\"user\",\"message\":{\"content\":[\
                          {\"type\":\"tool_result\",\"tool_use_id\":\"tu_1\",\"content\":\"hi\"}]}}\n\
                      {\"type\":\"assistant\",\"message\":{\"id\":\"msg_B\",\
                          \"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n\
                      {\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
                          \"total_cost_usd\":0.05,\"num_turns\":2,\"stop_reason\":\"end_turn\"}\n";
        let p = parse_stream(Cursor::new(stream), 40);
        assert_eq!(p.turns.len(), 2);
        assert_eq!(p.turns[0].tool_calls.len(), 1);
        assert_eq!(p.turns[0].tool_calls[0].name, "Bash");
        assert!(
            p.turns[0].tool_calls[0].args_json.contains("echo hi"),
            "args_json: {}",
            p.turns[0].tool_calls[0].args_json
        );
        assert_eq!(
            p.turns[0].tool_calls[0].result_truncated.as_deref(),
            Some("hi")
        );
        assert_eq!(p.turns[1].reply_text, "done");
        assert!(p.done);
        assert_eq!(p.cost_usd, Some(0.05));
    }

    /// Stream parser maps `is_error: true` + error subtype onto a
    /// non-done reply with the subtype as the stop_reason.
    #[test]
    fn parse_stream_error_subtype_maps_to_stop_reason() {
        let stream = "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_A\",\
                          \"content\":[{\"type\":\"text\",\"text\":\"start\"}]}}\n\
                      {\"type\":\"result\",\"subtype\":\"error_max_budget_usd\",\
                          \"is_error\":true,\"total_cost_usd\":0.20,\"num_turns\":1,\
                          \"stop_reason\":\"tool_use\"}\n";
        let p = parse_stream(Cursor::new(stream), 40);
        assert!(!p.done);
        assert_eq!(p.stop_reason, "error_max_budget_usd");
        assert_eq!(p.cost_usd, Some(0.20));
    }

    /// Post-stream `max_turns` cap truncates and marks as not-done
    /// (since CC 2.1.150 has no `--max-turns` flag, this is the
    /// harness's contract enforcement).
    #[test]
    fn parse_stream_post_caps_max_turns() {
        let stream = "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_A\",\
                          \"content\":[{\"type\":\"text\",\"text\":\"a\"}]}}\n\
                      {\"type\":\"assistant\",\"message\":{\"id\":\"msg_B\",\
                          \"content\":[{\"type\":\"text\",\"text\":\"b\"}]}}\n\
                      {\"type\":\"assistant\",\"message\":{\"id\":\"msg_C\",\
                          \"content\":[{\"type\":\"text\",\"text\":\"c\"}]}}\n\
                      {\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
                          \"total_cost_usd\":0.10,\"num_turns\":3,\"stop_reason\":\"end_turn\"}\n";
        let p = parse_stream(Cursor::new(stream), 2);
        assert_eq!(p.turns.len(), 2);
        assert!(!p.done, "post-cap overrides envelope's success");
        assert_eq!(p.stop_reason, "max_turns");
    }

    /// Parser tolerates unknown event types, malformed lines, and
    /// stream-events without a terminal `result`.
    #[test]
    fn parse_stream_tolerates_garbage_and_missing_result() {
        let stream = "\n\
            not json at all\n\
            {\"type\":\"future_event\",\"foo\":1}\n\
            {\"type\":\"assistant\",\"message\":{\"id\":\"msg_X\",\
                \"content\":[{\"type\":\"text\",\"text\":\"orphan\"}]}}\n";
        let p = parse_stream(Cursor::new(stream), 40);
        assert_eq!(p.turns.len(), 1);
        assert_eq!(p.turns[0].reply_text, "orphan");
        assert!(!p.done);
        assert_eq!(p.stop_reason, "stream_ended");
        assert!(p.cost_usd.is_none());
    }

    /// Tool result content can arrive as a list of blocks (not just
    /// a string). Parser handles both shapes.
    #[test]
    fn parse_stream_tool_result_block_list_shape() {
        let stream = "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_A\",\
                          \"content\":[{\"type\":\"tool_use\",\"id\":\"tu_1\",\
                              \"name\":\"Read\",\"input\":{\"file\":\"a.txt\"}}]}}\n\
                      {\"type\":\"user\",\"message\":{\"content\":[\
                          {\"type\":\"tool_result\",\"tool_use_id\":\"tu_1\",\
                              \"content\":[{\"type\":\"text\",\"text\":\"line1\"},\
                                           {\"type\":\"text\",\"text\":\"line2\"}]}]}}\n\
                      {\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
                          \"total_cost_usd\":0.01,\"num_turns\":1,\"stop_reason\":\"end_turn\"}\n";
        let p = parse_stream(Cursor::new(stream), 40);
        assert_eq!(p.turns.len(), 1);
        let tc = &p.turns[0].tool_calls[0];
        assert_eq!(tc.result_truncated.as_deref(), Some("line1\nline2"));
    }

    /// Truncator bounds bytes and preserves UTF-8 boundaries.
    #[test]
    fn truncate_result_bounds_and_marks() {
        let small = "abc".repeat(10);
        assert_eq!(truncate_result(&small), small);
        let big = "x".repeat(TOOL_RESULT_TRUNCATE_BYTES + 100);
        let t = truncate_result(&big);
        assert!(t.starts_with("xxxx"));
        assert!(t.ends_with("…[truncated]"));
        // UTF-8 safe: include a multi-byte char near the boundary
        let mut s = "x".repeat(TOOL_RESULT_TRUNCATE_BYTES - 1);
        s.push('é'); // 2 bytes — would land mid-boundary at TOOL_RESULT_TRUNCATE_BYTES
        s.push_str(&"y".repeat(100));
        let t = truncate_result(&s);
        assert!(t.is_char_boundary(0));
        // (any successful char-boundary truncation is acceptable; no panic == pass)
        assert!(t.ends_with("…[truncated]"));
    }
}
