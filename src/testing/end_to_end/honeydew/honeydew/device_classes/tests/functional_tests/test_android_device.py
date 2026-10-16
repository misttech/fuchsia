# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for android_device.py device class."""

import fuchsia_base_test
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.device_classes import android_device
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from mobly import asserts, test_runner


class AndroidDeviceTests(fuchsia_base_test.FuchsiaBaseTest):
    """AndroidDevice functional tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)

    async def test_device_instance(self) -> None:
        """Test case for AndroidDevice inheritance."""
        asserts.assert_is_instance(self.dut, fuchsia_device.FuchsiaDevice)
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)
        asserts.assert_is_instance(self.dut.is_starnix_device(), bool)

    async def test_adb_transport(self) -> None:
        """Test case for AndroidDevice.adb transport and su_root redirection."""
        asserts.assert_is_instance(self.dut.adb, adb_transport.Adb)
        output: str = self.dut.adb.run(["shell", 'echo "hello"'])
        asserts.assert_in("hello", output)

        try:
            self.dut.adb.unroot()
            asserts.assert_in(
                "uid=2000(shell)", self.dut.adb.run(["shell", "id"])
            )

            with self.dut.adb.use_adb_root():
                asserts.assert_in(
                    "uid=0(root)", self.dut.adb.run(["shell", "id"])
                )

            asserts.assert_in(
                "uid=2000(shell)", self.dut.adb.run(["shell", "id"])
            )

            async with self.dut.adb.use_adb_root():
                asserts.assert_in(
                    "uid=0(root)", self.dut.adb.run(["shell", "id"])
                )

            asserts.assert_in(
                "uid=2000(shell)", self.dut.adb.run(["shell", "id"])
            )
        finally:
            self.dut.adb.unroot()

    async def test_health_check(self) -> None:
        """Test case for AndroidDevice.health_check()."""
        self.dut.health_check()

    async def test_initialize(self) -> None:
        """Test case for AndroidDevice.initialize()."""
        self.dut.initialize()

    async def test_screen_size(self) -> None:
        """Test case for AndroidDevice.screen_size()."""
        size: ui_types.Size = self.dut.screen_size()
        asserts.assert_is_instance(size, ui_types.Size)
        asserts.assert_greater(size.width, 0)
        asserts.assert_greater(size.height, 0)


if __name__ == "__main__":
    test_runner.main()
