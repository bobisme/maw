# git-worktrees-bare arm crib

You coordinate multi-agent work using **plain git worktrees plus a hand-rolled convention**. The repo is a bare repo at `<root>/repo.git/` with worktrees checked out under sibling directories. The default integration target is the `main` worktree (typically `<root>/main/`). `git` is on `$PATH`; there is no maw, no jj.

## Core verbs

- `git worktree add ../<name> -b <name>` — make a new worktree on a fresh branch.
- `git worktree list` — list all worktrees.
- `git worktree remove <name>` — remove a worktree's checkout (the branch ref stays unless you also `git branch -D <name>`).
- `git fetch origin` and `git rebase origin/main` (or `git pull --rebase`) — bring a stale worktree up to the latest integration head.
- `git diff <branch>..HEAD` — show a worktree's changes vs the integration head.
- `git checkout main && git merge --no-ff <a> [b ...]` — merge worktree branches into `main`. On conflict, `git status` shows the marker-laden files; resolve manually then `git add` + `git commit`. `git merge --abort` rolls back.
- `git push` / `git pull` — sync with the bare repo when applicable.

## The coordination convention

Without a coordinator the agents must follow a written convention to avoid stepping on each other:

- **Claim files**: before starting work on a workspace, write a small file like `.coord/claims/<name>.json` recording who owns it and when. Release it (delete or mark released) when done.
- **Merge ordering**: integrate one workspace at a time into `main`. Never merge two unrelated worktrees in parallel.
- **Stale worktrees**: rebase onto `origin/main` (or `main`) before requesting a merge.
- **Recovery**: deleted worktree branches still live in `git reflog` and the bare repo's object store. Use `git reflog show <branch>` and `git checkout <sha>` to restore a lost branch.
- **Cleanup**: after `git worktree remove`, run `git worktree prune` to clear stale metadata.

The full convention is the document at `notes/sg2-worktrees-convention.md` (and equivalent in this substrate). Read it before coordinating.

## Conflicts

There is no "conflict as data" behavior here — `git rebase` and `git merge` abort or stop on conflict and require manual resolution. Resolve files in place, `git add` them, then `git rebase --continue` or `git commit`.

## What to read first

- This crib (always).
- The coordination convention document.
- `git worktree --help`.
