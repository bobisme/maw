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
