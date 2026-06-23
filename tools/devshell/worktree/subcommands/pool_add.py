# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import shlex
import subprocess
import sys
from typing import Any

from utils import run_fx
from worktree_pool import WorktreePool


def run(args: Any, pool: WorktreePool) -> None:
    wt = pool.add_worktree(args.name)

    if args.set is not None:
        for s in args.set:
            set_args = shlex.split(s)
            try:
                run_fx(wt.path, ["set"] + set_args, check=True)
            except FileNotFoundError as e:
                print(f"Warning: {e}, cannot run fx set", file=sys.stderr)
            except subprocess.CalledProcessError as e:
                print(f"Failed to run fx set '{s}': {e}", file=sys.stderr)
                sys.exit(1)
