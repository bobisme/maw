# Cached Fields Audit (bn-2r57)

## Deleted: `rebase_conflict_count: u32`

**What it was**: A counter in `.manifold/workspaces/<name>.toml` tracking unresolved rebase conflicts.

**Why it drifted**: Written by `ws sync --rebase` on conflict, but never reset when the user resolved via plain git (`git add` + `git commit`). This caused `ws merge` to falsely block. Three patch releases (v0.58.3--v0.58.5) tried to fix this with increasingly complex reconciliation logic, all of which had their own edge cases.

**Ground truth**: `find_conflicted_files()` -- a worktree scan that detects `<<<<<<<` markers. Takes <10ms on typical workspaces.

**Fix**: Delete the field. All readers now call `find_conflicted_files()` directly. The reconciliation function `reconcile_rebase_conflict_count` is deleted entirely.

## Remaining metadata fields (keep)

| Field | Rationale |
|---|---|
| `mode: WorkspaceMode` | Declarative user intent (ephemeral vs persistent). Not derivable. |
| `template: Option<WorkspaceTemplate>` | User's chosen archetype. Not derivable from worktree state. |
| `template_defaults: Option<TemplateDefaults>` | Materialized from template at creation time. Stable. |
| `change_id: Option<String>` | Bound change tracking ID. Declarative, not derivable. |
| `description: Option<String>` | User-provided text. Not derivable. |

None of these remaining fields are counters or status caches. They all represent user intent or configuration choices that don't change unless the user explicitly changes them.

## Pattern

**Derive from ground truth; don't cache what drifts.** A cached counter that exists only to disagree with the worktree is worse than no counter, because it adds a third source of truth that different commands can disagree with. If derivation is cheap (<50ms), always derive. If derivation is expensive, keep the cache but treat it as a hint validated against ground truth on every read.
