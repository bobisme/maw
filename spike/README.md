# SP1 / bn-imw8 — DST execution-model spike

Throwaway prototype. **Not production code.** Standalone cargo manifest
(`[workspace]` in its own `Cargo.toml`) so it can opt into the parent's
`failpoints` feature without perturbing the parent workspace.

See the decision in [`../notes/adr-dst-execution-model.md`](../notes/adr-dst-execution-model.md).

## Prerequisite

This spike applies a one-line fix to `../src/merge/commit.rs` (`fp_commit`:
`const fn` → `fn`) so the parent crate compiles with `--features
failpoints`. Tracking bone: bn-1cww. Without it, the in-proc prototype (and
any DST harness touching COMMIT failpoints) cannot build.

## Run

```sh
# In-process model: bit-exact, fast, links maw + maw-core, faults via
# failpoints::set(). Drives the real COMMIT FSM + real recovery, checks G3.
cargo run --bin inproc -- <seed>          # e.g. 1 2 3 7 42 99 → all PASS

# Subprocess / faithful model: real `maw`, real SIGKILL (bn-cm63 pattern),
# real recovery, Prime-Invariant oracle. Seeds hitting `validate` PASS;
# `build`-phase seeds show the sleep-window blind spot (ADR Finding A).
cargo run --bin subprocess -- <seed>      # e.g. 3, 8 → PASS
```

Same seed ⇒ same fault selection. Bit-exact OID replay requires the pinned
`GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` (already set in `inproc.rs`) — this is
a determinism-contract requirement carried into bn-kwm7.

## Files

- `src/inproc.rs` — in-process driver (the load-bearing reproducible run).
- `src/subprocess.rs` — faithful driver; demonstrates the bn-cm63 chaos
  pattern and the two faithful-only findings (sleep-window blind spot;
  zombie-masked owner liveness).
