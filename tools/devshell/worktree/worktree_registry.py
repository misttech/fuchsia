# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import random
from pathlib import Path

from utils import run_jiri
from worktree import NoFreeWorktreesError, Worktree, WorktreeState


class WorktreeRegistry:
    def __init__(self, fuchsia_dir: str | None = None):
        if fuchsia_dir is None:
            fuchsia_dir = os.environ.get("FUCHSIA_DIR")
        if fuchsia_dir is None:
            fuchsia_dir = str(
                Path(__file__).resolve().parent.parent.parent.parent
            )

        self.fuchsia_dir = Path(fuchsia_dir).resolve()

        if (
            self.fuchsia_dir.parent.name == "worktrees"
            and self.fuchsia_dir.parent.parent.name == ".jiri_root"
        ):
            self.fuchsia_dir = self.fuchsia_dir.parent.parent.parent

        self.jiri_root = self.fuchsia_dir / ".jiri_root"
        self.registry_file = self.jiri_root / "worktrees_registry"
        self.worktrees_dir = self.jiri_root / "worktrees"

    def get_worktrees(self) -> list[Worktree]:
        if not self.registry_file.exists():
            return []
        worktrees = []
        with open(self.registry_file, "r") as f:
            for line in f:
                line = line.strip()
                if line:
                    path = Path(line)
                    worktrees.append(
                        Worktree(path.name, path, self.fuchsia_dir)
                    )
        return worktrees

    def get_worktree_by_name(self, name: str) -> Worktree:
        for wt in self.get_worktrees():
            if wt.name == name:
                return wt
        raise KeyError(f"Worktree '{name}' not found")

    def get_any_free_worktree(self) -> Worktree:
        free_worktrees = [
            wt
            for wt in self.get_worktrees()
            if wt.get_state() == WorktreeState.FREE
        ]
        if not free_worktrees:
            raise NoFreeWorktreesError("No free worktrees available")
        return random.choice(free_worktrees)

    def add_worktree(self, name: str) -> Worktree:
        wt_path = self.worktrees_dir / name
        if wt_path.exists():
            raise FileExistsError(f"Worktree path '{wt_path}' already exists")

        run_jiri(self.jiri_root, ["worktree", "add", str(wt_path)], check=True)
        wt = Worktree(name, wt_path, self.fuchsia_dir)
        wt.set_state(wt.get_state())  # ensure meta dir / default state
        return wt

    def remove_worktree(self, name: str, force: bool = False) -> None:
        wt_path = self.worktrees_dir / name
        if not wt_path.exists():
            raise FileNotFoundError(f"Worktree '{name}' does not exist on disk")

        cmd = ["worktree", "remove"]
        if force:
            cmd.append("-force")
        cmd.append(str(wt_path))
        run_jiri(self.jiri_root, cmd, check=True)
