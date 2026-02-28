#!/usr/bin/env bash
# Merge performance baseline benchmark for Phase 0 (bn-3bc5).
#
# Measures wall time for `maw ws merge` across three scenarios:
#   S1: 1 workspace, ~10 files changed  (small)
#   S2: 1 workspace, ~100 files changed (large)
#   S3: 3 workspaces, ~10 files each    (multi)
#
# Each scenario is run N_RUNS times and min/median/max are reported.
#
# Usage:
#   bash notes/assurance/perf-merge-bench.sh
#
# Environment:
#   N_RUNS  number of repetitions per scenario (default: 5)

set -euo pipefail

N_RUNS="${N_RUNS:-5}"
MAW="$(command -v maw)"
echo "maw binary: $MAW"
echo "maw version: $("$MAW" --version)"
echo "runs per scenario: $N_RUNS"
echo ""

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Create a fresh manifold repo in a temp directory.
# Sets REPO_DIR to the path.
setup_repo() {
  REPO_DIR="$(mktemp -d /tmp/maw-perf-bench.XXXXXX)"
  (
    cd "$REPO_DIR"
    git init -b main >/dev/null 2>&1
    git config user.name "Bench"
    git config user.email "bench@localhost"
    git config commit.gpgsign false
    git config tag.gpgsign false
    # Seed a minimal file so epoch0 is non-empty
    echo "# bench repo" > README.md
    git add -A
    git commit -m "chore: seed" >/dev/null 2>&1
    "$MAW" init >/dev/null 2>&1
  )
}

cleanup_repo() {
  if [[ -n "${REPO_DIR:-}" && -d "${REPO_DIR:-}" ]]; then
    rm -rf "$REPO_DIR"
  fi
}

# Add N files to a workspace.
# Usage: add_files <workspace> <count> [prefix]
add_files() {
  local ws="$1"
  local count="$2"
  local prefix="${3:-file}"
  local ws_path="$REPO_DIR/ws/$ws"
  mkdir -p "$ws_path/src"
  for i in $(seq 1 "$count"); do
    echo "content of $prefix $i in workspace $ws" > "$ws_path/src/${prefix}_${i}.txt"
  done
}

# Time a single maw ws merge and return wall-clock milliseconds.
# Usage: time_merge <workspace_names...>
time_merge() {
  local start end elapsed_ms
  start=$(date +%s%N)
  "$MAW" ws merge "$@" --destroy >/dev/null 2>&1
  end=$(date +%s%N)
  elapsed_ms=$(( (end - start) / 1000000 ))
  echo "$elapsed_ms"
}

# Compute median from a newline-separated list of numbers on stdin.
median() {
  sort -n | awk '{ a[NR] = $1 } END { if (NR%2) print a[(NR+1)/2]; else print int((a[NR/2]+a[NR/2+1])/2) }'
}

# ---------------------------------------------------------------------------
# Scenario S1: 1 workspace, ~10 files
# ---------------------------------------------------------------------------

run_s1() {
  local times=()
  for _ in $(seq 1 "$N_RUNS"); do
    setup_repo
    (cd "$REPO_DIR" && "$MAW" ws create worker >/dev/null 2>&1)
    add_files worker 10
    local ms
    ms=$(cd "$REPO_DIR" && time_merge worker)
    times+=("$ms")
    cleanup_repo
  done

  local min max med
  min=$(printf '%s\n' "${times[@]}" | sort -n | head -1)
  max=$(printf '%s\n' "${times[@]}" | sort -n | tail -1)
  med=$(printf '%s\n' "${times[@]}" | median)
  echo "S1 (1 ws, 10 files): min=${min}ms  median=${med}ms  max=${max}ms  [${times[*]}]"
  S1_MIN="$min"; S1_MED="$med"; S1_MAX="$max"; S1_RAW="${times[*]}"
}

# ---------------------------------------------------------------------------
# Scenario S2: 1 workspace, ~100 files
# ---------------------------------------------------------------------------

run_s2() {
  local times=()
  for _ in $(seq 1 "$N_RUNS"); do
    setup_repo
    (cd "$REPO_DIR" && "$MAW" ws create worker >/dev/null 2>&1)
    add_files worker 100
    local ms
    ms=$(cd "$REPO_DIR" && time_merge worker)
    times+=("$ms")
    cleanup_repo
  done

  local min max med
  min=$(printf '%s\n' "${times[@]}" | sort -n | head -1)
  max=$(printf '%s\n' "${times[@]}" | sort -n | tail -1)
  med=$(printf '%s\n' "${times[@]}" | median)
  echo "S2 (1 ws, 100 files): min=${min}ms  median=${med}ms  max=${max}ms  [${times[*]}]"
  S2_MIN="$min"; S2_MED="$med"; S2_MAX="$max"; S2_RAW="${times[*]}"
}

# ---------------------------------------------------------------------------
# Scenario S3: 3 workspaces, ~10 files each
# ---------------------------------------------------------------------------

run_s3() {
  local times=()
  for _ in $(seq 1 "$N_RUNS"); do
    setup_repo
    (cd "$REPO_DIR" && "$MAW" ws create alice >/dev/null 2>&1)
    (cd "$REPO_DIR" && "$MAW" ws create bob >/dev/null 2>&1)
    (cd "$REPO_DIR" && "$MAW" ws create carol >/dev/null 2>&1)
    add_files alice 10 "alice"
    add_files bob 10 "bob"
    add_files carol 10 "carol"
    local ms
    ms=$(cd "$REPO_DIR" && time_merge alice bob carol)
    times+=("$ms")
    cleanup_repo
  done

  local min max med
  min=$(printf '%s\n' "${times[@]}" | sort -n | head -1)
  max=$(printf '%s\n' "${times[@]}" | sort -n | tail -1)
  med=$(printf '%s\n' "${times[@]}" | median)
  echo "S3 (3 ws, 10 files each): min=${min}ms  median=${med}ms  max=${max}ms  [${times[*]}]"
  S3_MIN="$min"; S3_MED="$med"; S3_MAX="$max"; S3_RAW="${times[*]}"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

echo "=== Merge Performance Baseline ==="
echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "Host: $(uname -srm)"
echo ""

run_s1
run_s2
run_s3

echo ""
echo "=== Summary (all times in ms) ==="
echo ""
printf '| %-30s | %8s | %8s | %8s |\n' "Scenario" "Min" "Median" "Max"
printf '| %-30s | %8s | %8s | %8s |\n' "------------------------------" "--------" "--------" "--------"
printf '| %-30s | %8s | %8s | %8s |\n' "S1: 1 ws, 10 files" "$S1_MIN" "$S1_MED" "$S1_MAX"
printf '| %-30s | %8s | %8s | %8s |\n' "S2: 1 ws, 100 files" "$S2_MIN" "$S2_MED" "$S2_MAX"
printf '| %-30s | %8s | %8s | %8s |\n' "S3: 3 ws, 10 files each" "$S3_MIN" "$S3_MED" "$S3_MAX"
echo ""
echo "Raw samples:"
echo "  S1: $S1_RAW"
echo "  S2: $S2_RAW"
echo "  S3: $S3_RAW"
