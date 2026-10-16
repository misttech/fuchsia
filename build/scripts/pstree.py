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

# Ensure fuchsia root is in sys.path
_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.profile import procfs

# JSON type aliases
JSONPrimitive = str | int | float | bool | None
JSONValue = JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]

# Formatter closure type alias
Formatter = Callable[["ProcessNode"], Iterable[str]]


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
    def from_dict(cls, data: dict[str, Any]) -> ProcessNode:
        """Deserializes a ProcessNode and all its descendants from a dictionary."""
        info = procfs.ProcessInfo(
            pid=int(data["pid"]),
            ppid=int(data["ppid"]),
            comm=str(data["comm"]),
            state=str(data["state"]),
            cmdline=list(data.get("cmdline") or []),
            pgrp=data.get("pgrp"),
            wchan=data.get("wchan"),
            utime=int(data.get("utime", 0)),
            stime=int(data.get("stime", 0)),
        )
        children = [cls.from_dict(child) for child in data.get("children", [])]
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


def find_leaves(root: ProcessNode) -> Iterator[ProcessNode]:
    """Yields all leaf nodes (processes with no children) in the subtree."""
    if not root.children:
        yield root
        return
    for child in root.children:
        yield from find_leaves(child)


def format_process_line(node: ProcessNode) -> str:
    """Formats a single process summary line."""
    wchan_str = f" [{node.wchan}]" if node.wchan else ""
    return (
        f"{node.pid} {node.comm} (state: {node.state}{wchan_str}): "
        f"{node.cmdline_str()}"
    )


def format_tree(
    root: ProcessNode,
    prefix: str = "",
    is_last: bool = True,
    is_root: bool = True,
) -> Iterator[str]:
    """Yields formatted lines for the process tree using ASCII branch graphics."""
    if is_root:
        yield format_process_line(root)
        next_prefix = ""
    else:
        branch = "+-- "
        yield f"{prefix}{branch}{format_process_line(root)}"
        next_prefix = prefix + ("    " if is_last else "|   ")

    num_children = len(root.children)
    for i, child in enumerate(root.children):
        child_is_last = i == num_children - 1
        yield from format_tree(
            child,
            prefix=next_prefix,
            is_last=child_is_last,
            is_root=False,
        )


def format_leaves(leaves: Iterable[ProcessNode]) -> Iterator[str]:
    """Yields formatted lines for leaf processes."""
    for leaf in leaves:
        yield format_process_line(leaf)


def dump_tree_json(root: ProcessNode, indent: int = 2) -> str:
    """Serializes the process tree rooted at root to a JSON string."""
    return json.dumps(root.to_dict(), indent=indent)


# Map of format names to formatter closures
FORMATTERS: dict[str, Formatter] = {
    "tree": lambda root: format_tree(root),
    "leaves": lambda root: format_leaves(find_leaves(root)),
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
