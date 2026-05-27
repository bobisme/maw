//! The frozen condition-spectrum grid and sweep-cell vocabulary.
//!
//! # The §5 frozen spectrum (verbatim)
//!
//! | id  | name     | K_overlap | K_concurrency | K_rounds | between-rounds |
//! | --- | -------- | --------- | ------------- | -------- | -------------- |
//! | C0  | benign   | 0/8       | 1             | 1        | n/a            |
//! | C1  | light    | 2/8       | 2             | 3        | serialized     |
//! | C2  | moderate | 4/8       | 3             | 5        | serialized     |
//! | C3  | heavy    | 6/8       | 3             | 8        | burst          |
//! | C4  | hostile  | 8/8       | 4             | 8        | burst          |
//!
//! The five `ConditionPoint`s below match this table exactly.
//!
//! # ConditionProfile mapping (driver-side amendment)
//!
//! The pre-reg §5 names the knobs (K_overlap, K_concurrency,
//! K_rounds, between-rounds) abstractly. The scenario generator
//! ([`maw_scenario::ConditionProfile`]) carries a different but
//! parallel set of knobs (concurrency_degree, mid_op_kill_prob,
//! overlapping_edit_rate, stale_workspace_rate). This module
//! provides a **driver-side mapping** from the frozen §5 spectrum
//! to a `ConditionProfile` so the sweep harness can drive the
//! generator without further coordination.
//!
//! This mapping is a **pre-reg amendment** (documented as such in
//! `notes/sg2-benchmark-preregistration.md` §9 when the first
//! measured run is committed). The mapping rule:
//!
//! - `concurrency_degree` = `K_concurrency`.
//! - `overlapping_edit_rate` = `K_overlap / 8` (the §5 fractions).
//! - `stale_workspace_rate` = `0.2` if `between-rounds = burst`,
//!   else `0.0` for serialized C0–C2 (the pre-reg's "between-rounds
//!   = burst" condition is when concurrent agents race over a stale
//!   epoch; that maps to a non-zero `stale_workspace_rate`).
//! - `mid_op_kill_prob` = `0.0` for the headline sweep — fault
//!   injection is an SG1 concern (`notes/sg1-soak-campaign.md`); the
//!   SG2 spectrum measures coordination contention, NOT fault
//!   recovery. (T2.6 future work: surface fault-rate as a separate
//!   sweep axis if a measurement need emerges.)
//!
//! # T-class application schedule (§5.1)
//!
//! - T0 at every condition (C0..C4).
//! - T1..T5 at C2 only.
//! - [`spectrum_grid`] returns this exact 5+5 = 10 (cell, T-class)
//!   schedule.

use serde::{Deserialize, Serialize};

use maw_scenario::ConditionProfile;

/// The frozen four-arm publication ordering. Used by the renderer
/// and the default sweep grid. Matches the pre-reg §1.3 arm order.
pub const ARMS_PUBLICATION: &[&str] = &[
    "maw",
    "git-worktrees-bare",
    "claude-native-worktrees",
    "jj-workspaces",
];

/// One §5 spectrum point — the frozen tuple (name + the four §5
/// knobs). Stored as a struct so callers can read knob values for
/// diagnostics without recomputing from the id.
///
/// Strings are owned (`String`) rather than `&'static str` so the
/// struct round-trips through serde without borrowing from the
/// constants in [`frozen_spectrum`]; the values are still
/// constants in practice.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConditionPoint {
    /// `C0..C4` — the stable identifier the §6.4 manifest carries.
    pub id: String,
    /// Human-friendly name (`benign`, `light`, ...).
    pub name: String,
    /// `K_overlap` numerator (over the frozen N=8 task battery).
    pub k_overlap_numerator: u32,
    /// `K_concurrency` — exact agent count.
    pub k_concurrency: u32,
    /// `K_rounds` — exact contention-round count.
    pub k_rounds: u32,
    /// True iff the between-rounds setting is `burst`. False ⇒ serialized.
    pub burst: bool,
}

impl ConditionPoint {
    /// Map this condition point to a [`ConditionProfile`] for the
    /// scenario generator. See module docs for the rule.
    #[must_use]
    pub fn to_profile(&self) -> ConditionProfile {
        self.to_profile_with_chaos(0.0)
    }

    /// Same as `to_profile` but lets callers inject `mid_op_kill_prob`
    /// instead of the hardcoded 0.0. The default `to_profile()` keeps
    /// the original "SG2 is fault-orthogonal to SG1" stance for
    /// back-compat. SweepDriver passes a non-zero kill-prob here when
    /// `--chaos=on` so the scenario generator emits Failpoint steps
    /// the chaos overlay can translate to MAW_FP. Per pre-reg §9
    /// Amendment A1 (bn-3hzt).
    #[must_use]
    pub fn to_profile_with_chaos(&self, mid_op_kill_prob: f64) -> ConditionProfile {
        let overlap_rate = f64::from(self.k_overlap_numerator) / 8.0;
        let stale_rate = if self.burst { 0.2 } else { 0.0 };
        ConditionProfile::new(
            u8::try_from(self.k_concurrency.min(255)).unwrap_or(1),
            mid_op_kill_prob,
            overlap_rate,
            stale_rate,
        )
    }

    /// The benign endpoint (C0). Convenience for tests.
    #[must_use]
    pub fn c0_benign() -> Self {
        Self {
            id: "C0".to_string(),
            name: "benign".to_string(),
            k_overlap_numerator: 0,
            k_concurrency: 1,
            k_rounds: 1,
            burst: false,
        }
    }

    /// The hostile endpoint (C4).
    #[must_use]
    pub fn c4_hostile() -> Self {
        Self {
            id: "C4".to_string(),
            name: "hostile".to_string(),
            k_overlap_numerator: 8,
            k_concurrency: 4,
            k_rounds: 8,
            burst: true,
        }
    }
}

/// The frozen five-point spectrum, in benign→hostile order.
#[must_use]
pub fn frozen_spectrum() -> [ConditionPoint; 5] {
    [
        ConditionPoint::c0_benign(),
        ConditionPoint {
            id: "C1".to_string(),
            name: "light".to_string(),
            k_overlap_numerator: 2,
            k_concurrency: 2,
            k_rounds: 3,
            burst: false,
        },
        ConditionPoint {
            id: "C2".to_string(),
            name: "moderate".to_string(),
            k_overlap_numerator: 4,
            k_concurrency: 3,
            k_rounds: 5,
            burst: false,
        },
        ConditionPoint {
            id: "C3".to_string(),
            name: "heavy".to_string(),
            k_overlap_numerator: 6,
            k_concurrency: 3,
            k_rounds: 8,
            burst: true,
        },
        ConditionPoint::c4_hostile(),
    ]
}

/// The §5.1 task-class taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TClass {
    /// T0 — code-only shared hotspot (the K_overlap default).
    T0,
    /// T1 — ignored-env setup required.
    T1,
    /// T2 — dependency / install side effects.
    T2,
    /// T3 — mergeback / PR required.
    T3,
    /// T4 — stale-base / rebase required.
    T4,
    /// T5 — cleanup / recovery after interrupted run.
    T5,
}

impl TClass {
    /// Stable string form used in §6.4 manifest entries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::T0 => "T0",
            Self::T1 => "T1",
            Self::T2 => "T2",
            Self::T3 => "T3",
            Self::T4 => "T4",
            Self::T5 => "T5",
        }
    }
}

/// One (condition, T-class) cell in a sweep. Per-cell N (seeds) is
/// carried at the [`SweepGrid`] level, not here, so a cell can be
/// resampled with different N without reshuffling the layout.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SweepCell {
    /// `C0..C4`.
    pub condition: ConditionPoint,
    /// `T0..T5`.
    pub t_class: TClass,
}

/// A planned sweep — the schedule of (cell × arm) the driver will
/// run, plus the seeds per cell and the arm vocabulary.
///
/// The grid is **declarative** — it does not own the harness, the
/// substrates, or the scenario generator. The [`crate::SweepDriver`]
/// consumes it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SweepGrid {
    /// The (condition × T-class) cells to run.
    pub cells: Vec<SweepCell>,
    /// Arms to test, in iteration order. Defaults to
    /// [`ARMS_PUBLICATION`] in [`spectrum_grid`]; callers can
    /// override for budget-constrained sub-arms (pre-reg §1.3 R14).
    pub arms: Vec<String>,
    /// Number of seeds to drive per cell × arm. Headline N=10 /
    /// loss-regime N=20 per pre-reg §6.1.
    pub seeds_per_cell: u32,
    /// Base seed for the sweep. Per-(cell, arm, replicate) seeds
    /// are derived deterministically from this so a re-run with the
    /// same base seed produces byte-identical scenario plans.
    pub base_seed: u64,
}

impl SweepGrid {
    /// Iterate every (cell, arm, replicate_id, derived_seed) the
    /// sweep will run. The replicate_id is 1-based.
    ///
    /// Per-(cell, arm, replicate) seed = `base_seed ^ hash(cell, arm,
    /// replicate)` so the seed depends on every coordinate but a
    /// re-run with the same `base_seed` is byte-identical.
    ///
    /// The iteration order is intentionally `cell -> arm -> replicate`
    /// (not `arm -> cell -> replicate`) so a partial sweep that gets
    /// interrupted still has data for every arm at the earliest
    /// completed cells. The pre-reg §6.2 block-randomized run order
    /// is a downstream concern of the harness wrapper (not this
    /// pure-data layout); the grid here is the *shape*, not the
    /// runtime schedule.
    pub fn iter_runs(&self) -> impl Iterator<Item = (SweepCell, String, u32, u64)> + '_ {
        self.cells.iter().flat_map(move |cell| {
            self.arms.iter().flat_map(move |arm| {
                (1..=self.seeds_per_cell).map(move |rep| {
                    let seed = derive_seed(self.base_seed, cell, arm, rep);
                    (cell.clone(), arm.clone(), rep, seed)
                })
            })
        })
    }
}

/// Deterministic seed derivation. Mixes the base seed with the
/// cell id, t-class, arm name, and 1-based replicate so every
/// (cell, arm, replicate) triple draws a different scenario.
#[must_use]
pub fn derive_seed(base_seed: u64, cell: &SweepCell, arm: &str, rep: u32) -> u64 {
    let mut s = base_seed;
    s = mix(s, fnv64(cell.condition.id.as_bytes()));
    s = mix(s, fnv64(cell.t_class.as_str().as_bytes()));
    s = mix(s, fnv64(arm.as_bytes()));
    mix(s, u64::from(rep))
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

const fn fnv64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h
}

const fn mix(a: u64, b: u64) -> u64 {
    // xorshift-style mixer; suffices for deterministic seed
    // derivation, NOT a hash function.
    let mut x = a ^ b.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 31;
    x
}

/// The frozen §5 + §5.1 minimum-commitment grid:
///
/// - T0 at every of C0..C4 (5 cells)
/// - T1..T5 at C2 (5 cells)
///
/// = 10 cells total. Default `seeds_per_cell = 10` per pre-reg §6.1
/// headline N. Real-run callers set 20 for the loss-regime cells
/// (C0 vs claude-native-worktrees; C3/C4 wedge band).
#[must_use]
pub fn spectrum_grid(base_seed: u64, seeds_per_cell: u32) -> SweepGrid {
    let spectrum = frozen_spectrum();
    let mut cells: Vec<SweepCell> = Vec::with_capacity(10);
    // T0 across the full spectrum.
    for cond in &spectrum {
        cells.push(SweepCell {
            condition: cond.clone(),
            t_class: TClass::T0,
        });
    }
    // T1..T5 at C2 only.
    let c2 = spectrum[2].clone();
    for t in [TClass::T1, TClass::T2, TClass::T3, TClass::T4, TClass::T5] {
        cells.push(SweepCell {
            condition: c2.clone(),
            t_class: t,
        });
    }
    SweepGrid {
        cells,
        arms: ARMS_PUBLICATION.iter().map(|s| (*s).to_string()).collect(),
        seeds_per_cell,
        base_seed,
    }
}

/// A minimal pilot grid for harness-validation only:
/// - 2 cells (C0 + C4 — the two endpoints of the spectrum)
/// - 3 substrates (maw, git-worktrees-bare, jj-workspaces — arm 4
///   omitted because the pilot uses MockAgent and ClaudeNative would
///   add no test signal at MockAgent fidelity)
/// - 3 seeds/cell
///
/// = 2 × 3 × 3 = 18 BenchRuns. Per §3.1 Pilot rule: this data is
/// excluded from any analysis and never sets a bar.
#[must_use]
pub fn pilot_grid(base_seed: u64) -> SweepGrid {
    let cells = vec![
        SweepCell {
            condition: ConditionPoint::c0_benign(),
            t_class: TClass::T0,
        },
        SweepCell {
            condition: ConditionPoint::c4_hostile(),
            t_class: TClass::T0,
        },
    ];
    SweepGrid {
        cells,
        arms: vec![
            "maw".to_string(),
            "git-worktrees-bare".to_string(),
            "jj-workspaces".to_string(),
        ],
        seeds_per_cell: 3,
        base_seed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectrum_is_exactly_five_points_benign_to_hostile() {
        let s = frozen_spectrum();
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].id, "C0");
        assert_eq!(s[4].id, "C4");
        // K_overlap is monotone non-decreasing benign->hostile.
        for w in s.windows(2) {
            assert!(w[0].k_overlap_numerator <= w[1].k_overlap_numerator);
        }
    }

    #[test]
    fn condition_to_profile_maps_overlap_rate_correctly() {
        let c0 = ConditionPoint::c0_benign();
        let p = c0.to_profile();
        assert_eq!(p.overlapping_edit_rate, 0.0);
        assert_eq!(p.concurrency_degree, 1);

        let c4 = ConditionPoint::c4_hostile();
        let p = c4.to_profile();
        assert_eq!(p.overlapping_edit_rate, 1.0);
        assert_eq!(p.concurrency_degree, 4);
        // burst → stale_workspace_rate non-zero
        assert!(p.stale_workspace_rate > 0.0);
        // fault-injection off for the headline spectrum.
        assert_eq!(p.mid_op_kill_prob, 0.0);
    }

    #[test]
    fn spectrum_grid_has_ten_cells_with_correct_t_distribution() {
        let g = spectrum_grid(42, 10);
        assert_eq!(g.cells.len(), 10);
        // 5 T0 cells (one per condition).
        assert_eq!(
            g.cells.iter().filter(|c| c.t_class == TClass::T0).count(),
            5
        );
        // 5 cells at C2 with T1..T5.
        assert_eq!(
            g.cells
                .iter()
                .filter(|c| c.condition.id == "C2" && c.t_class != TClass::T0)
                .count(),
            5
        );
    }

    #[test]
    fn pilot_grid_is_two_cells_three_arms_three_seeds() {
        let g = pilot_grid(7);
        assert_eq!(g.cells.len(), 2);
        assert_eq!(g.arms.len(), 3);
        assert_eq!(g.seeds_per_cell, 3);
        // 2 * 3 * 3 = 18 runs.
        assert_eq!(g.iter_runs().count(), 18);
    }

    #[test]
    fn iter_runs_seeds_are_unique_per_cell_arm_replicate() {
        let g = pilot_grid(7);
        let mut seeds: Vec<u64> = g.iter_runs().map(|(_, _, _, s)| s).collect();
        seeds.sort_unstable();
        let before = seeds.len();
        seeds.dedup();
        assert_eq!(seeds.len(), before, "seed collision in pilot grid");
    }

    #[test]
    fn seeds_are_deterministic_given_base_seed() {
        let g1 = pilot_grid(7);
        let g2 = pilot_grid(7);
        let s1: Vec<u64> = g1.iter_runs().map(|(_, _, _, s)| s).collect();
        let s2: Vec<u64> = g2.iter_runs().map(|(_, _, _, s)| s).collect();
        assert_eq!(s1, s2);
        // Different base seed → different seed set.
        let g3 = pilot_grid(8);
        let s3: Vec<u64> = g3.iter_runs().map(|(_, _, _, s)| s).collect();
        assert_ne!(s1, s3);
    }

    #[test]
    fn iter_runs_visits_each_cell_for_every_arm() {
        let g = pilot_grid(7);
        let mut seen: std::collections::BTreeSet<(String, String)> =
            std::collections::BTreeSet::new();
        for (cell, arm, _, _) in g.iter_runs() {
            seen.insert((cell.condition.id.clone(), arm));
        }
        // 2 cells × 3 arms = 6 unique pairs.
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn arms_publication_constant_is_frozen_four_in_pre_reg_order() {
        assert_eq!(
            ARMS_PUBLICATION,
            &[
                "maw",
                "git-worktrees-bare",
                "claude-native-worktrees",
                "jj-workspaces"
            ]
        );
    }
}
