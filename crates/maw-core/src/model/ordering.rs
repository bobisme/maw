//! Ordering key for causal ordering of operations (§5.9).
//!
//! Manifold uses a composite ordering key `(epoch_id, workspace_id, seq)` for
//! deterministic causal ordering. Wall clock is informational only — never used
//! for correctness, but clamped monotonically so it's always non-decreasing.
//!
//! # Ordering semantics
//!
//! The authoritative ordering triple is `(epoch_id, workspace_id, seq)`.
//! Wall clock is display-only and excluded from `Ord`/`PartialOrd`.
//!
//! Within a single workspace, `seq` is strictly monotonically increasing.
//! Across workspaces, ties are broken by `workspace_id` (lexicographic).
//! Across epochs, ties are broken by `epoch_id` (hex-string lexicographic).

use std::cmp::Ordering;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::types::{EpochId, WorkspaceId};

// ---------------------------------------------------------------------------
// OrderingKey
// ---------------------------------------------------------------------------

/// Composite ordering key for causal ordering of operations.
///
/// Ordering is determined by the authoritative triple `(epoch_id, workspace_id, seq)`.
/// The `wall_clock_ms` field is informational only and excluded from ordering.
#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
pub struct OrderingKey {
    /// The epoch this operation belongs to.
    pub epoch_id: EpochId,
    /// The workspace that produced this operation.
    pub workspace_id: WorkspaceId,
    /// Monotonically increasing sequence number within a workspace.
    pub seq: u64,
    /// Wall-clock milliseconds since Unix epoch (informational only).
    /// Clamped: never goes backward within a workspace.
    pub wall_clock_ms: u64,
}

impl OrderingKey {
    /// Create a new ordering key with explicit values.
    #[must_use]
    pub const fn new(
        epoch_id: EpochId,
        workspace_id: WorkspaceId,
        seq: u64,
        wall_clock_ms: u64,
    ) -> Self {
        Self {
            epoch_id,
            workspace_id,
            seq,
            wall_clock_ms,
        }
    }
}

// Ordering uses ONLY the authoritative triple.
impl PartialEq for OrderingKey {
    fn eq(&self, other: &Self) -> bool {
        self.epoch_id == other.epoch_id
            && self.workspace_id == other.workspace_id
            && self.seq == other.seq
    }
}

impl PartialOrd for OrderingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch_id
            .as_str()
            .cmp(other.epoch_id.as_str())
            .then_with(|| self.workspace_id.cmp(&other.workspace_id))
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

impl fmt::Display for OrderingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            &self.epoch_id.as_str()[..8],
            self.workspace_id,
            self.seq,
        )
    }
}

// ---------------------------------------------------------------------------
// SequenceGenerator — monotonic per-workspace sequence + wall-clock clamp
// ---------------------------------------------------------------------------

/// Per-workspace sequence and wall-clock generator.
///
/// Guarantees:
/// - `seq` is strictly monotonically increasing (starts at 1).
/// - `wall_clock_ms` is non-decreasing: `max(now_ms, last_seen + 1)`.
///
/// # Usage
///
/// ```rust,ignore
/// let mut seq_gen = SequenceGenerator::new();
/// let (seq, wall_ms) = seq_gen.next(); // (1, now_ms)
/// let (seq, wall_ms) = seq_gen.next(); // (2, max(now_ms, prev+1))
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SequenceGenerator {
    last_seq: u64,
    last_wall_clock_ms: u64,
}

impl SequenceGenerator {
    /// Create a new generator starting at seq=0 (next call returns 1).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_seq: 0,
            last_wall_clock_ms: 0,
        }
    }

    /// Resume from a known state (e.g., loaded from persistent storage).
    #[must_use]
    pub const fn resume(last_seq: u64, last_wall_clock_ms: u64) -> Self {
        Self {
            last_seq,
            last_wall_clock_ms,
        }
    }

    /// Generate the next `(seq, wall_clock_ms)` pair.
    ///
    /// Wall clock is clamped: if the system clock went backward (NTP step,
    /// VM resume), we use `last_seen + 1` instead of going backward.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> (u64, u64) {
        self.last_seq += 1;
        let now_ms = current_time_ms();
        self.last_wall_clock_ms = now_ms.max(self.last_wall_clock_ms + 1);
        (self.last_seq, self.last_wall_clock_ms)
    }

    /// Generate the next `(seq, wall_clock_ms)` pair using a provided wall clock.
    ///
    /// This is primarily for testing — in production use [`Self::next()`].
    pub fn next_with_clock(&mut self, now_ms: u64) -> (u64, u64) {
        self.last_seq += 1;
        self.last_wall_clock_ms = now_ms.max(self.last_wall_clock_ms + 1);
        (self.last_seq, self.last_wall_clock_ms)
    }

    /// The last sequence number generated (0 if none yet).
    #[must_use]
    pub const fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The last wall-clock value generated (0 if none yet).
    #[must_use]
    pub const fn last_wall_clock_ms(&self) -> u64 {
        self.last_wall_clock_ms
    }
}

impl Default for SequenceGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Get current wall-clock time in milliseconds since Unix epoch.
fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;
    use crate::model::types::EpochId;

    fn epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).unwrap()
    }

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn key(epoch_char: char, ws_name: &str, seq: u64, wall: u64) -> OrderingKey {
        OrderingKey::new(epoch(epoch_char), ws(ws_name), seq, wall)
    }

    // -----------------------------------------------------------------------
    // OrderingKey construction and display
    // -----------------------------------------------------------------------

    #[test]
    fn ordering_key_construction() {
        let k = key('a', "agent-1", 42, 1000);
        assert_eq!(k.epoch_id, epoch('a'));
        assert_eq!(k.workspace_id, ws("agent-1"));
        assert_eq!(k.seq, 42);
        assert_eq!(k.wall_clock_ms, 1000);
    }

    #[test]
    fn ordering_key_display() {
        let k = key('a', "agent-1", 5, 0);
        let display = format!("{k}");
        assert!(
            display.starts_with("aaaaaaaa"),
            "should start with epoch prefix"
        );
        assert!(display.contains("agent-1"), "should contain workspace id");
        assert!(display.ends_with(":5"), "should end with seq number");
    }

    // -----------------------------------------------------------------------
    // Ordering — authoritative triple only
    // -----------------------------------------------------------------------

    #[test]
    fn ordering_same_epoch_same_ws_by_seq() {
        let k1 = key('a', "w1", 1, 100);
        let k2 = key('a', "w1", 2, 50); // wall_clock lower but seq higher
        assert!(k1 < k2, "same epoch+ws: should order by seq");
    }

    #[test]
    fn ordering_same_epoch_different_ws() {
        let k1 = key('a', "agent-1", 1, 100);
        let k2 = key('a', "agent-2", 1, 100);
        assert!(
            k1 < k2,
            "same epoch+seq: should order by workspace_id lexicographic"
        );
    }

    #[test]
    fn ordering_different_epoch() {
        let k1 = key('a', "w1", 100, 100);
        let k2 = key('b', "w1", 1, 1);
        assert!(k1 < k2, "different epoch: epoch_id comparison comes first");
    }

    #[test]
    fn ordering_wall_clock_does_not_affect_ordering() {
        let k1 = key('a', "w1", 1, 9999);
        let k2 = key('a', "w1", 1, 1);
        assert_eq!(
            k1.cmp(&k2),
            Ordering::Equal,
            "wall_clock must not affect ordering"
        );
    }

    #[test]
    fn ordering_equality_ignores_wall_clock() {
        let k1 = key('a', "w1", 5, 100);
        let k2 = key('a', "w1", 5, 999);
        assert_eq!(k1, k2, "equality should ignore wall_clock");
    }

    #[test]
    fn ordering_inequality_by_seq() {
        let k1 = key('a', "w1", 1, 100);
        let k2 = key('a', "w1", 2, 100);
        assert_ne!(k1, k2);
    }

    #[test]
    fn ordering_is_total() {
        // Verify transitivity: a < b < c → a < c
        let a = key('a', "w1", 1, 0);
        let b = key('a', "w1", 2, 0);
        let c = key('a', "w1", 3, 0);
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }

    // -----------------------------------------------------------------------
    // SequenceGenerator — monotonic seq + wall-clock clamp
    // -----------------------------------------------------------------------

    #[test]
    fn seq_gen_starts_at_zero() {
        let seq_gen = SequenceGenerator::new();
        assert_eq!(seq_gen.last_seq(), 0);
        assert_eq!(seq_gen.last_wall_clock_ms(), 0);
    }

    #[test]
    fn seq_gen_first_call_returns_1() {
        let mut seq_gen = SequenceGenerator::new();
        let (seq, _) = seq_gen.next_with_clock(1000);
        assert_eq!(seq, 1);
    }

    #[test]
    fn seq_gen_monotonic_sequence() {
        let mut seq_gen = SequenceGenerator::new();
        let (s1, _) = seq_gen.next_with_clock(100);
        let (s2, _) = seq_gen.next_with_clock(200);
        let (s3, _) = seq_gen.next_with_clock(300);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(s3, 3);
    }

    #[test]
    fn seq_gen_wall_clock_forward() {
        let mut seq_gen = SequenceGenerator::new();
        let (_, w1) = seq_gen.next_with_clock(1000);
        let (_, w2) = seq_gen.next_with_clock(2000);
        assert_eq!(w1, 1000);
        assert_eq!(w2, 2000);
    }

    #[test]
    fn seq_gen_wall_clock_backward_clamped() {
        let mut seq_gen = SequenceGenerator::new();
        let (_, w1) = seq_gen.next_with_clock(5000);
        assert_eq!(w1, 5000);

        // Clock goes backward — should clamp to last+1
        let (_, w2) = seq_gen.next_with_clock(3000);
        assert_eq!(w2, 5001, "backward clock should clamp to last+1");

        // Clock goes even further backward
        let (_, w3) = seq_gen.next_with_clock(1000);
        assert_eq!(w3, 5002, "still clamped");
    }

    #[test]
    fn seq_gen_wall_clock_same_time_clamped() {
        let mut seq_gen = SequenceGenerator::new();
        let (_, w1) = seq_gen.next_with_clock(1000);
        let (_, w2) = seq_gen.next_with_clock(1000);
        assert_eq!(w1, 1000);
        assert_eq!(w2, 1001, "same time should advance by 1");
    }

    #[test]
    fn seq_gen_resume() {
        let mut seq_gen = SequenceGenerator::resume(10, 5000);
        assert_eq!(seq_gen.last_seq(), 10);
        assert_eq!(seq_gen.last_wall_clock_ms(), 5000);

        let (seq, wall) = seq_gen.next_with_clock(6000);
        assert_eq!(seq, 11, "should continue from last_seq");
        assert_eq!(wall, 6000);
    }

    #[test]
    fn seq_gen_resume_backward_clock() {
        let mut seq_gen = SequenceGenerator::resume(5, 10000);

        // Clock went backward (VM resume scenario)
        let (seq, wall) = seq_gen.next_with_clock(8000);
        assert_eq!(seq, 6);
        assert_eq!(wall, 10001, "should clamp: max(8000, 10000+1)");
    }

    #[test]
    fn seq_gen_next_uses_real_clock() {
        let mut seq_gen = SequenceGenerator::new();
        let (seq, wall) = seq_gen.next();
        assert_eq!(seq, 1);
        assert!(wall > 0, "wall clock should be positive from system time");
        // Sanity: wall clock should be after 2024-01-01 (1704067200000 ms)
        assert!(
            wall > 1_704_067_200_000,
            "wall clock {wall} seems too small"
        );
    }

    // -----------------------------------------------------------------------
    // Serde round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn ordering_key_serde_roundtrip() {
        let k = key('f', "agent-3", 99, 123_456_789);
        let json = serde_json::to_string(&k).unwrap();
        let parsed: OrderingKey = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.epoch_id, k.epoch_id);
        assert_eq!(parsed.workspace_id, k.workspace_id);
        assert_eq!(parsed.seq, k.seq);
        assert_eq!(parsed.wall_clock_ms, k.wall_clock_ms);
    }

    #[test]
    fn seq_gen_serde_roundtrip() {
        let mut seq_gen = SequenceGenerator::new();
        seq_gen.next_with_clock(5000);
        seq_gen.next_with_clock(6000);

        let json = serde_json::to_string(&seq_gen).unwrap();
        let restored: SequenceGenerator = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.last_seq(), seq_gen.last_seq());
        assert_eq!(restored.last_wall_clock_ms(), seq_gen.last_wall_clock_ms());
    }

    // -----------------------------------------------------------------------
    // Ordering consistency with causal chain
    // -----------------------------------------------------------------------

    #[test]
    fn causal_chain_ordering() {
        // Simulate a workspace producing operations in sequence
        let mut seq_gen = SequenceGenerator::new();
        let e = epoch('a');
        let w = ws("agent-1");

        let mut keys = Vec::new();
        for clock in [100, 200, 300, 400, 500] {
            let (seq, wall) = seq_gen.next_with_clock(clock);
            keys.push(OrderingKey::new(e.clone(), w.clone(), seq, wall));
        }

        // Verify strict ascending order
        for window in keys.windows(2) {
            assert!(
                window[0] < window[1],
                "causal chain must be strictly ascending: {:?} should be < {:?}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn causal_chain_with_backward_clock() {
        // NTP step scenario: clock goes backward mid-chain
        let mut seq_gen = SequenceGenerator::new();
        let e = epoch('b');
        let w = ws("agent-2");

        let clocks = [1000, 2000, 500, 300, 4000]; // backward at index 2,3
        let mut keys = Vec::new();
        for &clock in &clocks {
            let (seq, wall) = seq_gen.next_with_clock(clock);
            keys.push(OrderingKey::new(e.clone(), w.clone(), seq, wall));
        }

        // Wall clocks should be monotonically non-decreasing
        for window in keys.windows(2) {
            assert!(
                window[0].wall_clock_ms < window[1].wall_clock_ms,
                "wall clock must be strictly increasing after clamp"
            );
        }

        // Ordering must still be strictly ascending
        for window in keys.windows(2) {
            assert!(window[0] < window[1]);
        }
    }

    #[test]
    fn cross_workspace_ordering_deterministic() {
        // Two workspaces in same epoch — ordering is by workspace_id then seq
        let e = epoch('a');
        let keys = vec![
            OrderingKey::new(e.clone(), ws("alpha"), 1, 100),
            OrderingKey::new(e.clone(), ws("alpha"), 2, 200),
            OrderingKey::new(e.clone(), ws("beta"), 1, 150),
            OrderingKey::new(e, ws("beta"), 2, 250),
        ];

        let mut sorted = keys;
        sorted.sort();

        // Expected: alpha:1, alpha:2, beta:1, beta:2
        assert_eq!(sorted[0].workspace_id, ws("alpha"));
        assert_eq!(sorted[0].seq, 1);
        assert_eq!(sorted[1].workspace_id, ws("alpha"));
        assert_eq!(sorted[1].seq, 2);
        assert_eq!(sorted[2].workspace_id, ws("beta"));
        assert_eq!(sorted[2].seq, 1);
        assert_eq!(sorted[3].workspace_id, ws("beta"));
        assert_eq!(sorted[3].seq, 2);
    }
}
