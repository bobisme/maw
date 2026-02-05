# maw Feature Eval Framework

Simulation-based evaluation for maw features. Tests whether changes actually
reduce agent confusion and tool call overhead in realistic workflows.

## Philosophy

- **Before/after comparisons**: Every feature eval runs the same scenario with
  and without the feature to measure delta.
- **Non-leading prompts**: Agents get task descriptions, not instructions on
  _how_ to use maw. We measure whether they figure it out.
- **Quantitative scoring**: Tool calls, errors, and confusion markers are
  counted, not vibes-checked.
- **Reproducible**: Each eval creates fresh repos in /tmp, runs deterministic
  scenarios, and produces structured output.

## Scoring System

Each eval scenario produces these metrics:

| Metric | What it measures | How |
|--------|-----------------|-----|
| **tool_calls** | Total tool invocations (Bash, Read, Grep, etc.) | Count from agent transcript |
| **maw_commands** | maw/jj commands run | Count Bash calls containing `maw` or `jj` |
| **errors** | Non-zero exit codes from commands | Count failed Bash calls |
| **confusion_markers** | Agent expressing confusion, re-reading, retrying | Count re-reads of same file, repeated failed commands, "I'm confused" / "let me try again" / backtracking |
| **goal_achieved** | Did the agent complete the task? | Boolean (manual or heuristic check) |
| **recovery_steps** | Extra commands to recover from confusion | Commands after first error that aren't forward progress |

### Composite Score

```
efficiency = goal_achieved ? (1.0 / tool_calls) * 100 : 0
confusion_rate = confusion_markers / tool_calls
success_cost = tool_calls if goal_achieved else Inf
```

Lower `success_cost` and `confusion_rate` are better.
Higher `efficiency` is better.

## Eval Structure

### 1. Setup Phase

Create a test environment in /tmp:

```bash
# Create a project repo
REPO=/tmp/maw-eval-$$
mkdir -p $REPO && cd $REPO
git init && jj git init --colocate
# ... seed with code files ...
maw init

# For push scenarios: create a bare remote
REMOTE=/tmp/maw-eval-remote-$$
git init --bare $REMOTE
git remote add origin $REMOTE
# ... push initial commit ...
```

### 2. Scenario Phase

Define the task in plain English (non-leading):

> "You are working on a Rust project. An agent named 'alice' made changes in
> a workspace. Merge alice's work and push the result to origin."

The agent does NOT get told:
- Which maw commands to run
- That files might not be visible after merge
- How jj bookmarks work

### 3. Measurement Phase

Parse the agent's transcript for:
- Each tool call (type, args, exit code)
- Confusion indicators (re-reads, retries, "hmm", backtracking)
- Final state verification (did the code end up on main? did it push?)

### 4. Comparison Phase

Run same scenario with:
- **Before**: Old maw binary (or simulate by undoing new features)
- **After**: New maw binary with features

Compare metrics side-by-side.

## Scenario Library

### S1: Post-merge file visibility
- Setup: Create repo, create workspace, make changes, merge
- Task: "Verify the merged code is in the main repo"
- Measures: Can agent see files after merge without extra steps?

### S2: Push workflow
- Setup: Create repo with remote, make and merge changes
- Task: "Push the latest changes to origin"
- Measures: Does agent figure out push without jj bookmark knowledge?

### S3: Custom branch name
- Setup: Create repo with `branch = "dev"` in .maw.toml
- Task: "Merge workspace work and push"
- Measures: Does maw correctly use configured branch?

### S4: Full workflow (create -> edit -> merge -> push)
- Setup: Fresh repo with remote
- Task: "Create a workspace, add a hello.py file, merge it, push to origin"
- Measures: End-to-end agent experience

## Running Evals

```bash
# From maw repo root
# Run a specific scenario
./notes/eval-harness.sh S1-before
./notes/eval-harness.sh S1-after

# Or run all
./notes/eval-harness.sh all
```

Results go to `/tmp/maw-eval-results/`.

## Adding New Scenarios

1. Add scenario to this doc
2. Add setup/task functions to `eval-harness.sh`
3. Run before/after
4. Write up in an eval report (`notes/eval-report-<feature>.md`)
5. File follow-up beads for issues found

## Interpreting Results

Key signals:
- **tool_calls dropped significantly**: Feature is working as intended
- **confusion_markers dropped**: Agent UX improved
- **errors increased**: New feature might have unclear output
- **goal_achieved flipped false->true**: Critical improvement
- **No change**: Feature isn't helping the case we thought it would
