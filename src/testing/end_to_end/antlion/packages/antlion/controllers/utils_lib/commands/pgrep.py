# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import subprocess

from antlion.controllers.utils_lib.commands.command import LinuxCommand
from antlion.runner import Runner


class LinuxPgrepCommand(LinuxCommand):
    """Look through current running processes."""

    def __init__(self, runner: Runner, binary: str = "pgrep") -> None:
        super().__init__(runner, binary)

    def find(self, process: str) -> list[int] | None:
        """Find a process by name.

        Args:
            process: Name of the process to query

        Returns:
            List of process IDs if running, otherwise None.
        """
        try:
            result = self._run(["-x", process])
            return [int(line) for line in result.stdout.splitlines()]
        except subprocess.CalledProcessError as e:
            if e.stdout or e.stderr:
                # pgrep should not output anything to stdout or stderr
                raise e
            return None
