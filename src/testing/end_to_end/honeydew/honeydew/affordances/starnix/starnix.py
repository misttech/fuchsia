# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Abstract base class for Starnix affordance."""

import abc
import pathlib

import fuchsia_inspect


class Starnix(abc.ABC):
    """Abstract base class for an async Starnix affordance."""

    @abc.abstractmethod
    async def run_console_shell_cmd(self, cmd: list[str]) -> str:
        """Run a starnix console command and return its output.

        Args:
            cmd: cmd that need to be run excluding `starnix /bin/sh -c`.

        Returns:
            Output of `ffx -t {target} starnix /bin/sh -c {cmd}`.

        Raises:
            starnix_errors.StarnixError: In case of starnix command failure.
            errors.NotSupportedError: If Fuchsia device does not support Starnix.
        """

    @abc.abstractmethod
    def get_kernel_inspect(self) -> list[fuchsia_inspect.InspectData]:
        """Returns the inspect data of all Starnix kernels.

        Queries the `core/starnix_runner/kernels:*` monikers and raises an error
        if no Starnix kernel inspect data is found or if more than one Starnix
        kernel is reported.

        Returns:
            One `fuchsia_inspect.InspectData` entry per Starnix kernel.

        Raises:
            starnix_errors.StarnixError: Failed to collect inspect data, no
                Starnix kernel inspect data was found, or more than one kernel
                was found.
        """

    @abc.abstractmethod
    def save_kernel_inspect(
        self,
        directory: pathlib.Path | str | None = None,
        file: str | None = None,
    ) -> str:
        """Saves get_kernel_inspect() output to a JSON file on the host.

        Args:
            directory: Host directory to save the file in. Created if missing.
                Defaults to a new temporary directory if omitted.
            file: Name of the file. Defaults to
                "StarnixKernelInspect_<device_name>_<timestamp>.json". If
                provided without a ".json" suffix, ".json" is appended.

        Returns:
            Absolute path of the saved file.

        Raises:
            starnix_errors.StarnixError: Failed to collect or save inspect data.
        """
