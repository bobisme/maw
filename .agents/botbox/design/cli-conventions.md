# CLI Tool Design Conventions

Design CLI tools for three audiences: **humans**, **agents**, and **machines**. Each has different needs, but a well-designed CLI serves all three without compromise.

## Command Structure

Structure commands as `<tool> <nouns> <verb>`:

```bash
# Good - plural nouns, verb at end
br issues list
br issues create
crit reviews request
maw workspaces create

# Acceptable exceptions for common operations
br ready          # shorthand for frequent queries
tool doctor       # utility commands
tool version      # meta commands
```

Use plural nouns (`issues` not `issue`) for consistency. Make exceptions where singular reads better or for very common operations.

## Output Formats

Support three output formats via `--format` flag:

| Format | Audience | Description |
|--------|----------|-------------|
| `text` | Agents + pipes | Concise, structured plain text. Default for non-TTY. Token-efficient, parseable by convention. |
| `pretty` | Humans | Tables, color, box-drawing, alignment. Default for TTY. Never fed to LLMs or parsed. |
| `json` | Machines | Structured, parseable, stable schema. Object envelope with `advice` array. |

### Format Resolution

Resolution order (first match wins):

1. `--format` flag (explicit override)
2. `FORMAT` env var
3. TTY auto-detect: TTY → `pretty`, non-TTY → `text`

```bash
# Explicit flag always wins
tool items list --format json

# Environment variable sets default
FORMAT=json tool items list

# TTY auto-detection
tool items list          # → pretty (if TTY)
tool items list | jq .   # → text (piped, no TTY)
```

Agents always get `text` unless they explicitly request `--format json`. The `pretty` format exists solely for humans at a terminal — it is never the target for agent-consumable output.

### JSON Envelope Convention

**Every JSON response is an object.** Never return a bare array. Wrap collections in a named key and always include an `advice` array:

```json
{
  "workspaces": [
    { "name": "frost-castle", "status": "active" }
  ],
  "advice": [
    {
      "level": "warn",
      "type": "stale_workspace",
      "message": "Workspace 'frost-castle' has no changes in 3 days",
      "details": "Run `maw ws destroy frost-castle` to clean up"
    }
  ]
}
```

**Rules:**
- Top-level is always `{}`
- Collections use a descriptive plural key (`workspaces`, `reviews`, `claims`, `messages`)
- `advice` array is always present (empty `[]` when nothing to report)
- Each advice entry: `{ level: "info"|"warn"|"error", type: string, message: string, details?: string }`
- `type` is a machine-readable slug (`stale_workspace`, `missing_config`, `claim_expired`)
- `message` is a one-line human summary
- `details` is optional extended guidance (may include suggested commands)

**Singleton responses** (e.g., `show` commands) return the object directly with an `advice` array:

```json
{
  "id": "item-123",
  "title": "Fix login bug",
  "status": "open",
  "advice": []
}
```

**Mutation responses** return the result with `advice`:

```json
{
  "created": "item-456",
  "advice": [
    { "level": "info", "type": "next_step", "message": "Run `tool items update item-456 --status in_progress` to start work" }
  ]
}
```

### Text Output Guidelines

`text` format is the primary format agents consume. Design it for reliable field extraction without JSON parsing overhead.

**Structure rules:**
- **ID first** — every record starts with its ID (e.g., `item-123  Fix login bug  open`)
- **One record = one line group** — single-line for list items, indented block for detail views
- **Inline collections** — short lists joined with `, ` not newline-per-item (e.g., `Labels: bug, urgent`)
- **Parenthetical metadata** — secondary info in parens (e.g., `item-123  Fix login (P2, 3 comments)`)
- **Indented sub-properties** — nested data indented 2 spaces under parent

**Consistency rules:**
- **Fixed delimiters** — use `  ` (two spaces) between fields, `: ` for key-value pairs
- **No synonyms** — pick one term and use it everywhere (`status` not sometimes `state`)
- **Predictable field order** — same field order every time for the same command

**Omission rules:**
- **No prose** — no sentences, no "Successfully created!", no "Here are your results:"
- **No decoration** — no box-drawing, no emoji, no color codes
- **No redundancy** — don't echo back the command or repeat known context
- **Defaults invisible** — don't show fields at their default value unless requested

**LLM considerations:**
- **Self-labeling** — prefix ambiguous values with their field name (e.g., `Status: open` not just `open`)
- **Unambiguous delimiters** — fields separated by `  ` (two spaces), never single space (which appears in values)
- **Suggested next commands** — when there's an obvious workflow, include `Next: <command>`

### Pretty Format

`pretty` format is for humans reading terminal output. No constraints on structure — use whatever makes the data easiest to scan:

- Tables with column alignment
- ANSI colors and bold/dim
- Box-drawing characters for structure
- Progress bars and spinners on stderr
- Tree views for hierarchical data
- Summary statistics and counts

**Never** optimize `pretty` for token efficiency or parseability. Its only audience is a human at a terminal. Agents will never see it unless they explicitly request `--format pretty` (which they shouldn't).

## Help and Documentation

Every command and subcommand must have help. Help must include:

1. **Brief description** - One line explaining what it does
2. **Usage pattern** - The command syntax
3. **Examples** - Real, working examples showing common workflows
4. **Agent workflow** - The ideal sequence for automated use

```bash
$ tool items create --help
Create a new item in the tracker

Usage: tool items create [OPTIONS] --title <TITLE>

Options:
  -t, --title <TITLE>    Item title (required)
  -d, --desc <DESC>      Description
  -p, --priority <1-4>   Priority level [default: 2]
  --format <FORMAT>      Output format: text|pretty|json [default: auto]
  -h, --help             Print help

Examples:
  # Create a simple item
  tool items create --title "Fix login bug"

  # Create with full details
  tool items create --title "Add OAuth" --desc "Support Google OAuth" --priority 1

  # Agent workflow: create then update (two separate calls)
  # Call 1 - create returns the ID:
  tool items create --title "Task" --format json
  # {"id": "item-123", "title": "Task", "status": "open"}

  # Call 2 - agent parses output and uses ID in next call:
  tool items update item-123 --status in_progress
```

**Key principle:** Agents have no memory of previous tool usage. They will guess at flags and syntax. Design help to make correct usage obvious and incorrect usage fail fast with helpful errors.

## Standard Commands

### doctor

If your tool has prerequisites, configuration, or external dependencies, provide a `doctor` command:

```bash
$ tool doctor
[OK] Config file exists (~/.tool/config.json)
[OK] API key configured
[WARN] Cache directory missing, will be created on first use
[FAIL] Required dependency 'jq' not found in PATH

1 issue found. Run 'tool doctor --fix' to attempt auto-repair.
```

Doctor should:
- Check all prerequisites
- Validate configuration
- Test connectivity to external services
- Offer `--fix` to auto-repair what's possible
- Exit non-zero if any check fails

### config

If your tool has configuration, provide a `config` subcommand:

```bash
tool config list              # Show all config
tool config get some.key      # Get specific value
tool config set some.key val  # Set value
tool config unset some.key    # Remove value
tool config path              # Show config file location
```

### version

Always support version queries:

```bash
tool version      # Preferred
tool --version    # Also acceptable
```

Include version in bug reports and agent diagnostics.

## Exit Codes

Use consistent exit codes:

| Code | Meaning | Example |
|------|---------|---------|
| 0 | Success | Command completed normally |
| 1 | User error | Invalid arguments, missing required flags |
| 2 | System error | Network failure, file permission denied |

Document exit codes in help. Agents rely on these to determine success/failure.

## Error Handling

**Errors go to stderr.** Keep stdout clean for parsing.

**Errors must be actionable.** Include what went wrong and how to fix it:

```bash
# Bad
Error: config error

# Good
Error: Config file not found at ~/.tool/config.json
  Run 'tool init' to create default configuration
  Or set TOOL_CONFIG to specify a custom path
```

**Batch operations must report partial failures:**

```bash
$ tool items close item-1 item-2 item-3
Closed: item-1, item-2
Failed: item-3 (already closed)

Exit code: 1 (partial failure)
```

Exit non-zero if any item fails. Never silently succeed on 3/5 items.

## Destructive Operations

Three levels of protection:

### Level 1: Reversible operations
No confirmation needed. Just do it.
```bash
tool items update item-1 --status closed
```

### Level 2: Destructive but scriptable
Require `--yes` or `--force` flag. Prompt interactively if flag absent:
```bash
# Interactive - prompts for confirmation
tool items delete item-1

# Scripted - no prompt
tool items delete item-1 --yes
```

### Level 3: Human-only operations
Some operations are too dangerous for automation. These require an **interactive confirmation prompt that cannot be bypassed**:

```bash
$ tool data purge --all
WARNING: This will permanently delete all data.
Type 'purge my data' to confirm: _
```

- No `--yes` flag
- No stdin piping
- Must be a real TTY
- Confirmation phrase should be specific, not just 'y'

Use sparingly. Most operations should be Level 1 or 2.

## Idempotency

Commands should be safe to retry. Agents may run commands multiple times on failure:

```bash
# Good - running twice is fine
tool init                    # Creates config if missing, no-op if exists
tool items update X --status closed  # Closing closed item is no-op

# Dangerous without guards
tool items create --title "X"  # Creates duplicate on retry
```

For non-idempotent commands, consider:
- Return existing resource if duplicate detected
- Provide `--if-not-exists` flag
- Use unique identifiers in requests

## Preview Changes

Support `--dry-run` for mutations:

```bash
$ tool items delete item-1 item-2 --dry-run
Would delete:
  item-1: "Fix login bug"
  item-2: "Add OAuth"

Run without --dry-run to execute.
```

Agents can preview before committing to destructive actions.

## Agent Environment Constraints

Agents operate under specific constraints. Design for these:

### No persistent environment variables

Agents cannot run `export VAR=value` and have it persist. Always support flags:

```bash
# Bad - requires env setup
export TOOL_PROJECT=myproject
tool items list

# Good - flags work in single call
tool items list --project myproject

# Show flags in examples, not env vars
```

### No persistent working directory

Agents cannot `cd` and stay there. Each command runs fresh:

```bash
# Bad example in help
cd /path/to/project
tool init

# Good example in help
cd /path/to/project && tool init

# Or support --cwd
tool init --cwd /path/to/project
```

**Security note:** `--cwd` can allow agents to escape intended context. Consider whether your tool should support it, and if so, validate the path.

## Styling

### No emoji

Use unicode glyphs and ANSI colors instead:

```bash
# Bad
✅ Success! 🎉
❌ Failed 😢

# Good
[OK] Success
[FAIL] Failed

# Or with unicode symbols
● Success
▲ Warning
✗ Failed
```

### Respect color preferences

```bash
# Check in order:
1. --no-color flag (highest priority)
2. NO_COLOR environment variable
3. TERM=dumb
4. Not a TTY → disable color
5. Otherwise → enable color
```

### Progress output

Long operations should show progress on **stderr** so stdout stays parseable:

```bash
$ tool sync --format json 2>/dev/null
{"synced": 42, "status": "complete"}

$ tool sync
Syncing... [=====>    ] 50%
Synced 42 items.
```

## Token Efficiency

Agents pay per token, but they also have no memory of previous commands. Balance conciseness with actionable next steps:

```bash
# Wasteful - verbose prose, buries the useful info
The item with ID 'item-123' has been successfully created.
You can view it by running 'tool items show item-123'.
For more information about items, see the documentation at...

# Better - tl;dr with next step
Created: item-123
Next: tool items update item-123 --status in_progress
```

**Don't assume agents have read help.** Give them the command to run next. Be a tl;dr, not a man page.

See the **Text Output Guidelines** section above for detailed `text` format rules. The `pretty` format handles all human-facing presentation. The `text` format is the design target for agent usability.
