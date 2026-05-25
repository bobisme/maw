//! The sweep driver — turns a [`crate::SweepGrid`] into a directory
//! of per-run [`maw_bench::BenchRun`] JSONs.
//!
//! # Design
//!
//! The driver is **substrate-factory-shaped**: the caller supplies a
//! `make_substrate: FnMut(&str) -> Result<Box<dyn Substrate>, _>`
//! that produces a fresh substrate per arm-name on demand. We do
//! not own the substrates ourselves because each arm
//! (`maw_adapter`, `worktrees_adapter`, `jj_adapter`) has its own
//! constructor + setup story; coupling the sweep driver to a
//! specific arm impl would defeat the point of the [`SweepGrid`]
//! abstraction.
//!
//! Similarly the agent backend is supplied as a closure
//! (`make_agent: FnMut(u64) -> A`) so the caller controls whether
//! it gets a fresh [`maw_bench::MockAgent`] per run (deterministic
//! pilot use) or a shared [`maw_bench::claude::ClaudeBackend`] for
//! a real sweep.
//!
//! # Pilot determinism
//!
//! For the pilot recipe (`just sg2-sweep-pilot`) the driver is
//! invoked with [`maw_bench::MockAgent::with_pinned_clock`] and a
//! tempdir artifact path. Resulting BenchRun JSONs are
//! byte-identical across invocations (within a single host) — the
//! pilot test asserts this property.
//!
//! # What the driver does NOT do (deliberately)
//!
//! - It does not implement the §6.2 block-randomized run order.
//!   That's a wrapper concern (a real-run script can iterate the
//!   grid in a pre-shuffled order; the driver does not impose one).
//! - It does not invoke Oracle B for non-maw arms (the harness
//!   itself gates that via [`maw_bench::BenchConfig::run_oracle_b`]).
//! - It does not cap retries per §8.7. The pilot uses a no-retry
//!   policy; real runs implement the retry cap externally so the
//!   discard taxonomy is auditable.

use std::path::{Path, PathBuf};

use maw_bench::agent::AgentBackend;
use maw_bench::harness::{BenchConfig, BenchHarness, HarnessError};
use maw_bench::run::BenchRun;
use maw_bench::substrate::Substrate;
use maw_scenario::generate_plan;

use crate::grid::{SweepCell, SweepGrid};

/// Default plan length per cell. Mirrors the SP3-era smoke runs;
/// the real run knob is set per scenario in pre-reg §5 (battery
/// size N=8 tasks). The harness consumes a [`ScenarioPlan`] not a
/// task-battery; this knob sizes the plan length the generator
/// emits per cell.
pub const DEFAULT_PLAN_STEPS: usize = 32;

/// Sweep-driver-level errors.
#[derive(Debug, thiserror::Error)]
pub enum SweepDriverError {
    /// The harness reported a true harness-level failure (cannot
    /// write JSON, cannot encode, ...). Per-cell failures land in
    /// the resulting [`BenchRun`]; this error fires only on
    /// non-recoverable conditions.
    #[error("harness: {0}")]
    Harness(#[from] HarnessError),
    /// I/O error preparing the artifact directory.
    #[error("artifact dir setup: {0}")]
    ArtifactDir(#[from] std::io::Error),
    /// The supplied substrate factory could not produce a
    /// substrate for the named arm.
    #[error("substrate factory for arm {0:?} failed: {1}")]
    SubstrateFactory(String, String),
}

/// One planned (cell, arm, replicate, seed) tuple — the unit of
/// work the driver executes.
#[derive(Clone, Debug)]
pub struct SweepPlan {
    /// The cell this run belongs to.
    pub cell: SweepCell,
    /// Arm name (must match a key the substrate factory understands).
    pub arm: String,
    /// 1-based replicate id within the cell × arm.
    pub replicate: u32,
    /// Derived per-(cell, arm, replicate) seed.
    pub seed: u64,
}

/// The sweep driver. Stateless beyond the artifact directory.
pub struct SweepDriver {
    /// Root directory where per-run BenchRun JSONs are written.
    /// Each run gets its own file with a stable filename derived
    /// from `(cell.condition.id, cell.t_class, arm, replicate)`.
    artifact_dir: PathBuf,
    /// How many scenario plan-steps each run uses.
    plan_steps: usize,
    /// Pin timestamps in BenchRun manifests for deterministic JSON
    /// in tests. `Some((start, end))` ⇒ every run sets both.
    pinned_clock_ms: Option<(u64, u64)>,
}

impl SweepDriver {
    /// Construct a driver writing into `artifact_dir`. The directory
    /// is created if it does not exist.
    pub fn new(artifact_dir: impl Into<PathBuf>) -> Result<Self, SweepDriverError> {
        let dir = artifact_dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            artifact_dir: dir,
            plan_steps: DEFAULT_PLAN_STEPS,
            pinned_clock_ms: None,
        })
    }

    /// Override the plan-length used per cell.
    #[must_use]
    pub fn with_plan_steps(mut self, n: usize) -> Self {
        self.plan_steps = n.max(1);
        self
    }

    /// Pin per-run start/end timestamps. Used by pilot tests for
    /// byte-identical determinism.
    #[must_use]
    pub fn with_pinned_clock(mut self, start_ms: u64, end_ms: u64) -> Self {
        self.pinned_clock_ms = Some((start_ms, end_ms));
        self
    }

    /// Where the driver writes BenchRun JSONs.
    #[must_use]
    pub fn artifact_dir(&self) -> &Path {
        &self.artifact_dir
    }

    /// Drive the entire grid. `make_substrate` is called once per
    /// run; `make_agent` is called once per run with the
    /// per-(cell, arm, replicate) seed so each run gets a fresh
    /// (deterministic, seeded) agent backend.
    ///
    /// Returns the in-memory list of completed `BenchRun`s; the
    /// JSON files are also persisted to [`Self::artifact_dir`].
    ///
    /// # Errors
    ///
    /// Returns the first [`SweepDriverError`] encountered. Per-cell
    /// agent / substrate failures are NOT errors — they land in the
    /// resulting [`BenchRun`] (via verdict / oracle_b / stop_reason)
    /// and the sweep continues.
    pub fn drive<S, A, MS, MA>(
        &self,
        grid: &SweepGrid,
        mut make_substrate: MS,
        mut make_agent: MA,
    ) -> Result<Vec<BenchRun>, SweepDriverError>
    where
        S: Substrate,
        A: AgentBackend,
        MS: FnMut(&str) -> Result<S, String>,
        MA: FnMut(u64) -> A,
    {
        let mut out: Vec<BenchRun> =
            Vec::with_capacity(grid.cells.len() * grid.arms.len() * grid.seeds_per_cell as usize);

        for (cell, arm, replicate, seed) in grid.iter_runs() {
            let plan_steps = self.plan_steps;
            let plan = generate_plan(seed, &cell.condition.to_profile(), plan_steps);

            let substrate = make_substrate(&arm)
                .map_err(|e| SweepDriverError::SubstrateFactory(arm.clone(), e))?;
            let agent = make_agent(seed);
            let agent_config = maw_bench::agent::AgentConfig::default();

            let mut harness = BenchHarness::new(substrate, agent, agent_config);

            let cell_dir = self.cell_dir(&cell);
            std::fs::create_dir_all(&cell_dir)?;

            let run_id = stable_run_id(&cell, &arm, replicate);
            // We do NOT pass artifact_dir to the harness — instead we
            // persist after rewriting `manifest.arm` (the harness uses
            // the SubstrateHandle's label which is the substrate impl's
            // self-label, but the sweep treats arm names as the grid's
            // logical labels so multiple grid arms can share one
            // substrate impl during MockAgent pilots).
            let mut config = BenchConfig {
                artifact_dir: None,
                run_id: Some(run_id.clone()),
                condition_id: cell.condition.id.clone(),
                t_class: cell.t_class.as_str().to_string(),
                run_oracle_b: arm == "maw",
                oracle_b_skip_reason: skip_reason_for(&arm),
                ..BenchConfig::default()
            };
            if let Some((s, e)) = self.pinned_clock_ms {
                config.pinned_start_ms = Some(s);
                config.pinned_end_ms = Some(e);
            }

            let mut run = harness.run(&plan, &config)?;
            // Override the substrate-self-reported arm with the grid's
            // logical arm name — this is the load-bearing invariant
            // for the aggregator's CellKey.
            run.manifest.arm.clone_from(&arm);
            persist_run_json(&run, &cell_dir, &run_id)?;
            out.push(run);
        }
        Ok(out)
    }

    /// The per-cell subdirectory path. Stable + filesystem-safe.
    fn cell_dir(&self, cell: &SweepCell) -> PathBuf {
        self.artifact_dir
            .join(format!("{}-{}", cell.condition.id, cell.t_class.as_str()))
    }
}

/// Derive a stable, filesystem-safe run id from (cell, arm,
/// replicate). The harness uses this both as the JSON filename
/// stem and as `BenchRun.run_id`, so aggregator output is keyed
/// consistently with what was written.
#[must_use]
pub fn stable_run_id(cell: &SweepCell, arm: &str, replicate: u32) -> String {
    format!(
        "{cond}-{t}-{arm}-r{rep:03}",
        cond = cell.condition.id,
        t = cell.t_class.as_str(),
        arm = sanitize(arm),
        rep = replicate,
    )
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn skip_reason_for(arm: &str) -> String {
    if arm == "maw" {
        String::new()
    } else {
        format!("arm = {arm}; Oracle B scoped to maw refs")
    }
}

/// Atomic write of `run` to `<dir>/<run_id>.json`. Mirrors the
/// harness's own persist routine but bypasses
/// [`HarnessError::PersistFailed`] (we never need to box the
/// failed run back for inspection — the sweep already owns it).
fn persist_run_json(run: &BenchRun, dir: &Path, run_id: &str) -> Result<(), SweepDriverError> {
    let json = run
        .to_json()
        .map_err(|e| SweepDriverError::Harness(HarnessError::Encode(e)))?;
    let final_path = dir.join(format!("{run_id}.json"));
    let tmp_path = dir.join(format!("{run_id}.json.tmp"));
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{ConditionPoint, TClass, pilot_grid};
    use maw_bench::{MockAgent, MockScript, NoopSubstrate};

    fn finished_script() -> MockScript {
        MockScript::finished_in_one("done")
    }

    #[test]
    fn stable_run_id_is_filesystem_safe_and_unique_per_replicate() {
        let cell = SweepCell {
            condition: ConditionPoint::c0_benign(),
            t_class: TClass::T0,
        };
        let a = stable_run_id(&cell, "claude-native-worktrees", 1);
        let b = stable_run_id(&cell, "claude-native-worktrees", 2);
        assert_ne!(a, b);
        assert!(!a.contains(' '));
        assert!(!a.contains('/'));
        // arm name is preserved verbatim (already sanitized chars).
        assert!(a.contains("claude-native-worktrees"));
    }

    #[test]
    fn driver_writes_one_json_per_cell_arm_replicate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let driver = SweepDriver::new(tmp.path())
            .expect("driver")
            .with_plan_steps(4)
            .with_pinned_clock(1000, 2000);

        // Tiny grid: 1 cell, 1 arm ("noop-arm"), 2 replicates.
        let grid = SweepGrid {
            cells: vec![SweepCell {
                condition: ConditionPoint::c0_benign(),
                t_class: TClass::T0,
            }],
            arms: vec!["noop-arm".to_string()],
            seeds_per_cell: 2,
            base_seed: 1,
        };

        let runs = driver
            .drive(
                &grid,
                |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
                |_seed| MockAgent::with_pinned_clock(finished_script(), 1234),
            )
            .expect("drive ok");

        assert_eq!(runs.len(), 2);

        // Files on disk: 2 JSONs in C0-T0/.
        let cell_dir = tmp.path().join("C0-T0");
        let json_files: Vec<_> = std::fs::read_dir(&cell_dir)
            .expect("read cell dir")
            .filter_map(|e| {
                let p = e.ok()?.path();
                if p.extension().and_then(|x| x.to_str()) == Some("json") {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(json_files.len(), 2, "expected 2 BenchRun JSONs");
    }

    #[test]
    fn pilot_grid_produces_eighteen_runs_under_noop_substrate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let driver = SweepDriver::new(tmp.path())
            .expect("driver")
            .with_plan_steps(4)
            .with_pinned_clock(1000, 2000);
        let grid = pilot_grid(42);
        let runs = driver
            .drive(
                &grid,
                |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
                |_seed| MockAgent::with_pinned_clock(finished_script(), 1234),
            )
            .expect("drive ok");
        assert_eq!(runs.len(), 18);
        // Cell-dir layout: 2 cells, each with 3 arms × 3 reps = 9 files per cell dir.
        for cell_id in ["C0-T0", "C4-T0"] {
            let dir = tmp.path().join(cell_id);
            let n = std::fs::read_dir(&dir)
                .expect("cell dir")
                .filter(|e| {
                    e.as_ref().is_ok_and(|x| {
                        x.path().extension().and_then(|s| s.to_str()) == Some("json")
                    })
                })
                .count();
            assert_eq!(n, 9, "cell {cell_id} should have 9 BenchRun JSONs");
        }
    }
}
