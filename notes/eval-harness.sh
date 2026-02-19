#!/usr/bin/env bash
# Manifold v2 agent eval harness
#
# Creates a fresh repo in /tmp, prepares a sample Rust project, captures maw
# help text, runs an external agent command, and scores UX friction metrics.
#
# Agent command placeholders:
#   {PROMPT_FILE} -> absolute path to generated prompt file
#   {REPO_DIR}    -> absolute path to eval repo root
#   {RUN_DIR}     -> absolute path to run scratch directory
#
# Example:
#   notes/eval-harness.sh \
#     --agent-cmd 'claude --print --input-file {PROMPT_FILE}'

set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
SCENARIO="basic-lifecycle"
DRY_RUN=0
KEEP_TMP=0
AGENT_CMD="${CLAUDE_EVAL_CMD:-}"
RESULTS_DIR="${EVAL_RESULTS_DIR:-/tmp/manifold-eval-results}"
RUN_ID="${EVAL_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"

usage() {
  cat <<'EOF'
Manifold eval harness (Claude/agent UX)

Usage:
  notes/eval-harness.sh [options]

Options:
  --scenario <name>       Scenario ID (default: basic-lifecycle)
  --agent-cmd <command>   Command used to run the agent.
                          Supports placeholders: {PROMPT_FILE}, {REPO_DIR}, {RUN_DIR}
  --results-dir <dir>     Where JSON results are written (default: /tmp/manifold-eval-results)
  --run-id <id>           Stable run id (default: UTC timestamp + PID)
  --dry-run               Setup + prompt generation only (do not execute agent)
  --keep-tmp              Keep /tmp run directory after completion
  -h, --help              Show this help

Scenarios:
  basic-lifecycle         Create workspace -> edit file -> merge --destroy
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --scenario)
      SCENARIO="$2"
      shift 2
      ;;
    --agent-cmd)
      AGENT_CMD="$2"
      shift 2
      ;;
    --results-dir)
      RESULTS_DIR="$2"
      shift 2
      ;;
    --run-id)
      RUN_ID="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --keep-tmp)
      KEEP_TMP=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$SCENARIO" != "basic-lifecycle" ]]; then
  echo "ERROR: unsupported scenario '$SCENARIO'" >&2
  echo "To fix: use --scenario basic-lifecycle" >&2
  exit 2
fi

mkdir -p "$RESULTS_DIR"
RUN_DIR="$(mktemp -d "/tmp/manifold-eval-${RUN_ID}.XXXX")"
REPO_DIR="$RUN_DIR/repo"
PROMPT_FILE="$RUN_DIR/prompt.txt"
HELP_FILE="$RUN_DIR/maw-help.txt"
TRANSCRIPT_FILE="$RUN_DIR/agent-transcript.log"
RESULT_JSON="$RESULTS_DIR/$RUN_ID.json"

cleanup() {
  if [[ "$KEEP_TMP" -eq 1 ]]; then
    echo "IMPORTANT: keeping run dir: $RUN_DIR"
  else
    rm -rf "$RUN_DIR"
  fi
}
trap cleanup EXIT

setup_repo() {
  mkdir -p "$REPO_DIR/src"

  (
    cd "$REPO_DIR"
    git init -b main >/dev/null
    git config user.name "Eval Bot"
    git config user.email "eval@manifold.local"
    git config commit.gpgsign false
    git config tag.gpgsign false

    cat > Cargo.toml <<'TOML'
[package]
name = "agent-eval"
version = "0.1.0"
edition = "2021"

[dependencies]
TOML

    cat > src/main.rs <<'RUST'
fn main() {
    println!("hello from eval");
}
RUST

    cat > src/lib.rs <<'RUST'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
RUST

    git add -A
    git commit -m "chore: seed sample rust project" >/dev/null

    maw init >/dev/null
  )
}

capture_help() {
  {
    echo "# maw --help"
    maw --help
    echo
    echo "# maw ws --help"
    maw ws --help
    echo
    echo "# maw exec --help"
    maw exec --help
  } > "$HELP_FILE"
}

build_prompt() {
  cat > "$PROMPT_FILE" <<EOF
You are evaluating Manifold v2 agent UX.

Task (scenario: basic-lifecycle):
1. Create a workspace named agent-1.
2. Add a new file src/hello.rs with a function:
     pub fn hello() -> &'static str { "hello" }
3. Merge workspace agent-1 with --destroy.
4. Confirm src/hello.rs exists in ws/default.

Rules:
- Use only maw commands and file operations.
- Use absolute paths for file edits.
- Do not use git/jj directly.

Repository root:
$REPO_DIR

maw help output:
$(cat "$HELP_FILE")
EOF
}

expand_placeholders() {
  local cmd="$1"
  cmd="${cmd//\{PROMPT_FILE\}/$PROMPT_FILE}"
  cmd="${cmd//\{REPO_DIR\}/$REPO_DIR}"
  cmd="${cmd//\{RUN_DIR\}/$RUN_DIR}"
  printf '%s' "$cmd"
}

run_agent() {
  if [[ -z "$AGENT_CMD" ]]; then
    echo "ERROR: no agent command provided" >&2
    echo "To fix: pass --agent-cmd '<command with {PROMPT_FILE}>'" >&2
    echo "Example: --agent-cmd 'claude --print --input-file {PROMPT_FILE}'" >&2
    return 127
  fi

  local expanded
  expanded="$(expand_placeholders "$AGENT_CMD")"

  (
    cd "$REPO_DIR"
    bash -lc "$expanded"
  ) >"$TRANSCRIPT_FILE" 2>&1
}

count_pattern() {
  local pattern="$1"
  local file="$2"
  if [[ -f "$file" ]]; then
    grep -Eci "$pattern" "$file" || true
  else
    echo 0
  fi
}

compute_retry_count() {
  local file="$1"
  if [[ ! -f "$file" ]]; then
    echo 0
    return
  fi

  # Approximate retries by repeated identical maw command lines in transcript.
  awk '/(^|[[:space:]])maw[[:space:]]+/ {print $0}' "$file" |
    sed 's/^[[:space:]]*//' |
    sort |
    uniq -c |
    awk '{ if ($1 > 1) retries += ($1 - 1) } END { print retries + 0 }'
}

goal_achieved() {
  [[ -f "$REPO_DIR/ws/default/src/hello.rs" ]]
}

score_run() {
  local goal="$1"
  local errors="$2"
  local retries="$3"
  local confusion="$4"

  if [[ "$goal" -eq 1 && "$errors" -eq 0 && "$retries" -eq 0 && "$confusion" -eq 0 ]]; then
    echo 1
  elif [[ "$goal" -eq 1 && "$errors" -le 1 && "$retries" -le 1 ]]; then
    echo 2
  elif [[ "$goal" -eq 1 ]]; then
    echo 3
  elif [[ "$errors" -le 2 ]]; then
    echo 4
  else
    echo 5
  fi
}

setup_repo
capture_help
build_prompt

agent_exit=0
run_status="completed"

if [[ "$DRY_RUN" -eq 1 ]]; then
  run_status="dry-run"
  : > "$TRANSCRIPT_FILE"
else
  set +e
  run_agent
  agent_exit=$?
  set -e
  if [[ "$agent_exit" -ne 0 ]]; then
    run_status="agent-failed"
  fi
fi

tool_calls=$(count_pattern '(bash|read|write|edit|grep|glob|ls)\(' "$TRANSCRIPT_FILE")
maw_commands=$(count_pattern '(^|[[:space:]])maw[[:space:]]+' "$TRANSCRIPT_FILE")
errors=$(count_pattern '(exit code [1-9]|\berror\b|\bfailed\b)' "$TRANSCRIPT_FILE")
confusion=$(count_pattern '(not sure|confus|try again|let me retry|backtrack)' "$TRANSCRIPT_FILE")
retries=$(compute_retry_count "$TRANSCRIPT_FILE")

if goal_achieved; then
  goal=1
else
  goal=0
fi

score=$(score_run "$goal" "$errors" "$retries" "$confusion")

jq -n \
  --arg run_id "$RUN_ID" \
  --arg scenario "$SCENARIO" \
  --arg status "$run_status" \
  --arg repo_dir "$REPO_DIR" \
  --arg run_dir "$RUN_DIR" \
  --arg prompt_file "$PROMPT_FILE" \
  --arg help_file "$HELP_FILE" \
  --arg transcript_file "$TRANSCRIPT_FILE" \
  --argjson dry_run "$DRY_RUN" \
  --argjson agent_exit "$agent_exit" \
  --argjson tool_calls "$tool_calls" \
  --argjson maw_commands "$maw_commands" \
  --argjson errors "$errors" \
  --argjson retries "$retries" \
  --argjson confusion "$confusion" \
  --argjson goal_achieved "$goal" \
  --argjson score "$score" \
  '{
    run_id: $run_id,
    scenario: $scenario,
    status: $status,
    dry_run: ($dry_run == 1),
    paths: {
      repo_dir: $repo_dir,
      run_dir: $run_dir,
      prompt_file: $prompt_file,
      help_file: $help_file,
      transcript_file: $transcript_file
    },
    metrics: {
      tool_calls: $tool_calls,
      maw_commands: $maw_commands,
      errors: $errors,
      retries: $retries,
      confusion_markers: $confusion,
      goal_achieved: ($goal_achieved == 1),
      score_1_to_5: $score
    },
    agent: {
      exit_code: $agent_exit
    }
  }' > "$RESULT_JSON"

echo "Eval run complete"
echo "  Run ID: $RUN_ID"
echo "  Scenario: $SCENARIO"
echo "  Status: $run_status"
echo "  Score (1 best, 5 worst): $score"
echo "  Result JSON: $RESULT_JSON"
if [[ "$KEEP_TMP" -eq 1 ]]; then
  echo "  Run dir: $RUN_DIR"
fi
