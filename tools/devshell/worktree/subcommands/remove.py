# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from worktree_registry import WorktreeRegistry


def run(args: Any, registry: WorktreeRegistry) -> None:
    wt = registry.get_worktree_by_name(args.name)
    if wt.get_lease_info() and not args.force:
        raise RuntimeError(
            f"Worktree '{args.name}' is leased. Use --force to remove."
        )
    registry.remove_worktree(args.name, force=args.force)
