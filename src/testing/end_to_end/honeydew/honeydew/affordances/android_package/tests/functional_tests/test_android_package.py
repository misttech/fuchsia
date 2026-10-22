# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for AndroidPackage affordance."""

import pathlib
import tempfile

import fuchsia_base_test
from honeydew.affordances.android_package import android_package
from honeydew.affordances.android_package import errors as package_errors
from honeydew.affordances.android_session import android_session
from honeydew.device_classes import android_device
from honeydew.utils import common
from mobly import asserts, signals, test_runner

# App used by the force-stop and clear-data tests. Can be overridden with the
# "android_package_test_package" user param.
_DEFAULT_TEST_PACKAGE: str = "com.android.settings"
_DEFAULT_TEST_ACTIVITY_ACTION: str = "android.settings.SETTINGS"
_MISSING_PACKAGE: str = "com.google.honeydew.functional_test.not_installed"
_MISSING_PERMISSION: str = (
    "android.permission.HONEYDEW_FUNCTIONAL_TEST_DOES_NOT_EXIST"
)
_FORCE_STOP_TIMEOUT_SEC: float = 10


class AndroidPackageAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """AndroidPackage affordance tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)
        self.android_package: android_package.AndroidPackage = (
            self.dut.android_package
        )
        asserts.assert_is_instance(
            self.android_package, android_package.AndroidPackage
        )
        self.test_package: str = self.user_params.get(
            "android_package_test_package", _DEFAULT_TEST_PACKAGE
        )

    def _pid_of(self, package: str) -> int | None:
        return android_session.get_process_id_of(self.dut.adb, package)

    def _is_installed(self, package: str) -> bool:
        output: str = self.dut.adb.run(
            ["shell", "pm", "list", "packages", package]
        )
        return f"package:{package}" in output.split()

    def test_force_stop(self) -> None:
        """Test case for AndroidPackage.force_stop()."""
        if self.test_package == _DEFAULT_TEST_PACKAGE:
            self.dut.adb.run(
                [
                    "shell",
                    "am",
                    "start",
                    "-W",
                    "-a",
                    _DEFAULT_TEST_ACTIVITY_ACTION,
                ]
            )
        else:
            self.dut.adb.run(
                [
                    "shell",
                    "monkey",
                    "-p",
                    self.test_package,
                    "-c",
                    "android.intent.category.LAUNCHER",
                    "1",
                ]
            )
        asserts.assert_is_not_none(self._pid_of(self.test_package))

        self.android_package.force_stop(self.test_package)

        common.wait_for_state_sync(
            state_fn=lambda: self._pid_of(self.test_package) is None,
            expected_state=True,
            timeout=_FORCE_STOP_TIMEOUT_SEC,
        )
        asserts.assert_is_none(self._pid_of(self.test_package))

    def test_clear_package_data(self) -> None:
        """Test case for AndroidPackage.clear_package_data()."""
        # `pm clear` fails (and so raises) if the data could not be cleared.
        self.android_package.clear_package_data(self.test_package)

    def test_grant_permissions_is_best_effort(self) -> None:
        """Test case for AndroidPackage.grant_permissions()."""
        # An unknown permission must be logged, not raised.
        self.android_package.grant_permissions(
            self.test_package, [_MISSING_PERMISSION]
        )

    def test_uninstall_and_install_apk(self) -> None:
        """Test case for AndroidPackage.uninstall_package() and install_apk().

        Pulls the APK of an already installed single-APK third-party package,
        uninstalls the package and verifies it is removed, then installs the
        pulled APK back and verifies it is installed again.
        """
        with asserts.assert_raises(package_errors.AndroidPackageError):
            self.android_package.install_apk("/nonexistent/honeydew.apk")

        third_party: list[str] = [
            line.removeprefix("package:")
            for line in self.dut.adb.run(
                ["shell", "pm", "list", "packages", "-3"]
            ).split()
            if line.startswith("package:")
        ]
        package: str | None = None
        remote_apk: str = ""
        for candidate in third_party:
            apk_paths: list[str] = [
                line.removeprefix("package:")
                for line in self.dut.adb.run(
                    ["shell", "pm", "path", candidate]
                ).split()
                if line.startswith("package:")
            ]
            if len(apk_paths) == 1:
                package, remote_apk = candidate, apk_paths[0]
                break
        if package is None:
            raise signals.TestSkip("No single-APK third-party package found")
        with tempfile.TemporaryDirectory() as tmpdir:
            local_apk = pathlib.Path(tmpdir) / f"{package}.apk"
            self.dut.adb.run(["pull", remote_apk, str(local_apk)])
            try:
                self.android_package.uninstall_package(package)
                asserts.assert_false(self._is_installed(package), package)
            finally:
                self.android_package.install_apk(local_apk)
        asserts.assert_true(self._is_installed(package), package)

    def test_uninstall_package_missing_package_is_best_effort(self) -> None:
        """Test case for AndroidPackage.uninstall_package()."""
        asserts.assert_false(
            self._is_installed(_MISSING_PACKAGE),
            f"{_MISSING_PACKAGE} is unexpectedly installed",
        )
        self.android_package.uninstall_package(_MISSING_PACKAGE)
        asserts.assert_false(
            self._is_installed(_MISSING_PACKAGE),
            f"{_MISSING_PACKAGE} is unexpectedly installed",
        )


if __name__ == "__main__":
    test_runner.main()
