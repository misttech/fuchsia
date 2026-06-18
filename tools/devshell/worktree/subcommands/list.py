# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from utils import Colors, colorize
from worktree import SyncStatus, WorktreeState
from worktree_registry import WorktreeRegistry


def format_time_ago(sec: float | None) -> str:
    if sec is None:
        return colorize("never built", Colors.RED)
    if sec < 60:
        return "just now"
    minutes = int(sec) // 60
    hours = minutes // 60
    days = hours // 24
    if days > 0:
        return f"{days}d ago"
    if hours > 0:
        return f"{hours}h ago"
    return f"{minutes}m ago"


def run(args: Any, registry: WorktreeRegistry) -> None:
    worktrees = registry.get_worktrees()
    if not worktrees:
        return

    max_left_len = 0
    wt_entries = []

    for wt in worktrees:
        state = wt.get_state()
        state_str = state.value.capitalize()
        state_color = None

        if state == WorktreeState.LEASED:
            lease = wt.get_lease_info()
            if lease and lease.agent_id:
                state_str = f"In Use ({lease.agent_id})"
            else:
                state_str = "In Use"
            state_color = Colors.YELLOW
        elif state == WorktreeState.FREE:
            state_color = Colors.GREEN
        elif state == WorktreeState.RESERVED:
            state_color = Colors.BLUE

        state_formatted = (
            colorize(state_str, state_color) if state_color else state_str
        )
        parts = [state_formatted]

        sync_status, behind, new = wt.get_sync_status()
        if (
            sync_status != SyncStatus.SYNCED
            and sync_status != SyncStatus.UNKNOWN
        ):
            sync_parts = []
            if behind > 0:
                sync_parts.append(colorize(f"{behind} behind", Colors.RED))
            if new > 0:
                sync_parts.append(colorize(f"{new} new", Colors.BLUE))
            parts.append(", ".join(sync_parts))

        build_entries = []
        for bd in wt.build_dirs:
            try:
                out_rel = str(bd.path.relative_to(wt.path))
            except ValueError:
                out_rel = str(bd.path)
            max_left_len = max(max_left_len, 4 + len(out_rel) + 1)

            cfg = bd.get_build_config()
            time_str = format_time_ago(bd.get_build_time_ago_sec())
            build_entries.append((out_rel, cfg, time_str))

        wt_entries.append((wt.name, parts, build_entries))

    left_width = max_left_len + 4

    for name, parts, builds in wt_entries:
        header = f"{colorize(name, Colors.BOLD)} ({', '.join(parts)})"
        print(header)
        for i, (out_rel, cfg, time_str) in enumerate(builds):
            is_last = i == len(builds) - 1
            prefix = "└── " if is_last else "├── "
            left_part = f"{prefix}{out_rel}:"
            padding = " " * max(0, left_width - len(left_part))
            print(f"{left_part}{padding}{cfg} ({time_str})")
        print()
