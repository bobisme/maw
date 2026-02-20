# Workspace templates (bead archetypes)

`maw ws create` supports archetype templates to pre-seed agent-facing defaults.

## Usage

```bash
maw ws create <name> --template <archetype>
```

Supported archetypes:

- `feature` — user-facing changes
- `bugfix` — defect fixes and regressions
- `refactor` — internal behavior-preserving changes
- `eval` — exploration/prototype work
- `release` — release prep work

## What templates provide

Each template carries machine-readable metadata:

- `merge_policy` (policy hint for orchestrators)
- `default_checks` (recommended first checks)
- `recommended_validation` (extra validation commands)

This metadata is materialized in two places:

1. Workspace metadata: `.manifold/workspaces/<name>.toml`
2. Workspace-local artifact: `ws/<name>/.manifold/workspace-template.json`

## Machine-readable output

`maw ws list --format json` includes, per workspace (when templated):

- `template`
- `template_defaults` (merge policy + checks + validations)

This allows orchestrators to discover selected archetype and effective defaults
without parsing human text output.
