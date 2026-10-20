#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Build timeout orchestrator.

Wraps build commands under progressive timeout schedules (graceful_timeout.py),
records structured process tree snapshots (pstree.py) at escalation milestones,
and presents human-readable diagnostics upon timeout.
"""

import argparse
import json
import signal
import sys
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Self, TextIO

# Ensure fuchsia root is in sys.path
_FUCHSIA_ROOT = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_ROOT) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_ROOT))

from build.python.modules.serialization import serialization
from build.scripts import graceful_timeout, managed_process, pstree

# JSON type aliases
_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]

_SCRIPT: Path = Path(__file__)


def msg(text: str, file: TextIO | None = None) -> None:
    """Print a message prefixed with the script's basename."""
    if file is None:
        file = sys.stderr
    print(f"[{_SCRIPT.name}] {text}", file=file)


EXIT_TIMEOUT: int = graceful_timeout.EXIT_TIMEOUT
PSTREE_SCRIPT: Path = Path(__file__).parent / "pstree.py"
MAX_TIMEOUT_LEAVES_DISPLAY: int = 10
MAX_TIMEOUT_TREE_LINES_DISPLAY: int = 50
DEFAULT_METADATA_MAX_LEAVES: int = 8

# Default build escalation schedule embedded directly in code.
DEFAULT_BUILD_TIMEOUT_SCHEDULE = graceful_timeout.Schedule(
    steps=[
        graceful_timeout.ScheduleStep(
            signal=signal.SIGINT,
            target=managed_process.ProcessTarget.LEADER,
            scale=0.015,
            min_duration="15s",
            max_duration="45s",
            label="stop",
            description="Stop inflow: stops scheduling new tasks while allowing active in-flight subcommands to complete naturally.",
        ),
        graceful_timeout.ScheduleStep(
            signal=signal.SIGINT,
            target=managed_process.ProcessTarget.LEADER,
            scale=0.01,
            min_duration="15s",
            max_duration="45s",
            label="cancel",
            description="Cancel in-flight: interrupts active subcommands, allowing nested build layers to unwind and clean up temporary files.",
        ),
        graceful_timeout.ScheduleStep(
            signal=signal.SIGINT,
            target=managed_process.ProcessTarget.LEADER,
            scale=0.01,
            min_duration="15s",
            max_duration="45s",
            label="shutdown",
            description="Daemon shutdown: commands orchestrators and background daemons (RBE, ResultStore, profilers) to transmit final status and flush logs over the network.",
        ),
    ],
    final=graceful_timeout.FinalStep(
        signal=signal.SIGKILL,
        target=managed_process.ProcessTarget.GROUP,
        label="kill",
        description="Terminal fallback: unconditional kernel SIGKILL of any surviving processes in the process group.",
    ),
)


@dataclass(frozen=True)
class TimeoutMetadata:
    """Structured metadata for build timeout diagnostics."""

    log_dir: str
    snapshots: list[str] = field(default_factory=list)
    active_processes: list[str] = field(default_factory=list)
    total_matching_processes: int = 0
    total_leaf_processes: int = 0

    def to_dict(self) -> JSONObject:
        """Serializes timeout metadata to a dictionary using serialization."""
        return serialization.instance_to_dict(self)

    @classmethod
    def from_dict(cls, data: JSONObject) -> Self:
        """Deserializes a TimeoutMetadata instance from a dictionary using serialization."""
        return serialization.instance_from_dict(cls, data)

    @classmethod
    def collect(
        cls,
        timeout_dir: Path,
        filter_fn: Callable[[pstree.ProcessNode], bool] | None = None,
        max_leaves: int = DEFAULT_METADATA_MAX_LEAVES,
    ) -> Self | None:
        """Discovers snapshots and extracts active leaf processes from a timeout directory."""
        if not timeout_dir.exists():
            return None
        snapshots = sorted(f.name for f in timeout_dir.iterdir() if f.is_file())
        if not snapshots:
            return None

        active_procs: list[str] = []
        total_matching = 0
        total_leaves = 0

        stop_snapshot_path = timeout_dir / "pstree_stop.json"
        if stop_snapshot_path.exists():
            try:
                content = stop_snapshot_path.read_text(encoding="utf-8")
                data = json.loads(content)
            except OSError as e:
                msg(f"Failed to read {stop_snapshot_path.name}: {e}")
                return cls(
                    log_dir=str(timeout_dir.resolve()), snapshots=snapshots
                )
            except json.JSONDecodeError as e:
                msg(f"Failed to parse JSON from {stop_snapshot_path.name}: {e}")
                return cls(
                    log_dir=str(timeout_dir.resolve()), snapshots=snapshots
                )

            if not isinstance(data, dict):
                msg(
                    f"Expected JSON object in {stop_snapshot_path.name}, got {type(data).__name__}"
                )
                return cls(
                    log_dir=str(timeout_dir.resolve()), snapshots=snapshots
                )

            try:
                root_node = pstree.ProcessNode.from_dict(data)
            except (KeyError, TypeError, ValueError) as e:
                msg(
                    f"Invalid process tree schema in {stop_snapshot_path.name}: {e}"
                )
                return cls(
                    log_dir=str(timeout_dir.resolve()), snapshots=snapshots
                )

            summary = root_node.summarize_active_leaves(
                max_count=max_leaves, filter_fn=filter_fn
            )
            active_procs = summary.processes
            total_matching = summary.total_matching_leaves
            total_leaves = summary.total_leaves

        return cls(
            log_dir=str(timeout_dir.resolve()),
            snapshots=snapshots,
            active_processes=active_procs,
            total_matching_processes=total_matching,
            total_leaf_processes=total_leaves,
        )


def load_timeout_snapshots(
    timeout_dir: Path,
) -> dict[str, tuple[Path, pstree.ProcessNode]]:
    """Loads and deserializes all pstree JSON snapshots discovered in timeout_dir."""
    loaded: dict[str, tuple[Path, pstree.ProcessNode]] = {}
    for snapshot_file in sorted(timeout_dir.glob("pstree_*.json")):
        label = snapshot_file.stem.removeprefix("pstree_")
        try:
            content = snapshot_file.read_text(encoding="utf-8")
            data = json.loads(content)
            node = pstree.ProcessNode.from_dict(data)
            loaded[label] = (snapshot_file, node)
        except OSError as e:
            msg(f"Failed to read snapshot {snapshot_file.name}: {e}")
        except json.JSONDecodeError as e:
            msg(f"Failed to parse snapshot JSON {snapshot_file.name}: {e}")
        except (KeyError, TypeError, ValueError) as e:
            msg(f"Invalid process tree schema in {snapshot_file.name}: {e}")
    return loaded


def format_leaf_processes(
    header: str,
    snapshot_file: Path,
    root_node: pstree.ProcessNode,
    max_leaves: int = MAX_TIMEOUT_LEAVES_DISPLAY,
    filter_fn: Callable[[pstree.ProcessNode], bool] | None = None,
) -> Iterable[str]:
    """Yields formatted summary lines of active leaf processes in a snapshot."""
    label = snapshot_file.stem.removeprefix("pstree_")
    summary = root_node.summarize_active_leaves(
        max_count=max_leaves,
        filter_fn=filter_fn,
    )
    yield f"=== {header} [{label}] ({summary.total_matching_leaves} active) ==="
    for line in summary.processes:
        yield f"  * {line}"
    if summary.total_matching_leaves > len(summary.processes):
        yield (
            f"  ... and {summary.total_matching_leaves - len(summary.processes)} "
            f"more (see {snapshot_file.name})"
        )


def format_process_tree(
    label: str,
    snapshot_file: Path,
    root_node: pstree.ProcessNode,
    max_tree_lines: int = MAX_TIMEOUT_TREE_LINES_DISPLAY,
) -> Iterable[str]:
    """Yields formatted ASCII process tree lines capped to max_tree_lines."""
    yield f"--- Process Tree [{label}] ---"
    tree_lines = list(root_node.format_tree())
    for line in tree_lines[:max_tree_lines]:
        yield f"  {line}"
    if len(tree_lines) > max_tree_lines:
        yield (
            f"  ... and {len(tree_lines) - max_tree_lines} more lines (see {snapshot_file.name})"
        )


def format_timeout_diagnostics(
    timeout_dir: Path | None,
    max_leaves: int = MAX_TIMEOUT_LEAVES_DISPLAY,
    max_tree_lines: int = MAX_TIMEOUT_TREE_LINES_DISPLAY,
    filter_fn: Callable[[pstree.ProcessNode], bool] | None = None,
) -> Iterable[str]:
    """Yields formatted timeout and cancellation diagnostic lines."""
    if timeout_dir is None or not timeout_dir.exists():
        return

    snapshots = load_timeout_snapshots(timeout_dir)
    if not snapshots:
        return

    yield f"Process tree diagnostics saved to: {timeout_dir}"

    if "stop" in snapshots:
        snap_file, node = snapshots["stop"]
        yield from format_leaf_processes(
            "Active Leaf Processes at Timeout",
            snap_file,
            node,
            max_leaves=max_leaves,
            filter_fn=filter_fn,
        )

    if "kill" in snapshots:
        snap_file, node = snapshots["kill"]
        yield from format_leaf_processes(
            "Surviving Processes Before Final Kill",
            snap_file,
            node,
            max_leaves=max_leaves,
            filter_fn=filter_fn,
        )

    # Display process trees in chronological order
    label_order = ["stop", "cancel", "shutdown", "kill"]
    ordered_labels = [l for l in label_order if l in snapshots] + [
        l for l in snapshots if l not in label_order
    ]
    for label in ordered_labels:
        snap_file, root_node = snapshots[label]
        yield from format_process_tree(
            label, snap_file, root_node, max_tree_lines=max_tree_lines
        )


def present_timeout_diagnostics(
    timeout_dir: Path | None,
    result: graceful_timeout.TimeoutResult,
    timeout_str: str | None = None,
    max_leaves: int = MAX_TIMEOUT_LEAVES_DISPLAY,
    max_tree_lines: int = MAX_TIMEOUT_TREE_LINES_DISPLAY,
    filter_fn: Callable[[pstree.ProcessNode], bool] | None = None,
) -> None:
    """Presents process snapshots and diagnostics to stderr.

    Emits a single prefixed title message via msg(), followed by formatted
    diagnostic lines via print().
    """
    if result.timed_out:
        timeout_info = (
            f" (configured timeout: {timeout_str})" if timeout_str else ""
        )
        msg(f"BUILD TIMED OUT{timeout_info}")
    elif result.interrupted_signal is not None:
        sig_name = (
            signal.strsignal(result.interrupted_signal)
            or f"signal {result.interrupted_signal}"
        )
        msg(f"BUILD CANCELLED (interrupted by {sig_name})")

    for line in format_timeout_diagnostics(
        timeout_dir,
        max_leaves=max_leaves,
        max_tree_lines=max_tree_lines,
        filter_fn=filter_fn,
    ):
        print(line, file=sys.stderr)


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entrypoint."""
    if argv is None:
        argv = sys.argv[1:]

    parser = argparse.ArgumentParser(
        description="Wrap build commands under scheduled timeout and capture pstree diagnostics."
    )
    parser.add_argument(
        "--timeout",
        type=str,
        default=None,
        help="Command timeout budget (e.g. 90m, 5400s).",
    )
    parser.add_argument(
        "--schedule-file",
        type=Path,
        default=None,
        help="Path to JSON file defining the signal escalation schedule.",
    )
    parser.add_argument(
        "--log-dir",
        type=Path,
        default=None,
        help="Directory to save timeout process snapshots.",
    )
    parser.add_argument(
        "--diag-margin",
        type=str,
        default="10s",
        help="Maximum timeout for diagnostic snapshot commands (default: 10s).",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Enable verbose output.",
    )
    parser.add_argument(
        "--preserve-status",
        action="store_true",
        help="Preserve child exit status even on timeout.",
    )
    parser.add_argument(
        "--separate-pgrp",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Spawn child in a separate process group (default: True).",
    )
    parser.add_argument(
        "--max-leaves",
        type=int,
        default=MAX_TIMEOUT_LEAVES_DISPLAY,
        help=f"Maximum active leaf processes to display in summary (default: {MAX_TIMEOUT_LEAVES_DISPLAY}).",
    )
    parser.add_argument(
        "--max-tree-lines",
        type=int,
        default=MAX_TIMEOUT_TREE_LINES_DISPLAY,
        help=f"Maximum ASCII process tree lines to display per snapshot (default: {MAX_TIMEOUT_TREE_LINES_DISPLAY}).",
    )
    parser.add_argument(
        "--grace-period",
        type=str,
        default=None,
        help="Explicit total grace period budget to allocate across escalation "
        "steps (e.g. '60s', '60'). When omitted, the grace period is calculated "
        "dynamically from the schedule steps.",
    )
    parser.add_argument(
        "--exclude-process-from-summary",
        action="append",
        default=[],
        help="Process name to exclude from active leaf summaries (can be repeated). "
        "Does not affect full process tree snapshots.",
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="Command to run.",
    )

    args = parser.parse_args(argv)

    cmd = list(args.command)
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]

    if not cmd:
        parser.error("No command specified to run.")

    schedule = (
        graceful_timeout.load_schedule_file(args.schedule_file)
        if args.schedule_file
        else DEFAULT_BUILD_TIMEOUT_SCHEDULE
    )

    hard_timeout = (
        graceful_timeout.parse_duration_to_seconds(args.timeout)
        if args.timeout
        else None
    )
    grace_period = (
        graceful_timeout.parse_duration_to_seconds(args.grace_period)
        if args.grace_period
        else None
    )

    # Validate schedule against hard_timeout early before spawning child process
    if hard_timeout is not None:
        schedule.to_events(
            hard_timeout=hard_timeout,
            grace_period=grace_period,
            warn_stream=None,
        )

    diag_hook = None
    if args.log_dir:
        args.log_dir.mkdir(parents=True, exist_ok=True)
        snapshot_template = args.log_dir / "pstree_{label}.json"
        diag_hook = graceful_timeout.make_subprocess_hook(
            [
                sys.executable,
                str(PSTREE_SCRIPT),
                "{pid}",
                "--format",
                "json",
                "--output",
                str(snapshot_template),
            ],
            diag_margin=graceful_timeout.parse_duration_to_seconds(
                args.diag_margin
            ),
            verbose=args.verbose,
        )

    proc = managed_process.ManagedProcess.spawn(
        cmd, separate_pgrp=args.separate_pgrp
    )
    runner = graceful_timeout.GracefulTimeoutRunner(
        proc,
        schedule,
        hard_timeout=hard_timeout,
        grace_period=grace_period,
        diagnostic_hook=diag_hook,
        timeout_exit_code=EXIT_TIMEOUT,
        preserve_status=args.preserve_status,
        verbose=args.verbose,
    )

    result = runner.run()

    if result.timed_out or result.interrupted_signal is not None:
        exclude_set = set(args.exclude_process_from_summary)
        filter_fn = (
            (lambda n: n.comm not in exclude_set) if exclude_set else None
        )
        present_timeout_diagnostics(
            args.log_dir,
            result=result,
            timeout_str=args.timeout,
            max_leaves=args.max_leaves,
            max_tree_lines=args.max_tree_lines,
            filter_fn=filter_fn,
        )

    return result.returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
