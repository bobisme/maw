#!/usr/bin/env python3

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
ARTIFACT_ROOT = Path(
    os.environ.get(
        "DST_ARTIFACT_DIR", Path(tempfile.gettempdir()) / "maw-dst-artifacts"
    )
)


def workflow_command(seeds: int) -> str:
    return (
        f"WORKFLOW_DST_TRACES={seeds} cargo test -p maw-workspaces --test workflow_dst "
        "dst_seeded_workflows_preserve_contracts_long_run -- --ignored --nocapture"
    )


def action_command(seeds: int, steps: int) -> str:
    return (
        f"ACTION_DST_TRACES={seeds} ACTION_DST_STEPS={steps} cargo test -p maw-workspaces --test action_workflow_dst "
        "dst_action_sequences_preserve_contracts_long_run -- --ignored --nocapture"
    )


def workflow_replay(seed: int) -> str:
    return (
        f"WORKFLOW_DST_SEED={seed} cargo test -p maw-workspaces --test workflow_dst "
        "dst_seeded_workflows_preserve_contracts -- --exact --nocapture"
    )


def action_replay(seed: int, steps: int) -> str:
    return (
        f"ACTION_DST_SEED={seed} ACTION_DST_STEPS={steps} cargo test -p maw-workspaces --test action_workflow_dst "
        "dst_action_sequences_preserve_contracts -- --exact --nocapture"
    )


def latest_artifact(harness: str | None) -> Path:
    dirs = []
    if harness:
        dirs.append(ARTIFACT_ROOT / harness)
    else:
        dirs.extend(
            [
                ARTIFACT_ROOT / "workflow-dst",
                ARTIFACT_ROOT / "action-workflow-dst",
            ]
        )
    best = None
    best_mtime = None
    for d in dirs:
        if not d.is_dir():
            continue
        for child in d.iterdir():
            if not child.is_dir():
                continue
            candidate = child / "bundle.json"
            if not candidate.is_file():
                candidate = child / "summary.json"
            if not candidate.is_file():
                continue
            mtime = candidate.stat().st_mtime
            if best is None or mtime > best_mtime:
                best = candidate
                best_mtime = mtime
    if best is None:
        raise SystemExit(
            f"No DST artifacts found under {ARTIFACT_ROOT}.\n"
            "  To fix: run `just sim-run` first or pass an explicit bundle path."
        )
    return best


def load_bundle(path: Path) -> dict:
    return json.loads(path.read_text())


def bundle_command(bundle: dict, full: bool = False) -> str:
    if "settings" in bundle and "seeds" in bundle:
        raise SystemExit(
            "This is a DST success summary, not a replayable failure bundle.\n"
            "  To fix: use `just sim-inspect-latest` to inspect it, or replay an explicit seed with `just sim-replay-*`."
        )
    if not full and bundle.get("minimized_replay_command"):
        return bundle["minimized_replay_command"]
    return bundle["replay_command"]


def action_seed_steps_from_bundle(bundle: dict) -> tuple[int, int]:
    cmd = bundle_command(bundle, full=False)
    seed = None
    steps = None
    for token in cmd.split():
        if token.startswith("ACTION_DST_SEED="):
            seed = int(token.split("=", 1)[1])
        elif token.startswith("ACTION_DST_STEPS="):
            steps = int(token.split("=", 1)[1])
    if seed is None or steps is None:
        raise SystemExit(
            "Bundle does not contain an action replay command with ACTION_DST_SEED and ACTION_DST_STEPS."
        )
    return seed, steps


def run_shell(command: str) -> int:
    return subprocess.run(["sh", "-lc", command], cwd=ROOT).returncode


def cmd_run(args: argparse.Namespace) -> int:
    commands = []
    if args.harness in ("workflow", "all"):
        commands.append(workflow_command(args.seeds))
    if args.harness in ("action", "all"):
        commands.append(action_command(args.seeds, args.steps))
    if args.format == "json":
        print(
            json.dumps(
                {"commands": commands, "cwd": str(ROOT), "print_only": args.print_only},
                indent=2,
            )
        )
        if args.print_only:
            return 0
    elif args.print_only:
        print("Deterministic simulation campaign commands:")
        for command in commands:
            print(f"  {command}")
        print(f"Run from: {ROOT}")
        return 0

    for command in commands:
        print(f"Running: {command}")
        code = run_shell(command)
        if code != 0:
            return code
    return 0


def cmd_replay(args: argparse.Namespace) -> int:
    if args.bundle:
        bundle = load_bundle(Path(args.bundle))
        command = bundle_command(bundle, full=args.full)
    elif args.harness == "workflow":
        command = workflow_replay(args.seed)
    else:
        command = action_replay(args.seed, args.steps)
    if args.print_only:
        print(command)
        return 0
    return run_shell(command)


def cmd_shrink(args: argparse.Namespace) -> int:
    if args.bundle:
        bundle = load_bundle(Path(args.bundle))
        seed, max_steps = action_seed_steps_from_bundle(bundle)
    else:
        seed, max_steps = args.seed, args.max_steps
    if args.print_only:
        print(action_replay(seed, max_steps))
        return 0
    for steps in range(1, max_steps + 1):
        code = run_shell(action_replay(seed, steps))
        if code != 0:
            print(f"Min prefix: {steps}")
            print(action_replay(seed, steps))
            return code
    raise SystemExit(f"No failing prefix found up to {max_steps}")


def cmd_inspect(args: argparse.Namespace) -> int:
    path = Path(args.bundle) if args.bundle else latest_artifact(args.harness)
    bundle = load_bundle(path)
    if args.format == "json":
        print(json.dumps(bundle, indent=2))
        return 0
    if "settings" in bundle and "seeds" in bundle:
        print("Deterministic simulation success bundle:")
        print(f"  Path:     {path}")
        print(f"  Harness:  {bundle['harness']}")
        print(f"  Seeds:    {len(bundle['seeds'])}")
    else:
        print("Deterministic simulation failure bundle:")
        print(f"  Path:      {path}")
        print(f"  Harness:   {bundle['harness']}")
        print(f"  Seed:      {bundle['seed']}")
        print(f"  Replay:    {bundle['replay_command']}")
        if bundle.get("minimized_replay_command"):
            print(f"  Min replay:{bundle['minimized_replay_command']}")
    return 0


def cmd_open_latest(args: argparse.Namespace) -> int:
    path = latest_artifact(args.harness)
    editor = os.environ.get("EDITOR")
    if not editor:
        print(path)
        return 0

    code = subprocess.run(["sh", "-lc", f'{editor} "{path}"'], cwd=ROOT).returncode
    if code != 0:
        raise SystemExit(
            f"EDITOR command failed with exit code {code}.\n"
            f"  To fix: verify $EDITOR works, or unset it to print the path directly.\n"
            f"  Latest artifact: {path}"
        )
    return 0


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)

    run = sub.add_parser("run")
    run.add_argument("--harness", choices=["workflow", "action", "all"], default="all")
    run.add_argument("--seeds", type=int, default=12)
    run.add_argument("--steps", type=int, default=14)
    run.add_argument("--print-only", action="store_true")
    run.add_argument("--format", choices=["text", "json"], default="text")
    run.set_defaults(func=cmd_run)

    replay = sub.add_parser("replay")
    replay.add_argument("--bundle")
    replay.add_argument("--harness", choices=["workflow", "action"])
    replay.add_argument("--seed", type=int)
    replay.add_argument("--steps", type=int)
    replay.add_argument("--full", action="store_true")
    replay.add_argument("--print-only", action="store_true")
    replay.set_defaults(func=cmd_replay)

    shrink = sub.add_parser("shrink")
    shrink.add_argument("--bundle")
    shrink.add_argument("--seed", type=int)
    shrink.add_argument("--max-steps", type=int)
    shrink.add_argument("--print-only", action="store_true")
    shrink.set_defaults(func=cmd_shrink)

    inspect = sub.add_parser("inspect")
    inspect.add_argument("bundle", nargs="?")
    inspect.add_argument("--latest", action="store_true")
    inspect.add_argument("--harness", choices=["workflow-dst", "action-workflow-dst"])
    inspect.add_argument("--format", choices=["text", "json"], default="text")
    inspect.set_defaults(func=cmd_inspect)

    open_latest = sub.add_parser("open-latest")
    open_latest.add_argument(
        "--harness", choices=["workflow-dst", "action-workflow-dst"]
    )
    open_latest.set_defaults(func=cmd_open_latest)
    return p


if __name__ == "__main__":
    args = parser().parse_args()
    sys.exit(args.func(args))
