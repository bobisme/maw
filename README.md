# MAW - Multi-Agent Workflow

An experiment in coordinating multiple AI agents working on the same codebase.

## Architecture

**jj (Jujutsu)** handles:
- File isolation via workspaces (shared storage, separate working copies)
- Lock-free concurrent edits
- Automatic conflict detection and recording
- Operation log for history/undo

**botbus** handles:
- Agent registration and identity
- Intent broadcasting ("I'm working on X")
- Semantic claims ("I own src/api/**")
- Real-time observability (TUI)

## Quick Start

```bash
# Initialize (already done)
jj git init
botbus init

# Create a workspace for an agent
./scripts/agent-workspace.sh create alice

# In that workspace, the agent registers and works
cd .workspaces/alice
export BOTBUS_AGENT=alice
botbus register --name alice --description "Working on feature X"
botbus claim "src/feature-x/**"
# ... do work ...
jj commit -m "feat: implement feature X"
botbus release --all

# Watch all agent activity (from any workspace)
botbus ui
```

## Workflow

1. **Agent starts**: Creates workspace, registers with botbus
2. **Agent claims**: Announces intent via botbus claims
3. **Agent works**: Edits files in isolated jj workspace
4. **Agent commits**: Uses jj to commit (conflicts recorded, not blocking)
5. **Agent finishes**: Releases claims, announces completion
6. **Human observes**: Uses botbus TUI to watch coordination

## Why This Combination?

| Need | Tool | Why |
|------|------|-----|
| File isolation | jj workspaces | No disk duplication, instant creation |
| Concurrent edits | jj | Lock-free by design, merges operations |
| Intent/claims | botbus | Semantic "I own this module" not just file locks |
| Observability | botbus TUI | Watch agents coordinate in real-time |
| Conflict handling | jj | Records conflicts in commits, resolve later |
| History/undo | jj operation log | See what each agent did, undo mistakes |

## Open Questions

- Do we need tooling that wraps both, or is convention enough?
- How should agents handle jj conflicts?
- Should botbus integrate with jj operation log?

## Test Notes

Agent A edited README
Agent B edited README
