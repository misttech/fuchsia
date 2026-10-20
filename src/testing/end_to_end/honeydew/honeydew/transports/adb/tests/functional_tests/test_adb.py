#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for ADB transport."""

import fuchsia_base_test
from honeydew.device_classes import android_device
from honeydew.transports.adb import adb as adb_transport
from mobly import asserts, test_runner


class AdbTest(fuchsia_base_test.FuchsiaBaseTest):
    """ADB transport functional tests."""

    dut: android_device.AndroidDevice
    adb: adb_transport.Adb

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        assert isinstance(self.dut, android_device.AndroidDevice)
        # Ensure ADB is supported and enabled on this device before starting the test
        # (raises NotSupportedError or NotEnabledError otherwise).
        self.adb = self.dut.adb
        # TODO(b/564946370): Remove this unbind once AndroidDevice.adb's
        # temporary su_root monkey-patch is removed.
        for attr in ("root", "unroot", "use_adb_root"):
            self.adb.__dict__.pop(attr, None)

    async def test_verify_supported_and_check_connection(self) -> None:
        """Test case for ADB.verify_supported(), check_connection(), and wait_for_boot_complete()."""
        self.adb.verify_supported()
        self.adb.check_connection()
        self.adb.wait_for_boot_complete()

    async def test_list_devices(self) -> None:
        """Test case for ADB.run(["devices", "-l"])."""
        output = self.adb.run(["devices", "-l"], include_serial=False)
        serial = self.dut.ffx.serial_number
        asserts.assert_regex(output, rf"{serial}\s+device\b")

    async def test_adb_shell(self) -> None:
        """Test case for ADB.run(["shell", ...])."""
        output = self.adb.run(["shell", 'echo "hello"'])
        asserts.assert_in("hello", output)

    async def test_adb_root_unroot(self) -> None:
        """Test case for ADB root(), unroot(), is_root, and use_adb_root()."""
        try:
            # Ensure we start as non-root
            self.adb.unroot()
            self.adb.su_unroot()
            asserts.assert_false(
                self.adb.is_root, "Expected is_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Switch to root
            self.adb.root()
            asserts.assert_true(self.adb.is_root, "Expected is_root to be True")
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=0(root)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Switch back to non-root
            self.adb.unroot()
            asserts.assert_false(
                self.adb.is_root, "Expected is_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Test synchronous use_adb_root context manager
            with self.adb.use_adb_root():
                asserts.assert_true(
                    self.adb.is_root, "Expected is_root to be True"
                )
                output = self.adb.run(["shell", "id"])
                asserts.assert_in("uid=0(root)", output)
                output = self.adb.run(["shell", 'echo "hello"'])
                asserts.assert_in("hello", output)

            asserts.assert_false(
                self.adb.is_root, "Expected is_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Test asynchronous use_adb_root context manager
            async with self.adb.use_adb_root():
                asserts.assert_true(
                    self.adb.is_root, "Expected is_root to be True"
                )
                output = self.adb.run(["shell", "id"])
                asserts.assert_in("uid=0(root)", output)
                output = self.adb.run(["shell", 'echo "hello"'])
                asserts.assert_in("hello", output)

            asserts.assert_false(
                self.adb.is_root, "Expected is_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)
        finally:
            # Always restore non-root state
            self.adb.su_unroot()
            self.adb.unroot()
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

    async def test_adb_getprop_setprop(self) -> None:
        """Test case for ADB.getprop() and ADB.setprop()."""
        test_prop = "debug.honeydew.test_prop"
        test_val = "test_val_123"
        self.adb.setprop(test_prop, test_val)
        asserts.assert_equal(self.adb.getprop(test_prop), test_val)

    async def test_adb_su_root_unroot(self) -> None:
        """Test case for ADB su_root(), su_unroot(), is_su_root, and use_su_root()."""
        try:
            # Ensure we start as non-root
            self.adb.unroot()
            self.adb.su_unroot()
            asserts.assert_false(
                self.adb.is_su_root, "Expected is_su_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Switch to root via su_root
            self.adb.su_root()
            asserts.assert_true(
                self.adb.is_su_root, "Expected is_su_root to be True"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=0(root)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Switch back to non-root via su_unroot
            self.adb.su_unroot()
            asserts.assert_false(
                self.adb.is_su_root, "Expected is_su_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

            # Test use_su_root context manager
            with self.adb.use_su_root():
                asserts.assert_true(
                    self.adb.is_su_root, "Expected is_su_root to be True"
                )
                output = self.adb.run(["shell", "id"])
                asserts.assert_in("uid=0(root)", output)
                output = self.adb.run(["shell", 'echo "hello"'])
                asserts.assert_in("hello", output)

            asserts.assert_false(
                self.adb.is_su_root, "Expected is_su_root to be False"
            )
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)
        finally:
            # Always restore non-root state
            self.adb.su_unroot()
            output = self.adb.run(["shell", "id"])
            asserts.assert_in("uid=2000(shell)", output)
            output = self.adb.run(["shell", 'echo "hello"'])
            asserts.assert_in("hello", output)

    async def test_android_reboot(self) -> None:
        """Test case for ensuring ADB transport works after rebooting via Android."""
        self.adb.run(["reboot"])

        await self.dut.wait_for_online()
        await self.dut.on_device_boot()

        output = self.adb.run(["shell", 'echo "hello"'])
        asserts.assert_in("hello", output)

    async def test_fuchsia_reboot(self) -> None:
        """Test case for ensuring ADB transport works after rebooting via Fuchsia."""
        await self.dut.reboot()

        output = self.adb.run(["shell", 'echo "hello"'])
        asserts.assert_in("hello", output)


if __name__ == "__main__":
    test_runner.main()
