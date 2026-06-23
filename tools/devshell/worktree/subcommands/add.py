# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import subprocess
import sys
from typing import Any

from utils import run_git, run_jiri
from worktree import NoFreeWorktreesError
from worktree_pool import WorktreePool


def run(args: Any, pool: WorktreePool) -> None:
    task_name = getattr(args, "name", None)
    pool_name = getattr(args, "pool_name", None)

    if not task_name:
        print("Error: Must specify a worktree name", file=sys.stderr)
        sys.exit(1)

    try:
        if pool_name:
            wt = pool.get_worktree_by_name(pool_name)
            wt.acquire_lease(task_id=task_name)
        else:
            wt = pool.get_any_free_worktree()
            wt.acquire_lease(task_id=task_name)
    except NoFreeWorktreesError:
        print(
            "\n[!] No free worktrees available in the pool.\n"
            "    Run 'fx worktree pool add' to provision additional build capacity.\n",
            file=sys.stderr,
        )
        sys.exit(1)
    except (KeyError, RuntimeError, FileExistsError) as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

    branch_name = task_name
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
                f"Failed to manage git branch '{branch_name}': {e}",
                file=sys.stderr,
            )
            wt.release_lease()
            sys.exit(1)

    if getattr(args, "sync", False):
        try:
            run_jiri(
                pool.jiri_root,
                ["worktree", "sync", str(wt.path)],
                check=True,
            )
        except subprocess.CalledProcessError as e:
            print(f"Failed to sync worktree: {e}", file=sys.stderr)
            wt.release_lease()
            sys.exit(1)

    symlink_path = pool.worktrees_dir / task_name
    if symlink_path != wt.path:
        if symlink_path.is_symlink() or symlink_path.exists():
            symlink_path.unlink()
        symlink_path.symlink_to(wt.name)

    if getattr(args, "json", False):
        print(
            json.dumps(
                {
                    "worktree_id": task_name,
                    "path": str(symlink_path),
                }
            )
        )
    else:
        print(
            f"Successfully created worktree '.jiri_root/worktrees/{task_name}'"
        )
