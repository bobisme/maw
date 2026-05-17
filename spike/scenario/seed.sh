#!/usr/bin/env bash
# Deterministic seed repo for the SP3 ergonomics scenario.
# Arm-agnostic: produces an identical starting tree for maw / git-wt / jj.
#
# Design goal: 2 tasks, 3 agents, with a DELIBERATE shared-file hotspot
# (lib.rs module list + Cargo-ish manifest) so concurrent agents *must*
# touch overlapping lines. That hotspot is what exercises the coordination
# layer — the entire point of the maw-vs-git-wt-vs-jj comparison. A
# scenario with zero overlap would make all three arms look identical and
# the benchmark non-informative.
set -euo pipefail
DEST="${1:?usage: seed.sh <dest-dir>}"
rm -rf "$DEST"
mkdir -p "$DEST/src"

cat > "$DEST/src/lib.rs" <<'EOF'
// Shared hotspot: every task adds a module here. Concurrent edits collide
// on this file by construction.
pub mod core;

pub fn version() -> &'static str {
    "0.0.0"
}
EOF

cat > "$DEST/src/core.rs" <<'EOF'
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}
EOF

cat > "$DEST/Cargo.toml" <<'EOF'
[package]
name = "sp3-scenario"
version = "0.0.0"
edition = "2021"
EOF

cat > "$DEST/TASKS.md" <<'EOF'
# Tasks (3 agents, 2 task units, shared hotspot)

- TASK-A (agent-1): add a `mul` function to a new module `arith`, register
  `pub mod arith;` in src/lib.rs, file src/arith.rs.
- TASK-B (agent-2): add a `sub` function to a new module `arith2`, register
  `pub mod arith2;` in src/lib.rs, file src/arith2.rs.
- TASK-C (agent-3): bump src/lib.rs `version()` return to "0.1.0" AND bump
  Cargo.toml version to "0.1.0".

All three edit src/lib.rs. agent-3 also edits Cargo.toml. By construction
the coordination layer must reconcile concurrent edits to src/lib.rs.
EOF

git -C "$DEST" init -q
git -C "$DEST" config user.email sp3@example.com
git -C "$DEST" config user.name sp3
git -C "$DEST" add -A
git -C "$DEST" commit -qm "seed"
echo "seeded: $DEST"
