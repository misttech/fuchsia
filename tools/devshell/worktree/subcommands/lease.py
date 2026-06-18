# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import subprocess
import sys
from typing import Any

from utils import run_git, run_jiri
from worktree_registry import WorktreeRegistry


def run(args: Any, registry: WorktreeRegistry) -> None:
    wt = registry.get_worktree_by_name(args.name)
    wt.acquire_lease(agent_id=args.agent_id)

    if args.sync:
        try:
            run_jiri(
                registry.jiri_root,
                ["worktree", "sync", str(wt.path)],
                check=True,
            )
        except subprocess.CalledProcessError as e:
            print(f"Failed to sync worktree: {e}", file=sys.stderr)
            wt.release_lease()
            sys.exit(1)

    if args.agent_id:
        branch_name = f"feat/{args.agent_id}"
        try:
            run_git(
                wt.path,
                ["checkout", branch_name],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except subprocess.CalledProcessError:
            try:
                run_git(wt.path, ["checkout", "-b", branch_name], check=True)
            except subprocess.CalledProcessError as e:
                print(
                    f"Failed to manage git branch {branch_name}: {e}",
                    file=sys.stderr,
                )
                wt.release_lease()
                sys.exit(1)

    if args.json:
        print(json.dumps({"worktree_id": args.name}))
    else:
        print(f"Successfully leased worktree '{args.name}'")
