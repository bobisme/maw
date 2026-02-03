use anyhow::Result;

/// Show jj intro for git users
#[allow(clippy::unnecessary_wraps)]
pub fn run() -> Result<()> {
    print!("{}", jj_intro_text());
    Ok(())
}

fn jj_intro_text() -> &'static str {
    r#"# jj Quick Reference for Git Users

jj is a git-compatible version control system. Your work is backed by git, you
can push/pull from GitHub, and git commands still work on the same repo.

## Mental Model: Key Differences

**1. No staging area** — jj tracks all changes automatically
   Git: edit → git add → git commit
   jj:  edit → done (changes are in your commit)

**2. You're always in a commit** — use 'describe' to set the message
   Git: working copy is separate, git commit creates new commit
   jj:  working copy IS a commit, 'describe' sets its message

**3. Conflicts don't block** — recorded in commits, resolve later
   Git: merge conflicts stop the operation
   jj:  operations complete, conflicts marked in files

**4. Change IDs are stable** — survive rebases
   Git: commit SHA changes on rebase
   jj:  change ID stays same, commit ID changes

## Essential Commands (Git → jj)

| Task                    | Git                      | jj / maw                           |
|-------------------------|--------------------------|----------------------------------- |
| See status              | git status               | jj status                          |
| See history             | git log --oneline        | jj log                             |
| See changes             | git diff                 | jj diff                            |
| Set commit message      | git commit --amend -m    | jj describe -m "message"           |
| Create new commit       | git commit -m            | jj commit -m "message"             |
| Rebase onto main        | git rebase main          | jj rebase -d main                  |
| Fetch from remote       | git fetch                | jj git fetch                       |
| Push to remote          | git push                 | jj git push                        |
| Undo last operation     | git reset --hard HEAD~   | jj undo                            |
| Abandon commit          | git reset --hard HEAD~   | jj abandon <change-id>             |

## How to Push to GitHub (The Common Question)

After you've merged agent work with `maw ws merge`, you have a merge commit in
your working copy but it's not on main yet. Here's how to push:

### Step 1: Check for push blockers

```bash
maw ws status   # Shows undescribed commits, conflicts, etc
```

If you see warnings about undescribed commits (commits with no message), fix them:

```bash
# Option A: Rebase onto main (skips scaffolding commits)
jj rebase -r @- -d main
#         @- = parent of working copy (your merge commit)

# Option B: Give them descriptions
jj describe <change-id> -m "workspace setup"
```

### Step 2: Move the 'main' bookmark to your merge commit

```bash
jj bookmark set main -r @-
#   bookmark = jj's name for branches
#   @- = parent of working copy (your merge commit)
```

### Step 3: Push to GitHub

```bash
jj git push
```

**IMPORTANT**: When jj says `Changes to push to origin:`, the push is ALREADY DONE.
This is different from git — jj reports what it pushed, not what it will push.
Do NOT run `git push` afterwards (it would fail or be a no-op).

### Step 4: Verify push succeeded (optional)

```bash
# Compare local and remote commit hashes — they should match
jj log -r main --no-graph -T 'commit_id.short()'
git ls-remote origin refs/heads/main | cut -c1-12
```

### Full example

```bash
# After maw ws merge alice bob
maw ws status              # Check for issues
jj rebase -r @- -d main    # Skip empty commits
jj bookmark set main -r @- # Point main at merge
jj git push                # Push to GitHub
```

## maw-Specific Notes

**Use `maw ws jj` for jj commands in workspaces:**

```bash
# Instead of: cd .workspaces/alice && jj describe -m "feat: ..."
# Use:        maw ws jj alice describe -m "feat: ..."
```

This works reliably in sandboxed environments where `cd` doesn't persist.

**Workspace syntax:**

```bash
alice@     # alice workspace's working copy commit
bob@       # bob workspace's working copy commit
@          # current workspace's working copy
@-         # parent of working copy
main       # bookmark (jj's branch)
main@origin # remote-tracking bookmark
```

## Common Revsets (jj's Query Language)

```bash
@             # Current working copy
@-            # Parent of working copy
main          # Bookmark named 'main'
main@origin   # Remote main
::@           # Ancestors of @ (all history leading to current)
main..@       # Commits between main and @ (what would be pushed)
conflicts()   # Commits with conflicts
empty()       # Commits with no file changes
```

## Daily Workflow

```bash
# Start work
jj new main -m "feat: new feature"

# Edit files (no git add needed)
vim src/main.rs

# Update commit message anytime
jj describe -m "feat: new feature (updated)"

# See what changed
jj diff
jj status

# When ready for next task, commit and start fresh
jj commit -m "feat: complete feature"

# Or keep working in same commit
# (just describe again with updated message)
```

## Stale Workspace

If you see "working copy is stale" — another workspace modified shared history:

```bash
maw ws sync
```

This is normal in multi-workspace setups. Run `maw ws sync` at session start.

## Conflict Resolution

Conflicts are recorded in commits. To resolve:

```bash
jj status                          # Shows conflicted files
# Edit files, remove <<<<<<< markers
jj describe -m "resolve: ..."     # Update commit message
```

## More Help

- Full jj docs: https://martinvonz.github.io/jj/
- maw agent guide: maw agents show
- System check: maw doctor
"#
}
