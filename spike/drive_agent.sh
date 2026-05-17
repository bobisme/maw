#!/usr/bin/env bash
# Fresh-context agent driver for the SP3 ergonomics harness.
#
# Contract: invoke ONE fresh-context coding agent with a single task
# prompt, in a given working dir, non-interactively, capturing the
# machine-readable result envelope (cost, turns, permission denials).
#
# Reproducibility levers (all pinned here so every arm/run is identical):
#   --model        pinned (sonnet) — same model every run
#   --max-turns    hard turn cap so a wedged run terminates deterministically
#   --max-budget-usd hard $ cap (defense-in-depth vs runaway loops)
#   context isolation: scenario lives in /tmp with NO CLAUDE.md/AGENTS.md,
#                  so the agent's context is EXACTLY the task prompt + the
#                  scenario repo. (We DELIBERATELY do NOT use --bare: in this
#                  Claude Code build --bare forces auth to ANTHROPIC_API_KEY
#                  / apiKeyHelper only and refuses the OAuth/keychain session
#                  -> "Not logged in". Context isolation via /tmp placement
#                  is the portable substitute. See feasibility memo §Auth.)
#   --permission-mode bypassPermissions — non-interactive, no prompts
#   --add-dir      scenario workdir only
#   --strict-mcp-config / no .mcp.json in /tmp scenario — no MCP leakage
#
# Emits the final JSON result object on stdout.
set -euo pipefail
WORKDIR="${1:?usage: drive_agent.sh <workdir> <prompt-file>}"
PROMPT_FILE="${2:?usage: drive_agent.sh <workdir> <prompt-file>}"
MODEL="${SP3_MODEL:-sonnet}"
MAX_TURNS="${SP3_MAX_TURNS:-40}"
MAX_BUDGET="${SP3_MAX_BUDGET:-2.00}"

cd "$WORKDIR"
claude -p "$(cat "$PROMPT_FILE")" \
  --output-format json \
  --model "$MODEL" \
  --max-turns "$MAX_TURNS" \
  --max-budget-usd "$MAX_BUDGET" \
  --permission-mode bypassPermissions \
  --add-dir "$WORKDIR"
