# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ADB Root/Unroot Stress Test."""

import logging

import fuchsia_base_test
from honeydew.device_classes import android_device
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class AdbRootUnrootStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test for loop/stress testing ADB root/unroot.

    Required Mobly Test Params:
        num_iterations (int, optional): Defaults to 10.
    """

    dut: android_device.AndroidDevice

    async def pre_run(self) -> None:
        """Mobly method used to generate the test cases at run time."""
        test_arg_tuple_list: list[tuple[int]] = []

        num_iterations = max(1, int(self.user_params.get("num_iterations", 10)))
        for iteration in range(1, num_iterations + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        assert isinstance(self.dut, android_device.AndroidDevice)
        # Ensure ADB is supported and enabled on this device before starting the test
        # (raises NotSupportedError or NotEnabledError otherwise).
        _ = self.dut.adb
        # TODO(b/564946370): Remove this unbind once AndroidDevice.adb's
        # temporary su_root monkey-patch is removed.
        for attr in ("root", "unroot", "use_adb_root"):
            self.dut.adb.__dict__.pop(attr, None)

    async def _test_logic(self, iteration: int) -> None:
        """Test case logic that performs root/unroot and su_root/su_unroot."""
        _LOGGER.info(
            "Starting the ADB Root/Unroot test iteration# %s", iteration
        )

        try:
            # Ensure we start as non-root
            self.dut.adb.su_unroot()
            self.dut.adb.unroot(timeout=30.0)
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) but got: {output}",
            )

            # Switch to root
            self.dut.adb.root(timeout=30.0)
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=0(root)",
                output,
                msg=f"Expected root user (uid=0) but got: {output}",
            )

            # Switch back to non-root
            self.dut.adb.unroot(timeout=30.0)
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) but got: {output}",
            )

            # Test use_adb_root context manager
            with self.dut.adb.use_adb_root(timeout=30.0):
                output = self.dut.adb.run(["shell", "id"])
                asserts.assert_in(
                    "uid=0(root)",
                    output,
                    msg=f"Expected root user (uid=0) but got: {output}",
                )

            # Ensure previous non-root state was restored upon exiting context
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) but got: {output}",
            )

            # Switch to root via su_root
            self.dut.adb.su_root()
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=0(root)",
                output,
                msg=f"Expected root user (uid=0) via su_root but got: {output}",
            )

            # Switch back to non-root via su_unroot
            self.dut.adb.su_unroot()
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) after su_unroot but got: {output}",
            )

            # Test use_su_root context manager
            with self.dut.adb.use_su_root():
                output = self.dut.adb.run(["shell", "id"])
                asserts.assert_in(
                    "uid=0(root)",
                    output,
                    msg=f"Expected root user (uid=0) via use_su_root but got: {output}",
                )

            # Ensure previous non-root state was restored upon exiting use_su_root context
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) after use_su_root exit but got: {output}",
            )
        finally:
            # Always restore non-root state
            self.dut.adb.su_unroot()
            self.dut.adb.unroot(timeout=30.0)
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) but got: {output}",
            )

        # Ensure Fuchsia side is also online and IP is resolved (in case USB reset changed it)
        await self.dut.wait_for_online()

        _LOGGER.info(
            "Successfully ended the ADB Root/Unroot test iteration# %s",
            iteration,
        )

    def _name_func(self, iteration: int) -> str:
        """Generates name for each iteration test case."""
        return f"test_adb_root_unroot_{iteration}"


if __name__ == "__main__":
    test_runner.main()
