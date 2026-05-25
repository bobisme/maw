# SG3 — proposed AGENTS.md changes (T3.2, bn-2sw3)

Per SP5 §6 risk #3 (AGENTS.md root-vs-stub indirection) and per the
SG3 layout design §1.2: in the consolidated `.maw/` layout the **root
IS the source** — AGENTS.md becomes a tracked file at root, not a stub
plus a `ws/default/AGENTS.md`. v2 repos keep the existing stub
indirection (the root remains bare).

Lead to apply these patches to `ws/default/AGENTS.md` and the root
`AGENTS.md` stub. Worker (this bone) is forbidden from editing
`ws/default/`; the patches are tracked here for the lead.

---

## A. Root AGENTS.md (NEW REPOS, consolidated)

The existing root `AGENTS.md` stub:

```
**Do not edit the root AGENTS.md for memories or instructions. Use the AGENTS.md in ws/default/.**
@ws/default/AGENTS.md
```

is correct for **v2 repos** (existing maw) and must be retained.

**For NEW consolidated repos**, the root AGENTS.md replaces the stub
with the real content (the file IS the source, no indirection). `maw
init` does not write an AGENTS.md by default — the user/lead creates
one when the project gets a CLAUDE/AGENTS contract. The consolidated
layout has no `ws/default/` so the stub indirection is meaningless and
must not be written.

## B. ws/default/AGENTS.md — patch for both layouts

Add an explicit "layout detection" preamble near the top of the
`## Architecture` section (after the `## Quick Start`):

```markdown
## Layout

This project uses one of two on-disk layouts. Detection is automatic
(`maw doctor` shows which one); commands work identically in both.

- **Consolidated `.maw/` layout** (v1.0 default for new repos):
  - Root is a normal git checkout — source files live AT the root
    (`src/`, `Cargo.toml`, `README.md`, etc.).
  - Workspaces live under `.maw/workspaces/<name>/`.
  - Manifold metadata lives under `.maw/manifold/`.
  - The default "workspace" IS the root checkout.
  - Path to your workspace: `.maw/workspaces/<name>/`.
- **Legacy v2 `ws/` layout** (existing repos):
  - Root is a bare repo (no source files at root).
  - Default workspace at `ws/default/`.
  - Workspaces under `ws/<name>/`.
  - Manifold metadata at `.manifold/`.
  - Path to your workspace: `ws/<name>/`.

Use `maw cd <name>` to print the absolute path (useful for `cd
"$(maw cd alice)"` in human shells). In agent loops prefer `maw exec
<name> -- <cmd>` — it is path-agnostic and the recommended interface
in either layout (SP5 §6 risk #2: the consolidated path is ~12 chars
longer; `maw exec` avoids the cumulative cost).
```

Update the "Quick Start" block's "Edit files in `ws/<your-name>/`" to:

```markdown
# Work in your workspace
# - Consolidated layout: edit files in .maw/workspaces/<your-name>/
# - Legacy v2 layout:    edit files in ws/<your-name>/
# (Use `maw cd <your-name>` to print the absolute path.)
```

Update the "Architecture" / "Directory Structure (maw v2)" section to
document **both** shapes:

```markdown
### Directory Structure

**Consolidated `.maw/` layout** (v1.0 default for new repos):

```
project-root/          ← normal git checkout WITH source files
├── src/, Cargo.toml, …  ← project content lives at root
├── .maw/
│   ├── workspaces/
│   │   ├── bn-1abc/    ← agent workspace
│   │   └── bn-2def/
│   ├── manifold/        ← maw metadata
│   ├── config.toml      ← bootstrap config
│   ├── .gitignore       ← tracks config.toml, ignores runtime
│   └── cache/           ← reserved
├── .gitignore            ← tracks /.maw/, /repo.git/, /.manifold/
├── .git → repo.git       ← gitfile redirect
└── repo.git/             ← git common-dir
```

**Legacy v2 `ws/` layout** (existing repos):

```
project-root/          ← bare repo (no source files here)
├── ws/
│   ├── default/       ← main working copy (AGENTS.md, src/, etc.)
│   └── bn-1abc/       ← agent workspace
├── .manifold/         ← maw metadata
├── .git              ← gitfile
└── repo.git/         ← git common-dir
```
```

Update the "Bones Quick Reference" table's `maw exec default -- bn ...`
guidance to call out that in the consolidated layout `default` is the
root checkout (the same `maw exec default -- bn ...` call is correct in
both layouts — the resolver dispatches on the detected layout).

Update the "Use `maw exec <ws> -- <command>` to run commands in a
workspace context" note to be the **primary** guidance (it is the path-
agnostic interface; `cd` doesn't persist across tool calls). The
existing wording already says this — just add the SP5 §6 risk #2
rationale ("the consolidated layout doubles the absolute-path length;
`maw exec` survives that") inline so future agents know the win.

---

## C. Migration touch-up (deferred to T3.3)

T3.3 (`maw migrate`) is responsible for:

- Removing the root `AGENTS.md` stub on migration (the consolidated
  root is the new authoritative source).
- Moving `ws/default/AGENTS.md` content to `AGENTS.md` at root.
- Ensuring the `.gitignore` shape matches what `maw init` would have
  written for a fresh consolidated repo (SP5 §6 risk #4).

T3.2 does NOT do the move (no `ws/` repo touches), only enables the
new shape for greenfield repos.
