# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.android_package.android_package.py."""

import pathlib
import tempfile
import unittest
from typing import Any
from unittest import mock

from honeydew.affordances.android_package import android_package
from honeydew.affordances.android_package import errors as package_errors
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors

_PACKAGE: str = "com.example.app"


class AndroidPackageTests(unittest.TestCase):
    """Unit tests for AndroidPackage."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )
        self.android_package_obj = android_package.AndroidPackage(
            device_name="fuchsia-emulator", adb=self.mock_adb
        )
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.tmp_path = pathlib.Path(temp_dir.name)

    def test_verify_supported_called_in_init(self) -> None:
        """Verifies that __init__ calls verify_supported() and delegates to adb."""
        self.mock_adb.verify_supported.assert_called_once_with()
        with mock.patch.object(
            android_package.AndroidPackage,
            "verify_supported",
            autospec=True,
        ) as mock_verify_supported:
            android_package.AndroidPackage(
                device_name="fuchsia-emulator", adb=self.mock_adb
            )
        mock_verify_supported.assert_called_once()

    def test_force_stop(self) -> None:
        """Verifies force_stop() runs `am force-stop`."""
        self.android_package_obj.force_stop(_PACKAGE)
        self.mock_adb.run.assert_called_once_with(
            ["shell", "am", "force-stop", _PACKAGE]
        )

    def test_force_stop_wraps_adb_error(self) -> None:
        """Verifies force_stop() wraps AdbCommandError in AndroidPackageError."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("failed")
        with self.assertRaises(package_errors.AndroidPackageError):
            self.android_package_obj.force_stop(_PACKAGE)

    def test_clear_package_data(self) -> None:
        """Verifies clear_package_data() runs `pm clear`."""
        self.android_package_obj.clear_package_data(_PACKAGE)
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pm", "clear", _PACKAGE]
        )

    def test_clear_package_data_wraps_adb_error(self) -> None:
        """Verifies clear_package_data() wraps AdbCommandError in AndroidPackageError."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("failed")
        with self.assertRaises(package_errors.AndroidPackageError):
            self.android_package_obj.clear_package_data(_PACKAGE)

    def test_grant_permissions(self) -> None:
        """Verifies grant_permissions() grants every permission."""
        self.android_package_obj.grant_permissions(
            _PACKAGE,
            ["android.permission.CAMERA", "android.permission.RECORD_AUDIO"],
        )
        self.mock_adb.run.assert_has_calls(
            [
                mock.call(
                    [
                        "shell",
                        "pm",
                        "grant",
                        _PACKAGE,
                        "android.permission.CAMERA",
                    ]
                ),
                mock.call(
                    [
                        "shell",
                        "pm",
                        "grant",
                        _PACKAGE,
                        "android.permission.RECORD_AUDIO",
                    ]
                ),
            ]
        )

    def test_grant_permissions_single_string(self) -> None:
        """Verifies grant_permissions() treats a bare string as a single permission."""
        self.android_package_obj.grant_permissions(
            _PACKAGE, "android.permission.CAMERA"
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pm", "grant", _PACKAGE, "android.permission.CAMERA"]
        )

    def test_grant_permissions_is_best_effort(self) -> None:
        """Verifies a failing grant is logged and the others still run."""
        self.mock_adb.run.side_effect = [
            adb_errors.AdbCommandError("failed"),
            "",
        ]
        with self.assertLogs(android_package._LOGGER, "WARNING"):
            self.android_package_obj.grant_permissions(
                _PACKAGE,
                [
                    "android.permission.CAMERA",
                    "android.permission.RECORD_AUDIO",
                ],
            )
        self.assertEqual(self.mock_adb.run.call_count, 2)

    def test_grant_permissions_wraps_unexpected_errors(self) -> None:
        """Verifies non-ADB-command errors are wrapped in AndroidPackageError."""
        self.mock_adb.run.side_effect = adb_errors.AdbTimeoutError("timeout")
        with self.assertRaises(package_errors.AndroidPackageError):
            self.android_package_obj.grant_permissions(
                _PACKAGE, ["android.permission.CAMERA"]
            )

    def test_install_apk_missing_file_raises(self) -> None:
        """Verifies install_apk() fails if the APK does not exist."""
        with self.assertRaises(package_errors.AndroidPackageError) as ctx:
            self.android_package_obj.install_apk(
                self.tmp_path / "nonexistent.apk"
            )
        self.assertIsInstance(ctx.exception.__cause__, FileNotFoundError)
        self.mock_adb.run.assert_not_called()

    def test_install_apk(self) -> None:
        """Verifies install_apk() runs `adb install` once."""
        apk = self.tmp_path / "app.apk"
        apk.write_bytes(b"apk")
        self.mock_adb.run.return_value = "Performing Streamed Install\nSuccess"
        self.android_package_obj.install_apk(apk)
        self.mock_adb.run.assert_called_once_with(
            ["install", "-r", "-t", "-g", str(apk)], attempts=1
        )

    def test_install_apk_bypass_low_target(self) -> None:
        """Verifies install_apk() can bypass the low target SDK block."""
        apk = self.tmp_path / "app.apk"
        apk.write_bytes(b"apk")
        self.mock_adb.run.return_value = "Performing Streamed Install\nSuccess"
        self.android_package_obj.install_apk(str(apk), bypass_low_target=True)
        self.mock_adb.run.assert_called_once_with(
            [
                "install",
                "--bypass-low-target-sdk-block",
                "-r",
                "-t",
                "-g",
                str(apk),
            ],
            attempts=1,
        )

    def test_install_apk_failure_output_raises(self) -> None:
        """Verifies install_apk() raises AndroidPackageError when output reports Failure."""
        apk = self.tmp_path / "app.apk"
        apk.write_bytes(b"apk")
        self.mock_adb.run.return_value = (
            "Performing Streamed Install\nFailure [INSTALL_FAILED_INVALID_APK]"
        )
        with self.assertRaises(package_errors.AndroidPackageError):
            self.android_package_obj.install_apk(apk)

    def test_install_apk_wraps_adb_error(self) -> None:
        """Verifies install_apk() wraps AdbCommandError in AndroidPackageError."""
        apk = self.tmp_path / "app.apk"
        apk.write_bytes(b"apk")
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("failed")
        with self.assertRaises(package_errors.AndroidPackageError):
            self.android_package_obj.install_apk(apk)

    def test_uninstall_package(self) -> None:
        """Verifies uninstall_package() runs `adb uninstall`."""
        self.android_package_obj.uninstall_package(_PACKAGE)
        self.mock_adb.run.assert_called_once_with(["uninstall", _PACKAGE])

    def test_uninstall_package_is_best_effort(self) -> None:
        """Verifies uninstall failures are logged, not raised."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "not installed"
        )
        with self.assertLogs(android_package._LOGGER, "WARNING"):
            self.android_package_obj.uninstall_package(_PACKAGE)

    def test_uninstall_package_wraps_unexpected_errors(self) -> None:
        """Verifies non-ADB-command errors during uninstall are wrapped in AndroidPackageError."""
        self.mock_adb.run.side_effect = adb_errors.AdbTimeoutError("timeout")
        with self.assertRaises(package_errors.AndroidPackageError):
            self.android_package_obj.uninstall_package(_PACKAGE)


if __name__ == "__main__":
    unittest.main()
