use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

/// Agents subcommands for `AGENTS.md` management
#[derive(Subcommand)]
pub enum AgentsCommands {
    /// Generate or update AGENTS.md with MAW workflow instructions
    ///
    /// Creates or updates the AGENTS.md file with instructions for AI agents
    /// on how to use MAW workspaces. If AGENTS.md already exists, this will
    /// append or update the MAW section.
    Init {
        /// Overwrite existing MAW section if present
        #[arg(short, long)]
        force: bool,
    },

    /// Print the MAW section that would be added to AGENTS.md
    Show,
}

pub fn run(cmd: &AgentsCommands) -> Result<()> {
    match cmd {
        AgentsCommands::Init { force } => init(*force),
        AgentsCommands::Show => show(),
    }
}

const MAW_SECTION_START: &str = "<!-- maw-agent-instructions-v1 -->";
const MAW_SECTION_END: &str = "<!-- end-maw-agent-instructions -->";

fn maw_instructions() -> String {
    format!(
        r#"{MAW_SECTION_START}

## Multi-Agent Workflow with MAW

This project uses MAW for coordinating multiple agents via jj workspaces.
Each agent gets an isolated working copy and **their own commit** - you can edit files without blocking other agents.

### Quick Start

```bash
maw ws create <your-name>      # Creates workspace + your own commit
cd .workspaces/<your-name>
# ... edit files ...
jj describe -m "feat: what you did"
maw ws status                  # See all agent work
```

### Quick Reference

| Task | Command |
|------|---------|
| Create workspace | `maw ws create <name>` |
| Check status | `maw ws status` |
| Sync stale workspace | `maw ws sync` |
| Merge work | `maw ws merge <a> <b>` |
| Destroy workspace | `maw ws destroy <name> --force` |

**Note:** Your workspace starts with an empty commit. This is intentional - it gives you ownership immediately, preventing conflicts when multiple agents work concurrently.

### Session Start

Always run at the beginning of a session:

```bash
maw ws sync                    # Handle stale workspace (safe if not stale)
maw ws status                  # See all agent work
```

### During Work

```bash
jj diff                        # See changes
jj log                         # See commit graph
jj log -r 'working_copies()'   # See all workspace commits
jj describe -m "feat: ..."     # Save work to your commit
jj commit -m "feat: ..."       # Commit and start fresh
```

### Stale Workspace

If you see "working copy is stale":

```bash
maw ws sync
```

### Conflicts

jj records conflicts in commits (non-blocking). If you see conflicts:

```bash
jj status                      # Shows conflicted files
# Edit files to resolve
jj describe -m "resolve: ..."
```

### Pushing to Remote (Coordinator)

After merging workspaces, move the bookmark and push:

```bash
jj bookmark set main -r @-     # Move main to merge commit
jj git push                    # Push to remote
```

{MAW_SECTION_END}
"#
    )
}

fn init(force: bool) -> Result<()> {
    let agents_path = Path::new("AGENTS.md");
    let section = maw_instructions();

    if agents_path.exists() {
        let content = fs::read_to_string(agents_path).context("Failed to read AGENTS.md")?;

        // Check if section already exists
        if content.contains(MAW_SECTION_START) {
            if force {
                // Replace existing section
                let start_idx = content.find(MAW_SECTION_START).unwrap();
                let end_idx = content
                    .find(MAW_SECTION_END)
                    .map_or(content.len(), |i| i + MAW_SECTION_END.len());

                let new_content = format!(
                    "{}{}{}",
                    &content[..start_idx],
                    section.trim(),
                    &content[end_idx..]
                );

                fs::write(agents_path, new_content).context("Failed to write AGENTS.md")?;
                println!("Updated MAW section in AGENTS.md");
            } else {
                println!("MAW section already exists in AGENTS.md");
                println!("Use --force to overwrite");
                return Ok(());
            }
        } else {
            // Append section
            let new_content = format!("{content}\n{section}");
            fs::write(agents_path, new_content).context("Failed to write AGENTS.md")?;
            println!("Added MAW section to AGENTS.md");
        }
    } else {
        // Create new file
        let content = format!("# Agent Guide\n\n{section}");
        fs::write(agents_path, content).context("Failed to create AGENTS.md")?;
        println!("Created AGENTS.md with MAW instructions");
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn show() -> Result<()> {
    print!("{}", maw_instructions());
    Ok(())
}
