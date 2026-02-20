//! Deterministic N-way merge engine.
//!
//! Implements the collect → partition → resolve → build pipeline described in
//! design doc §6.1. Each phase is a separate module.
//!
//! # Phase 1 (this implementation)
//!
//! - **collect**: Scan each source workspace and capture changed files as [`PatchSet`]s.
//! - **partition**: Group changes by path into unique vs shared paths.
//! - **resolve**: Resolve shared paths via hash equality + diff3 with structured conflicts.
//! - **build**: Take epoch + resolved changes, produce a new git tree + commit ([`build`] module).
//!
//! # Determinism guarantee
//!
//! The same set of epoch + workspace patch-sets always produces the same merge
//! result, regardless of workspace creation order or iteration order:
//!
//! - Paths are processed in lexicographic order.
//! - File content (blob identity) drives resolution, not timestamps.
//! - diff3 is itself deterministic given the same inputs.

pub mod build;
pub mod build_phase;
pub mod collect;
pub mod commit;
pub mod partition;
pub mod plan;
pub mod prepare;
pub mod resolve;
pub mod types;
pub mod validate;

pub use build::{BuildError, ResolvedChange, build_merge_commit};
pub use build_phase::{
    BuildPhaseError, BuildPhaseOutput, run_build_phase, run_build_phase_with_inputs,
};
pub use plan::{
    DriverInfo, MergePlan, PlanArtifactError, PredictedConflict, ValidationInfo, WorkspaceChange,
    WorkspaceReport, compute_merge_id, write_plan_artifact, write_workspace_report_artifact,
};
pub use collect::{CollectError, collect_snapshots};
pub use partition::{PartitionResult, PathEntry, partition_by_path};
pub use prepare::{FrozenInputs, PrepareError, run_prepare_phase, run_prepare_phase_with_epoch};
pub use resolve::{
    ConflictReason, ConflictRecord, ConflictSide, ResolveError, ResolveResult, parse_diff3_atoms,
    resolve_partition,
};
pub use types::{ChangeKind, FileChange, PatchSet};
pub use validate::{
    ValidateError, ValidateOutcome, run_validate_in_dir, run_validate_phase,
    run_validate_pipeline_in_dir, write_validation_artifact,
};

#[cfg(test)]
mod determinism_tests;
