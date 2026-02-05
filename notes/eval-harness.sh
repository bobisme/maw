#!/usr/bin/env bash
# maw eval harness - sets up test repos for simulation evals
# Usage: source this file, then call setup functions

set -euo pipefail

EVAL_BASE="/tmp/maw-eval-$$"
EVAL_RESULTS="/tmp/maw-eval-results"
mkdir -p "$EVAL_RESULTS"

log() { echo "[eval] $*"; }

# Create a test project with some Rust-like code
setup_project() {
    local dir="$1"
    local name="${2:-testproject}"

    mkdir -p "$dir"
    cd "$dir"

    git init -q
    git config user.email "eval@test.local"
    git config user.name "Eval"

    # Create a simple Rust-like project
    mkdir -p src
    cat > Cargo.toml << 'TOML'
[package]
name = "testproject"
version = "0.1.0"
edition = "2021"
TOML

    cat > src/main.rs << 'RUST'
fn main() {
    println!("Hello from testproject!");
}
RUST

    cat > README.md << 'MD'
# testproject

A simple test project.

## Usage

```bash
cargo run
```
MD

    git add -A
    git commit -q -m "Initial commit: project scaffold"

    # Init jj colocated
    jj git init --colocate 2>/dev/null || true
    log "Project created at $dir"
}

# Add a bare remote to push to
setup_remote() {
    local project_dir="$1"
    local remote_dir="${2:-${project_dir}-remote}"

    git init --bare -q "$remote_dir"
    cd "$project_dir"
    git remote add origin "$remote_dir" 2>/dev/null || git remote set-url origin "$remote_dir"
    git push -q origin main 2>/dev/null || git push -q origin master
    jj git fetch 2>/dev/null || true
    jj bookmark track main@origin 2>/dev/null || true
    log "Remote created at $remote_dir"
}

# Init maw in the project
setup_maw() {
    local dir="$1"
    cd "$dir"
    maw init 2>/dev/null
    jj describe -m "" 2>/dev/null  # clean working copy
    log "maw initialized"
}

# Create a workspace with some changes already made
setup_workspace_with_changes() {
    local dir="$1"
    local ws_name="${2:-alice}"

    cd "$dir"
    maw ws create "$ws_name" 2>/dev/null

    local ws_path="$dir/.workspaces/$ws_name"

    # Make some changes in the workspace
    cat > "$ws_path/src/main.rs" << 'RUST'
fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}

fn main() {
    let greeting = greet("world");
    println!("{}", greeting);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greet() {
        assert_eq!(greet("Alice"), "Hello, Alice!");
    }
}
RUST

    cat > "$ws_path/src/lib.rs" << 'RUST'
/// Add two numbers together.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }
}
RUST

    # Describe the workspace commit
    cd "$ws_path"
    jj describe -m "feat: add greeting function with tests and lib module" 2>/dev/null
    cd "$dir"
    log "Workspace '$ws_name' created with changes"
}

# --- Scenario Setups ---

# S1: Post-merge file visibility test
setup_s1() {
    local variant="${1:-after}"  # "before" or "after"
    local dir="$EVAL_BASE/s1-$variant"

    setup_project "$dir"
    setup_maw "$dir"
    setup_workspace_with_changes "$dir" "alice"

    echo "$dir"
}

# S2: Push workflow test
setup_s2() {
    local variant="${1:-after}"
    local dir="$EVAL_BASE/s2-$variant"

    setup_project "$dir"
    setup_remote "$dir"
    setup_maw "$dir"
    setup_workspace_with_changes "$dir" "alice"

    # Pre-merge the workspace so the task is just "push"
    cd "$dir"
    maw ws merge alice --destroy 2>/dev/null

    echo "$dir"
}

# S4: Full workflow (create -> edit -> merge -> push)
setup_s4() {
    local variant="${1:-after}"
    local dir="$EVAL_BASE/s4-$variant"

    setup_project "$dir"
    setup_remote "$dir"
    setup_maw "$dir"

    echo "$dir"
}

echo "Eval harness loaded. EVAL_BASE=$EVAL_BASE"
echo "Functions: setup_project, setup_remote, setup_maw, setup_workspace_with_changes"
echo "Scenarios: setup_s1, setup_s2, setup_s4"
