# Local SG1 DST soak cron

Accrues fault-injected op-steps toward the **v1.0 release-gate floor of 1e8
op-steps at `ConditionProfile::default()` with 0 Oracle A/B violations**
(bn-2yzz; `notes/sg1-soak-campaign.md`). Replaces the GitHub Actions
`dst-soak.yml` cron, which timed out at the 180-min job cap every night and
accrued nothing (and burned free Actions minutes).

## Why local
The soak is **I/O-bound on git/worktree ops** (~30–46 op-steps/sec/core,
release or debug — build mode barely matters). 1e8 is therefore a multi-week
accrual at low parallelism. Run it as a background cron at `nice -19` +
`ionice -c3` so it yields CPU and disk to your foreground compiles.

## Mechanics
- A **frozen copy** of the prebuilt release `sg1_dst` test binary is pinned
  into the state dir at setup, so rebuilding/developing in the repo never
  perturbs an in-flight campaign. Re-pinning (a deliberate harness change)
  resets accrual per campaign §2 stop-condition 3.
- State lives **outside the repo** at `~/.local/state/maw-sg1-soak/` (override
  with `SG1_SOAK_STATE`) so cron never dirties your working tree.
- Each slot runs `SLOT_SEEDS` seeds × `STEPS` steps from a **disjoint** base
  seed (atomically allocated cursor), appends a row to `ledger.jsonl`, and adds
  `clean × STEPS` op-steps to `cumulative`.
- `flock` bounds concurrency to `PARALLEL` (default 2). Cron can fire often; if
  all slots are busy it exits immediately.
- **A slot whose binary exits non-zero = an Oracle violation.** The slot writes
  `STOP` + a `violations/*.log` and halts the campaign. That is the gate doing
  its job — investigate (shrink → fix → reset → restart), don't just retry.

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
scripts/sg1-soak/status.sh                       # cumulative, %, Wilson UB, rate, ETA, violations
systemctl --user list-timers 'sg1-soak@*'        # schedule
journalctl --user -u 'sg1-soak@*' -f             # live slot logs
```
- **Go faster:** `systemctl --user enable --now sg1-soak@3.timer` (and bump
  `PARALLEL=` in `~/.local/state/maw-sg1-soak/config.env` to ≥ instance count).
  It's `nice -19` + `ionice` idle, so extra workers mostly steal *idle* cycles
  from your compiles. **Slower:** disable an instance.
- **Pause:** `touch ~/.local/state/maw-sg1-soak/STOP` (remove to resume).
- **Stop entirely:** `systemctl --user disable --now sg1-soak@{1,2}.timer`.
- **On a violation (STOP appears):** read `violations/*.log`, replay with
  `SG1_SEED=<seed> just sg1-per-commit`, promote to the corpus, fix, then start
  a FRESH campaign (`rm -rf ~/.local/state/maw-sg1-soak && setup.sh`) — the
  Wilson bound resets to 0 on a fixed surface.
- **At DONE (1e8 reached):** fill `notes/sg1-soak-campaign.md` §7.1 (final N,
  Wilson CI, seed-range manifest = base_start..cursor, pinned harness SHA) and
  the §8 slot ledger from `ledger.jsonl`. That is the SG1 release-gate evidence.
