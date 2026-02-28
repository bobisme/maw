build:
  cargo build --release

test:
  cargo test

install:
  cargo install --locked --path .

# Assurance CI gates

# dst-fast: 256 seeded DST traces per PR (<60s)
dst-fast:
  cargo test --test dst_harness -- dst_g1 dst_g3 dst_determinism

# formal-check: Stateright model checking (pre-release)
formal-check:
  cargo test --features assurance --test formal_model

# contract-drift: doc/code consistency checks
contract-drift:
  cargo test --test contract_drift

# dst-nightly: 10k+ traces (nightly, ~15-30 min)
dst-nightly:
  cargo test --test dst_harness -- --ignored dst_nightly --nocapture

# incident-replay: replay failing traces from corpus
incident-replay:
  cargo test --test dst_harness -- incident_replay

# All assurance gates combined
check: test dst-fast contract-drift
