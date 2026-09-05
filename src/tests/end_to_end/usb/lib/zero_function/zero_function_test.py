# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host unit tests for the zero_function test harness library."""

import subprocess
import sys
import types
import unittest
from typing import Any
from unittest import mock

# Provide fallback stubs for hermetic external libraries if running in standalone host test environment
for mod_name in (
    "fuchsia_base_test",
    "honeydew",
    "honeydew.errors",
    "usb_lib",
    "usb_lib.sysfs_usb",
    "usb_lib.usb_config",
    "mobly",
    "mobly.asserts",
    "mobly.config_parser",
):
    if mod_name not in sys.modules:
        m = types.ModuleType(mod_name)
        m.__path__ = []  # type: ignore[attr-defined]
        sys.modules[mod_name] = m

if not hasattr(sys.modules["fuchsia_base_test"], "FuchsiaBaseTest"):

    class _MockFuchsiaBaseTest:
        def __init__(self, config: Any = None) -> None:
            self.dut = mock.MagicMock()
            self.log_path = "/tmp"
            self.current_test_info = mock.MagicMock()
            self.current_test_info.name = "test_mock"
            self._any_test_failed = False

        async def setup_class(self) -> None:
            pass

        async def teardown_class(self) -> None:
            pass

        async def on_fail(self, record: Any) -> None:
            pass

    sys.modules["fuchsia_base_test"].FuchsiaBaseTest = _MockFuchsiaBaseTest  # type: ignore[attr-defined]
    sys.modules["fuchsia_base_test"].SnapshotOn = mock.MagicMock()  # type: ignore[attr-defined]
    sys.modules["fuchsia_base_test"].TracingOn = mock.MagicMock()  # type: ignore[attr-defined]

if not hasattr(sys.modules["usb_lib.sysfs_usb"], "find_usb_device_node"):
    sys.modules["usb_lib.sysfs_usb"].find_usb_device_node = mock.MagicMock(  # type: ignore[attr-defined]
        return_value="/dev/bus/usb/001/002"
    )
    sys.modules["usb_lib.sysfs_usb"].wait_for_usb_device = mock.MagicMock(  # type: ignore[attr-defined]
        return_value="/dev/bus/usb/001/002"
    )

if not hasattr(sys.modules["usb_lib.usb_config"], "set_usb_config"):
    sys.modules["usb_lib.usb_config"].get_dut_serial = mock.MagicMock(  # type: ignore[attr-defined]
        return_value="test_serial"
    )
    sys.modules["usb_lib.usb_config"].get_usb_config = mock.MagicMock(  # type: ignore[attr-defined]
        return_value="sourcesink"
    )
    sys.modules["usb_lib.usb_config"].parse_usb_config_functions = mock.MagicMock(  # type: ignore[attr-defined]
        return_value=["sourcesink"]
    )
    sys.modules["usb_lib.usb_config"].set_usb_config = mock.MagicMock()  # type: ignore[attr-defined]

if not hasattr(sys.modules["mobly.asserts"], "assert_equal"):
    sys.modules["mobly.asserts"].skip = mock.MagicMock()  # type: ignore[attr-defined]
    sys.modules["mobly.asserts"].assert_equal = unittest.TestCase().assertEqual  # type: ignore[attr-defined]

try:
    from zero_function import (
        ALL_SUPPORTED_TEST_IDS,
        KNOWN_TEST_DEVICES,
        USB_ZERO_PID,
        USB_ZERO_VID,
        ZeroFunctionBaseTest,
        find_zero_function_device_node,
        wait_for_zero_function,
    )
    from zero_function.usbtest_controller import UsbTestController
except ImportError:
    from usbtest_controller import UsbTestController  # type: ignore[no-redef]
    from zero_function import (  # type: ignore[no-redef]
        ALL_SUPPORTED_TEST_IDS,
        KNOWN_TEST_DEVICES,
        USB_ZERO_PID,
        USB_ZERO_VID,
        ZeroFunctionBaseTest,
        find_zero_function_device_node,
        wait_for_zero_function,
    )


class ZeroFunctionConstantsTest(unittest.TestCase):
    """Verifies Zero Function identifiers and test catalog constants."""

    def test_vendor_and_product_ids(self) -> None:
        self.assertEqual(USB_ZERO_VID, 0x18D1)
        self.assertEqual(USB_ZERO_PID, 0xA022)
        self.assertIn((USB_ZERO_VID, USB_ZERO_PID), KNOWN_TEST_DEVICES)

    def test_supported_test_ids(self) -> None:
        self.assertEqual(len(ALL_SUPPORTED_TEST_IDS), 27)
        # Excludes isochronous test IDs
        for iso_id in (15, 16, 22, 23, 26):
            self.assertNotIn(iso_id, ALL_SUPPORTED_TEST_IDS)
        # Includes key control and bulk transfer test IDs
        for tid in (0, 1, 2, 3, 5, 7, 9, 10, 14, 19, 21, 27, 31):
            self.assertIn(tid, ALL_SUPPORTED_TEST_IDS)


class UsbTestControllerTest(unittest.TestCase):
    """Tests host usbtest driver controller behavior and execution modes."""

    def test_initialization_defaults(self) -> None:
        controller = UsbTestController()
        self.assertEqual(controller.vendor, 0x18D1)
        self.assertEqual(controller.product, 0xA022)
        self.assertEqual(controller.alt, 0)

    @mock.patch("os.path.exists")
    def test_load_driver_already_loaded(
        self, mock_exists: mock.MagicMock
    ) -> None:
        mock_exists.side_effect = lambda p: p == "/sys/module/usbtest"
        controller = UsbTestController()
        # Should return without attempting execution
        controller.load_driver()

    @mock.patch("subprocess.run")
    @mock.patch("os.path.exists")
    def test_load_driver_via_dmc(
        self, mock_exists: mock.MagicMock, mock_run: mock.MagicMock
    ) -> None:
        mock_exists.side_effect = lambda p: p != "/sys/module/usbtest"
        controller = UsbTestController(vendor=0x18D1, product=0xA022, alt=0)
        controller._dmc_path = "/usr/bin/dmc"
        controller.load_driver()
        mock_run.assert_called_once_with(
            [
                "/usr/bin/dmc",
                "load-usbtest-driver",
                "-vendor",
                "0x18d1",
                "-product",
                "0xa022",
                "-alt",
                "0",
            ],
            check=True,
            capture_output=True,
            text=True,
        )

    @mock.patch("os.path.exists", return_value=False)
    def test_load_driver_without_dmc_raises(
        self, mock_exists: mock.MagicMock
    ) -> None:
        controller = UsbTestController()
        controller._dmc_path = None
        with self.assertRaises(RuntimeError):
            controller.load_driver()

    @mock.patch("subprocess.run")
    def test_unload_driver_via_dmc(self, mock_run: mock.MagicMock) -> None:
        controller = UsbTestController()
        controller._dmc_path = "/usr/bin/dmc"
        with mock.patch("os.path.exists", return_value=True):
            controller.unload_driver()
        mock_run.assert_called_once_with(
            ["/usr/bin/dmc", "unload-usbtest-driver"],
            check=False,
            capture_output=True,
            text=True,
        )


class ZeroFunctionBaseTestLifecycleTest(unittest.IsolatedAsyncioTestCase):
    """Tests ZeroFunctionBaseTest lifecycle and execution."""

    async def test_teardown_class_without_setup_does_not_crash(self) -> None:
        """Verifies teardown_class handles uninitialized attributes gracefully."""
        test_instance = ZeroFunctionBaseTest()
        # Simulate setup_class never running or failing before setting attributes:
        # _initial_functions and driver_controller do not exist.
        with mock.patch.object(
            test_instance, "teardown_class", wraps=test_instance.teardown_class
        ):
            await test_instance.teardown_class()

    async def test_on_fail_delegates_to_super(self) -> None:
        """Verifies on_fail sets failure flag and delegates to super."""
        test_instance = ZeroFunctionBaseTest()
        record = mock.MagicMock()
        await test_instance.on_fail(record)
        self.assertTrue(test_instance._any_test_failed)

    @mock.patch("subprocess.run")
    def test_execute_testusb_cli(self, mock_run: mock.MagicMock) -> None:
        """Verifies execute_testusb_cli builds command and executes subprocess."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="Summary: 1 passed", stderr=""
        )
        test_instance = ZeroFunctionBaseTest()
        proc = test_instance.execute_testusb_cli(
            dev_node="/dev/bus/usb/001/002",
            test_ids=[0],
            mode="sourcesink",
            iterations=5,
            testusb_bin="/custom/bin/testusb",
        )
        self.assertEqual(proc.returncode, 0)
        mock_run.assert_called_once_with(
            [
                "/custom/bin/testusb",
                "-D",
                "/dev/bus/usb/001/002",
                "-m",
                "sourcesink",
                "-c",
                "5",
                "-t",
                "0",
            ],
            capture_output=True,
            text=True,
            timeout=60.0,
        )


if __name__ == "__main__":
    unittest.main()
