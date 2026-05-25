# T3.4 — guardrail relocation + workspace alias surface (bn-1jqo)

**Status:** worker-produced patch doc; lead applies to `ws/default/AGENTS.md`.
**Parent:** SG3 (bn-2yh1) — relocate the anti-skip guardrail off the
layout in preparation for the consolidated `.maw/` default (T3.2 shipped,
T3.5 is the formal eval gate this work feeds).
**Companion file:** `notes/sg3-agents-md-proposal.md` (T3.2 layout-detection
preamble; this doc extends that draft, it does NOT supersede it).
**Implementation:** clap-native visible aliases on the workspace
subcommand group (`crates/maw-cli/src/main.rs`); patch doc here mirrors
the T3.2 proposal-doc pattern (the worker is forbidden from touching
`ws/default/AGENTS.md` — the lead applies).

---

## Why this patch exists

The current `ws/` layout (root is bare, source under `ws/default/`) doubles
as an implicit guardrail: agents see "the repo root has nothing editable"
and that fact alone discourages root-file edits / skipping `maw exec`. With
the consolidated `.maw/` default (T3.2 shipped) the root **is** the source
checkout, so the layout no longer carries that signal. Per the
`maw-design-rationale-agent-fluency` memory and the bn-1jqo brief, the
guardrail belongs in the agent's *instructions*, not the *layout*:

1. **the path handed to the agent** (primary mechanism — `maw exec <ws> --`
   already sets cwd to the workspace),
2. **AGENTS.md guidance** (the agent's only durable context),
3. **an optional pre-tool guard hook** (documented, not enabled).

This patch (a) adds the AGENTS.md section that names mechanisms 1 + 2
explicitly and (b) documents the hook design for projects that want hard
enforcement. It also pins the **2026-05-25 terminology decision** —
`workspaces` stays canonical, `worktree` / `wt` are git-fluent aliases.

---

## Acceptance traceback (bn-1jqo)

| Acceptance bullet | Where it is satisfied |
|---|---|
| Agents reliably stay in their workspace in the SP3 harness without the `ws/` cue | Patch §A (Quick-Start sentence + Layout addendum: `maw exec <name>` cwd, absolute-path rule, `maw cd <name>` recipe), Patch §B (Guardrail section), Patch §D (Quick-Reference row) |
| Guardrail mechanism documented | Patch §B explicitly enumerates the three mechanisms; Patch §C documents the optional hook interface for projects that want hard enforcement |
| (scope addition) `maw worktree` / `maw wt` route to the same code as `maw ws` | `crates/maw-cli/src/main.rs` — one `#[command(visible_aliases = ["ws", "worktree", "wt"])]` annotation on the `Workspace` variant; four unit tests in `tests/` pin the contract |

---

## A. Quick Start — one-line guardrail callout

Replace the existing "Quick Start" block sentence:

> ```bash
> # Work in your workspace
> # ... edit files in ws/<your-name>/ ...
> ```

with the path-agnostic, layout-agnostic version (this also propagates the
T3.2-shipped consolidated/v2 dual-layout reality from the
`sg3-agents-md-proposal.md` draft):

```markdown
# Work in your workspace
#   - Path:           run `maw cd <your-name>` (prints absolute path).
#   - Editing files:  use absolute paths under that workspace ONLY.
#   - Running tools:  `maw exec <your-name> -- <cmd> <args>`
#     (this sets cwd to the workspace; the surrounding repo root is
#      OFF-LIMITS — see "Workspace Guardrail" below).
```

`maw cd <name>` is the canonical "where am I?" verb (T3.2-shipped). It
returns the workspace's absolute path in both layouts so the rest of the
section is layout-agnostic.

---

## B. New section — "Workspace Guardrail"

Insert a new section after `## Quick Start` and before
`## Workspace Naming`:

```markdown
## Workspace Guardrail — stay in your workspace

You have been assigned a workspace path (printed by `maw ws create`
and recoverable any time via `maw cd <your-name>`). **All file reads,
writes, edits, and tool invocations MUST be scoped to that path.** This
rule replaces the `ws/`-layout cue that older versions of maw relied on
for the same protection.

The guardrail rests on three reinforcing mechanisms:

1. **The path handed to you (primary).** `maw ws create <name>` prints
   an absolute workspace path AND `maw exec <name> -- <cmd>` runs the
   command with cwd set to that path. Prefer `maw exec` for every tool
   invocation — it is the *path-agnostic interface* that survives both
   on-disk layouts (legacy `ws/<name>/` and consolidated
   `.maw/workspaces/<name>/`) and the absolute-path doubling SP5 §6
   risk #2 calls out.

2. **AGENTS.md instructions (this section).** When you use Read/Write/
   Edit tools with absolute paths, the path itself is your guardrail.
   Always Read `maw cd <your-name>` (or remember the path printed by
   `maw ws create`) and reject any path that does NOT live under it.
   Do not edit files at the repo root, in `ws/default/`, or in any
   other workspace.

3. **Optional guard hook (off by default).** Projects that want hard
   enforcement can register a Claude-Code `PreToolUse` (or shell
   `pre-commit`) hook that asserts the tool's path argument is a
   descendant of the agent's workspace. See "Optional guard hook"
   below for the interface; the hook is opt-in because (1) and (2) are
   sufficient for well-behaved agents and the hook adds latency to
   every tool call.

**If you find yourself wanting to edit something outside your
workspace** (the root `AGENTS.md`, `ws/default/`, another workspace,
shared dotfiles): stop, post a question to your coordinator on the
project channel, or open a bone — do NOT edit. The Prime Invariant
("no work is ever lost") is *yours* to uphold by routing every change
through your workspace.

### Why this matters

Cross-workspace edits silently bypass `maw`'s merge engine: the change
never reaches the epoch-delta machinery, the destroy-recovery snapshot,
or the merge-conflict surface. From the lead-agent perspective the
change looks like a phantom — it is committed somewhere, but not in
any workspace `maw ws merge` knows about. The 2026-05-25 SG3 layout
flip (`.maw/` default) means the root looks like a normal git checkout,
which makes the temptation to "just edit the root file" stronger than
before. This section is the durable counter-pressure.

### Optional guard hook (interface, NOT enabled by default)

For projects that want hard enforcement (e.g. multi-agent eval
harnesses where a stray root edit invalidates a comparison), register
a `PreToolUse` hook in `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read|Write|Edit|MultiEdit|NotebookEdit",
        "command": "maw guard assert-cwd",
        "blocking": true
      }
    ]
  }
}
```

The hook contract (proposed; NOT shipped in T3.4):

- **Input:** environment variable `MAW_AGENT_WORKSPACE` (the absolute
  workspace path, set by `maw exec`) and the tool's path argument
  (passed via stdin as JSON by the harness).
- **Behaviour:** exit 0 if the path is a descendant of
  `$MAW_AGENT_WORKSPACE`; exit non-zero with a stderr message naming
  the offending path AND the recovery command
  (`maw exec <ws> -- <tool> <args>`) otherwise.
- **Why opt-in:** mechanisms (1) and (2) are sufficient for
  well-behaved agents; the hook adds latency to every Read/Write/Edit
  call (~5 ms cold + maw startup); the harness needs to set
  `MAW_AGENT_WORKSPACE` for the hook to know what to check, which
  is a coordination contract the lead chooses to adopt or not.
- **Failure mode:** if the env var is unset, the hook MUST exit 0
  (no false positives in non-agent shells).

A reference implementation lives in `crates/maw-cli/src/guard.rs` (TBD
follow-up bone — T3.4 only documents the interface).
```

---

## C. Workspace alias surface (the 2026-05-25 scope addition)

Insert immediately after the new "Workspace Guardrail" section (or
fold into the existing "## Workspace Commands" table — lead's call):

```markdown
## Command aliases — `ws`, `worktree`, `wt`

`maw workspace` is the canonical subcommand. Three visible aliases
route to the same code and accept the same arguments:

| Alias | Use case |
|---|---|
| `maw ws ...` | The short canonical short-form used in agent loops and docs. |
| `maw worktree ...` | Git-fluent alias for agents who reach for the git verb from muscle memory. |
| `maw wt ...` | Two-character git-fluent alias. |

All four invocations of `create` are equivalent:

```bash
maw workspace create alice --from main
maw ws        create alice --from main
maw worktree  create alice --from main
maw wt        create alice --from main
```

**Why three aliases?** Per the 2026-05-25 terminology decision,
*workspaces* stays canonical in commands, docs, and on-disk paths
(`ws/<name>/` legacy, `.maw/workspaces/<name>/` consolidated). The
`worktree` / `wt` aliases give agents who reach for the git-fluent
name from muscle memory a working command instead of an
`unrecognized subcommand` error, and serve as a future-switch escape
hatch: if the project ever flips terminology, the alias has already
been a public path so it is not a true breaking change.

Docs and example outputs continue to lead with `ws` (the short
canonical form). The aliases are shown by `maw --help` and
`maw workspace --help` so they are discoverable without reading
AGENTS.md first.
```

---

## D. "Workspace Quick Reference" table — add a guardrail row

In the existing "Workspace Quick Reference" table (currently inside the
edict-managed block) add a single row near the top:

```markdown
| Print my workspace path | `maw cd <my-name>` |
```

This makes the path-recovery verb discoverable from the same table that
already documents `maw ws create` / `maw ws list` / `maw ws merge`. The
row is layout-agnostic (T3.2-shipped: `maw cd` works in both legacy and
consolidated layouts).

---

## E. Root `AGENTS.md` stub — leave alone

The root `AGENTS.md` stub:

```
**Do not edit the root AGENTS.md for memories or instructions. Use the AGENTS.md in ws/default/.**
@ws/default/AGENTS.md
```

is correct for **v2 repos** (the current maw repo) and remains the
indirection target. The T3.2 proposal (sg3-agents-md-proposal.md §A)
covers what new consolidated repos should ship at root instead — that
is T3.3's territory (the `maw migrate` flow), not T3.4's.

---

## Out of scope (deferred follow-ups)

- `maw guard` subcommand and reference hook implementation — separate
  bone; T3.4 only ships the interface in this doc.
- AGENTS.md re-flow / table-of-contents updates — defer to whatever
  bone reorganises the full file (T2.8 / bn-u9iy on the agent-crib
  direction would be a natural place).
- T3.3 (`maw migrate`) is responsible for emitting the new root
  `AGENTS.md` at consolidated-repo migration time, including the
  guardrail section above as a default snippet for greenfield repos.
- T3.5 (the formal eval gate) will measure whether the patched
  AGENTS.md actually moves agents off root-file edits in the SP3
  harness; if the eval shows the guardrail is too soft, the follow-up
  bone is "ship the guard hook by default for the eval harness only".
