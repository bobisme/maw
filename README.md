# MAW - Multi-Agent Workflow

Coordinate multiple AI agents working on the same codebase using jj workspaces.

## Install

```bash
cargo install --git https://github.com/bobisme/maw
```

Requires [jj (Jujutsu)](https://martinvonz.github.io/jj/) to be installed.

## Quick Start

```bash
# Check your setup
maw doctor

# Create workspaces for agents
maw ws create alice
maw ws create bob

# Agents work in their workspaces
cd .workspaces/alice
# ... edit files ...
jj describe -m "feat: implement feature X"

# See all agent work
maw ws status

# Merge all agent work
maw ws merge --all --destroy
```

## Commands

| Command | Description |
|---------|-------------|
| `maw ws create <name>` | Create isolated workspace for an agent |
| `maw ws list` | List all workspaces |
| `maw ws status` | Show all agent work, conflicts, stale warnings |
| `maw ws sync` | Handle stale workspace |
| `maw ws merge --all` | Merge all agent workspaces |
| `maw ws destroy <name>` | Remove a workspace |
| `maw doctor` | Check system requirements |
| `maw agents init` | Add MAW section to AGENTS.md |

## How It Works

Each agent gets an isolated jj workspace in `.workspaces/<name>/`. Workspaces share the repository's backing store (no disk duplication) but have separate working copies.

Agents can edit files concurrently without blocking each other. jj records conflicts in commits rather than preventing work - resolve them when merging.

```
.workspaces/
  alice/     # Alice's isolated workspace
  bob/       # Bob's isolated workspace
  carol/     # Carol's isolated workspace
```

## Why jj?

- **Lock-free**: Multiple agents edit simultaneously, no locks
- **Instant workspaces**: Shared storage, separate working copies
- **Conflict recording**: Conflicts are recorded in commits, not blocking
- **Operation log**: Full history of what each agent did

## Optional Integrations

- **[botbus](https://github.com/anthropics/botbus)**: Agent coordination and claims
- **[beads](https://github.com/Dicklesworthstone/beads_rust)**: Issue tracking

## License

MIT
