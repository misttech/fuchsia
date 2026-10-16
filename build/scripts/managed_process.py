#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Subprocess lifecycle management and process-group signal routing."""

import enum
import functools
import os
import signal
import subprocess
from collections.abc import Sequence
from typing import Any, Self


class ProcessTarget(enum.Enum):
    """Target scope for signals dispatched to a process."""

    LEADER = "leader"
    GROUP = "group"


def preexec_setup(separate_pgrp: bool = True) -> None:
    """Prepares a child process for robust signal handling.

    This function is intended to be used as a 'preexec_fn' in subprocess.Popen.
    It performs two critical tasks:
    1. Resets standard signals (SIGINT, SIGHUP, SIGTERM, SIGQUIT) to SIG_DFL.
       This is necessary because if a child starts with these signals ignored
       (common in background jobs), bash wrappers cannot set their own traps.
    2. Optionally creates a new process group for the child tree.

    Args:
        separate_pgrp: If True, calls os.setpgrp() to isolate the child.
    """
    for sig in (signal.SIGINT, signal.SIGHUP, signal.SIGTERM):
        signal.signal(sig, signal.SIG_DFL)
    if hasattr(signal, "SIGQUIT"):
        signal.signal(signal.SIGQUIT, signal.SIG_DFL)

    if separate_pgrp:
        os.setpgrp()


class ManagedProcess:
    """Encapsulates a managed subprocess with isolated process group and signal routing."""

    def __init__(
        self, process: subprocess.Popen[Any], pgid: int | None = None
    ) -> None:
        self._process = process
        self.pid = process.pid
        if pgid is not None:
            self._pgid = pgid
        else:
            try:
                self._pgid = os.getpgid(self.pid)
            except OSError:
                self._pgid = self.pid

    @property
    def process(self) -> subprocess.Popen[Any]:
        """Underlying subprocess.Popen instance."""
        return self._process

    @property
    def pgid(self) -> int:
        """Process group ID, resolved and cached at instantiation."""
        return self._pgid

    @property
    def is_group_leader(self) -> bool:
        """Whether this process is the group leader of its process group."""
        return self.pgid == self.pid

    @property
    def returncode(self) -> int | None:
        """Exit code if the process has terminated."""
        return self._process.returncode

    def poll(self) -> int | None:
        """Checks if process has finished without blocking."""
        return self._process.poll()

    def wait(self, timeout: float | None = None) -> int:
        """Waits up to timeout seconds for process termination."""
        return self._process.wait(timeout=timeout)

    def send_signal(
        self, sig: int, target: ProcessTarget = ProcessTarget.LEADER
    ) -> None:
        """Sends signal to leader PID or entire process group, safely handling exit races."""
        try:
            if target == ProcessTarget.LEADER or not self.is_group_leader:
                os.kill(self.pid, sig)
            else:
                os.killpg(self.pgid, sig)
        except ProcessLookupError:
            pass

    @classmethod
    def spawn(
        cls,
        command: Sequence[str],
        *,
        separate_pgrp: bool = True,
        **popen_kwargs: Any,
    ) -> Self:
        """Spawns child with isolated process group and default signals."""
        kwargs = dict(popen_kwargs)
        if "preexec_fn" not in kwargs:
            kwargs["preexec_fn"] = functools.partial(
                preexec_setup, separate_pgrp=separate_pgrp
            )
        proc = subprocess.Popen(command, **kwargs)
        expected_pgid = proc.pid if separate_pgrp else os.getpgid(0)
        return cls(proc, pgid=expected_pgid)
