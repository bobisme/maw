#!/usr/bin/env bash
# Run ONE SG1 DST soak slot, low-priority, and accrue toward the 1e8 floor.
# Invoked by cron every few minutes; flock bounds concurrency to PARALLEL.
# Safe to run by hand. Exits 0 quickly if all parallel slots are busy / DONE / STOP.
#
# A non-zero exit from the pinned binary = an Oracle A/B violation (the gate
# firing). That HALTS the campaign (writes STOP + a violation log) — it is a
# Prime-Invariant finding to investigate, not a flake to retry.
#
# ONE exception (bn-30v6e): exit 75 (EX_TEMPFAIL) together with a
# `[sg1] INFRA-FAILURE:` marker line means the HOST ran out of disk quota /
# space / file descriptors — not an oracle verdict. That slot is logged to
# infra/, recorded in ledger.jsonl as status "infra" (its seed range is
# consumed, never reused, and accrues NO op-steps), and the campaign keeps
# going. INFRA_HALT_AFTER (default 3) consecutive infra slots write STOP with
# an "INFRA-HALT" message so a full disk still gets human attention.
# Anything else non-zero — including exit 75 WITHOUT the marker, or the marker
# with any other exit code — is still a violation (fail closed).
#
# bn-25pac: a seed the oracles could not actually judge (plan step failed to
# apply, unreadable state, oracle tooling error, vacuous evidence) is a
# HarnessError and fails the run like a violation. Clean rows record the
# evidence totals (oracle_a_checks, witnesses, harness_errors).
set -uo pipefail

INFRA_EXIT_CODE=75
INFRA_MARKER='[sg1] INFRA-FAILURE:'

STATE="${SG1_SOAK_STATE:-$HOME/.local/state/maw-sg1-soak}"
[ -f "$STATE/config.env" ] || { echo "no $STATE/config.env — run scripts/sg1-soak/setup.sh first" >&2; exit 1; }
# shellcheck disable=SC1091
source "$STATE/config.env"

[ -e "$STATE/DONE" ] && exit 0
[ -e "$STATE/STOP" ] && exit 0
[ -x "$STATE/sg1_dst.pinned" ] || { echo "pinned binary missing — re-run setup.sh" >&2; exit 1; }
INFRA_HALT_AFTER="${INFRA_HALT_AFTER:-3}"

# Temp repos go to TMPDIR. Keep them OFF /tmp: on this box /tmp is a
# per-user-quota tmpfs, and filling it halted the pre.6 campaign (bn-30v6e).
# The systemd unit sets TMPDIR=/var/tmp/maw-sg1-soak; this is the default for
# hand runs.
export TMPDIR="${TMPDIR:-/var/tmp/maw-sg1-soak}"
mkdir -p "$TMPDIR" || { echo "cannot create TMPDIR=$TMPDIR" >&2; exit 1; }

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
end_seeds=$(grep -oP 'nightly soak end: seeds=\K[0-9]+' <<<"$out" | head -1)
# bn-2qamr: accrue ONLY what this slot allocated, and only if the harness
# provably ran it. The begin line must echo back SLOT_SEEDS / STEPS / base (an
# env value the harness failed to parse silently falls back to its default —
# the op-step count would be wrong and the seeds could overlap other slots).
# The harness also replays CANONICAL_BN_CM63_SEED first in EVERY slot: the
# same deterministic plan each time, so it adds no new op-steps and is not
# counted (range_clean = clean minus the seeds outside the allocated range).
begin_line=$(grep -m1 'nightly soak begin:' <<<"$out" || true)
b_seeds=$(grep -oP '\bseeds=\K[0-9]+' <<<"$begin_line" | head -1)
b_steps=$(grep -oP '\bsteps=\K[0-9]+' <<<"$begin_line" | head -1)
b_base=$(grep -oP '\bbase_seed=0x\K[0-9a-fA-F]+' <<<"$begin_line" | head -1)
range_clean=""
if [ -n "$clean" ] && [ -n "$end_seeds" ] && [ "$clean" = "$end_seeds" ] \
   && [ "$b_seeds" = "$SLOT_SEEDS" ] && [ "$b_steps" = "$STEPS" ] \
   && [ "${b_base,,}" = "$(printf '%016x' "$base")" ] \
   && [ "$end_seeds" -ge "$SLOT_SEEDS" ] && [ "$end_seeds" -le $(( SLOT_SEEDS + 1 )) ]; then
  range_clean=$SLOT_SEEDS
fi
# bn-25pac evidence totals from the same summary line (JSON null when the
# pinned binary predates them). Recorded in the ledger so a campaign's
# accrual can be audited against what the oracles actually judged.
evidence() {
  local v
  v=$(grep -m1 'nightly soak end:' <<<"$out" | grep -oP "\b$1=\K[0-9]+" | head -1)
  printf '%s' "${v:-null}"
}
ev_checks=$(evidence oracle_a_checks)
ev_witnesses=$(evidence witnesses)
ev_harness=$(evidence harness_errors)
infra_line=$(grep -m1 '^\[sg1\] INFRA-FAILURE:' <<<"$out" || true)
# Belt and braces: any sign of an oracle violation in the output vetoes the
# infra path (the harness already refuses to exit 75 after a violation).
viol_seen=""
grep -qE 'violations=[1-9]|harness_errors=[1-9]|HARNESS-ERROR|soak FAILED|random budget FAILED' <<<"$out" && viol_seen=1

# --- infrastructure failure: record, do not accrue, do not halt (bounded) ---
# Requires the dedicated exit code AND the marker line AND no violation sign
# (fail closed: everything else falls through to the violation path below).
if [ "$rc" -eq "$INFRA_EXIT_CODE" ] && [ -n "$infra_line" ] && [ -z "$viol_seen" ]; then
  mkdir -p "$STATE/infra"
  log="$STATE/infra/base-${base}-${ts//[:]/-}.log"
  printf '%s\n' "$out" > "$log"
  # JSON-safe one-line reason (drop quotes, backslashes, control chars).
  reason=$(printf '%s' "${infra_line#"$INFRA_MARKER"}" | tr -d '"\\' | tr -d '\000-\037' | sed 's/^ *//')
  printf '{"ts":"%s","end_ts":"%s","base_seed":%s,"slot_seeds":%s,"steps":%s,"op_steps":0,"status":"infra","rc":%s,"reason":"%s","log":"%s"}\n' \
    "$ts" "$end_ts" "$base" "$SLOT_SEEDS" "$STEPS" "$rc" "$reason" "$log" >> "$STATE/ledger.jsonl"

  exec {tf}>"$STATE/total.lock"; flock "$tf"
  streak=$(( $(cat "$STATE/infra_consecutive" 2>/dev/null || echo 0) + 1 ))
  echo "$streak" > "$STATE/infra_consecutive"
  flock -u "$tf"; exec {tf}>&-

  echo "SG1 soak slot hit an INFRASTRUCTURE failure at base_seed=$base (rc=$rc): $reason" >&2
  echo "  not an Oracle verdict; no op-steps accrued; seed range recorded as infra. log: $log" >&2
  if [ "$streak" -ge "$INFRA_HALT_AFTER" ]; then
    # bn-2qamr: noclobber (O_EXCL) — never overwrite a STOP a PARALLEL slot
    # wrote (e.g. a VIOLATION): "rm STOP to resume" would then silently resume
    # the campaign past a real finding. A violation still overwrites an
    # INFRA-HALT (it writes without noclobber), so a finding always wins.
    ( set -C
      printf 'INFRA-HALT: %s consecutive infrastructure failures (last: base_seed=%s, %s). Free disk/quota/fds, check TMPDIR=%s, then rm STOP to resume. Last log: %s\n' \
        "$streak" "$base" "$reason" "$TMPDIR" "$log" > "$STATE/STOP" ) 2>/dev/null \
      || echo "  (STOP already present — left as is: $(head -1 "$STATE/STOP" 2>/dev/null))" >&2
    echo "SG1 SOAK INFRA-HALT: $streak consecutive infra failures (limit $INFRA_HALT_AFTER). Campaign STOPped for a human." >&2
    exit 1
  fi
  exit 0
fi

if [ "$rc" -ne 0 ] || [ -z "$clean" ] || [ -z "$range_clean" ]; then
  mkdir -p "$STATE/violations"
  log="$STATE/violations/base-${base}-${ts//[:]/-}.log"
  printf '%s\n' "$out" > "$log"
  printf '{"ts":"%s","base_seed":%s,"slot_seeds":%s,"steps":%s,"status":"VIOLATION_OR_ERROR","rc":%s,"log":"%s"}\n' \
    "$ts" "$base" "$SLOT_SEEDS" "$STEPS" "$rc" "$log" >> "$STATE/ledger.jsonl"
  printf 'VIOLATION: Oracle violation or unclassified error at base_seed=%s (rc=%s). Log: %s\n' \
    "$base" "$rc" "$log" > "$STATE/STOP"
  echo "SG1 SOAK HALTED: violation/error at base_seed=$base (rc=$rc). Campaign STOPped." >&2
  echo "  details: $log" >&2
  echo "  replay:  SG1_SEED=<seed> just sg1-per-commit   (seeds in [$base, $((base+SLOT_SEEDS))))" >&2
  exit 1
fi

op=$(( range_clean * STEPS ))
printf '{"ts":"%s","end_ts":"%s","base_seed":%s,"slot_seeds":%s,"steps":%s,"clean":%s,"range_clean":%s,"op_steps":%s,"oracle_a_checks":%s,"witnesses":%s,"harness_errors":%s,"status":"clean"}\n' \
  "$ts" "$end_ts" "$base" "$SLOT_SEEDS" "$STEPS" "$clean" "$range_clean" "$op" "$ev_checks" "$ev_witnesses" "$ev_harness" >> "$STATE/ledger.jsonl"

exec {tf}>"$STATE/total.lock"; flock "$tf"
cum=$(( $(cat "$STATE/cumulative") + op )); echo "$cum" > "$STATE/cumulative"
echo 0 > "$STATE/infra_consecutive"   # a clean slot ends an infra streak
flock -u "$tf"; exec {tf}>&-

# bn-2qamr: never declare DONE over a violation. A PARALLEL slot may have
# written STOP (or a VIOLATION_OR_ERROR row) while this slot was running.
if [ "$cum" -ge "$TARGET_OPSTEPS" ] && [ ! -e "$STATE/STOP" ] \
   && ! grep -q '"status":"VIOLATION_OR_ERROR"' "$STATE/ledger.jsonl"; then
  touch "$STATE/DONE"
  echo "SG1 SOAK DONE: cumulative=$cum >= $TARGET_OPSTEPS (1e8 floor reached, 0 violations)." >&2
fi
exit 0
