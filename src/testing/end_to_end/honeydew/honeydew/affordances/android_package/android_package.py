# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidPackage affordance implementation using ADB."""

import logging
import pathlib
from collections.abc import Iterable

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.android_package import errors as package_errors
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors

_LOGGER: logging.Logger = logging.getLogger(__name__)


class AndroidPackage(affordance.Affordance):
    """AndroidPackage affordance implementation using ADB.

    Manages Android packages (apps) on an Android (Starnix) device.

    Args:
        device_name: Device name returned by `ffx target list`.
        adb: ADB transport.
    """

    def __init__(self, device_name: str, adb: adb_transport.Adb) -> None:
        self._device_name: str = device_name
        self._adb: adb_transport.Adb = adb

        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that AndroidPackage affordance is supported by the device.

        Raises:
            NotSupportedError: If affordance is not supported.
        """
        self._adb.verify_supported()

    def force_stop(self, package: str) -> None:
        """Force stops an Android package.

        Args:
            package: Name of the package.

        Raises:
            AndroidPackageError: If stopping the package fails.
        """
        try:
            self._adb.run(["shell", "am", "force-stop", package])
        except errors.HoneydewError as e:
            raise package_errors.AndroidPackageError(
                f"Failed to force-stop {package} on {self._device_name}."
            ) from e

    def clear_package_data(self, package: str) -> None:
        """Clears user data for an Android package.

        Args:
            package: Name of the package.

        Raises:
            AndroidPackageError: If clearing package data fails.
        """
        try:
            self._adb.run(["shell", "pm", "clear", package])
        except errors.HoneydewError as e:
            raise package_errors.AndroidPackageError(
                f"Failed to clear package data for {package} on "
                f"{self._device_name}."
            ) from e

    def grant_permissions(
        self, package: str, permissions: Iterable[str]
    ) -> None:
        """Grants runtime permissions to a package, best-effort.

        A permission that cannot be granted is logged and skipped.

        Args:
            package: Name of the package.
            permissions: Runtime permissions to grant.

        Raises:
            AndroidPackageError: If an unexpected transport error occurs.
        """
        if isinstance(permissions, str):
            permissions = [permissions]
        for perm in permissions:
            try:
                self._adb.run(["shell", "pm", "grant", package, perm])
            except adb_errors.AdbCommandError as e:
                _LOGGER.warning(
                    "Could not grant permission %s to %s on %s: %s",
                    perm,
                    package,
                    self._device_name,
                    e,
                )
            except errors.HoneydewError as e:
                raise package_errors.AndroidPackageError(
                    f"Failed while granting permission {perm} to {package} on "
                    f"{self._device_name}."
                ) from e

    def install_apk(
        self, apk_path: pathlib.Path | str, bypass_low_target: bool = False
    ) -> None:
        """Installs (or re-installs) an APK from the host.

        Args:
            apk_path: Path of the APK on the host.
            bypass_low_target: Whether to bypass the low target SDK block.

        Raises:
            AndroidPackageError: If the APK does not exist on the host or if
                the installation fails.
        """
        if not pathlib.Path(apk_path).exists():
            msg = (
                f"{apk_path} does not exist; "
                "make sure it is brought via a host_test_data GN rule."
            )
            raise package_errors.AndroidPackageError(
                msg
            ) from FileNotFoundError(msg)
        cmd = ["install", "-r", "-t", "-g", str(apk_path)]
        if bypass_low_target:
            cmd.insert(1, "--bypass-low-target-sdk-block")
        try:
            output = self._adb.run(cmd, attempts=1)
        except errors.HoneydewError as e:
            raise package_errors.AndroidPackageError(
                f"Failed to install {apk_path} on {self._device_name}."
            ) from e
        if "Failure" in output or "Success" not in output:
            raise package_errors.AndroidPackageError(
                f"Failed to install {apk_path} on {self._device_name}: {output}"
            )

    def uninstall_package(self, package: str) -> None:
        """Uninstalls a package from the device, best-effort.

        A failure (e.g. the package is not installed) is logged.

        Args:
            package: Name of the package.

        Raises:
            AndroidPackageError: If an unexpected transport error occurs.
        """
        try:
            self._adb.run(["uninstall", package])
        except adb_errors.AdbCommandError as e:
            _LOGGER.warning(
                "Package %s was not installed or uninstall failed on %s: %s",
                package,
                self._device_name,
                e,
            )
        except errors.HoneydewError as e:
            raise package_errors.AndroidPackageError(
                f"Failed while uninstalling {package} on {self._device_name}."
            ) from e
