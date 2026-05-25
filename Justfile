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
