default:
  just --list

build:
  cargo build --release

fmt:
  cargo fmt --all

fmt-check:
  cargo fmt --all -- --check

clippy:
  cargo clippy --workspace --all-targets -- -D warnings

test:
  cargo test

install:
  cargo install --locked --path crates/maw-cli

# Assurance CI gates

# dst-fast: 256 seeded DST traces per PR (<60s)
dst-fast:
  cargo test --features assurance --test dst_harness -- --ignored dst_g1 dst_g2 dst_g3 dst_g4 dst_determinism

# formal-check: Stateright model checking (pre-release)
formal-check:
  cargo test --features assurance --test formal_model -- --ignored

# contract-drift: doc/code consistency checks
contract-drift:
  cargo test --test contract_drift

# dst-nightly: 10k+ traces (nightly, ~15-30 min)
dst-nightly:
  cargo test --features assurance --test dst_harness -- --ignored dst_nightly --nocapture

# incident-replay: replay failing traces from corpus
incident-replay:
  cargo test --features assurance --test dst_harness -- --ignored incident_replay

# kani-fast: classify_shared_path proofs only (~seconds)
kani-fast:
  cargo kani --no-default-features

# kani-full: all Kani proofs including resolve_entries (~49 min)
kani-full:
  cargo kani --no-default-features --features kani-slow

# All assurance gates combined
check: fmt-check clippy test dst-fast contract-drift

coverage:
  cargo llvm-cov

# Repo-local deterministic simulation workflows
sim-run harness='all' seeds='12' steps='14':
  python3 scripts/dst.py run --harness {{harness}} --seeds {{seeds}} --steps {{steps}}

sim-run-print harness='all' seeds='12' steps='14':
  python3 scripts/dst.py run --harness {{harness}} --seeds {{seeds}} --steps {{steps}} --print-only

sim-replay-workflow seed:
  python3 scripts/dst.py replay --harness workflow --seed {{seed}} --print-only

sim-replay-action seed steps:
  python3 scripts/dst.py replay --harness action --seed {{seed}} --steps {{steps}} --print-only

sim-replay-bundle bundle:
  python3 scripts/dst.py replay --bundle {{bundle}} --print-only

sim-shrink seed max_steps:
  python3 scripts/dst.py shrink --seed {{seed}} --max-steps {{max_steps}} --print-only

sim-shrink-bundle bundle:
  python3 scripts/dst.py shrink --bundle {{bundle}} --print-only

sim-inspect bundle:
  python3 scripts/dst.py inspect {{bundle}}

sim-inspect-latest:
  python3 scripts/dst.py inspect --latest

sim-inspect-latest-harness harness:
  python3 scripts/dst.py inspect --latest --harness {{harness}}

sim-open-latest:
  python3 scripts/dst.py open-latest

sim-open-latest-harness harness:
  python3 scripts/dst.py open-latest --harness {{harness}}

# -----------------------------------------------------------------------------
# SG1 DST gates (bn-1gp4, T1.7) — wire the assurance crate's in-proc +
# shrinker + scenario substrate into CI on top of the legacy `sim-*`
# recipes above. The legacy recipes stay (they still exercise the
# pre-ScenarioPlan harness families in tests/workflow_dst.rs +
# tests/action_workflow_dst.rs); the recipes below are the NEW
# release-gate path per `notes/sg1-dst-architecture.md` §7.
#
# Recipes are prefixed `sg1-` to avoid colliding with the pre-existing
# `dst-fast`/`dst-nightly`/`incident-replay` recipes that belong to the
# legacy `tests/dst_harness.rs` harness.

# sg1-per-commit: bounded SG1 sweep — corpus replay + small random
# budget (default 64 seeds × 32 steps). Hard wall-clock cap 8 min.
# Hard-fails on ANY oracle violation (release-blocking).
# Invoked by `.github/workflows/dst.yml` per-commit job.
sg1-per-commit:
  cargo test -p maw-assurance --features oracles --test sg1_dst -- --nocapture

# sg1-per-commit-smoke: the planted-violation sanity check — must RED.
# CI runs this once per workflow as a self-test to prove "the gate
# actually turns red when something is wrong". If this ever passes
# silently, the gate is broken and v1.0 cannot ship.
sg1-per-commit-smoke:
  #!/usr/bin/env bash
  set -u
  SG1_PLANT_VIOLATION=1 SG1_PER_COMMIT_SEEDS=4 SG1_PER_COMMIT_STEPS=12 \
    cargo test -p maw-assurance --features oracles --test sg1_dst \
    sg1_per_commit_random_budget -- --exact --nocapture
  rc=$?
  if [ $rc -ne 0 ]; then
    echo ""
    echo "ERROR: SG1 planted-violation smoke exited non-zero (rc=$rc)."
    echo "       Either the harness itself is broken OR the plant tripped"
    echo "       AND the test assertion fired — read the output above."
    exit $rc
  fi
  echo ""
  echo "SG1 planted-violation smoke OK (gate tripped on the planted defect)."

# sg1-nightly: long soak — large seed budget (default 100k × 64 steps).
# Failing seeds auto-shrink; bundle.json under DST_ARTIFACT_DIR.
# Invoked by `.github/workflows/dst-soak.yml` cron job.
sg1-nightly:
  cargo test -p maw-assurance --features oracles --test sg1_dst \
    sg1_nightly_soak -- --ignored --nocapture

# sg1-soak-pilot: small-N validation of the soak harness end-to-end
# (T1.9 / bn-6308). Uses the SAME `sg1_nightly_soak` test as the
# full nightly + the published soak campaign, only with `SG1_NIGHTLY_SEEDS`
# turned down to a few-minute local-run budget. The canonical bn-cm63
# seed is exercised on every run (the harness pushes it onto `seeds`
# regardless of N). Use this to verify a green pilot before kicking off
# the multi-day published soak.
#
# Default: 2k seeds × 32 steps ≈ 64k op-steps ≈ few minutes wall in
# release. Override SG1_NIGHTLY_SEEDS / SG1_NIGHTLY_STEPS to retune.
# See notes/sg1-soak-campaign.md for the campaign target + cadence.
sg1-soak-pilot:
  SG1_NIGHTLY_SEEDS=${SG1_NIGHTLY_SEEDS:-2000} \
  SG1_NIGHTLY_STEPS=${SG1_NIGHTLY_STEPS:-32} \
  cargo test --release -p maw-assurance --features oracles --test sg1_dst \
    sg1_nightly_soak -- --ignored --nocapture

# sg1-nightly-faithful: curated faithful (subprocess+SIGKILL) tier —
# replays the bn-cm63 + 2026-02-05 chaos patterns through the real
# `maw` binary built with `--features failpoints`. Today this delegates
# to the existing crash_recovery + destroy_vs_merge_head_ref sweeps;
# T1.5 (bn-263u) will broaden this once the MAW_FP env bridge lands its
# CI seed set. Invoked by `.github/workflows/dst-soak.yml` after sg1-nightly.
sg1-nightly-faithful:
  cargo test --features assurance --test crash_recovery -- --nocapture
  cargo test --features assurance --test destroy_vs_merge_head_ref -- --nocapture

# sg1-faithful-clippy: builds the failpoints variant and runs clippy on
# it so the faithful tier stays buildable. Per bn-2ors, BOTH the default
# (no failpoints) AND the failpoints clippy passes must be -D warnings
# clean. Invoked by `.github/workflows/dst-faithful.yml`.
sg1-faithful-clippy:
  cargo clippy --workspace --all-targets -- -D warnings
  cargo clippy -p maw-cli --features failpoints --all-targets -- -D warnings

# sg1-faithful-build: produces the failpoints-enabled binary so the
# nightly soak (and any out-of-band chaos campaign) can spawn the real
# maw with `fp!()` call sites live. Default release stays clean &
# zero-overhead — `fp!()` compiles to nothing without the feature.
sg1-faithful-build:
  cargo build -p maw-cli --features failpoints --release

# ----------------------------------------------------------------------------
# SG2 — agent-ergonomics benchmark recipes (bn-2jwi). T2.4 / bn-oko4.
# ----------------------------------------------------------------------------

# sg2-report: render the per-arm dominance table over a directory of
# BenchRun JSONs (one .json file per run, produced by maw-bench).
# By the bone (bn-oko4) + the pre-reg §4 the table NEVER contains a
# composite score; correctness and efficiency are printed as separate
# axes and the reader composes their own dominance verdict.
#
#   just sg2-report <artifact-dir>           # per-run table only
#   just sg2-report <artifact-dir> --median  # add per-arm median rows
sg2-report dir *flags:
  cargo run --quiet -p maw-bench-metrics --features bench --bin sg2-report -- {{dir}} {{flags}}

# sg2-sweep-pilot: drive the condition-spectrum sweep harness end-to-end
# under MockAgent + NoopSubstrate (no spend, no network). 2 cells x 3
# substrates x 3 seeds = 18 BenchRuns; aggregates them; prints the
# spectrum table + crossover doc scaffold. T2.6 / bn-3l1f.
#
# Per pre-reg §3.1 Pilot rule: this is HARNESS-ONLY data. Output MUST
# NOT be used to set bars or support publication claims; the recipe
# exists to confirm the pipeline writes/aggregates/renders end-to-end.
# The real condition-spectrum campaign is a downstream calendar
# artifact (real-LLM, ~$100s of tokens) — invoked by the lead, not by
# this recipe.
#
#   just sg2-sweep-pilot              # tempdir under /tmp
#   just sg2-sweep-pilot <dir>        # explicit artifact dir
sg2-sweep-pilot dir='':
  cargo run --quiet -p maw-bench-sweep --features bench --bin sg2-sweep-pilot -- {{dir}}

# sg2-friction-list: reduce a directory of BenchRun JSONs into the
# prioritized maw friction list (SG4's input). T2.8 / bn-u9iy.
#
# Output: pretty-JSON FrictionList on stdout (the SG4 input format);
# Markdown preview on stderr (the human-readable peer).
#
# Per pre-reg §3.1 Pilot rule: pilot-run numbers are HARNESS-ONLY and
# the Markdown stamps an explicit TEMPLATE banner. The real friction
# list lands when the publication-grade campaign artifacts are reduced.
#
#   just sg2-friction-list <artifact-dir>
sg2-friction-list dir:
  cargo run --quiet -p maw-bench-metrics --features bench --bin sg2-friction-list -- {{dir}}

# sg2-friction-list-pilot: end-to-end pilot for T2.8. Runs the T2.6
# sweep pilot to produce BenchRun JSONs, then reduces them into a
# FrictionList + Markdown scaffold. Asserts the ranking is well-formed
# (sort DESC by total_cost), the unattributed bucket is surfaced, and
# the doc has the expected sections. T2.8 / bn-u9iy.
#
# Per pre-reg §3.1: harness-only data; the Markdown carries the
# TEMPLATE banner so a reader cannot mistake it for publication.
sg2-friction-list-pilot:
  #!/usr/bin/env bash
  set -euo pipefail
  PILOT_DIR="${TMPDIR:-/tmp}/sg2-friction-list-pilot-$$"
  rm -rf "$PILOT_DIR"
  mkdir -p "$PILOT_DIR"
  echo "sg2-friction-list-pilot: artifact dir = $PILOT_DIR"
  # Stage 1: real T2.6 sweep pilot (MockAgent → clean transcripts).
  # Exercises the read-recursive BenchRun path; expected outcome is
  # an empty friction list (the MockAgent doesn't plant friction).
  cargo run --quiet -p maw-bench-sweep --features bench --bin sg2-sweep-pilot -- "$PILOT_DIR" >/dev/null
  RUN_COUNT=$(find "$PILOT_DIR" -name '*.json' | wc -l)
  echo "sg2-friction-list-pilot: BenchRun count = $RUN_COUNT"
  cargo run --quiet -p maw-bench-metrics --features bench --bin sg2-friction-list -- \
    "$PILOT_DIR" \
    --out-json "$PILOT_DIR/friction-list-from-sweep.json" \
    --out-md "$PILOT_DIR/friction-list-from-sweep.md"
  # Stage 2: synthetic-demo with planted clusters so the doc scaffold
  # surfaces non-trivial rows (the publication path will replace this
  # with real-campaign data — the TEMPLATE banner makes the
  # provenance unambiguous).
  cargo run --quiet -p maw-bench-metrics --features bench --bin sg2-friction-list -- \
    --synthetic-demo \
    --out-json "$PILOT_DIR/friction-list.json" \
    --out-md "$PILOT_DIR/friction-list.md"
  echo ""
  echo "----- friction-list.md (synthetic-demo, head) -----"
  head -60 "$PILOT_DIR/friction-list.md"
  echo "----- end head -----"
  # Smoke assertions on the rendered doc.
  grep -q "TEMPLATE" "$PILOT_DIR/friction-list.md"
  grep -q "## Unattributed bucket" "$PILOT_DIR/friction-list.md"
  grep -q "## SG4 handoff" "$PILOT_DIR/friction-list.md"
  grep -q "## #1 — " "$PILOT_DIR/friction-list.md"
  # Ranking well-formed: rank 1 has the largest total_cost.
  python3 -c "import json; d=json.load(open('$PILOT_DIR/friction-list.json')); \
    assert d['schema_version']==1, d; \
    assert 'ranked_clusters' in d and len(d['ranked_clusters']) > 0; \
    costs=[c['total_cost_turns'] for c in d['ranked_clusters']]; \
    assert costs == sorted(costs, reverse=True), costs; \
    assert d['ranked_clusters'][0]['rank']==1; \
    assert 'total_unattributed_wasted_turns' in d and d['total_unattributed_wasted_turns'] >= 0"
  echo "sg2-friction-list-pilot: OK"
  echo "  sweep-derived:   $PILOT_DIR/friction-list-from-sweep.{json,md}"
  echo "  synthetic-demo:  $PILOT_DIR/friction-list.{json,md}"

# sp5-pilot: SP5 layout-ergonomics directional spike (bone bn-2kgu).
# Runs the structural-ergonomics comparison between the current `ws/`
# layout and the proposed consolidated `.maw/workspaces/` layout under
# MockAgent (no LLM spend, no network). Prints a directional verdict
# and optional Markdown report.
#
# Per pre-reg §3.1 Pilot rule: SP5 output is HARNESS-VALIDATION ONLY.
# It MUST NOT set bars and MUST NOT feed SG2/SG3/SG4 publication.
# The verdict gates T3.2 (bn-2sw3) implementation strategy only;
# T3.5 (bn-1uzn) is the binding real-LLM gate.
#
#   just sp5-pilot                     # prints verdict to stdout
#   just sp5-pilot <out.md>            # also writes Markdown to file
#   SP5_REPS=5 just sp5-pilot          # override wall-time replicate count (default 3)
sp5-pilot out='':
  cargo run --quiet -p maw-bench-adapters --features bench --bin sp5-layout-pilot -- {{out}}
