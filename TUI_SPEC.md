# MAW TUI Specification

A terminal UI for MAW, inspired by lazygit. Built with ratatui.

## Launch

```bash
maw ui
```

## Design Goals

1. **At-a-glance status** - See all agent workspaces and their state instantly
2. **Quick actions** - Common operations (create, merge, destroy) accessible via single keys
3. **Live updates** - Watch agents work in real-time
4. **Keyboard-driven** - Full control without mouse, vim-style navigation

## Layout

```
┌─[1]-Workspaces────────────────┬─[0]-Details─────────────────────────┐
│ > default        @ main       │ Workspace: agent-1                  │
│   agent-1        abc123 wip   │ Path: .workspaces/agent-1           │
│   agent-2        def456 feat  │ Change: abc123                      │
│   agent-3        (stale)      │ Status: active                      │
│                               │                                     │
│                               │ Description:                        │
│                               │   wip: implementing auth            │
│                               │                                     │
│                         3 of 4│ Files changed: 3                    │
├─[2]-Commits───────────────────┤   M src/auth.rs                     │
│ ○ abc123 wip: implementing    │   A src/auth_test.rs                │
│ ○ def456 feat: add user model │   M Cargo.toml                      │
│ ○─┬ 12c05c merge: agent work  │                                     │
│   ├─○ d654a4 feat(ws): status │                                     │
│   └─○ 869308 feat(ws): sync   │                                     │
│ ◆ 396d8a main: initial impl   ├─────────────────────────────────────┤
│                               │ [Command Log]                       │
│                        6 of 12│ > jj workspace list                 │
├─[3]-Issues────────────────────│ > jj log -r @                       │
│ ● bd-1cj [P1] Add TUI         │ > jj status                         │
│ ○ bd-v29 [P2] Add tests       │                                     │
│                         2 of 2│                                     │
└───────────────────────────────┴─────────────────────────────────────┘
 c:create  d:destroy  m:merge  s:sync  r:refresh  ?:help  q:quit
```

## Panels

### [1] Workspaces (Primary)

Lists all jj workspaces with their current state.

| Column | Description |
|--------|-------------|
| Selection | `>` marks selected row |
| Name | Workspace name |
| Current | `@` marks the workspace you're currently in |
| Change ID | Short change ID (8 chars) |
| Description | Commit description (truncated) |

**Status indicators** (shown after name):
- `(stale)` - Working copy is stale, needs sync
- `(conflict)` - Has unresolved conflicts
- `(empty)` - No changes yet

### [2] Commits

Shows `jj log --all` as a graph, highlighting commits from the selected workspace.

| Symbol | Meaning |
|--------|---------|
| `○` | Normal commit |
| `◆` | Immutable commit (pushed) |
| `●` | Commit with conflicts |
| `├─` `└─` | Merge lines |

### [3] Issues (Optional)

Shows beads issues if `.beads/` directory exists. Hidden if beads not configured.

| Column | Description |
|--------|-------------|
| Status | `●` open, `○` closed |
| ID | Issue ID (e.g., `bd-1cj`) |
| Priority | `[P1]` through `[P5]` |
| Title | Issue title (truncated) |

### [0] Details (Right Panel, Top)

Context-sensitive detail view based on selected panel:

**Workspace selected:**
```
Workspace: agent-1
Path: .workspaces/agent-1
Change: abc123def456
Status: active | stale | conflict

Description:
  wip: implementing authentication

Files changed: 3
  M src/auth.rs
  A src/auth_test.rs
  M Cargo.toml
```

**Commit selected:**
```
Commit: abc123def456
Author: bob@example.com
Date: 2024-01-15 10:30:00

feat: add authentication module

This commit adds the basic auth module with
JWT token support.

Files: 3 changed, +150, -20
```

**Issue selected:**
```
bd-1cj: Add TUI for maw
Priority: P1 | Type: feature | Status: open

Description:
  Build a lazygit-inspired TUI using ratatui.
  Should show workspaces, commits, and issues.

Blocked by: (none)
Blocking: bd-2ab, bd-3cd
```

### Command Log (Right Panel, Bottom)

Shows recent commands executed by the TUI:

```
[Command Log]
> jj workspace list
> jj log -r 'all()' --no-graph -T '...'
> jj status
```

Scrollable with `j/k` when focused. Helps users understand what the TUI is doing.

## Keybindings

### Global

| Key | Action |
|-----|--------|
| `1` | Focus Workspaces panel |
| `2` | Focus Commits panel |
| `3` | Focus Issues panel |
| `0` | Focus Details panel |
| `Tab` | Cycle panels forward |
| `Shift+Tab` | Cycle panels backward |
| `r` | Refresh all data |
| `?` | Show help popup |
| `q` / `Ctrl+c` | Quit |

### Navigation (All Panels)

| Key | Action |
|-----|--------|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `g` | Go to top |
| `G` | Go to bottom |
| `Enter` | Select / expand |
| `/` | Search (future) |

### Workspaces Panel

| Key | Action |
|-----|--------|
| `c` | Create new workspace |
| `d` | Destroy selected workspace |
| `s` | Sync selected workspace (update-stale) |
| `m` | Merge selected workspace into current |
| `M` | Merge all workspaces |
| `Enter` | Show workspace details |

### Commits Panel

| Key | Action |
|-----|--------|
| `Enter` | Show commit details/diff |
| `e` | Edit commit message (jj describe) |

### Issues Panel

| Key | Action |
|-----|--------|
| `Enter` | Show issue details |
| `i` | Mark as in_progress |
| `x` | Close issue |
| `n` | Create new issue |

## Popups

### Create Workspace

```
┌─ Create Workspace ─────────────────┐
│                                    │
│  Name: agent-1█                    │
│                                    │
│  Base revision: [main]             │
│                                    │
│         [Create]  [Cancel]         │
│                                    │
│  Enter: confirm  Esc: cancel       │
└────────────────────────────────────┘
```

- Name field auto-validates (shows error for invalid names)
- Base revision dropdown: main, @, or type custom

### Confirm Destroy

```
┌─ Destroy Workspace ────────────────┐
│                                    │
│  Destroy workspace 'agent-1'?      │
│                                    │
│  This will:                        │
│  • Forget workspace from jj        │
│  • Delete .workspaces/agent-1/     │
│                                    │
│  ⚠ Unmerged changes will be lost!  │
│                                    │
│          [Yes]  [No]               │
│                                    │
│  y: confirm  n/Esc: cancel         │
└────────────────────────────────────┘
```

- Shows warning if workspace has unmerged changes
- Requires explicit confirmation

### Merge Workspaces

```
┌─ Merge Workspaces ─────────────────┐
│                                    │
│  Select workspaces to merge:       │
│                                    │
│  [x] agent-1     abc123 wip: auth  │
│  [x] agent-2     def456 feat: api  │
│  [ ] agent-3     (stale)           │
│                                    │
│  ─────────────────────────────────│
│  [ ] Destroy workspaces after      │
│                                    │
│  Message:                          │
│  merge: combine agent work█        │
│                                    │
│         [Merge]  [Cancel]          │
│                                    │
│  Space: toggle  Enter: merge       │
└────────────────────────────────────┘
```

- Checkbox list of workspaces
- Stale workspaces shown but unchecked by default
- Optional: destroy after merge
- Editable merge commit message

### Help

```
┌─ Keybindings ──────────────────────────────────────┐
│                                                    │
│  Navigation                                        │
│    j/k, ↑/↓    Move up/down                        │
│    g/G         Go to top/bottom                    │
│    1-3         Focus panel                         │
│    Tab         Cycle panels                        │
│                                                    │
│  Workspaces                                        │
│    c           Create new workspace                │
│    d           Destroy workspace                   │
│    s           Sync (fix stale)                    │
│    m           Merge selected                      │
│    M           Merge all                           │
│                                                    │
│  Issues                                            │
│    i           Mark in progress                    │
│    x           Close issue                         │
│    n           New issue                           │
│                                                    │
│  General                                           │
│    r           Refresh                             │
│    ?           This help                           │
│    q           Quit                                │
│                                                    │
│                    [Close]                         │
└────────────────────────────────────────────────────┘
```

## Color Scheme

Following lazygit conventions for familiarity:

| Element | Color |
|---------|-------|
| Panel titles | Cyan |
| Selected/focused | Cyan background |
| Current workspace (`@`) | Green |
| Stale indicator | Yellow |
| Conflict indicator | Red |
| Immutable commits | Blue |
| Issue priorities P1-P2 | Red |
| Issue priorities P3 | Yellow |
| Issue priorities P4-P5 | Gray |
| Keybindings in status bar | Yellow |
| Secondary text | Gray |

## Data Refresh

| Trigger | Action |
|---------|--------|
| Startup | Full refresh of all data |
| Focus change | Refresh details for selected item |
| Every 2s | Poll for workspace changes |
| `r` key | Force full refresh |
| After action | Refresh affected panels |

## Implementation

### Dependencies

```toml
[dependencies]
ratatui = "0.29"
crossterm = "0.28"
tokio = { version = "1", features = ["full", "rt-multi-thread"] }
```

### Module Structure

```
src/
  tui/
    mod.rs              # pub fn run() entry point
    app.rs              # App state struct
    ui.rs               # Main layout rendering
    event.rs            # Event handling (keys, resize)
    panels/
      mod.rs
      workspaces.rs     # Workspace list panel
      commits.rs        # Commit graph panel
      issues.rs         # Issues panel
      details.rs        # Detail view panel
      command_log.rs    # Command log panel
    popups/
      mod.rs
      create.rs         # Create workspace popup
      destroy.rs        # Confirm destroy popup
      merge.rs          # Merge options popup
      help.rs           # Help popup
    commands.rs         # Execute jj/br commands
    theme.rs            # Colors and styles
```

### App State

```rust
pub struct App {
    // Data
    workspaces: Vec<Workspace>,
    commits: Vec<Commit>,
    issues: Vec<Issue>,
    command_log: Vec<String>,
    
    // Selection
    focused_panel: Panel,
    workspace_state: ListState,
    commit_state: ListState,
    issue_state: ListState,
    
    // UI
    popup: Option<Popup>,
    should_quit: bool,
    last_refresh: Instant,
}

pub struct Workspace {
    name: String,
    change_id: String,
    commit_id: String,
    description: String,
    is_current: bool,
    is_stale: bool,
    has_conflict: bool,
}
```

## Future Enhancements

1. **Diff view** - Full diff viewing with syntax highlighting
2. **Botbus integration** - Show agent messages in a panel
3. **Split view** - Compare two workspaces side-by-side
4. **Search** - `/` to search across panels
5. **Mouse support** - Click to select, scroll
6. **Themes** - Light mode, custom colors

## Open Questions

1. Should Issues panel be hidden when beads unavailable, or show "not configured"?
2. Mouse support: yes/no? (lazygit supports it)
3. Should we show botbus messages if available?
