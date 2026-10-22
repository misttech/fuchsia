# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for AndroidDiagnostics affordance."""

import pathlib
import tempfile
import zipfile

import fuchsia_base_test
from honeydew.affordances.android_diagnostics import android_diagnostics
from honeydew.device_classes import android_device
from mobly import asserts, test_runner


class AndroidDiagnosticsAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """AndroidDiagnostics affordance tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)
        self.diagnostics: android_diagnostics.AndroidDiagnostics = (
            self.dut.diagnostics
        )
        asserts.assert_is_instance(
            self.diagnostics, android_diagnostics.AndroidDiagnostics
        )

    def test_save_bugreport(self) -> None:
        """Test case for AndroidDiagnostics.save_bugreport()."""
        with tempfile.TemporaryDirectory() as tmpdir:
            path: pathlib.Path = self.diagnostics.save_bugreport(
                directory=tmpdir, file="bugreport.zip"
            )
            asserts.assert_equal(
                path, pathlib.Path(tmpdir).absolute() / "bugreport.zip"
            )
            asserts.assert_true(
                zipfile.is_zipfile(path), f"{path} is not a zip file"
            )

    def test_save_logcat_dump(self) -> None:
        """Test case for AndroidDiagnostics.save_logcat_dump()."""
        with tempfile.TemporaryDirectory() as tmpdir:
            path: pathlib.Path = self.diagnostics.save_logcat_dump(
                directory=pathlib.Path(tmpdir) / "new_dir"
            )
            asserts.assert_regex(path.name, r"^Logcat_.+\.txt$")
            asserts.assert_greater(path.stat().st_size, 0)

    def test_take_dumpsys(self) -> None:
        """Test case for AndroidDiagnostics.take_dumpsys()."""
        with tempfile.TemporaryDirectory() as tmpdir:
            out_file: pathlib.Path = pathlib.Path(tmpdir) / "sub" / "window.txt"
            result: str = self.diagnostics.take_dumpsys(
                ["window"], file=out_file
            )
            asserts.assert_true(result.strip(), "dumpsys window is empty")
            asserts.assert_in("WINDOW MANAGER", result)
            asserts.assert_not_in("Can't find service", result)
            asserts.assert_equal(out_file.read_text(encoding="utf-8"), result)

    def test_run_ui_dumpsys(self) -> None:
        """Test case for AndroidDiagnostics.run_ui_dumpsys()."""
        expected_headers: dict[str, str] = {
            "activity": "ACTIVITY MANAGER",
            "SurfaceFlinger": "SurfaceFlinger",
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            saved_paths: list[pathlib.Path] = self.diagnostics.run_ui_dumpsys(
                tmpdir
            )
            expected_paths: list[pathlib.Path] = [
                pathlib.Path(tmpdir).absolute() / f"dumpsys_{service}.txt"
                for service in android_diagnostics._UI_DUMPSYS_SERVICES
            ]
            asserts.assert_equal(saved_paths, expected_paths)
            for service in android_diagnostics._UI_DUMPSYS_SERVICES:
                path: pathlib.Path = (
                    pathlib.Path(tmpdir) / f"dumpsys_{service}.txt"
                )
                asserts.assert_true(
                    path.is_file() and path.stat().st_size > 0,
                    f"{path} is missing or empty",
                )
                content: str = path.read_text(encoding="utf-8")
                asserts.assert_not_in("Can't find service", content)
                if service in expected_headers:
                    asserts.assert_in(expected_headers[service], content)


if __name__ == "__main__":
    test_runner.main()
