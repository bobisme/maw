//! Back-compat re-export of the driver-agnostic scenario plan generator.
//!
//! As of bn-4qwp (T2.1) the scenario generator lives in the sibling
//! `maw-scenario` crate so the SG2 real-agent driver (T2.2 bn-1sqo) and
//! substrate adapters (T2.3 bn-mit2) can consume the same plan stream
//! without pulling in maw-assurance's heavyweight oracles / in-proc / fault
//! / maw-core / failpoints / tempfile dep surface.
//!
//! This module preserves the pre-factor public import path
//! `maw_assurance::scenario::*` (gated behind the existing `scenario`
//! feature) by re-exporting `maw_scenario` verbatim. SG1's existing
//! consumers (`in_proc`, `shrinker`, the per-commit corpus / nightly soak
//! tests in `tests/sg1_dst.rs`) compile and run unchanged.
//!
//! Both drivers — the SG1 in-proc driver here and the SG2 real-agent driver
//! in the benchmark harness — therefore call into the SAME generator code,
//! satisfying the bone's "no fork/copy" acceptance criterion and the
//! "build once, drive two ways" contract in
//! `notes/sg1-dst-architecture.md` §2.

#![cfg(feature = "scenario")]

pub use maw_scenario::*;
