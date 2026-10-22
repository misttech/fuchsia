# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for android_diagnostics.py."""

import pathlib
import tempfile
import unittest
from unittest import mock

from honeydew.affordances.android_diagnostics import (
    android_diagnostics,
)
from honeydew.affordances.android_diagnostics import (
    errors as diagnostics_errors,
)
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors

_DEVICE_NAME: str = "fuchsia-emulator"


class AndroidDiagnosticsTests(unittest.TestCase):
    """Unit tests for AndroidDiagnostics."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_adb = mock.create_autospec(adb_transport.Adb, instance=True)
        self.diagnostics = android_diagnostics.AndroidDiagnostics(
            device_name=_DEVICE_NAME, adb=self.mock_adb
        )
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.tmp_path = pathlib.Path(temp_dir.name)

    def test_verify_supported_called_in_init(self) -> None:
        """Verifies that __init__ calls verify_supported() and delegates to adb."""
        self.mock_adb.verify_supported.assert_called_once_with()
        with mock.patch.object(
            android_diagnostics.AndroidDiagnostics,
            "verify_supported",
            autospec=True,
        ) as mock_verify_supported:
            android_diagnostics.AndroidDiagnostics(
                device_name=_DEVICE_NAME, adb=self.mock_adb
            )
        mock_verify_supported.assert_called_once()

    def test_save_bugreport_with_file_name(self) -> None:
        """Verifies save_bugreport() writes to the provided file name."""
        out_dir = self.tmp_path / "new_dir"

        def _fake_bugreport(cmd: list[str], attempts: int = 1) -> str:
            pathlib.Path(cmd[1]).write_bytes(b"PK\x03\x04dummy_zip")
            return ""

        self.mock_adb.run.side_effect = _fake_bugreport
        path = self.diagnostics.save_bugreport(out_dir, file="bugreport.zip")
        expected = out_dir.absolute() / "bugreport.zip"
        self.assertEqual(path, expected)
        self.assertTrue(out_dir.is_dir())
        self.mock_adb.run.assert_called_once_with(
            ["bugreport", str(expected)], attempts=1
        )

    def test_save_bugreport_appends_zip_suffix(self) -> None:
        """Verifies save_bugreport() appends .zip when missing."""

        def _fake_bugreport(cmd: list[str], attempts: int = 1) -> str:
            pathlib.Path(cmd[1]).write_bytes(b"PK\x03\x04dummy_zip")
            return ""

        self.mock_adb.run.side_effect = _fake_bugreport
        path = self.diagnostics.save_bugreport(
            self.tmp_path, file="custom_bugreport"
        )
        expected = self.tmp_path.absolute() / "custom_bugreport.zip"
        self.assertEqual(path, expected)
        self.mock_adb.run.assert_called_once_with(
            ["bugreport", str(expected)], attempts=1
        )

    def test_save_bugreport_default_file_name(self) -> None:
        """Verifies save_bugreport() generates a default file name."""

        def _fake_bugreport(cmd: list[str], attempts: int = 1) -> str:
            pathlib.Path(cmd[1]).write_bytes(b"PK\x03\x04dummy_zip")
            return ""

        self.mock_adb.run.side_effect = _fake_bugreport
        path = self.diagnostics.save_bugreport(str(self.tmp_path))
        self.assertIsInstance(path, pathlib.Path)
        self.assertEqual(path.parent, self.tmp_path.absolute())
        self.assertRegex(
            path.name,
            rf"^Bugreport_{_DEVICE_NAME}_\d{{4}}-\d{{2}}-\d{{2}}-\d{{2}}-\d{{2}}-\d{{2}}\.zip$",
        )
        self.mock_adb.run.assert_called_once_with(
            ["bugreport", str(path)], attempts=1
        )

    def test_save_bugreport_default_directory(self) -> None:
        """Verifies save_bugreport() uses tempfile.mkdtemp() when directory is None."""

        def _fake_bugreport(cmd: list[str], attempts: int = 1) -> str:
            pathlib.Path(cmd[1]).write_bytes(b"PK\x03\x04dummy_zip")
            return ""

        self.mock_adb.run.side_effect = _fake_bugreport
        with mock.patch("tempfile.mkdtemp", return_value=str(self.tmp_path)):
            path = self.diagnostics.save_bugreport()
        self.assertEqual(path.parent, self.tmp_path.absolute())

    def test_save_bugreport_raises_on_missing_or_empty_file(self) -> None:
        """Verifies save_bugreport() raises AndroidDiagnosticsError if file is missing or empty."""
        self.mock_adb.run.return_value = ""
        with self.assertRaises(diagnostics_errors.AndroidDiagnosticsError):
            self.diagnostics.save_bugreport(self.tmp_path, file="missing.zip")

    def test_save_bugreport_wraps_adb_error(self) -> None:
        """Verifies save_bugreport() wraps AdbError in AndroidDiagnosticsError."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("adb failed")
        with self.assertRaises(diagnostics_errors.AndroidDiagnosticsError):
            self.diagnostics.save_bugreport(self.tmp_path)

    def test_save_logcat_dump_with_file_name(self) -> None:
        """Verifies save_logcat_dump() writes logcat output to the file."""
        self.mock_adb.run.return_value = "logcat_lines"
        path = self.diagnostics.save_logcat_dump(
            self.tmp_path, file="sub/logcat"
        )
        self.assertEqual(path, self.tmp_path.absolute() / "sub" / "logcat.txt")
        self.assertEqual(path.read_text(encoding="utf-8"), "logcat_lines")
        self.mock_adb.run.assert_called_once_with(["logcat", "-d"])

    def test_save_logcat_dump_default_file_name(self) -> None:
        """Verifies save_logcat_dump() generates a default file name."""
        self.mock_adb.run.return_value = "logcat_lines"
        path = self.diagnostics.save_logcat_dump(self.tmp_path)
        self.assertIsInstance(path, pathlib.Path)
        self.assertTrue(path.name.startswith(f"Logcat_{_DEVICE_NAME}_"))
        self.assertEqual(path.suffix, ".txt")
        self.assertEqual(path.read_text(encoding="utf-8"), "logcat_lines")

    def test_save_logcat_dump_wraps_adb_error(self) -> None:
        """Verifies save_logcat_dump() wraps AdbError in AndroidDiagnosticsError."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("adb failed")
        with self.assertRaises(diagnostics_errors.AndroidDiagnosticsError):
            self.diagnostics.save_logcat_dump(self.tmp_path)

    def test_take_dumpsys(self) -> None:
        """Verifies take_dumpsys() returns and optionally saves output."""
        self.mock_adb.run.return_value = "dumpsys_output"
        self.assertEqual(
            self.diagnostics.take_dumpsys(["window"]), "dumpsys_output"
        )
        self.mock_adb.run.assert_called_with(["shell", "dumpsys", "window"])

        dumpsys_file = self.tmp_path / "sub" / "window_dumpsys.txt"
        res = self.diagnostics.take_dumpsys(["window"], file=dumpsys_file)
        self.assertEqual(res, "dumpsys_output")
        self.assertEqual(
            dumpsys_file.read_text(encoding="utf-8"), "dumpsys_output"
        )

    def test_take_dumpsys_raises_on_service_error(self) -> None:
        """Verifies take_dumpsys() raises AndroidDiagnosticsError on missing/failed service."""
        self.mock_adb.run.return_value = "Can't find service: bogus"
        dumpsys_file = self.tmp_path / "bogus.txt"
        with self.assertRaises(diagnostics_errors.AndroidDiagnosticsError):
            self.diagnostics.take_dumpsys(["bogus"], file=dumpsys_file)
        self.assertFalse(dumpsys_file.exists())

    def test_take_dumpsys_wraps_adb_error(self) -> None:
        """Verifies take_dumpsys() wraps AdbError in AndroidDiagnosticsError."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("adb failed")
        with self.assertRaises(diagnostics_errors.AndroidDiagnosticsError):
            self.diagnostics.take_dumpsys(["window"])

    def test_run_ui_dumpsys(self) -> None:
        """Verifies run_ui_dumpsys() saves UI dumpsys services."""
        self.mock_adb.run.return_value = "dumpsys_output"
        ui_dir = self.tmp_path / "ui_dumpsys"
        saved = self.diagnostics.run_ui_dumpsys(ui_dir)
        self.assertEqual(
            saved,
            [
                ui_dir.absolute() / f"dumpsys_{service}.txt"
                for service in android_diagnostics._UI_DUMPSYS_SERVICES
            ],
        )
        for service in android_diagnostics._UI_DUMPSYS_SERVICES:
            self.assertEqual(
                (ui_dir / f"dumpsys_{service}.txt").read_text(encoding="utf-8"),
                "dumpsys_output",
            )

    def test_run_ui_dumpsys_attempts_all_services_and_raises_on_failure(
        self,
    ) -> None:
        """Verifies run_ui_dumpsys() attempts all services and raises AndroidDiagnosticsError."""
        (
            first_service,
            second_service,
        ) = android_diagnostics._UI_DUMPSYS_SERVICES
        self.mock_adb.run.side_effect = [
            adb_errors.AdbCommandError("boom"),
            "sf_output",
        ]
        ui_dir = self.tmp_path / "ui_dumpsys"
        with (
            self.assertLogs(level="WARNING") as log,
            self.assertRaises(diagnostics_errors.AndroidDiagnosticsError),
        ):
            self.diagnostics.run_ui_dumpsys(ui_dir)
        self.assertIn(
            f"Failed to capture dumpsys for {first_service}", log.output[0]
        )
        self.assertFalse((ui_dir / f"dumpsys_{first_service}.txt").exists())
        self.assertEqual(
            (ui_dir / f"dumpsys_{second_service}.txt").read_text(
                encoding="utf-8"
            ),
            "sf_output",
        )


if __name__ == "__main__":
    unittest.main()
