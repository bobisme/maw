default:
  just --list

build:
  cargo build --release

test:
  cargo test

install:
  cargo install --locked --path .

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
check: test dst-fast contract-drift

coverage:
  cargo llvm-cov
