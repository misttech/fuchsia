# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Starnix affordance implementation using FFX. """

import dataclasses
import json
import logging
import os
import pathlib
import pty
import re
import subprocess
import tempfile
from datetime import datetime
from typing import Any

import fuchsia_inspect
from honeydew import affordances_capable, errors
from honeydew.affordances.starnix import errors as starnix_errors
from honeydew.affordances.starnix import starnix
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.utils import decorators

_MAX_READ_SIZE: int = 1024

_LOGGER: logging.Logger = logging.getLogger(__name__)
_STARNIX_KERNEL_PREFIX: str = "core/starnix_runner/kernels:"
_STARNIX_KERNELS_MONIKER: str = f"{_STARNIX_KERNEL_PREFIX}*"
_TIMESTAMP_FORMAT: str = "%Y-%m-%d-%H-%M-%S-%f"


def _inspect_data_to_dict(item: fuchsia_inspect.InspectData) -> dict[str, Any]:
    """Converts a parsed InspectData entry into a JSON-serializable dict."""

    def _dict_factory(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        return {
            k: (
                v.nanoseconds()
                if isinstance(v, fuchsia_inspect.Timestamp)
                else v
            )
            for k, v in pairs
        }

    entry: dict[str, Any] = dataclasses.asdict(item, dict_factory=_dict_factory)
    entry.setdefault("data_source", "Inspect")
    return entry


class _StarnixCmds:
    """Class to hold Starnix commands."""

    PREFIX: list[str] = [
        "starnix",
        "console",
        "/bin/sh",
        "-c",
    ]


class _RegExPatterns:
    """Class to hold Regular Expression patterns."""

    STARNIX_CMD_SUCCESS: re.Pattern[str] = re.compile(r"(exit code: 0)")

    STARNIX_NOT_SUPPORTED: re.Pattern[str] = re.compile(
        r"Unable to find Starnix container in the session"
    )


class StarnixUsingFfx(starnix.Starnix):
    """Async Starnix affordance implementation using FFX.

    Args:
        device_name: Device name returned by `ffx target list`.
        ffx: interfaces.transports.FFX implementation.
        inspect: Device implementation of InspectCapableDevice.

    Raises:
        errors.NotSupportedError: If Fuchsia device does not support Starnix.
    """

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        inspect: affordances_capable.InspectCapableDevice,
    ) -> None:
        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx
        self._inspect: affordances_capable.InspectCapableDevice = inspect

        self.verify_supported()

    # List all the public methods
    def verify_supported(self) -> None:
        """Verifies that the starnix affordance is supported by the Fuchsia device.

        This method should be called in `__init__()` so that if this affordance was called on a
        Fuchsia device that does not support it, it will raise NotSupportedError.

        Raises:
            NotSupportedError: If affordance is not supported.
            StarnixError: In case of other starnix command failure.
        """
        _LOGGER.debug(
            "Checking if %s supports %s affordance...",
            self._device_name,
            self.__class__.__name__,
        )

        try:
            component_list = json.loads(self._ffx.run(["component", "list"]))
            instances = component_list.get("instances", [])
            if not any(
                instance.get("moniker", "").startswith(_STARNIX_KERNEL_PREFIX)
                for instance in instances
            ):
                raise errors.NotSupportedError(
                    f"{self._device_name} does not support Starnix"
                )

        except (ffx_errors.FfxCommandError, json.JSONDecodeError) as err:
            raise starnix_errors.StarnixError(err) from err

        _LOGGER.debug(
            "%s supports %s affordance...",
            self._device_name,
            self.__class__.__name__,
        )

    @decorators.liveness_check
    async def run_console_shell_cmd(self, cmd: list[str]) -> str:
        """Run a starnix console command and return its output.

        Args:
            cmd: cmd that need to be run excluding `starnix /bin/sh -c`.

        Returns:
            Output of `ffx -t {target} starnix /bin/sh -c {cmd}`.

        Raises:
            errors.StarnixError: In case of starnix command failure.
            errors.NotSupportedError: If Fuchsia device does not support Starnix.
        """

        # starnix console requires the process to run in tty:
        host_fd: int
        child_fd: int
        host_fd, child_fd = pty.openpty()

        starnix_cmd: list[str] = _StarnixCmds.PREFIX + cmd
        starnix_cmd_str: str = " ".join(starnix_cmd)
        process: subprocess.Popen[str] = self._ffx.popen(
            cmd=starnix_cmd,
            stdin=child_fd,
            stdout=child_fd,
            stderr=child_fd,
        )
        process.wait()

        # Note: This call may sometime return less chars than _MAX_READ_SIZE
        # even when command output contains more chars. This happened with
        # `getprop` command output but not with suspend-resume related
        # operations. So consider exploring better ways to read command output
        # such that this method can be used with other starnix console commands
        output: str = os.read(host_fd, _MAX_READ_SIZE).decode("utf-8")

        _LOGGER.debug(
            "Starnix console cmd `%s` completed. returncode=%s, output:\n%s",
            starnix_cmd_str,
            process.returncode,
            output,
        )

        if _RegExPatterns.STARNIX_CMD_SUCCESS.search(output):
            return output
        elif _RegExPatterns.STARNIX_NOT_SUPPORTED.search(output):
            board: str | None = None
            product: str | None = None
            try:
                board = self._ffx.get_target_board()
                product = self._ffx.get_target_product()
            except errors.HoneydewError:
                pass
            error_msg: str
            if board and product:
                error_msg = (
                    f"{self._device_name} running {product}.{board} does not "
                    f"support Starnix"
                )
            else:
                error_msg = f"{self._device_name} does not support Starnix"
            raise errors.NotSupportedError(error_msg)
        else:
            raise starnix_errors.StarnixError(
                f"Starnix console cmd `{starnix_cmd_str}` failed. (See debug "
                "logs for command output)"
            )

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
        try:
            collection = self._inspect.get_inspect_data(
                monikers=[_STARNIX_KERNELS_MONIKER]
            )
            kernels: list[fuchsia_inspect.InspectData] = list(collection.data)
        except (errors.HoneydewError, ValueError) as e:
            raise starnix_errors.StarnixError(
                f"Failed to collect Starnix kernel inspect data on "
                f"{self._device_name}."
            ) from e

        if not kernels:
            raise starnix_errors.StarnixError(
                f"No Starnix kernel inspect data found on {self._device_name}."
            )
        if len(kernels) > 1:
            raise starnix_errors.StarnixError(
                f"Expected 1 Starnix kernel inspect entry on "
                f"{self._device_name}, found {len(kernels)}."
            )
        return kernels

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
        kernels: list[fuchsia_inspect.InspectData] = self.get_kernel_inspect()
        try:
            serialized = [_inspect_data_to_dict(item) for item in kernels]
            out_dir: pathlib.Path = (
                pathlib.Path(directory)
                if directory is not None
                else pathlib.Path(tempfile.mkdtemp())
            )
            out_dir.mkdir(parents=True, exist_ok=True)
            if file is None:
                timestamp: str = datetime.now().strftime(_TIMESTAMP_FORMAT)
                file = (
                    f"StarnixKernelInspect_{self._device_name}_{timestamp}.json"
                )
            elif not file.endswith(".json"):
                file = f"{file}.json"
            out_file: pathlib.Path = (out_dir / file).absolute()
            out_file.parent.mkdir(parents=True, exist_ok=True)
            _LOGGER.info(
                "Saving Starnix kernel inspect data of %s to %s",
                self._device_name,
                out_file,
            )
            out_file.write_text(json.dumps(serialized, indent=4))
            return str(out_file)
        except (OSError, TypeError, ValueError) as e:
            raise starnix_errors.StarnixError(
                f"Failed to save Starnix kernel inspect data of "
                f"{self._device_name}."
            ) from e
