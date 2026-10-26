#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Linux procfs-based process tree inspection tool.

Dual-use: importable library and standalone CLI.
Inspects /proc to snapshot process trees, states, command lines, and wait channels
without relying on external system utilities.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Callable, Iterable, Iterator, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from build.profile import procfs

# JSON type aliases
_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]

# Formatter closure type alias
Formatter = Callable[["ProcessNode"], Iterable[str]]


@dataclass(frozen=True)
class ActiveLeafSummary:
    """Summary of active leaf processes extracted from a process tree."""

    processes: list[str] = field(default_factory=list)
    total_matching_leaves: int = 0
    total_leaves: int = 0


# Maps procfs.ProcessInfo.state codes to execution state sorting priority.
# Active states (CPU, DISK-WAIT) come first, followed by sleeping/stopped.
_STATE_PRIORITIES_MAP: dict[str, int] = {
    "R": 0,  # [CPU]
    "D": 1,  # [DISK-WAIT]
    "S": 2,  # [WAITING]
    "T": 3,  # [STOPPED]
    "Z": 4,  # [ZOMBIE]
}
_DEFAULT_STATE_PRIORITY: int = 5

# Maps procfs.ProcessInfo.state codes to human-readable display tags.
_STATE_TAGS_MAP: dict[str, str] = {
    "R": "[CPU]",
    "D": "[DISK-WAIT]",
    "S": "[WAITING]",
    "T": "[STOPPED]",
    "Z": "[ZOMBIE]",
}


def proc_state_to_priority(state: str) -> int:
    """Returns the sorting priority for a procfs.ProcessInfo.state code."""
    return _STATE_PRIORITIES_MAP.get(state, _DEFAULT_STATE_PRIORITY)


def proc_state_to_display_tag(state: str) -> str:
    """Returns the human-readable display tag for a procfs.ProcessInfo.state code."""
    return _STATE_TAGS_MAP.get(state, f"[{state}]")


DEFAULT_ACTIVE_CMD_MAX_LEN: int = 120


def rank_active_leaf_processes(
    leaves: Iterable[ProcessNode],
    filter_fn: Callable[[ProcessNode], bool] | None = None,
) -> list[ProcessNode]:
    """Filters and ranks leaf processes by activity.

    Args:
        leaves: Iterable of ProcessNode leaf nodes.
        filter_fn: Optional predicate returning True for nodes to include.

    Sort order:
      1. Execution state priority ([CPU] -> [DISK-WAIT] -> [WAITING] -> other).
      2. CPU time consumed (utime + stime descending).
      3. PID ascending.
    """
    all_leaves = list(leaves)
    candidates = (
        all_leaves
        if not filter_fn
        else [node for node in all_leaves if filter_fn(node)]
    )
    # If custom filter excluded everything (e.g. all leaf processes were
    # daemons because all compilers had already terminated or failed to spawn),
    # fall back to returning unfiltered leaves so the diagnostic summary is
    # never completely empty when processes are alive in the process group.
    if not candidates and all_leaves:
        candidates = all_leaves

    def sort_key(n: ProcessNode) -> tuple[int, int, int]:
        cpu_time = n.utime + n.stime
        return (proc_state_to_priority(n.state), -cpu_time, n.pid)

    return sorted(candidates, key=sort_key)


def _int_val(val: JSONValue, default: int = 0) -> int:
    """Safely converts a JSONValue to int, raising ValueError if invalid."""
    if isinstance(val, (int, float)):
        return int(val)
    if isinstance(val, str):
        return int(val)
    if val is None:
        return default
    raise ValueError(
        f"Expected int or int-convertible value, got {type(val).__name__}"
    )


@dataclass
class ProcessNode:
    """Represents a process and its children in the process hierarchy."""

    info: procfs.ProcessInfo
    children: list[ProcessNode] = field(default_factory=list)

    @property
    def pid(self) -> int:
        return self.info.pid

    @property
    def ppid(self) -> int:
        return self.info.ppid

    @property
    def comm(self) -> str:
        return self.info.comm

    @property
    def state(self) -> str:
        return self.info.state

    @property
    def cmdline(self) -> list[str]:
        return self.info.cmdline

    @property
    def pgrp(self) -> int | None:
        return self.info.pgrp

    @property
    def wchan(self) -> str | None:
        return self.info.wchan

    @property
    def utime(self) -> int:
        return self.info.utime

    @property
    def stime(self) -> int:
        return self.info.stime

    def to_dict(self) -> JSONObject:
        """Serializes the process node and all descendants to a JSON object."""
        return {
            "pid": self.info.pid,
            "ppid": self.info.ppid,
            "comm": self.info.comm,
            "state": self.info.state,
            "cmdline": self.info.cmdline,
            "pgrp": self.info.pgrp,
            "wchan": self.info.wchan,
            "utime": self.info.utime,
            "stime": self.info.stime,
            "children": [child.to_dict() for child in self.children],
        }

    @classmethod
    def from_dict(cls, data: JSONObject) -> ProcessNode:
        """Deserializes a ProcessNode and all its descendants from a dictionary."""
        if "pid" not in data or "ppid" not in data:
            raise KeyError("ProcessNode dict missing required 'pid' or 'ppid'")
        raw_cmdline = data.get("cmdline")
        cmdline = (
            [str(arg) for arg in raw_cmdline]
            if isinstance(raw_cmdline, list)
            else []
        )
        pgrp_val = data.get("pgrp")
        wchan_val = data.get("wchan")
        info = procfs.ProcessInfo(
            pid=_int_val(data["pid"]),
            ppid=_int_val(data["ppid"]),
            comm=str(data.get("comm", "")),
            state=str(data.get("state", "")),
            cmdline=cmdline,
            pgrp=_int_val(pgrp_val) if pgrp_val is not None else None,
            wchan=str(wchan_val) if isinstance(wchan_val, str) else None,
            utime=_int_val(data.get("utime", 0)),
            stime=_int_val(data.get("stime", 0)),
        )
        raw_children = data.get("children")
        children = (
            [
                cls.from_dict(child)
                for child in raw_children
                if isinstance(child, dict)
            ]
            if isinstance(raw_children, list)
            else []
        )
        return cls(info=info, children=children)

    def cmdline_str(self) -> str:
        """Returns the command line as a string, falling back to comm.

        Embedded newlines and carriage returns are escaped so output lines
        remain on a single line per process.
        """
        return self.info.cmdline_str()

    @classmethod
    def from_process_info(cls, info: procfs.ProcessInfo) -> ProcessNode:
        """Instantiates a ProcessNode from a procfs.ProcessInfo record."""
        return cls(info=info)

    @classmethod
    def create(
        cls,
        pid: int,
        ppid: int = 0,
        comm: str = "",
        state: str = "S",
        cmdline: list[str] | None = None,
        pgrp: int | None = None,
        wchan: str | None = None,
        utime: int = 0,
        stime: int = 0,
        children: list[ProcessNode] | None = None,
    ) -> ProcessNode:
        """Convenience factory to create a ProcessNode from raw field values."""
        return cls(
            info=procfs.ProcessInfo(
                pid=pid,
                ppid=ppid,
                comm=comm,
                state=state,
                pgrp=pgrp,
                cmdline=cmdline or [],
                wchan=wchan,
                utime=utime,
                stime=stime,
            ),
            children=children or [],
        )

    def format_line(self) -> str:
        """Formats a single process summary line."""
        wchan_str = f" [{self.wchan}]" if self.wchan else ""
        return (
            f"{self.pid} {self.comm} (state: {self.state}{wchan_str}): "
            f"{self.cmdline_str()}"
        )

    def format_active_line(
        self, max_cmd_len: int = DEFAULT_ACTIVE_CMD_MAX_LEN
    ) -> str:
        """Formats an active process line with human-readable state tag and deemphasized PID.

        Format:
            [{STATE_TAG}] {cmd_preview} (pid {pid})
        """
        tag = proc_state_to_display_tag(self.state)
        cmd = self.cmdline_str()
        if len(cmd) > max_cmd_len:
            cmd = cmd[: max_cmd_len - 3] + "..."
        return f"{tag} {cmd} (pid {self.pid})"

    def format_tree(
        self,
        prefix: str = "",
        is_last: bool = True,
        is_root: bool = True,
    ) -> Iterator[str]:
        """Yields formatted lines for the process tree rooted at self using ASCII branch graphics."""
        if is_root:
            yield self.format_line()
            next_prefix = ""
        else:
            branch = "+-- "
            yield f"{prefix}{branch}{self.format_line()}"
            next_prefix = prefix + ("    " if is_last else "|   ")

        num_children = len(self.children)
        for i, child in enumerate(self.children):
            child_is_last = i == num_children - 1
            yield from child.format_tree(
                prefix=next_prefix,
                is_last=child_is_last,
                is_root=False,
            )

    def find_leaves(self) -> Iterator[ProcessNode]:
        """Yields all descendant leaf processes (nodes with no children)."""
        if not self.children:
            yield self
            return
        for child in self.children:
            yield from child.find_leaves()

    def summarize_active_leaves(
        self,
        max_count: int = 5,
        filter_fn: Callable[[ProcessNode], bool] | None = None,
        max_cmd_len: int = DEFAULT_ACTIVE_CMD_MAX_LEN,
    ) -> ActiveLeafSummary:
        """Extracts, ranks, and formats the top active leaf processes from the subtree.

        Args:
            max_count: Maximum number of formatted process lines to return.
            filter_fn: Optional predicate returning True to include a leaf node.
            max_cmd_len: Maximum length of the command preview string.

        Returns:
            ActiveLeafSummary containing formatted lines, matching count, and total count.
        """
        all_leaves = list(self.find_leaves())
        total_leaves = len(all_leaves)
        ranked = rank_active_leaf_processes(all_leaves, filter_fn=filter_fn)
        total_matching = len(ranked)
        formatted = [
            node.format_active_line(max_cmd_len=max_cmd_len)
            for node in ranked[:max_count]
        ]
        return ActiveLeafSummary(
            processes=formatted,
            total_matching_leaves=total_matching,
            total_leaves=total_leaves,
        )


def read_process_node(
    pid: int, proc_root: Path = procfs.DEFAULT_PROC_ROOT
) -> ProcessNode | None:
    """Reads process metadata for a given PID from procfs."""
    info = procfs.read_process_info(pid, proc_root=proc_root)
    return ProcessNode(info) if info else None


def scan_all_processes(
    proc_root: Path = procfs.DEFAULT_PROC_ROOT,
) -> dict[int, ProcessNode]:
    """Scans all numeric PID directories under procfs."""
    return {
        pid: ProcessNode(info)
        for pid, info in procfs.read_all_processes(proc_root=proc_root).items()
    }


def build_process_tree(
    root_pid: int,
    proc_root: Path = procfs.DEFAULT_PROC_ROOT,
    include_pgrp: bool = True,
) -> ProcessNode | None:
    """Builds a process subtree rooted at root_pid from procfs.

    If include_pgrp is True and root_pid is the process group leader, any
    orphaned child processes sharing the same process group (e.g. reparented to
    init after an intermediate parent exited) are also attached under the root.
    """
    all_processes = scan_all_processes(proc_root=proc_root)
    if root_pid not in all_processes:
        # Fall back to checking root_pid directly in case scan missed it
        direct_node = read_process_node(root_pid, proc_root=proc_root)
        if not direct_node:
            return None
        all_processes[root_pid] = direct_node

    root_node = all_processes[root_pid]

    # Link children to parents while avoiding self-parenting and cycles
    for pid, node in all_processes.items():
        if pid == root_pid or node.ppid == pid:
            continue
        # Verify node.ppid does not create a cycle back to pid
        curr = node.ppid
        has_cycle = False
        visited = {pid}
        while curr in all_processes and curr != root_pid:
            if curr in visited:
                has_cycle = True
                break
            visited.add(curr)
            curr = all_processes[curr].ppid
        if has_cycle:
            continue
        if node.ppid in all_processes:
            all_processes[node.ppid].children.append(node)

    # Attach orphaned descendants sharing the process group
    if include_pgrp and root_pid != 1 and root_node.pgrp == root_pid:
        reachable: set[int] = set()

        def _collect_reachable(n: ProcessNode) -> None:
            if n.pid in reachable:
                return
            reachable.add(n.pid)
            for child in n.children:
                _collect_reachable(child)

        _collect_reachable(root_node)

        def _has_pgrp_ancestor(node: ProcessNode) -> bool:
            curr = node.ppid
            visited = {node.pid}
            while curr in all_processes and curr not in visited:
                if curr == root_pid or all_processes[curr].pgrp == root_pid:
                    return True
                visited.add(curr)
                curr = all_processes[curr].ppid
            return False

        for pid, node in all_processes.items():
            if node.pgrp == root_pid and pid not in reachable:
                # Only attach an unreachable pgrp node to root when no ancestor
                # along its ppid chain is root_pid or has pgrp == root_pid.
                if not _has_pgrp_ancestor(node):
                    root_node.children.append(node)
                    _collect_reachable(node)

    # Sort children by PID for deterministic output
    for node in all_processes.values():
        node.children.sort(key=lambda c: c.pid)

    return root_node


def dump_tree_json(root: ProcessNode, indent: int = 2) -> str:
    """Serializes the process tree rooted at root to a JSON string."""
    return json.dumps(root.to_dict(), indent=indent)


# Map of format names to formatter closures
FORMATTERS: dict[str, Formatter] = {
    "tree": lambda root: root.format_tree(),
    "leaves": lambda root: (leaf.format_line() for leaf in root.find_leaves()),
    "json": lambda root: (dump_tree_json(root),),
}


def _main_argparser() -> argparse.ArgumentParser:
    """Creates the argument parser for standalone CLI execution."""
    parser = argparse.ArgumentParser(
        description="Inspect process tree using procfs."
    )
    parser.add_argument(
        "pid",
        nargs="?",
        type=int,
        default=None,
        help="Root PID of the process tree (defaults to 1 for full system tree).",
    )
    parser.add_argument(
        "--root",
        type=int,
        dest="root_pid",
        default=None,
        help="Root PID of the process tree.",
    )
    parser.add_argument(
        "--proc-root",
        type=Path,
        default=procfs.DEFAULT_PROC_ROOT,
        help="Procfs root directory (defaults to /proc).",
    )
    parser.add_argument(
        "--format",
        choices=list(FORMATTERS.keys()),
        default="tree",
        help="Output format (default: tree).",
    )
    parser.add_argument(
        "--output",
        "-o",
        type=Path,
        default=None,
        help="Path to write output to (defaults to stdout).",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entrypoint."""
    if argv is None:
        argv = sys.argv[1:]
    parser = _main_argparser()
    args = parser.parse_args(argv)

    root_pid = args.root_pid or args.pid
    if root_pid is None:
        root_pid = 1

    tree = build_process_tree(root_pid, proc_root=args.proc_root)
    if not tree:
        print(
            f"Process {root_pid} not found in {args.proc_root}",
            file=sys.stderr,
        )
        return 1

    formatter = FORMATTERS[args.format]
    output_text = "\n".join(formatter(tree)) + "\n"

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output_text, encoding="utf-8")
        return 0

    try:
        sys.stdout.write(output_text)
        sys.stdout.flush()
    except BrokenPipeError:
        # Downstream pipe closed prematurely (e.g. piped into `head` or `grep -m 1`).
        # Close sys.stdout so Python runtime teardown doesn't attempt to flush remaining
        # buffered output to the broken pipe on exit, and exit cleanly with status 0.
        try:
            sys.stdout.close()
        except OSError:
            pass
        return 0

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
