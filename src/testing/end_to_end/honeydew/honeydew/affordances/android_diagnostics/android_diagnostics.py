# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidDiagnostics affordance implementation using ADB."""

import logging
import pathlib
import tempfile
from datetime import datetime

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.android_diagnostics import (
    errors as diagnostics_errors,
)
from honeydew.transports.adb import adb as adb_transport

_LOGGER: logging.Logger = logging.getLogger(__name__)

_TIMESTAMP_FORMAT: str = "%Y-%m-%d-%H-%M-%S"
_UI_DUMPSYS_SERVICES: tuple[str, ...] = ("activity", "SurfaceFlinger")
_DUMPSYS_ERROR_PREFIXES: tuple[str, ...] = (
    "Can't find service:",
    "Failure calling service ",
)


class AndroidDiagnostics(affordance.Affordance):
    """AndroidDiagnostics affordance implementation using ADB.

    Collects Android (Starnix) diagnostic artifacts such as bugreports, logcat
    dumps and dumpsys output from the device.
    """

    def __init__(self, device_name: str, adb: adb_transport.Adb) -> None:
        self._device_name: str = device_name
        self._adb: adb_transport.Adb = adb

        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that AndroidDiagnostics affordance is supported by the device.

        Raises:
            NotSupportedError: If affordance is not supported.
        """
        self._adb.verify_supported()

    def save_bugreport(
        self,
        directory: pathlib.Path | str | None = None,
        file: str | None = None,
    ) -> pathlib.Path:
        """Collects an Android bugreport from the device.

        Args:
            directory: Path on the host where the bugreport file will be saved.
                If None, a temporary directory is created. If this directory
                does not exist, this method will create it.
            file: Name of the output bugreport file. If not provided, a name
                using "Bugreport_{device_name}_{timestamp}.zip" format is used.
                If provided without a ".zip" suffix, ".zip" is appended.

        Returns:
            Absolute path of the bugreport file.

        Raises:
            AndroidDiagnosticsError: If collecting or saving the bugreport
                fails, or if the resulting file is missing or empty.
        """
        try:
            file_path: pathlib.Path = self._resolve_output_file(
                directory, file, prefix="Bugreport", suffix="zip"
            )
            _LOGGER.info(
                "Collecting bugreport on %s to %s...",
                self._device_name,
                file_path,
            )
            self._adb.run(["bugreport", str(file_path)], attempts=1)
        except (errors.HoneydewError, OSError) as err:
            raise diagnostics_errors.AndroidDiagnosticsError(
                f"Failed to collect bugreport on {self._device_name}"
            ) from err

        if not file_path.is_file() or file_path.stat().st_size == 0:
            raise diagnostics_errors.AndroidDiagnosticsError(
                f"Bugreport file was not created or is empty at {file_path}"
            )
        return file_path

    def save_logcat_dump(
        self,
        directory: pathlib.Path | str | None = None,
        file: str | None = None,
    ) -> pathlib.Path:
        """Dumps the current logcat buffer from the device.

        Args:
            directory: Path on the host where the logcat file will be saved.
                If None, a temporary directory is created. If this directory
                does not exist, this method will create it.
            file: Name of the output logcat file. If not provided, a name using
                "Logcat_{device_name}_{timestamp}.txt" format is used. If
                provided without a ".txt" suffix, ".txt" is appended.

        Returns:
            Absolute path of the logcat file.

        Raises:
            AndroidDiagnosticsError: If dumping or saving logcat fails.
        """
        try:
            file_path: pathlib.Path = self._resolve_output_file(
                directory, file, prefix="Logcat", suffix="txt"
            )
            _LOGGER.info(
                "Saving logcat dump on %s to %s...",
                self._device_name,
                file_path,
            )
            logcat_output: str = self._adb.run(["logcat", "-d"])
            file_path.write_text(logcat_output, encoding="utf-8")
        except (errors.HoneydewError, OSError, UnicodeError) as err:
            raise diagnostics_errors.AndroidDiagnosticsError(
                f"Failed to save logcat dump on {self._device_name}"
            ) from err
        return file_path

    def take_dumpsys(
        self,
        args: list[str],
        file: pathlib.Path | str | None = None,
    ) -> str:
        """Runs `dumpsys` with the given arguments on the device.

        Args:
            args: Arguments passed to `dumpsys` (e.g. ["window"]).
            file: If provided, the dumpsys output is also saved to this file on
                the host.

        Returns:
            The dumpsys output.

        Raises:
            AndroidDiagnosticsError: If running `dumpsys` fails, the requested
                service is not found, or saving the output file fails.
        """
        try:
            result: str = self._adb.run(["shell", "dumpsys", *args])
        except errors.HoneydewError as err:
            raise diagnostics_errors.AndroidDiagnosticsError(
                f"Failed to run dumpsys {args} on {self._device_name}"
            ) from err

        for line in result.splitlines():
            stripped: str = line.strip()
            if stripped.startswith(_DUMPSYS_ERROR_PREFIXES):
                raise diagnostics_errors.AndroidDiagnosticsError(
                    f"dumpsys {args} failed on {self._device_name}: {stripped}"
                )

        if file:
            try:
                out_file = pathlib.Path(file)
                out_file.parent.mkdir(parents=True, exist_ok=True)
                out_file.write_text(result, encoding="utf-8")
            except (OSError, UnicodeError) as err:
                raise diagnostics_errors.AndroidDiagnosticsError(
                    f"Failed to write dumpsys output to {file}"
                ) from err
        return result

    def run_ui_dumpsys(
        self, directory: pathlib.Path | str | None = None
    ) -> list[pathlib.Path]:
        """Dumps UI-related services (activity, SurfaceFlinger) for triage.

        Each service is written to `<directory>/dumpsys_<service>.txt`. All UI
        services are attempted before raising on failure.

        Args:
            directory: Path on the host where the dumpsys files will be saved.
                If None, a temporary directory is created. If this directory
                does not exist, this method will create it.

        Returns:
            List of absolute paths of the saved dumpsys files.

        Raises:
            AndroidDiagnosticsError: If capturing or saving any UI service
                dumpsys fails.
        """
        try:
            out_dir = (
                pathlib.Path(directory).absolute()
                if directory is not None
                else pathlib.Path(tempfile.mkdtemp()).absolute()
            )
            out_dir.mkdir(parents=True, exist_ok=True)
        except OSError as err:
            raise diagnostics_errors.AndroidDiagnosticsError(
                f"Failed to create directory {directory} for UI dumpsys"
            ) from err

        saved_files: list[pathlib.Path] = []
        failed_services: list[str] = []
        last_error: diagnostics_errors.AndroidDiagnosticsError | None = None
        for service in _UI_DUMPSYS_SERVICES:
            file_path = out_dir / f"dumpsys_{service}.txt"
            try:
                self.take_dumpsys([service], file=file_path)
                saved_files.append(file_path)
            except diagnostics_errors.AndroidDiagnosticsError as err:
                _LOGGER.warning(
                    "Failed to capture dumpsys for %s on %s: %s",
                    service,
                    self._device_name,
                    err,
                )
                failed_services.append(service)
                last_error = err

        if failed_services:
            raise diagnostics_errors.AndroidDiagnosticsError(
                f"Failed to capture UI dumpsys for {failed_services} on "
                f"{self._device_name}"
            ) from last_error

        return saved_files

    def _resolve_output_file(
        self,
        directory: pathlib.Path | str | None,
        file_name: str | None,
        prefix: str,
        suffix: str,
    ) -> pathlib.Path:
        """Creates `directory` if needed and returns the output file path."""
        out_dir = (
            pathlib.Path(directory).absolute()
            if directory is not None
            else pathlib.Path(tempfile.mkdtemp()).absolute()
        )
        out_dir.mkdir(parents=True, exist_ok=True)
        if not file_name:
            timestamp: str = datetime.now().strftime(_TIMESTAMP_FORMAT)
            file_name = f"{prefix}_{self._device_name}_{timestamp}.{suffix}"
        elif not file_name.endswith(f".{suffix}"):
            file_name = f"{file_name}.{suffix}"
        out_file = out_dir / file_name
        out_file.parent.mkdir(parents=True, exist_ok=True)
        return out_file
