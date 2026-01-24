use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

/// Agents subcommands for AGENTS.md management
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

pub fn run(cmd: AgentsCommands) -> Result<()> {
    match cmd {
        AgentsCommands::Init { force } => init(force),
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

### Quick Reference

| Task | Command |
|------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Destroy workspace | `maw ws destroy <name>` |
| See all work | `jj log --all` |
| Check status | `maw ws status` |
| Sync stale workspace | `maw ws sync` |
| Merge all work | `maw ws merge --all` |

### Starting Work

```bash
# Create your workspace
maw ws create <your-name>
cd .workspaces/<your-name>

# Start working - jj tracks changes automatically
# Describe what you're doing
jj describe -m "wip: implementing feature X"
```

### During Work

```bash
# See your changes
jj diff
jj status

# Commit and continue (creates new empty working copy)
jj commit -m "feat: add feature X"

# See what other agents are doing
jj log --all
```

### Handling Stale Workspace

If you see "working copy is stale", run:

```bash
maw ws sync
```

This happens when the main repo changes while you're working.

### Finishing Work

```bash
# From the main workspace, merge all agent work
maw ws merge --all

# Or merge specific workspaces
maw ws merge alice bob

# Merge and clean up workspaces in one step
maw ws merge --all --destroy
```

### Resolving Conflicts

jj records conflicts in commits rather than blocking. If you see conflicts:

```bash
jj status  # shows conflicted files
# Edit the files to resolve (remove conflict markers)
jj describe -m "resolve: merge conflicts from feature X and Y"
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
                    .map(|i| i + MAW_SECTION_END.len())
                    .unwrap_or(content.len());

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

fn show() -> Result<()> {
    print!("{}", maw_instructions());
    Ok(())
}
