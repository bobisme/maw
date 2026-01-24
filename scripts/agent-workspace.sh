#!/usr/bin/env bash
# Create or destroy a jj workspace for an agent
# Workspaces live in .workspaces/<name>/ inside the project
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
WORKSPACES_DIR="$PROJECT_ROOT/.workspaces"

usage() {
	echo "Usage: $0 <command> <agent-name>"
	echo ""
	echo "Commands:"
	echo "  create <name>   Create a workspace for an agent"
	echo "  destroy <name>  Remove an agent's workspace"
	echo "  list            List all workspaces"
	echo ""
	echo "Workspaces are created in .workspaces/<name>/"
	echo ""
	echo "Examples:"
	echo "  $0 create alice"
	echo "  $0 destroy alice"
	echo "  $0 list"
	exit 1
}

create_workspace() {
	local name="$1"
	local workspace_path="$WORKSPACES_DIR/$name"

	if [[ -d "$workspace_path" ]]; then
		echo "Error: Workspace already exists at $workspace_path"
		exit 1
	fi

	echo "Creating workspace for agent '$name' at .workspaces/$name ..."

	cd "$PROJECT_ROOT"
	mkdir -p "$WORKSPACES_DIR"

	# Create workspace starting from main branch (or current HEAD if no main)
	# The -r flag sets the parent revision for the new working copy
	if jj log -r main --no-graph -T '' 2>/dev/null; then
		jj workspace add "$workspace_path" --name "$name" -r main
	else
		jj workspace add "$workspace_path" --name "$name"
	fi

	echo ""
	echo "Workspace created! To start working:"
	echo ""
	echo "  cd $workspace_path"
	echo "  export BOTBUS_AGENT=$name"
	echo "  botbus register --name $name --description \"Description here\""
	echo ""
}

destroy_workspace() {
	local name="$1"
	local workspace_path="$WORKSPACES_DIR/$name"

	if [[ ! -d "$workspace_path" ]]; then
		echo "Error: Workspace does not exist at $workspace_path"
		exit 1
	fi

	echo "Destroying workspace for agent '$name'..."

	cd "$PROJECT_ROOT"
	jj workspace forget "$name" 2>/dev/null || true
	rm -rf "$workspace_path"

	echo "Workspace destroyed."
}

list_workspaces() {
	cd "$PROJECT_ROOT"
	echo "Workspaces:"
	jj workspace list

	if [[ -d "$WORKSPACES_DIR" ]]; then
		echo ""
		echo "Workspace directories in .workspaces/:"
		ls -1 "$WORKSPACES_DIR" 2>/dev/null || echo "  (none)"
	fi
}

# Main
[[ $# -lt 1 ]] && usage

command="$1"
shift

case "$command" in
create)
	[[ $# -lt 1 ]] && usage
	create_workspace "$1"
	;;
destroy)
	[[ $# -lt 1 ]] && usage
	destroy_workspace "$1"
	;;
list)
	list_workspaces
	;;
*)
	usage
	;;
esac
