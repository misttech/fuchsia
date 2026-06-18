# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from worktree_registry import WorktreeRegistry


def run(args: Any, registry: WorktreeRegistry) -> None:
    wt = registry.get_worktree_by_name(args.name)
    wt.release_lease()
    print(f"Successfully released worktree '{args.name}'")
