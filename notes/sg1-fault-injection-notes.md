# SG1 Fault-Injection Layer — Implementation Notes (bn-263u)

- **Bone:** bn-263u (T1.5 fault injection), parent SG1 bn-3nw1, depends on
  bn-imw8 (SP1) + bn-1cww (the `fp_commit` `const fn` fix, already landed).
- **Dependents:** bn-32k3 (the SG1 DST harness that *drives* this layer).
- **SP1 decision implemented:** HYBRID execution model + the `MAW_FP` env
  bridge. See `notes/adr-dst-execution-model.md` §"Failpoint
  external-control mechanism".

## What shipped

### 1. Production: `MAW_FP` env bridge (`crates/maw-core/src/failpoints.rs`)

All gated behind `#[cfg(feature = "failpoints")]` — the default release build
links **none** of it (`init_from_env()` is a no-op `#[inline]` stub without
the feature, so call sites need no `#[cfg]`). Zero-overhead contract intact.

- `pub fn parse_env_spec(&str) -> Vec<(String, FailpointAction)>` — the
  SP1-specced parser. Grammar:

  ```
  MAW_FP="NAME=action;NAME=action;..."
  NAME    = literal FP_* name | trailing-`*` prefix glob (e.g. FP_CLEANUP_*)
  action  = off | error[:msg] | panic[:msg] | abort | sleep:<ms>
  ```

  Forgiving by design (a bad env var must never abort a production process
  that merely happens to be a failpoints build): empty/`=`-less/whitespace
  segments are skipped; an unrecognised action drops only that segment;
  `error`/`panic` default their message when `:msg` is omitted; `sleep`
  requires a parseable `:<ms>` else the segment is dropped. A trailing-`*`
  glob is **expanded at load time** against the new canonical
  `KNOWN_FAILPOINTS` table, so `check()` stays an exact-match O(1) lookup
  with zero added hot-path cost; an unknown bare name is kept verbatim (so
  negative-test typos remain injectable); an unknown glob matches nothing.

- `pub const KNOWN_FAILPOINTS: &[&str]` — the canonical 22-entry table of
  real `FP_*` sites (test fixtures excluded). Used only for glob expansion.
  **Keep in sync** with `fp!()`/`fp_commit()` call sites under
  `src/merge/*` and `crates/maw-cli/src/**` (destroy/recover/capture).

- `pub fn init_from_env()` — one-time loader. A `std::sync::OnceLock` guard
  makes it idempotent + process-global, so it is safe to call
  unconditionally at every `maw` entry point. Wired into
  `crates/maw-cli/src/main.rs::main()` (first line after telemetry init).
  Env-derived names are owned `String`; the `REGISTRY` keys are
  `&'static str` (set sites pass literals), so `init_from_env` interns each
  name via `Box::leak` — bounded (≤ the handful of names in `MAW_FP`), runs
  **once**, never on a hot path, never in the default build.

- `crates/maw-cli/Cargo.toml` gained a `failpoints` feature
  (`["maw/failpoints", "maw-core/failpoints"]`) so the *shipped* binary can
  be built for the faithful tier with:
  `cargo build -p maw-cli --features failpoints`. Off by default.

- 9 unit tests in `failpoints::tests::env_bridge` (single/multi segment,
  whitespace/junk tolerance, error/panic msg defaulting, sleep parse +
  rejection, glob expansion, unknown glob, unknown action, empty spec,
  `off`). All isolated (pure `parse_env_spec`, no shared registry).

### 2 & 3. Harness: fault/kill/recovery driver (`crates/maw-assurance/src/fault.rs`)

Behind a new `fault-injection` cargo feature on `maw-assurance`
(`["dep:maw-core","dep:rand","maw-core/failpoints"]`), off by default. This
is the net-new code SP1's reuse map placed in maw-assurance.

- `FaultPlan::from_seed(seed)` — deterministic `(phase, failpoint, kind)`
  selection. Same seed ⇒ identical plan (replayability). A site in
  `DANGEROUS_FAILPOINTS` (the COMMIT two-ref CAS straddle + CLEANUP/PREPARE
  writes) is **always** forced to `CrashKind::SigKill`; other sites get
  seed-chosen unwind vs panic. `CRASHABLE_BY_PHASE` orders sites per-phase
  so appending a new failpoint to a later phase does not reshuffle existing
  seeds for earlier phases.

- `InProcFault::arm(failpoint, &plan)` — workhorse tier. Sets the seed's
  fault in the `maw_core::failpoints` registry; RAII `Drop` disarms so a
  panicking test never leaks a live failpoint into a sibling test in the
  same process. SigKill degrades to a clean `Error` unwind in-process
  (no separate process to kill — that is exactly what the faithful tier is
  for).

- `SubprocFault::run()` — faithful tier. Spawns the shipped `maw` **detached
  via `setsid`** (own process group), exports `plan.maw_fp_spec()` as
  `MAW_FP` so the binary reaches the boundary **deterministically** (SP1
  Finding A: the old `sleep`-window could only crash the one widened phase),
  polls `<root>/.manifold/merge-state.json` until the target phase, then a
  **real** `kill -9 -<pgid>`. `GIT_*_DATE` are pinned (SP1 determinism
  contract → seed-pure commit OIDs). Returns a `SubprocOutcome`
  (`Recovered{attempts}` / `PhaseNotObserved` / `RecoveryExhausted`) — every
  expected crash path is data, not `Err`; `Err` only if the binary can't
  spawn at all. The caller then runs Oracles A/B (`crate::oracle`) against
  `repo_root`.

- **SP1 Finding B handled in `recover_with_retry()`**: after the kill,
  `child.wait()` is called **unconditionally and before any recovery
  attempt** — a SIGKILLed child is a zombie until reaped, and maw's
  `/proc/<pid>` owner-liveness probe reads a zombie as *alive* and refuses
  recovery. The `setsid` leader is reaped by `wait()`; the kernel
  orphan-reaper collects the rest of the group. Recovery is then a **bounded
  retry loop** (default 8 × 1 s backoff — SP1: 1 s alone was too short,
  ~6 s total reliable), re-running the same `maw ws merge` invocation, which
  IS the recovery path (it detects the orphaned `merge-state.json` and
  self-heals via `recovery_outcome_for_phase` once the dead owner is
  observed). This is documented in-code as a **harness requirement, not a
  maw bug**.

- `unsafe` is forbidden workspace-wide, so the real kill is delivered via
  the `kill(1)` binary (`kill -9 -<pgid>`, the bn-cm63 chaos pattern), not a
  raw `libc::kill` FFI call (the spike used `unsafe extern "C"` — not
  portable to this crate's lint policy).

- 6 unit tests (`fault::tests`): determinism, all targets are known
  failpoints, dangerous sites always SIGKILL, `maw_fp_spec()` round-trips
  through the production `parse_env_spec`, all phases reachable, missing
  state file → `None`.

## Recovery / Oracle integration (for bn-32k3)

The recovery surface the oracle needs is exactly "re-run `maw ws merge`"
(there is no separate `--abort` flag; the retry path self-heals — confirmed
against the SP1 subprocess spike and `maw-core::recover_from_merge_state`).
bn-32k3 wiring:

1. in-proc: `InProcFault::arm` → drive real FSM (`maw::merge::commit`) →
   drop the guard → run `maw-core` recovery → `oracle::check_all(pre,post)`.
2. faithful: build `maw` with `--features failpoints`; `SubprocFault::new(
   plan, maw_bin, root, vec!["ws","merge","rz","--into","default",
   "--message","rz"]).run()`; on `Recovered`/`RecoveryExhausted` call
   `oracle::capture_state(root)` + `check_all`. Permanent regression seeds:
   include the bn-cm63 + prior lost-commits incident scenarios.

## Verification (this workspace)

- `cargo check --features failpoints` — PASS.
- `cargo test -p maw-core --features failpoints failpoints` —
  15/15 PASS serial (all 9 new `env_bridge` parser tests pass in parallel
  too; they share no global state).
- `cargo check` (no failpoints) — PASS (zero-overhead default preserved).
- `cargo test -p maw-assurance --features fault-injection fault` —
  6/6 PASS.
- `cargo build -p maw-cli --features failpoints` — PASS (shipped binary
  honours `MAW_FP`).
- Clippy clean (pedantic+nursery deny) on `maw-core --features failpoints`
  and `maw-assurance --features fault-injection --tests`.

## Pre-existing issues (NOT introduced here, NOT in scope)

1. `failpoints::tests::clear_single_failpoint` is flaky **under parallel
   test threads** — `clear_all_resets` calls the global `clear_all()` which
   wipes `FP_KEEP`. This shared-mutable-`REGISTRY`-without-isolation flake
   exists byte-identically on pristine `main` (verified via
   `git show main:`). Passes single-threaded. Not caused by this change; the
   new parser tests are fully isolated. (Candidate cleanup bone: serialize
   the registry tests or give each its own namespace.)
2. `clippy -p maw-cli --features failpoints` flags two
   `used_underscore_binding`/`doc_markdown` lints in
   `src/merge/commit.rs::fp_commit` — that is bn-1cww's already-landed code,
   byte-identical on `main`, untouched here. Only surfaces with
   `clippy --features failpoints` (not a CI gate; the bone's required gates
   are `cargo check`/`cargo test`). Out of scope for bn-263u.

## Downstream impact

- **bn-32k3 (SG1 DST harness):** unblocked. Drive via `FaultPlan` +
  `InProcFault`/`SubprocFault` exactly as in "Recovery/Oracle integration".
  No API gaps found.
- **bn-kwm7 (architecture doc):** record the implemented grammar and the
  `failpoints` feature on `maw-cli` (faithful binary build command) in the
  determinism contract section.
- **v1.0 plan (bn-142y):** no strategic change. If the project ever adds
  `clippy --features failpoints` to CI, file a tiny follow-up to clean the
  two pre-existing `fp_commit` lints (item 2 above) — not a bn-263u change.
