#!/usr/bin/env bash
# Report SG1 DST soak accrual progress toward the 1e8 floor.
set -uo pipefail
STATE="${SG1_SOAK_STATE:-$HOME/.local/state/maw-sg1-soak}"
[ -f "$STATE/config.env" ] || { echo "no campaign at $STATE — run setup.sh" >&2; exit 1; }
# shellcheck disable=SC1091
source "$STATE/config.env"

cum=$(cat "$STATE/cumulative" 2>/dev/null || echo 0)
ledger="$STATE/ledger.jsonl"
clean_slots=$(grep -c '"status":"clean"' "$ledger" 2>/dev/null); clean_slots=${clean_slots:-0}
viol=$(grep -c '"status":"VIOLATION_OR_ERROR"' "$ledger" 2>/dev/null); viol=${viol:-0}

first_ts=$(grep -oP '"ts":"\K[^"]+' "$ledger" 2>/dev/null | head -1)
rate=""; eta=""
if [ -n "$first_ts" ] && [ "$cum" -gt 0 ]; then
  now=$(date +%s); t0=$(date -d "$first_ts" +%s 2>/dev/null || echo "$now")
  span=$(( now - t0 )); [ "$span" -lt 1 ] && span=1
  rate=$(awk -v c="$cum" -v s="$span" 'BEGIN{printf "%.1f", c/s}')
  rem=$(( TARGET_OPSTEPS - cum ))
  if [ "$rem" -gt 0 ]; then
    eta=$(awk -v r="$rem" -v rt="$rate" 'BEGIN{ if(rt>0) printf "%.1f days", (r/rt)/86400; else print "n/a" }')
  else eta="reached"; fi
fi

pct=$(awk -v c="$cum" -v t="$TARGET_OPSTEPS" 'BEGIN{printf "%.2f", 100*c/t}')
# One-sided ~95% Wilson upper bound at 0 observed events ≈ z²/N (z=1.96 ⇒ 3.8416).
wilson=$(awk -v n="$cum" 'BEGIN{ if(n>0) printf "%.3e", 3.8416/n; else print "n/a" }')

echo "SG1 DST soak — accrual toward v1.0 floor (bn-2yzz)"
echo "  state dir:          $STATE"
echo "  pinned harness:     ${PINNED_SRC_SHA:0:12}  (binary frozen ${PINNED_AT:-?})"
echo "  cumulative op-steps: $cum / $TARGET_OPSTEPS   (${pct}%)"
echo "  clean slots:        $clean_slots   (SLOT_SEEDS=$SLOT_SEEDS x STEPS=$STEPS, PARALLEL=$PARALLEL)"
echo "  Wilson 95% UB:      $wilson    (gate requires <= 3.84e-8 at 1e8, 0 violations)"
[ -n "$rate" ] && echo "  observed rate:      $rate op-steps/sec wall   (ETA to 1e8: ${eta:-n/a})"
echo "  violations:         $viol"
if   [ -e "$STATE/DONE" ]; then echo "  STATUS: ✅ DONE — 1e8 reached with 0 violations. Fill notes/sg1-soak-campaign.md §7.1/§8 from the ledger."
elif [ -e "$STATE/STOP" ]; then echo "  STATUS: ⛔ STOPPED — see $STATE/violations/ (Oracle violation = a finding; shrink → fix → reset → restart)."
else                            echo "  STATUS: ▶ accruing (cron active if installed; or run scripts/sg1-soak/slot.sh)."
fi
