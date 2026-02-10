use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

/// Agents subcommands for `AGENTS.md` management
#[derive(Subcommand)]
pub enum AgentsCommands {
    /// Generate or update AGENTS.md with maw workflow instructions
    ///
    /// Creates or updates the AGENTS.md file with instructions for AI agents
    /// on how to use maw workspaces. If AGENTS.md already exists, this will
    /// append or update the maw section.
    Init {
        /// Overwrite existing maw section if present
        #[arg(short, long)]
        force: bool,
    },

    /// Print the maw section that would be added to AGENTS.md
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

## Multi-Agent Workspaces with maw

This project uses maw for coordinating multiple agents via jj (Jujutsu) workspaces.
jj is a **git-compatible version control system**. Key differences from git:
- **No staging area** — all changes are tracked automatically (no `git add`)
- **Always in a commit** — your work is always saved in a commit; use `describe` to set the message
- **Conflicts don't block** — jj records conflicts in commits, you resolve them later

Each agent gets an isolated working copy (like a git worktree but lightweight) and **their own commit** — you can edit files without blocking other agents.

### Quick Start

```bash
maw ws create <your-name>      # Creates workspace + your own commit
# Edit files using the absolute workspace path shown by create
# Set your commit message (like git commit --amend -m):
maw exec <your-name> -- jj describe -m "feat: what you did"
maw ws status                  # See all agent work
```

### Quick Reference

| Task | Command |
|------|---------|
| Create workspace | `maw ws create <name>` |
| Check status | `maw ws status` |
| Sync stale workspace | `maw ws sync` |
| Run command in workspace | `maw exec <name> -- <command>` |
| Merge work | `maw ws merge <a> <b>` |
| Destroy workspace | `maw ws destroy <name> --force` |

**Note:** Your workspace starts with an empty commit (no file changes yet). This is intentional — it gives you a dedicated commit immediately, preventing conflicts when multiple agents work concurrently. Just use `describe` to set the message as you work.

### Session Start

Always run at the beginning of a session:

```bash
maw ws sync                    # Handle stale workspace (safe if not stale)
maw ws status                  # See all agent work
```

### During Work

```bash
maw exec <name> -- jj diff                        # See changes (like git diff)
maw exec <name> -- jj log                         # See commit history (like git log)
maw exec <name> -- jj log -r 'working_copies()'   # See all workspace commits (revset query)
maw exec <name> -- jj describe -m "feat: ..."     # Set commit message (like git commit --amend -m)
maw exec <name> -- jj commit -m "feat: ..."       # Save current work + start new empty commit
```

`maw exec` runs any command in the workspace directory. Use this instead of `cd ws/<name> && ...` — it works reliably in sandboxed environments where cd doesn't persist.

### Stale Workspace

If you see "working copy is stale" (another workspace changed shared history — your files are outdated):

```bash
maw ws sync
```

### Conflicts

jj records conflicts in commits instead of blocking your work. If you see conflicts:

```bash
maw exec <name> -- jj status                      # Shows conflicted files
# Edit files to remove <<<<<<< conflict markers (similar to git)
maw exec <name> -- jj describe -m "resolve: ..."  # Update commit message
```

### Pushing to Remote (Coordinator)

After merging workspaces:

```bash
maw push                       # Push branch to origin (handles bookmarks automatically)
```

If you committed directly (not via merge), advance the branch first:

```bash
maw push --advance             # Move branch to parent of working copy, then push
```

For tagged releases:

```bash
maw release v1.2.3             # Tag + push branch + push tag in one step
```

**IMPORTANT**: When jj/maw says "Changes to push to origin:", the push is ALREADY DONE.
This is different from git — it reports what it pushed, not what it will push.
Do NOT run `git push` afterwards.

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
                println!("Updated maw section in AGENTS.md");
            } else {
                println!("maw section already exists in AGENTS.md");
                println!("Use --force to overwrite");
                return Ok(());
            }
        } else {
            // Append section
            let new_content = format!("{content}\n{section}");
            fs::write(agents_path, new_content).context("Failed to write AGENTS.md")?;
            println!("Added maw section to AGENTS.md");
        }
    } else {
        // Create new file
        let content = format!("# Agent Guide\n\n{section}");
        fs::write(agents_path, content).context("Failed to create AGENTS.md")?;
        println!("Created AGENTS.md with maw instructions");
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn show() -> Result<()> {
    print!("{}", maw_instructions());
    Ok(())
}
