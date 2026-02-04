# Maw Project Improvement Ideas

This document presents 30 improvement ideas for the maw project, critically evaluates each, and provides detailed implementation plans for the ones that pass scrutiny.

## Part 1: Initial 30 Ideas (Brief One-Liners)

1. Add `--dry-run` flag to `maw ws merge` to preview merge without committing
2. Implement workspace templates for common project setups
3. Add `maw ws copy` to duplicate a workspace with all its state
4. Implement merge conflict visualization in TUI
5. Add workspace history tracking (what commits were made, when)
6. Implement `maw ws diff` to compare two workspaces
7. Add performance caching for `jj workspace list` output
8. Implement workspace pinning to prevent accidental destruction
9. Add `maw ws archive` to compress and store inactive workspaces
10. Implement parallel workspace creation for batch operations
11. Add `maw ws rename` to rename a workspace
12. Implement workspace status indicators in shell prompt
13. Add configurable workspace naming schemes
14. Implement automatic stale workspace detection and notification
15. Add `maw ws export` to export workspace as patch/bundle
16. Implement workspace isolation levels (full, partial, none)
17. Add `maw ws prune` to clean up all stale/orphaned workspaces
18. Implement workspace health checks beyond just stale status
19. Add structured logging with configurable verbosity
20. Implement pre/post-merge hooks for custom actions
21. Add `maw ws fork` to branch from another workspace's state
22. Implement workspace resource limits (disk space warnings)
23. Add `maw ws attach` to resume an orphaned jj workspace
24. Implement automatic conflict resolution strategies
25. Add workspace metadata storage (notes, tags, owner)
26. Implement `maw ws squash` to collapse workspace history
27. Add TUI file browser for viewing workspace contents
28. Implement workspace time travel (view state at point in time)
29. Add `maw ws sync --all` to sync all workspaces at once
30. Implement workspace dependency tracking (A depends on B's work)

---

## Part 2: Critical Evaluation

### Ideas Rejected with Reasoning

**2. Workspace templates** - REJECTED
- maw workspaces are just jj working copies, not project scaffolding. Templates belong in project generators, not version control tooling.

**3. `maw ws copy`** - REJECTED
- jj already handles this via `jj duplicate`. Adding this would be redundant and could confuse users about which tool does what.

**6. `maw ws diff`** - REJECTED
- `jj diff` already supports comparing any two revisions. Adding wrapper complexity provides minimal value over `jj diff ws1@ ws2@`.

**8. Workspace pinning** - REJECTED
- Low utility. The `--confirm` flag already provides protection. Adding pin state means maintaining more metadata.

**9. `maw ws archive`** - REJECTED
- Over-engineering. jj workspaces are lightweight; just destroy and recreate. Archive adds complexity for a rare use case.

**10. Parallel workspace creation** - REJECTED
- Workspaces are created serially by jj anyway. This would add complexity for no actual parallelism gain.

**11. `maw ws rename`** - REJECTED
- jj doesn't support workspace renaming. Would require destroy + recreate, which users can already do manually.

**12. Shell prompt indicators** - REJECTED
- Out of scope. This belongs in shell configuration, not maw. Users can add their own prompt integration.

**15. `maw ws export`** - REJECTED
- `jj git export` and `jj log -p` already provide this. Wrapper would be redundant.

**16. Workspace isolation levels** - REJECTED
- Over-engineering. The current full-isolation model is exactly right for agent coordination.

**21. `maw ws fork`** - REJECTED
- `maw ws create --revision ws2@` already does this. The `-r` flag exists for this purpose.

**22. Resource limits** - REJECTED
- Out of scope. Disk monitoring belongs in system tooling, not version control coordination.

**24. Automatic conflict resolution strategies** - REJECTED
- Already implemented via `.maw.toml` `auto_resolve_from_main`. The existing feature is sufficient.

**26. `maw ws squash`** - REJECTED
- `jj squash` already does this. No need for a wrapper.

**28. Time travel** - REJECTED
- `jj log` and `jj show` already provide this via operation log. Over-engineering.

**30. Workspace dependency tracking** - REJECTED
- Mixes coordination concerns (beads/botbus territory) with version control. Keep maw focused.

### Ideas Requiring More Thought (Deferred)

**4. Merge conflict visualization in TUI** - DEFERRED
- Good idea but the TUI is still being finalized. Better to stabilize the basic TUI first.

**7. Performance caching** - DEFERRED
- Premature optimization. Current performance is adequate for typical workspace counts (<10).

**13. Configurable naming schemes** - DEFERRED
- Nice-to-have but low priority. Current adjective-noun scheme works well.

**18. Enhanced workspace health checks** - DEFERRED
- Potentially useful but vague. Need concrete health indicators beyond stale status.

**19. Structured logging** - DEFERRED
- Would be nice for debugging but adds significant complexity. Consider after other features stabilize.

**27. TUI file browser** - DEFERRED
- Nice-to-have but TUI basics need to be solid first. File browsing is tangential to core purpose.

---

## Part 3: Ideas That Passed Scrutiny

The following 10 ideas passed critical evaluation and are worth implementing:

1. `--dry-run` flag for merge
5. Workspace history tracking
14. Automatic stale workspace detection
17. `maw ws prune` command
20. Pre/post-merge hooks
23. `maw ws attach` command
25. Workspace metadata
29. `maw ws sync --all`

Plus two bonus ideas from analysis:

- Better error recovery guidance
- Undescribed commit auto-fix

---

## Part 4: Detailed Implementation Plans

### Idea 1: `--dry-run` Flag for Merge

**What it is:**
Add a `--dry-run` flag to `maw ws merge` that shows what the merge would do without actually creating any commits. This lets users preview conflicts, see which files would change, and validate the merge before committing.

**Concrete implementation plan:**

```rust
// In workspace.rs, add to WorkspaceCommands::Merge
Merge {
    #[arg(long)]
    dry_run: bool,  // NEW
    // ... existing fields
}

// In merge() function, add early exit for dry-run
fn merge(workspaces: &[String], dry_run: bool, ...) -> Result<()> {
    // ... existing validation ...

    if dry_run {
        return preview_merge(workspaces);
    }

    // ... existing merge logic ...
}

fn preview_merge(workspaces: &[String]) -> Result<()> {
    println!("=== Merge Preview ===");
    println!("Workspaces to merge: {}", workspaces.join(", "));
    println!();

    // Show each workspace's changes
    for ws in workspaces {
        let diff = Command::new("jj")
            .args(["diff", "-r", &format!("{ws}@"), "--stat"])
            .output()?;
        println!("Changes in {}:", ws);
        println!("{}", String::from_utf8_lossy(&diff.stdout));
    }

    // Check for potential conflicts by comparing file lists
    // Use jj's internal conflict detection via interdiff
    let mut args = vec!["interdiff"];
    for ws in workspaces {
        args.push("-r");
        args.push(&format!("{ws}@"));
    }

    let conflict_check = Command::new("jj")
        .args(&args)
        .output()?;

    if !conflict_check.stdout.is_empty() {
        println!("Potential conflicts detected:");
        println!("{}", String::from_utf8_lossy(&conflict_check.stdout));
    } else {
        println!("No conflicts expected.");
    }

    println!();
    println!("Run without --dry-run to perform the merge.");
    Ok(())
}
```

**Why it's a good improvement:**
- Prevents accidental merges that create conflicts
- Allows agents to validate merge safety before committing
- Follows Unix convention of previewing destructive operations
- Zero risk - adds a flag without changing existing behavior

**Possible downsides:**
- Adds code complexity (~50 lines)
- Preview might not perfectly predict actual conflicts in edge cases

**Confidence: 85%**
This is a standard feature in merge tools. The main uncertainty is whether jj's interdiff provides the right conflict prediction.

---

### Idea 5: Workspace History Tracking

**What it is:**
Add `maw ws history <name>` to show a timeline of commits made in that workspace, making it easy to understand what work an agent did and when.

**Concrete implementation plan:**

```rust
// Add new subcommand
WorkspaceCommands::History {
    /// Workspace name
    name: String,

    /// Number of commits to show
    #[arg(short = 'n', long, default_value = "10")]
    limit: usize,
}

fn history(name: &str, limit: usize) -> Result<()> {
    validate_workspace_name(name)?;
    let path = workspace_path(name)?;

    if !path.exists() {
        bail!("Workspace '{name}' does not exist");
    }

    // Get commits from workspace's working copy through its ancestors
    // but only up to where it diverged from main
    let output = Command::new("jj")
        .args([
            "log",
            "-r", &format!("{}@:: & ~::main", name),
            "--no-graph",
            "-n", &limit.to_string(),
            "-T", r#"change_id.short() ++ " " ++ author.timestamp().format("%Y-%m-%d %H:%M") ++ " " ++ description.first_line() ++ "\n""#,
        ])
        .current_dir(&path)
        .output()
        .context("Failed to get workspace history")?;

    let history = String::from_utf8_lossy(&output.stdout);

    if history.trim().is_empty() {
        println!("Workspace '{}' has no commits yet.", name);
        println!("  (The workspace exists but hasn't been used.)");
        return Ok(());
    }

    println!("=== Workspace '{}' History ===", name);
    println!();
    for line in history.lines() {
        println!("  {}", line);
    }
    println!();
    println!("Use 'jj show <change-id>' to see full commit details.");

    Ok(())
}
```

**Why it's a good improvement:**
- Makes agent work more observable
- Helps debug what happened when things go wrong
- Simple to implement using existing jj capabilities
- Useful for lead devs coordinating multiple agents

**Possible downsides:**
- Adds another command to learn
- The revset `{}@:: & ~::main` might not work for all workspace patterns

**Confidence: 80%**
The concept is solid. The revset needs testing to ensure it captures the right commits.

---

### Idea 14: Automatic Stale Workspace Detection

**What it is:**
When running any workspace command, automatically detect stale workspaces and display a warning at the end of output. This catches stale state early without requiring explicit `maw ws status` calls.

**Concrete implementation plan:**

```rust
// Add utility function for stale detection
fn check_workspace_staleness(name: &str) -> Option<String> {
    let path = workspace_path(name).ok()?;
    if !path.exists() {
        return None;
    }

    let output = Command::new("jj")
        .args(["status"])
        .current_dir(&path)
        .output()
        .ok()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("working copy is stale") {
        Some(name.to_string())
    } else {
        None
    }
}

// Add to end of list() function
fn list(...) -> Result<()> {
    // ... existing list code ...

    // Check for stale workspaces
    let stale: Vec<String> = workspace_names
        .iter()
        .filter_map(|ws| check_workspace_staleness(ws))
        .collect();

    if !stale.is_empty() {
        println!();
        println!("WARNING: {} workspace(s) stale: {}", stale.len(), stale.join(", "));
        println!("  Fix: maw ws sync (from workspace) or maw ws jj <name> workspace update-stale");
    }

    Ok(())
}

// Add similar checks to other commands
```

**Why it's a good improvement:**
- Catches stale state before it causes confusion
- Agents often forget to check for staleness
- Non-intrusive - just a warning, doesn't block operations
- Makes the system more self-diagnosing

**Possible downsides:**
- Adds overhead to list operations (must check each workspace)
- Could be noisy in repos with many workspaces

**Confidence: 75%**
The concept is good. The main risk is performance impact on large workspace counts. Could be made optional via config.

---

### Idea 17: `maw ws prune` Command

**What it is:**
Add `maw ws prune` to clean up orphaned, stale, or empty workspaces in batch. This helps maintain hygiene when many workspaces accumulate.

**Concrete implementation plan:**

```rust
WorkspaceCommands::Prune {
    /// Actually delete (without this, just shows what would be pruned)
    #[arg(long)]
    force: bool,

    /// Also remove workspaces with only empty commits
    #[arg(long)]
    empty: bool,
}

fn prune(force: bool, include_empty: bool) -> Result<()> {
    let root = ensure_repo_root()?;
    let ws_dir = root.join(".workspaces");

    // Get list of jj-known workspaces
    let jj_output = Command::new("jj")
        .args(["workspace", "list"])
        .output()?;
    let jj_workspaces: HashSet<String> = String::from_utf8_lossy(&jj_output.stdout)
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .collect();

    // Get list of directories in .workspaces
    let mut to_prune = Vec::new();

    if ws_dir.exists() {
        for entry in std::fs::read_dir(&ws_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            if name == "default" {
                continue;
            }

            // Case 1: Directory exists but workspace forgotten
            if !jj_workspaces.contains(&name) {
                to_prune.push((name, "orphaned (not in jj)"));
                continue;
            }

            // Case 2: Empty workspace (if flag set)
            if include_empty {
                let diff = Command::new("jj")
                    .args(["diff", "-r", &format!("{name}@")])
                    .current_dir(&ws_dir.join(&name))
                    .output()?;
                if diff.stdout.is_empty() {
                    to_prune.push((name, "empty (no changes)"));
                }
            }
        }
    }

    // Also check for jj workspaces without directories
    for ws in &jj_workspaces {
        if ws != "default" && !ws_dir.join(ws).exists() {
            to_prune.push((ws.clone(), "missing directory"));
        }
    }

    if to_prune.is_empty() {
        println!("No workspaces to prune.");
        return Ok(());
    }

    println!("Workspaces to prune:");
    for (name, reason) in &to_prune {
        println!("  - {} ({})", name, reason);
    }

    if !force {
        println!();
        println!("Run with --force to actually delete these workspaces.");
        return Ok(());
    }

    // Actually prune
    for (name, _) in &to_prune {
        let _ = Command::new("jj")
            .args(["workspace", "forget", name])
            .status();
        let path = ws_dir.join(name);
        if path.exists() {
            std::fs::remove_dir_all(&path).ok();
        }
        println!("Pruned: {}", name);
    }

    println!("Pruned {} workspace(s).", to_prune.len());
    Ok(())
}
```

**Why it's a good improvement:**
- Maintains repo hygiene automatically
- Catches orphaned state that accumulates over time
- Safe by default (requires --force to actually delete)
- Useful for long-running projects with many agents

**Possible downsides:**
- Could accidentally delete workspaces with important work
- "Empty" detection might be overly aggressive

**Confidence: 85%**
The design is conservative (requires --force) and covers real maintenance needs.

---

### Idea 20: Pre/Post-Merge Hooks

**What it is:**
Add support for `.maw.toml` hook configuration that runs commands before and after merge operations. This enables custom validation, notifications, or cleanup.

**Concrete implementation plan:**

```toml
# .maw.toml additions
[hooks]
# Run before merge (abort if non-zero exit)
pre_merge = ["cargo check", "cargo test --no-run"]

# Run after merge (informational, doesn't block)
post_merge = ["echo 'Merge complete'"]
```

```rust
// In workspace.rs, extend MawConfig
#[derive(Debug, Default, Deserialize)]
struct HooksConfig {
    #[serde(default)]
    pre_merge: Vec<String>,
    #[serde(default)]
    post_merge: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct MawConfig {
    #[serde(default)]
    merge: MergeConfig,
    #[serde(default)]
    hooks: HooksConfig,
}

// Add hook runner
fn run_hooks(hooks: &[String], phase: &str, root: &Path) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }

    println!("Running {} hooks...", phase);
    for cmd in hooks {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let status = Command::new(parts[0])
            .args(&parts[1..])
            .current_dir(root)
            .status()
            .with_context(|| format!("Failed to run hook: {}", cmd))?;

        if !status.success() {
            if phase == "pre_merge" {
                bail!("Pre-merge hook failed: {}\n  Merge aborted.", cmd);
            } else {
                eprintln!("Warning: Post-merge hook failed: {}", cmd);
            }
        }
    }
    Ok(())
}

// In merge() function
fn merge(...) -> Result<()> {
    let root = ensure_repo_root()?;
    let config = MawConfig::load(&root)?;

    // Pre-merge hooks (abort on failure)
    run_hooks(&config.hooks.pre_merge, "pre_merge", &root)?;

    // ... existing merge logic ...

    // Post-merge hooks (don't abort on failure)
    if let Err(e) = run_hooks(&config.hooks.post_merge, "post_merge", &root) {
        eprintln!("Warning: Post-merge hook error: {}", e);
    }

    Ok(())
}
```

**Why it's a good improvement:**
- Enables custom validation (run tests before merge)
- Allows integration with external systems (notifications)
- Follows established hook pattern from git
- Configurable per-project via .maw.toml

**Possible downsides:**
- Hook commands are run with shell splitting, which could be fragile
- Could slow down merges if hooks are slow
- Security consideration: hooks run arbitrary commands

**Confidence: 75%**
Good feature but needs careful implementation. The shell command splitting is a common source of bugs.

---

### Idea 23: `maw ws attach` Command

**What it is:**
Add `maw ws attach <name>` to reconnect an orphaned workspace directory (one where `jj workspace forget` was run but the directory remains) back to jj's workspace tracking.

**Concrete implementation plan:**

```rust
WorkspaceCommands::Attach {
    /// Name of the orphaned workspace directory
    name: String,

    /// The revision to attach to (default: attempts to detect from .jj)
    #[arg(short, long)]
    revision: Option<String>,
}

fn attach(name: &str, revision: Option<&str>) -> Result<()> {
    let root = ensure_repo_root()?;
    let path = workspace_path(name)?;

    if !path.exists() {
        bail!(
            "Directory .workspaces/{} does not exist.\n  \
             Use 'maw ws create {}' to create a new workspace.",
            name, name
        );
    }

    // Check if already tracked
    let list = Command::new("jj")
        .args(["workspace", "list"])
        .output()?;
    let list_text = String::from_utf8_lossy(&list.stdout);
    if list_text.contains(&format!("{}:", name)) || list_text.contains(&format!("{}@:", name)) {
        bail!(
            "Workspace '{}' is already tracked by jj.\n  \
             Use 'maw ws list' to see all workspaces.",
            name
        );
    }

    println!("Attaching orphaned directory .workspaces/{} ...", name);

    // Determine revision to attach to
    let rev = if let Some(r) = revision {
        r.to_string()
    } else {
        // Try to find the last known working copy commit
        // This info might be in .jj/working_copy/
        let wc_file = path.join(".jj").join("working_copy").join("commit_id");
        if wc_file.exists() {
            std::fs::read_to_string(&wc_file)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "@".to_string())
        } else {
            "@".to_string()
        }
    };

    // Re-add the workspace
    let output = Command::new("jj")
        .args([
            "workspace", "add",
            path.to_str().unwrap(),
            "--name", name,
            "-r", &rev,
        ])
        .current_dir(&root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to attach workspace: {}\n  \
             Try specifying a revision: maw ws attach {} -r <rev>",
            stderr.trim(), name
        );
    }

    println!("Workspace '{}' attached successfully.", name);
    println!("  Run 'maw ws sync' if the working copy is stale.");

    Ok(())
}
```

**Why it's a good improvement:**
- Recovers from accidental `jj workspace forget`
- Prevents data loss from orphaned directories
- Complements the existing destroy command
- Useful for debugging and recovery

**Possible downsides:**
- The working_copy detection might not work reliably
- Could confuse users about workspace lifecycle

**Confidence: 70%**
The concept is useful but the implementation is tricky. The `.jj/working_copy/commit_id` path might not be stable across jj versions.

---

### Idea 25: Workspace Metadata Storage

**What it is:**
Add support for storing metadata (owner, notes, tags) with each workspace via a `.maw-meta` file, making workspaces self-documenting.

**Concrete implementation plan:**

```rust
// New structure for workspace metadata
#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspaceMeta {
    /// Who created/owns this workspace
    owner: Option<String>,
    /// Creation timestamp
    created_at: Option<String>,
    /// Free-form notes
    notes: Option<String>,
    /// Tags for categorization
    #[serde(default)]
    tags: Vec<String>,
    /// Associated issue/task ID
    task_id: Option<String>,
}

impl WorkspaceMeta {
    fn load(ws_path: &Path) -> Self {
        let meta_path = ws_path.join(".maw-meta");
        if meta_path.exists() {
            std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    fn save(&self, ws_path: &Path) -> Result<()> {
        let meta_path = ws_path.join(".maw-meta");
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&meta_path, content)?;
        Ok(())
    }
}

// Add new subcommand
WorkspaceCommands::Meta {
    /// Workspace name
    name: String,

    #[command(subcommand)]
    action: MetaAction,
}

#[derive(Subcommand)]
enum MetaAction {
    /// Show workspace metadata
    Show,
    /// Set metadata field
    Set {
        /// Field name (owner, notes, task_id)
        field: String,
        /// Value to set
        value: String,
    },
    /// Add a tag
    Tag { tag: String },
    /// Remove a tag
    Untag { tag: String },
}

// Update create() to initialize metadata
fn create(name: &str, ...) -> Result<()> {
    // ... existing create logic ...

    // Initialize metadata
    let meta = WorkspaceMeta {
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    meta.save(&path)?;

    // ... rest of create ...
}

// Update list() to show metadata
fn list(...) -> Result<()> {
    // ... in verbose mode, show metadata ...
    if verbose {
        let meta = WorkspaceMeta::load(&path);
        if let Some(owner) = &meta.owner {
            println!("    owner: {}", owner);
        }
        if !meta.tags.is_empty() {
            println!("    tags: {}", meta.tags.join(", "));
        }
    }
}
```

**Why it's a good improvement:**
- Makes workspaces self-documenting
- Helps track who owns what workspace
- Enables filtering/searching by metadata
- Useful for auditing and coordination

**Possible downsides:**
- Adds complexity to workspace lifecycle
- .maw-meta files would be gitignored (local-only)
- Metadata could become stale if not maintained

**Confidence: 70%**
Nice-to-have feature but not critical. The local-only nature limits usefulness for distributed teams.

---

### Idea 29: `maw ws sync --all`

**What it is:**
Add `--all` flag to `maw ws sync` to sync all workspaces at once, useful after pulling changes that affect multiple workspaces.

**Concrete implementation plan:**

```rust
// Update Sync command
WorkspaceCommands::Sync {
    /// Sync all workspaces instead of just current
    #[arg(long)]
    all: bool,
}

fn sync(all: bool) -> Result<()> {
    if !all {
        // Existing single-workspace sync
        return sync_current();
    }

    let root = repo_root()?;

    // Get all workspaces
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()?;

    let ws_list = String::from_utf8_lossy(&output.stdout);
    let workspaces: Vec<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("Syncing {} workspace(s)...", workspaces.len());
    println!();

    let mut synced = 0;
    let mut already_current = 0;
    let mut errors = Vec::new();

    for ws in &workspaces {
        let path = if ws == "default" {
            root.clone()
        } else {
            root.join(".workspaces").join(ws)
        };

        if !path.exists() {
            errors.push(format!("{}: directory missing", ws));
            continue;
        }

        // Check if stale
        let status = Command::new("jj")
            .args(["status"])
            .current_dir(&path)
            .output()?;

        let stderr = String::from_utf8_lossy(&status.stderr);
        if !stderr.contains("working copy is stale") {
            already_current += 1;
            continue;
        }

        // Sync
        let sync_result = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&path)
            .output();

        match sync_result {
            Ok(out) if out.status.success() => {
                println!("  {} - synced", ws);
                synced += 1;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                errors.push(format!("{}: {}", ws, err.trim()));
            }
            Err(e) => {
                errors.push(format!("{}: {}", ws, e));
            }
        }
    }

    println!();
    println!("Results: {} synced, {} already current, {} errors",
             synced, already_current, errors.len());

    if !errors.is_empty() {
        println!();
        println!("Errors:");
        for err in &errors {
            println!("  - {}", err);
        }
    }

    Ok(())
}
```

**Why it's a good improvement:**
- Common need after `jj git fetch` or receiving merged work
- Batch operation saves time vs. syncing each manually
- Reports results clearly for debugging

**Possible downsides:**
- Could be slow with many workspaces
- Errors in one workspace shouldn't block others (handled)

**Confidence: 90%**
Simple, useful feature with clear implementation. Low risk.

---

### Bonus Idea A: Better Error Recovery Guidance

**What it is:**
Enhance error messages throughout maw to always include a "Next steps" section with copy-pasteable commands, making recovery obvious for agents.

**Concrete implementation plan:**

```rust
// Create a helper macro or function for consistent error formatting
macro_rules! maw_error {
    ($msg:expr, $($fix:expr),+) => {
        anyhow::anyhow!(concat!(
            $msg,
            "\n\nTo fix:\n",
            $("  ", $fix, "\n",)+
        ))
    };
}

// Example usage in create()
if path.exists() {
    bail!(maw_error!(
        "Workspace already exists at {}",
        "maw ws destroy {} (to remove and recreate)",
        "maw ws list (to see existing workspaces)"
    ), path.display(), name);
}

// Or simpler function approach
fn error_with_fixes(msg: &str, fixes: &[&str]) -> anyhow::Error {
    let fix_lines: String = fixes
        .iter()
        .map(|f| format!("  {}", f))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::anyhow!("{}\n\nTo fix:\n{}", msg, fix_lines)
}

// Apply to all error sites
fn create(name: &str, ...) -> Result<()> {
    // Instead of:
    // bail!("Workspace already exists at {}", path.display());

    // Use:
    return Err(error_with_fixes(
        &format!("Workspace already exists at {}", path.display()),
        &[
            &format!("maw ws destroy {} (remove and recreate)", name),
            "maw ws list (see existing workspaces)",
        ]
    ));
}
```

**Why it's a good improvement:**
- Agents can't remember context between messages
- Copy-pasteable commands reduce friction
- Consistent error format aids debugging
- Follows maw's stated output guidelines

**Possible downsides:**
- More verbose errors might be annoying for humans
- Need to update every error site (time-consuming)

**Confidence: 85%**
Directly aligns with maw's stated goal of being agent-friendly.

---

### Bonus Idea B: Undescribed Commit Auto-Fix

**What it is:**
Add `--auto-describe` flag to merge that automatically gives empty descriptions to undescribed workspace scaffolding commits, preventing the common "can't push with empty descriptions" error.

**Concrete implementation plan:**

```rust
// Add flag to Merge command
Merge {
    // ... existing fields ...

    /// Auto-describe empty commits as "workspace setup"
    #[arg(long)]
    auto_describe: bool,
}

fn merge(..., auto_describe: bool, ...) -> Result<()> {
    // ... existing merge ...

    // After merge, before checking for undescribed commits
    if auto_describe {
        let empty_commits = Command::new("jj")
            .args([
                "log", "--no-graph",
                "-r", "description(exact:\"\") & ::@- & ~root()",
                "-T", "change_id.short() ++ \"\\n\"",
            ])
            .current_dir(&root)
            .output()?;

        let commits: Vec<&str> = String::from_utf8_lossy(&empty_commits.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();

        for change_id in commits {
            let _ = Command::new("jj")
                .args(["describe", change_id, "-m", "workspace setup"])
                .current_dir(&root)
                .status();
            println!("Auto-described {} as 'workspace setup'", change_id);
        }
    }

    // ... rest of merge ...
}
```

**Why it's a good improvement:**
- Removes friction from merge → push workflow
- Agents frequently hit the "undescribed commit" error
- Optional flag keeps default behavior safe
- Simple implementation

**Possible downsides:**
- Could hide important information (what did the empty commit intend?)
- "workspace setup" is generic and not very informative

**Confidence: 80%**
Practical solution to a common annoyance. The generic message is acceptable for scaffolding commits.

---

## Summary Table

| # | Idea | Status | Confidence | Complexity |
|---|------|--------|------------|------------|
| 1 | `--dry-run` for merge | **PASS** | 85% | Low |
| 5 | Workspace history | **PASS** | 80% | Low |
| 14 | Auto stale detection | **PASS** | 75% | Low |
| 17 | `maw ws prune` | **PASS** | 85% | Medium |
| 20 | Pre/post-merge hooks | **PASS** | 75% | Medium |
| 23 | `maw ws attach` | **PASS** | 70% | Medium |
| 25 | Workspace metadata | **PASS** | 70% | Medium |
| 29 | `maw ws sync --all` | **PASS** | 90% | Low |
| A | Better error guidance | **PASS** | 85% | Low |
| B | Auto-describe commits | **PASS** | 80% | Low |

## Recommended Priority Order

1. **`maw ws sync --all`** (90% confidence, low complexity) - Quick win
2. **`--dry-run` for merge** (85% confidence, low complexity) - High value
3. **Better error guidance** (85% confidence, low complexity) - Aligns with goals
4. **`maw ws prune`** (85% confidence, medium) - Maintenance essential
5. **Workspace history** (80% confidence, low) - Good visibility feature
6. **Auto-describe commits** (80% confidence, low) - Removes friction
7. **Pre/post-merge hooks** (75% confidence, medium) - Extensibility
8. **Auto stale detection** (75% confidence, low) - Nice-to-have
9. **`maw ws attach`** (70% confidence, medium) - Recovery tool
10. **Workspace metadata** (70% confidence, medium) - Nice-to-have
