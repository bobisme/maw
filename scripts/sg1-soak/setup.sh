#!/usr/bin/env bash
# One-time setup for the local SG1 DST soak accrual campaign (bn-2yzz).
#
# Pins a FROZEN copy of the prebuilt release sg1_dst test binary into a state
# dir outside the repo, so your ongoing dev/rebuilds never perturb the running
# campaign. Re-run only to (re)pin after a deliberate harness change — that
# resets accrual (campaign §2 stop-condition 3).
#
# Env overrides: MAW_REPO, SG1_SOAK_STATE, SG1_SOAK_STEPS, SG1_SOAK_SLOT_SEEDS,
#                SG1_SOAK_PARALLEL, SG1_SOAK_TARGET, SG1_SOAK_BASE_START,
#                SG1_SOAK_FORCE=1 (allow re-pin mid-campaign without reset).
set -euo pipefail

REPO="${MAW_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
STATE="${SG1_SOAK_STATE:-$HOME/.local/state/maw-sg1-soak}"
STEPS="${SG1_SOAK_STEPS:-64}"
SLOT_SEEDS="${SG1_SOAK_SLOT_SEEDS:-500}"
PARALLEL="${SG1_SOAK_PARALLEL:-2}"
TARGET="${SG1_SOAK_TARGET:-100000000}"           # 1e8 — v1.0 release-gate floor
BASE_START="${SG1_SOAK_BASE_START:-4294967296}"  # 0x1_0000_0000, clear of corpus/canonical seeds

mkdir -p "$STATE"

BIN=$(find "$REPO/target/release/deps" -maxdepth 1 -type f -executable -name 'sg1_dst-*' \
        -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)
if [ -z "$BIN" ]; then
  echo "ERROR: no release sg1_dst binary found under $REPO/target/release/deps/" >&2
  echo "Build it: cargo test --release -p maw-assurance --features oracles --test sg1_dst --no-run" >&2
  exit 1
fi

NEW_BINSHA=$(sha256sum "$BIN" | cut -d' ' -f1)
CUM=0; [ -f "$STATE/cumulative" ] && CUM=$(cat "$STATE/cumulative")
if [ -f "$STATE/config.env" ] && [ "${CUM:-0}" -gt 0 ] && [ "${SG1_SOAK_FORCE:-0}" != "1" ]; then
  OLD_BINSHA=$(grep -oP '^PINNED_BIN_SHA256=\K.*' "$STATE/config.env" || true)
  if [ "$NEW_BINSHA" != "$OLD_BINSHA" ]; then
    echo "REFUSING: harness binary changed but cumulative=$CUM (>0)." >&2
    echo "A surface change resets accrual (campaign §2). To start a FRESH campaign:" >&2
    echo "    rm -rf '$STATE' && $0" >&2
    echo "Or to force re-pin and KEEP the counter (only if the change is provably" >&2
    echo "behaviour-neutral): SG1_SOAK_FORCE=1 $0" >&2
    exit 1
  fi
fi

cp -f "$BIN" "$STATE/sg1_dst.pinned"
chmod +x "$STATE/sg1_dst.pinned"
SRC_SHA=$(git -C "$REPO" rev-parse HEAD)

cat > "$STATE/config.env" <<EOF
MAW_REPO=$REPO
STEPS=$STEPS
SLOT_SEEDS=$SLOT_SEEDS
PARALLEL=$PARALLEL
TARGET_OPSTEPS=$TARGET
PINNED_SRC=$BIN
PINNED_SRC_SHA=$SRC_SHA
PINNED_BIN_SHA256=$NEW_BINSHA
PINNED_AT=$(date -uIs)
EOF

[ -f "$STATE/cursor" ]     || echo "$BASE_START" > "$STATE/cursor"
[ -f "$STATE/cumulative" ] || echo 0 > "$STATE/cumulative"
touch "$STATE/ledger.jsonl"
rm -f "$STATE/DONE" "$STATE/STOP"

echo "SG1 soak campaign pinned:"
echo "  state:    $STATE"
echo "  binary:   $STATE/sg1_dst.pinned   (from $BIN @ $(git -C "$REPO" rev-parse --short HEAD))"
echo "  params:   STEPS=$STEPS SLOT_SEEDS=$SLOT_SEEDS PARALLEL=$PARALLEL TARGET=$TARGET"
echo "  cursor:   $(cat "$STATE/cursor")   cumulative: $(cat "$STATE/cumulative")"
echo
echo "Next: install the cron (see scripts/sg1-soak/README.md), or run one slot now:"
echo "    $(dirname "${BASH_SOURCE[0]}")/slot.sh"
