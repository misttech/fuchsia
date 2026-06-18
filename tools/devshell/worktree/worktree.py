# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import subprocess
import time
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

from build_dir import BuildDir
from utils import run_git


class NoFreeWorktreesError(RuntimeError):
    pass


class WorktreeState(Enum):
    FREE = "free"
    LEASED = "leased"


class SyncStatus(Enum):
    SYNCED = "Synced"
    BEHIND = "behind"
    NEW = "new"
    DIVERGED = "behind, new"
    UNKNOWN = "Unknown"


@dataclass
class LeaseInfo:
    worktree_id: str
    pid: int
    timestamp_sec: int
    agent_id: str | None = None


class Worktree:
    def __init__(self, name: str, path: Path, main_checkout_dir: Path):
        self.name = name
        self.path = Path(path).resolve()
        self.main_checkout_dir = Path(main_checkout_dir).resolve()
        self.meta_dir = self.path / ".jiri_root"
        self.state_file = self.meta_dir / "worktree-state"
        self.lease_file = self.meta_dir / "lease.json"

    def get_state(self) -> WorktreeState:
        if self.lease_file.exists():
            return WorktreeState.LEASED
        if not self.state_file.exists():
            return WorktreeState.FREE
        state_str = self.state_file.read_text().strip()
        try:
            return WorktreeState(state_str)
        except ValueError:
            return WorktreeState.FREE

    def set_state(self, state: WorktreeState) -> None:
        self.meta_dir.mkdir(parents=True, exist_ok=True)
        self.state_file.write_text(f"{state.value}\n")

    def get_lease_info(self) -> LeaseInfo | None:
        if not self.lease_file.exists():
            return None
        try:
            content = json.loads(self.lease_file.read_text())
            return LeaseInfo(
                worktree_id=content.get("worktree_id", self.name),
                pid=content.get("pid", 0),
                timestamp_sec=content.get("timestamp_sec", 0),
                agent_id=content.get("agent_id"),
            )
        except Exception:
            return None

    @property
    def build_dirs(self) -> list[BuildDir]:
        dirs = set()
        fx_build_dir_file = self.path / ".fx-build-dir"
        if fx_build_dir_file.exists():
            build_dir_rel = fx_build_dir_file.read_text().strip()
            if build_dir_rel:
                try:
                    dirs.add((self.path / build_dir_rel).resolve())
                except OSError:
                    pass
        else:
            try:
                dirs.add((self.path / "out" / "default").resolve())
            except OSError:
                pass

        out_dir = self.path / "out"
        if out_dir.exists():
            for child in out_dir.iterdir():
                if child.is_dir():
                    if (child / "args.gn").exists() or (
                        child / "build.ninja"
                    ).exists():
                        try:
                            dirs.add(child.resolve())
                        except OSError:
                            pass

        return [BuildDir(d) for d in sorted(dirs) if d.exists() and d.is_dir()]

    def get_sync_status(self) -> tuple[SyncStatus, int, int]:
        try:
            main_head = run_git(
                self.main_checkout_dir,
                ["rev-parse", "HEAD"],
                quiet=True,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            wt_head = run_git(
                self.path,
                ["rev-parse", "HEAD"],
                quiet=True,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()

            behind = int(
                run_git(
                    self.main_checkout_dir,
                    ["rev-list", "--count", f"{wt_head}..{main_head}"],
                    quiet=True,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip()
            )
            new = int(
                run_git(
                    self.main_checkout_dir,
                    ["rev-list", "--count", f"{main_head}..{wt_head}"],
                    quiet=True,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip()
            )

            if behind == 0 and new == 0:
                return SyncStatus.SYNCED, 0, 0
            if behind > 0 and new == 0:
                return SyncStatus.BEHIND, behind, 0
            if behind == 0 and new > 0:
                return SyncStatus.NEW, 0, new
            return SyncStatus.DIVERGED, behind, new
        except (subprocess.CalledProcessError, FileNotFoundError):
            return SyncStatus.UNKNOWN, 0, 0

    def acquire_lease(self, agent_id: str | None = None) -> None:
        state = self.get_state()
        if state != WorktreeState.FREE:
            raise RuntimeError(
                f"Worktree '{self.name}' is not free (state: {state.value})"
            )

        try:
            self.meta_dir.mkdir(parents=True, exist_ok=True)
            fd = os.open(self.lease_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
            lease_data = {
                "worktree_id": self.name,
                "pid": os.getpid(),
                "timestamp_sec": int(time.time()),
                "path": str(self.path),
            }
            if agent_id:
                lease_data["agent_id"] = agent_id
            with os.fdopen(fd, "w") as f:
                json.dump(lease_data, f, separators=(",", ":"))
        except FileExistsError:
            raise RuntimeError(f"Worktree '{self.name}' is already leased")

        for bd in self.build_dirs:
            bd.backup_args()

        self.set_state(WorktreeState.LEASED)

    def release_lease(self) -> None:
        state = self.get_state()
        if state != WorktreeState.LEASED:
            raise RuntimeError(
                f"Cannot release '{self.name}': worktree is not leased (state: {state.value})"
            )

        for bd in self.build_dirs:
            bd.restore_args()

        try:
            if self.lease_file.exists():
                os.remove(self.lease_file)
        except OSError as e:
            raise RuntimeError(f"Failed to remove lease file: {e}")

        self.set_state(WorktreeState.FREE)
