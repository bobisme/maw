use anyhow::Result;

/// Show jj intro for git users
#[allow(clippy::unnecessary_wraps)]
pub fn run() -> Result<()> {
    print!("{}", jj_intro_text());
    Ok(())
}

#[allow(clippy::too_many_lines)]
const fn jj_intro_text() -> &'static str {
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
| Push to remote          | git push                 | maw push                           |
| Undo last operation     | git reset --hard HEAD~   | jj undo                            |
| Abandon commit          | git reset --hard HEAD~   | jj abandon <change-id>             |

## How to Push to GitHub

After merging agent work with `maw ws merge`, push to remote:

```bash
maw push                       # Push branch to origin (handles bookmarks automatically)
```

If you committed directly (not via merge), advance the branch first:

```bash
maw push --advance             # Move branch to your commit, then push
```

### For tagged releases

```bash
maw release v1.2.3             # Tag + push branch + push tag in one step
```

**IMPORTANT**: When maw/jj says `Changes to push to origin:`, the push is ALREADY DONE.
This is different from git — it reports what it pushed, not what it will push.
Do NOT run `git push` afterwards (it would fail or be a no-op).

### Under the Hood

`maw push` wraps `jj git push` with bookmark management and sync checks.
If you need manual control:

```bash
jj bookmark set main -r @-     # Point 'main' at parent of working copy
jj git push                    # Push to remote
```

## maw-Specific Notes

**Use `maw exec` for commands in workspaces:**

```bash
# Instead of: cd ws/alice && jj describe -m "feat: ..."
# Use:        maw exec alice -- jj describe -m "feat: ..."
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
