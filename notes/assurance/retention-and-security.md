# Recovery Retention and Security Policy

Canonical doc: `notes/assurance-plan.md`.

Status: normative policy draft
Purpose: define how long recovery data lives and how searchable recovery is handled safely

## 1) Policy goals

- preserve enough history to satisfy assurance guarantees
- avoid silent guarantee erosion through untracked pruning
- make secret exposure risk explicit for searchable snapshots

## 2) Retention policy

Default policy (safe baseline):

- no automatic pruning of `refs/manifold/recovery/**`
- no automatic pruning of required recovery artifacts under `.manifold/artifacts/**`

Rationale: guarantees G1-G6 remain unconditional for retained history.

If pruning is introduced later, it must be explicit and contract-aware:

1. prune command must support `--dry-run`
2. prune command must output exact refs/artifacts to be removed
3. prune action must require explicit confirmation flag
4. prune action must write a tombstone manifest under `.manifold/artifacts/prune/`
5. claims must declare the retention window boundary after prune adoption

## 3) Artifact/ref consistency requirements

Before deleting any recovery ref or artifact, tooling must verify:

- paired metadata exists (ref <-> artifact link)
- item is not referenced by open incident/review marker
- item is outside configured retention horizon (if horizon exists)

## 4) Security model for searchable recovery

Recovery snapshots may contain sensitive content, including secrets present in
untracked files at capture boundaries.

Operational requirements:

- repository permissions must restrict access to trusted operators/agents
- search output should remain bounded (context + max-hits) by default
- full byte extraction should require explicit `--show`/restore command

## 5) Logging and audit

Recommended minimum audit events:

- search invocation (`pattern hash`, filters, hit count)
- show invocation (ref/path)
- restore invocation (ref/new workspace)
- prune invocation (if supported)

Audit records should avoid logging raw secret-bearing snippets.

## 6) Secret handling guidance

- do not treat recovery search as a safe redaction boundary
- operators should assume hits may include credentials/tokens
- incident playbooks should include immediate secret rotation if exposed

## 7) CI/verification hooks

- contract drift check fails if retention/security docs diverge from claims
- integration tests verify bounded search output and explicit show/restore flows
- future prune tests must verify no violation of declared retention guarantees

## 8) Open decisions to resolve

1. whether to support automatic retention windows at all
2. who can run prune commands in shared environments
3. whether to add snippet redaction modes for common secret patterns
