# Local SG1 DST soak cron

Accrues fault-injected op-steps toward the **v1.0 release-gate floor of 1e8
op-steps at `ConditionProfile::sg1_soak()` with 0 oracle violations**
(bn-2yzz; `notes/sg1-soak-campaign.md`). Replaces the GitHub Actions
`dst-soak.yml` cron, which timed out at the 180-min job cap every night and
accrued nothing (and burned free Actions minutes).

## What a seed exercises (bn-1h9ue)
Since bn-1h9ue the in-proc driver has a real default worktree (`<root>/ws/default`)
and every modelled merge runs the **production** target update
(`maw_cli::workspace::update_default_workspace` — snapshot, checkout, replay of
the uncommitted trunk) in a self-exec'd helper of the pinned binary, so the
crash windows really `abort()`. `ConditionProfile::sg1_soak()` adds rich
uncommitted trunk edits (content, new files, deletions, exec-bit flips,
symlinks, file<->directory), workspace commits that change modes and entry
types, and crashes inside the target update (`dirty_trunk_crash_pct`). Per
step, besides Oracle A/B: `TrunkDirtyPreservation`, `TrunkDirtyDisplacement`
and the `TrunkReplayFaithfulness` reference model (`crates/maw-assurance/src/trunk.rs`).
Clean ledger rows also record `trunk_updates`, `trunk_crashes`,
`dirty_trunk_merges`, `displacement_checks` and `replay_checks`.

## Why local
The soak is **I/O-bound on git/worktree ops**. With the dirty-trunk tier a
64-step seed takes ~1.3 s of driver time (~50 op-steps/sec/core; ~0.7 s /
~90 op-steps/sec before bn-1h9ue), so 1e8 op-steps (1.56M seeds) at
`PARALLEL=2` is roughly 12 days of dedicated wall time (more under `nice`).
Run it as a background cron at `nice -19` + `ionice -c3` so it yields CPU and
disk to your foreground compiles.

## Mechanics
- A **frozen copy** of the prebuilt release `sg1_dst` test binary is pinned
  into the state dir at setup, so rebuilding/developing in the repo never
  perturbs an in-flight campaign. Re-pinning (a deliberate harness change)
  resets accrual per campaign §2 stop-condition 3.
- State lives **outside the repo** at `~/.local/state/maw-sg1-soak/` (override
  with `SG1_SOAK_STATE`) so cron never dirties your working tree.
- Each slot runs `SLOT_SEEDS` seeds × `STEPS` steps from a **disjoint** base
  seed (atomically allocated cursor), appends a row to `ledger.jsonl`, and adds
  `SLOT_SEEDS × STEPS` op-steps to `cumulative`. The harness also replays
  `CANONICAL_BN_CM63_SEED` in every slot; that fixed plan is judged but NOT
  counted (the ledger's `clean` includes it, `range_clean` does not). The slot
  only accrues if the harness's begin line echoes back the allocated
  `SLOT_SEEDS`/`STEPS`/base seed; any mismatch fails closed like a violation.
- `flock` bounds concurrency to `PARALLEL` (default 2). Cron can fire often; if
  all slots are busy it exits immediately.
- **A slot whose binary exits non-zero = an Oracle violation.** The slot writes
  `STOP` (first line `VIOLATION: …`) + a `violations/*.log` and halts the
  campaign. That is the gate doing its job — investigate (shrink → fix →
  reset → restart), don't just retry.
- **Exception: infrastructure failures (bn-30v6e).** When the host runs out of
  disk quota, disk space or file descriptors (EDQUOT / ENOSPC / EMFILE /
  ENFILE), the harness prints one line `[sg1] INFRA-FAILURE: <reason>` and
  exits **75** (`EX_TEMPFAIL`). The classifier is
  `crates/maw-assurance/src/infra.rs`. slot.sh then:
  - writes the output to `infra/base-<seed>-<ts>.log`;
  - appends a ledger row with `"status":"infra"` and `"op_steps":0`. The
    slot's seed range is consumed (never reused) so the seed manifest stays
    honest, but nothing accrues and it is **not** a violation;
  - does **not** write STOP, unless `INFRA_HALT_AFTER` (default 3, in
    `config.env`) slots in a row were infra. Then it writes STOP with first
    line `INFRA-HALT: …`, so a full disk still gets a human. A clean slot
    resets the streak (`infra_consecutive`).
  - Fail closed: it needs exit 75 AND a marker line at the start of a line
    AND no sign of a violation in the output. Any other shape (exit 75
    without the marker, the marker with another exit code, an error the
    classifier does not recognise) is a violation. The harness itself never
    exits 75 after it has seen an oracle violation in the same run.
- **TMPDIR.** Every seed creates a throwaway git repo under `$TMPDIR`. The
  systemd unit sets `TMPDIR=/var/tmp/maw-sg1-soak`; slot.sh uses that path
  when `TMPDIR` is unset, and `mkdir -p`s it. Do **not** point it at `/tmp`
  on this box: `/tmp` is a per-user-quota tmpfs, and EDQUOT there halted the
  pre.6 campaign at 96% with no oracle finding.

## Install (systemd user timer — this box has no cron)
A **templated** timer drives the workers: concurrency = number of enabled
instances (`@1`, `@2`, …), capped by `PARALLEL` in `config.env` as a backstop.
Each instance loops: run one ~18-min slot, wait 2 min, repeat.
```bash
# 1. (one time) build the release test binary, then pin a frozen copy:
cargo test --release -p maw-assurance --features oracles --test sg1_dst --no-run
scripts/sg1-soak/setup.sh                       # SG1_SOAK_PARALLEL=N to raise the cap

# 2. smoke-check the gate actually turns red before trusting a clean run:
just sg1-per-commit-smoke

# 3. install + enable the timer (runs without an active login via `loginctl enable-linger`):
cp scripts/sg1-soak/systemd/sg1-soak@.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
loginctl enable-linger "$USER"
systemctl --user enable --now sg1-soak@1.timer sg1-soak@2.timer   # 2 workers

# (cron alternative, if you `pacman -S cronie && systemctl enable --now cronie`:)
#   */10 * * * * $HOME/src/maw/scripts/sg1-soak/slot.sh >> $HOME/.local/state/maw-sg1-soak/cron.log 2>&1
```

## Operate
```bash
scripts/sg1-soak/status.sh                       # cumulative, %, Wilson UB, rate, ETA, violations, infra
systemctl --user list-timers 'sg1-soak@*'        # schedule
journalctl --user -u 'sg1-soak@*' -f             # live slot logs
```
- **Go faster:** `systemctl --user enable --now sg1-soak@3.timer` (and bump
  `PARALLEL=` in `~/.local/state/maw-sg1-soak/config.env` to ≥ instance count).
  It's `nice -19` + `ionice` idle, so extra workers mostly steal *idle* cycles
  from your compiles. **Slower:** disable an instance.
- **Pause:** `touch ~/.local/state/maw-sg1-soak/STOP` (remove to resume).
- **Stop entirely:** `systemctl --user disable --now sg1-soak@{1,2}.timer`.
- **On a violation (STOP says `VIOLATION:`):** read `violations/*.log`, replay
  with `SG1_SEED=<seed> just sg1-per-commit`, promote to the corpus, fix, then
  start a FRESH campaign (see *Reset* below) — the Wilson bound resets to 0 on
  a fixed surface.
- **On an INFRA-HALT (STOP says `INFRA-HALT:`):** read `infra/*.log`, free the
  resource (disk, quota, fd limit; check `TMPDIR`), then `rm STOP`. Accrual is
  intact: infra slots never added op-steps, so the campaign just resumes.
- **A bare/empty STOP** is a manual pause (or a halt from before bn-30v6e);
  status.sh reports it as "manual pause or violation". Check `violations/`
  before you remove it.

## Reset (fresh campaign)
A harness re-pin or a fix to the tested surface resets accrual. Archive the old
state dir instead of deleting it: its ledger and logs are evidence for the
campaign notes.
```bash
systemctl --user stop 'sg1-soak@*.timer'
mv ~/.local/state/maw-sg1-soak ~/.local/state/maw-sg1-soak.archive-$(date -u +%Y%m%dT%H%M%SZ)
cargo test --release -p maw-assurance --features oracles --test sg1_dst --no-run
scripts/sg1-soak/setup.sh
just sg1-per-commit-smoke
cp scripts/sg1-soak/systemd/sg1-soak@.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user start 'sg1-soak@1.timer' 'sg1-soak@2.timer'
```
Record the reset (why, archived path, final cumulative) in
`notes/sg1-soak-campaign.md`.
- **At DONE (1e8 reached):** fill `notes/sg1-soak-campaign.md` §7.1 (final N,
  Wilson CI, seed-range manifest = base_start..cursor, pinned harness SHA) and
  the §8 slot ledger from `ledger.jsonl`. That is the SG1 release-gate evidence.
