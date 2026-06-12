#!/usr/bin/env bash
# Run ONE SG1 DST soak slot, low-priority, and accrue toward the 1e8 floor.
# Invoked by cron every few minutes; flock bounds concurrency to PARALLEL.
# Safe to run by hand. Exits 0 quickly if all parallel slots are busy / DONE / STOP.
#
# A non-zero exit from the pinned binary = an Oracle A/B violation (the gate
# firing). That HALTS the campaign (writes STOP + a violation log) — it is a
# Prime-Invariant finding to investigate, not a flake to retry.
set -uo pipefail

STATE="${SG1_SOAK_STATE:-$HOME/.local/state/maw-sg1-soak}"
[ -f "$STATE/config.env" ] || { echo "no $STATE/config.env — run scripts/sg1-soak/setup.sh first" >&2; exit 1; }
# shellcheck disable=SC1091
source "$STATE/config.env"

[ -e "$STATE/DONE" ] && exit 0
[ -e "$STATE/STOP" ] && exit 0
[ -x "$STATE/sg1_dst.pinned" ] || { echo "pinned binary missing — re-run setup.sh" >&2; exit 1; }

# --- acquire one of PARALLEL slot locks (held for this slot's duration) -------
slot=""
for i in $(seq 1 "$PARALLEL"); do
  exec {lf}>"$STATE/slot-$i.lock"
  if flock -n "$lf"; then slot=$i; break; fi
  exec {lf}>&-
done
[ -n "$slot" ] || exit 0   # all parallel slots busy → nothing to do

# --- atomically allocate a disjoint base-seed range --------------------------
exec {cf}>"$STATE/cursor.lock"
flock "$cf"
base=$(cat "$STATE/cursor")
echo $(( base + SLOT_SEEDS )) > "$STATE/cursor"
flock -u "$cf"; exec {cf}>&-

ts=$(date -uIs)
out=$(SG1_BASE_SEED="$base" SG1_NIGHTLY_SEEDS="$SLOT_SEEDS" SG1_NIGHTLY_STEPS="$STEPS" \
      nice -n 19 ionice -c3 "$STATE/sg1_dst.pinned" \
        sg1_nightly_soak --ignored --exact --nocapture 2>&1)
rc=$?
end_ts=$(date -uIs)
clean=$(grep -oP 'nightly soak end: seeds=[0-9]+ clean=\K[0-9]+' <<<"$out" | head -1)

if [ "$rc" -ne 0 ] || [ -z "$clean" ]; then
  mkdir -p "$STATE/violations"
  log="$STATE/violations/base-${base}-${ts//[:]/-}.log"
  printf '%s\n' "$out" > "$log"
  printf '{"ts":"%s","base_seed":%s,"slot_seeds":%s,"steps":%s,"status":"VIOLATION_OR_ERROR","rc":%s,"log":"%s"}\n' \
    "$ts" "$base" "$SLOT_SEEDS" "$STEPS" "$rc" "$log" >> "$STATE/ledger.jsonl"
  touch "$STATE/STOP"
  echo "SG1 SOAK HALTED: violation/error at base_seed=$base (rc=$rc). Campaign STOPped." >&2
  echo "  details: $log" >&2
  echo "  replay:  SG1_SEED=<seed> just sg1-per-commit   (seeds in [$base, $((base+SLOT_SEEDS))))" >&2
  exit 1
fi

op=$(( clean * STEPS ))
printf '{"ts":"%s","end_ts":"%s","base_seed":%s,"slot_seeds":%s,"steps":%s,"clean":%s,"op_steps":%s,"status":"clean"}\n' \
  "$ts" "$end_ts" "$base" "$SLOT_SEEDS" "$STEPS" "$clean" "$op" >> "$STATE/ledger.jsonl"

exec {tf}>"$STATE/total.lock"; flock "$tf"
cum=$(( $(cat "$STATE/cumulative") + op )); echo "$cum" > "$STATE/cumulative"
flock -u "$tf"; exec {tf}>&-

if [ "$cum" -ge "$TARGET_OPSTEPS" ]; then
  touch "$STATE/DONE"
  echo "SG1 SOAK DONE: cumulative=$cum >= $TARGET_OPSTEPS (1e8 floor reached, 0 violations)." >&2
fi
exit 0
