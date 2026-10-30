# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for linux_virtual_usb_hub.py."""

import platform
import unittest
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.utils import common, host_shell
from testusb import ioctl
from usb_lib import linux_virtual_usb_hub, usb_power_hub_base_test, usb_reset


# pylint: disable=protected-access
# pylint: disable=unused-argument
class LinuxVirtualUsbAuthorizePowerHubTests(unittest.TestCase):
    """Unit tests for linux_virtual_usb_hub.py."""

    def setUp(self) -> None:
        super().setUp()

        # Default to Linux host for tests, unless overridden
        self.platform_patcher = mock.patch.object(
            platform, "system", return_value="Linux"
        )
        self.platform_patcher.start()

        # Pretend host udev rules are installed so the at-desk permission
        # check passes against the fake sysfs paths used below.
        self.access_patcher = mock.patch("os.access", return_value=True)
        self.access_patcher.start()
        self.addCleanup(self.access_patcher.stop)

    def tearDown(self) -> None:
        self.platform_patcher.stop()
        super().tearDown()

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch("glob.glob", return_value=["/sys/bus/usb/devices/1-6"])
    def test_init_discovery_success(
        self,
        mock_glob: mock.Mock,
        mock_exists: mock.Mock,
        mock_open_file: mock.Mock,
    ) -> None:
        """Test instantiation with successful auto-discovery."""

        def mock_open_side_effect(
            filepath: str, *args: Any, **kwargs: Any
        ) -> Any:
            if "idVendor" in filepath:
                return mock.mock_open(read_data="18d1\n")()
            elif "idProduct" in filepath:
                return mock.mock_open(read_data="a02b\n")()
            return mock.mock_open()()

        mock_open_file.side_effect = mock_open_side_effect

        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        self.assertEqual(hub._usb_bus_id, "1-6")
        self.assertIsInstance(hub, usb_power_hub.UsbPowerHub)

    def test_init_non_linux_raises_error(self) -> None:
        """Test instantiation on non-Linux host raises error."""
        self.platform_patcher.stop()
        with mock.patch.object(platform, "system", return_value="Darwin"):
            with self.assertRaisesRegex(
                usb_power_hub.UsbPowerHubError,
                "LinuxVirtualUsbAuthorizePowerHub is only supported on Linux",
            ):
                linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        self.platform_patcher.start()

    @mock.patch("glob.glob", return_value=[])
    def test_init_discovery_failure_no_device(
        self, mock_glob: mock.Mock
    ) -> None:
        """Test instantiation fails when no matching device is found."""
        with self.assertRaisesRegex(ValueError, "No USB device with vendor"):
            linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch(
        "glob.glob",
        return_value=["/sys/bus/usb/devices/1-6", "/sys/bus/usb/devices/1-7"],
    )
    def test_init_discovery_failure_multiple_devices(
        self,
        mock_glob: mock.Mock,
        mock_exists: mock.Mock,
        mock_open_file: mock.Mock,
    ) -> None:
        """Test instantiation fails when multiple matching devices are found."""

        def mock_open_side_effect(
            filepath: str, *args: Any, **kwargs: Any
        ) -> Any:
            if "idVendor" in filepath:
                return mock.mock_open(read_data="18d1\n")()
            elif "idProduct" in filepath:
                return mock.mock_open(read_data="a02b\n")()
            return mock.mock_open()()

        mock_open_file.side_effect = mock_open_side_effect

        with self.assertRaisesRegex(ValueError, "Multiple USB devices"):
            linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch("glob.glob", return_value=["/sys/bus/usb/devices/1-6;reboot"])
    def test_init_with_invalid_bus_id_raises_error(
        self,
        mock_glob: mock.Mock,
        mock_exists: mock.Mock,
        mock_open_file: mock.Mock,
    ) -> None:
        """Test instantiation with invalid bus ID pattern raises error."""

        def mock_open_side_effect(
            filepath: str, *args: Any, **kwargs: Any
        ) -> Any:
            if "idVendor" in filepath:
                return mock.mock_open(read_data="18d1\n")()
            elif "idProduct" in filepath:
                return mock.mock_open(read_data="a02b\n")()
            return mock.mock_open()()

        mock_open_file.side_effect = mock_open_side_effect

        with self.assertRaisesRegex(ValueError, "Invalid usb_bus_id format"):
            linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()

    @mock.patch.object(
        linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub,
        "_find_usb_bus_id",
        return_value="1-6",
    )
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_off_success(
        self, mock_run: mock.Mock, mock_find_bus_id: mock.Mock
    ) -> None:
        """Test power_off success path."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        hub.power_off()
        mock_run.assert_called_once_with(
            cmd=["sh", "-c", "echo 0 > /sys/bus/usb/devices/1-6/authorized"]
        )

    @mock.patch("time.sleep")
    @mock.patch.object(
        linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub,
        "_find_usb_bus_id",
        side_effect=["1-6", ValueError("not found"), "2-3"],
    )
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_off_retries_bus_id_lookup(
        self,
        mock_run: mock.Mock,
        mock_find_bus_id: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test power_off retries a failed bus ID lookup."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub(
            bus_id_lookup_attempts=2
        )
        hub.power_off()
        self.assertEqual(mock_find_bus_id.call_count, 3)
        mock_sleep.assert_called_once()
        mock_run.assert_called_once_with(
            cmd=["sh", "-c", "echo 0 > /sys/bus/usb/devices/2-3/authorized"]
        )

    @mock.patch.object(
        linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub,
        "_find_usb_bus_id",
        side_effect=["1-6", "2-3"],
    )
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_off_re_resolves_bus_id(
        self, mock_run: mock.Mock, mock_find_bus_id: mock.Mock
    ) -> None:
        """Test power_off uses a re-resolved bus ID that power_on reuses."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        hub.power_off()
        hub.power_on()
        mock_run.assert_has_calls(
            [
                mock.call(
                    cmd=[
                        "sh",
                        "-c",
                        "echo 0 > /sys/bus/usb/devices/2-3/authorized",
                    ]
                ),
                mock.call(
                    cmd=[
                        "sh",
                        "-c",
                        "echo 1 > /sys/bus/usb/devices/2-3/authorized",
                    ]
                ),
            ]
        )

    @mock.patch.object(
        linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub,
        "_find_usb_bus_id",
        return_value="1-6",
    )
    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error"),
        autospec=True,
    )
    def test_power_off_failure_raises_error(
        self, mock_run: mock.Mock, mock_find_bus_id: mock.Mock
    ) -> None:
        """Test power_off failure raises UsbPowerHubError."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            hub.power_off()

    @mock.patch.object(
        linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub,
        "_find_usb_bus_id",
        return_value="1-6",
    )
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_on_success(
        self, mock_run: mock.Mock, mock_find_bus_id: mock.Mock
    ) -> None:
        """Test power_on success path."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        hub.power_on()
        mock_run.assert_called_once_with(
            cmd=["sh", "-c", "echo 1 > /sys/bus/usb/devices/1-6/authorized"]
        )

    @mock.patch.object(
        linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub,
        "_find_usb_bus_id",
        return_value="1-6",
    )
    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error"),
        autospec=True,
    )
    def test_power_on_failure_raises_error(
        self, mock_run: mock.Mock, mock_find_bus_id: mock.Mock
    ) -> None:
        """Test power_on failure raises UsbPowerHubError."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub()
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            hub.power_on()

    # Tests for discovery with serial number matching
    @mock.patch("builtins.open", new_callable=mock.mock_open)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch(
        "glob.glob",
        return_value=["/sys/bus/usb/devices/1-6", "/sys/bus/usb/devices/1-7"],
    )
    def test_init_discovery_with_explicit_serial_success(
        self,
        mock_glob: mock.Mock,
        mock_exists: mock.Mock,
        mock_open_file: mock.Mock,
    ) -> None:
        """Test discovery matches serial number passed in constructor."""

        def mock_open_side_effect(
            filepath: str, *args: Any, **kwargs: Any
        ) -> Any:
            if "idVendor" in filepath:
                return mock.mock_open(read_data="18d1\n")()
            elif "idProduct" in filepath:
                return mock.mock_open(read_data="a02b\n")()
            elif "1-6/serial" in filepath:
                return mock.mock_open(read_data="OTHER_SERIAL\n")()
            elif "1-7/serial" in filepath:
                return mock.mock_open(read_data="MY_SERIAL\n")()
            return mock.mock_open()()

        mock_open_file.side_effect = mock_open_side_effect

        hub = linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub(
            target_serial="MY_SERIAL"
        )
        self.assertEqual(hub._usb_bus_id, "1-7")

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch(
        "glob.glob",
        return_value=["/sys/bus/usb/devices/1-6", "/sys/bus/usb/devices/1-7"],
    )
    def test_init_discovery_with_non_matching_serial_raises_error(
        self,
        mock_glob: mock.Mock,
        mock_exists: mock.Mock,
        mock_open_file: mock.Mock,
    ) -> None:
        """Test discovery fails when specified target_serial does not match."""

        def mock_open_side_effect(
            filepath: str, *args: Any, **kwargs: Any
        ) -> Any:
            if "idVendor" in filepath:
                return mock.mock_open(read_data="18d1\n")()
            elif "idProduct" in filepath:
                return mock.mock_open(read_data="a02b\n")()
            elif "1-6/serial" in filepath:
                return mock.mock_open(read_data="OTHER_SERIAL\n")()
            elif "1-7/serial" in filepath:
                return mock.mock_open(read_data="MY_SERIAL\n")()
            return mock.mock_open()()

        mock_open_file.side_effect = mock_open_side_effect

        with self.assertRaisesRegex(ValueError, "No USB device with vendor"):
            linux_virtual_usb_hub.LinuxVirtualUsbAuthorizePowerHub(
                target_serial="NON_EXISTENT_SERIAL"
            )

    @mock.patch.object(
        linux_virtual_usb_hub, "LinuxVirtualUsbAuthorizePowerHub", autospec=True
    )
    def test_usb_power_hub_base_test_lookup_virtual_hub(
        self, mock_virtual_hub_cls: mock.Mock
    ) -> None:
        """Test UsbPowerHubBaseTest._lookup_usb_power_hub creates LinuxVirtualUsbAuthorizePowerHub."""
        test_obj = object.__new__(usb_power_hub_base_test.UsbPowerHubBaseTest)
        test_obj.user_params = {
            "use_virtual_usb_hub": True,
            "bus_id_lookup_attempts": 10,
            "bus_id_lookup_timeout_sec": 15.0,
        }
        mock_device = mock.MagicMock()
        mock_device.device_name = "fuchsia-dut"

        with mock.patch.object(
            usb_power_hub_base_test.UsbPowerHubBaseTest,
            "_get_device_config",
            return_value={"device_serial": "SER123"},
        ):
            hub, port = test_obj._lookup_usb_power_hub(mock_device)

        self.assertIs(hub, mock_virtual_hub_cls.return_value)
        self.assertIsNone(port)
        mock_virtual_hub_cls.assert_called_once_with(
            target_serial="SER123",
            bus_id_lookup_attempts=10,
            bus_id_lookup_timeout_sec=15.0,
        )

    @mock.patch.object(
        linux_virtual_usb_hub, "LinuxVirtualUsbResetPowerHub", autospec=True
    )
    def test_usb_power_hub_base_test_lookup_reset_hub(
        self, mock_reset_hub_cls: mock.Mock
    ) -> None:
        """Test `virtual_usb_hub_mode: reset` creates LinuxVirtualUsbResetPowerHub."""
        test_obj = object.__new__(usb_power_hub_base_test.UsbPowerHubBaseTest)
        test_obj.user_params = {
            "use_virtual_usb_hub": True,
            "virtual_usb_hub_mode": "reset",
        }
        mock_device = mock.MagicMock()
        mock_device.device_name = "fuchsia-dut"

        with mock.patch.object(
            usb_power_hub_base_test.UsbPowerHubBaseTest,
            "_get_device_config",
            return_value={"device_serial": "SER123"},
        ):
            hub, port = test_obj._lookup_usb_power_hub(mock_device)

        self.assertIs(hub, mock_reset_hub_cls.return_value)
        self.assertIsNone(port)
        mock_reset_hub_cls.assert_called_once_with(
            target_serial="SER123",
            bus_id_lookup_attempts=linux_virtual_usb_hub.DEFAULT_BUS_ID_LOOKUP_ATTEMPTS,
            bus_id_lookup_timeout_sec=linux_virtual_usb_hub.DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC,
        )

    def test_usb_power_hub_base_test_lookup_unsupported_mode(self) -> None:
        """Test an unsupported `virtual_usb_hub_mode` raises ValueError."""
        test_obj = object.__new__(usb_power_hub_base_test.UsbPowerHubBaseTest)
        test_obj.user_params = {
            "use_virtual_usb_hub": True,
            "virtual_usb_hub_mode": "unplug",
        }
        mock_device = mock.MagicMock()
        mock_device.device_name = "fuchsia-dut"

        with mock.patch.object(
            usb_power_hub_base_test.UsbPowerHubBaseTest,
            "_get_device_config",
            return_value={},
        ):
            with self.assertRaisesRegex(ValueError, "virtual_usb_hub_mode"):
                test_obj._lookup_usb_power_hub(mock_device)


def _fake_usb_device_node_open(busnum: int, devnum: int) -> Any:
    """Returns an `open` side effect serving sysfs busnum/devnum."""

    def side_effect(filepath: str, *args: Any, **kwargs: Any) -> Any:
        if filepath.endswith("/busnum"):
            return mock.mock_open(read_data=f"{busnum}\n")()
        if filepath.endswith("/devnum"):
            return mock.mock_open(read_data=f"{devnum}\n")()
        raise FileNotFoundError(filepath)

    return side_effect


class LinuxVirtualUsbResetPowerHubTests(unittest.TestCase):
    """Unit tests for LinuxVirtualUsbResetPowerHub."""

    def setUp(self) -> None:
        super().setUp()
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(platform, "system", return_value="Linux").start()
        # The device node permission check only runs outside infra.
        mock.patch.object(common, "is_infra", return_value=False).start()
        mock.patch.object(
            linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub,
            "_find_usb_bus_id",
            return_value="1-6",
        ).start()
        mock.patch(
            "builtins.open", side_effect=_fake_usb_device_node_open(1, 5)
        ).start()
        self.mock_access = mock.patch("os.access", return_value=True).start()
        self.mock_reset = mock.patch.object(
            ioctl, "reset_usb_device", return_value=False
        ).start()

    def test_init_checks_device_node_is_writable(self) -> None:
        """Test init checks write access to the usbfs device node."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        self.assertIsInstance(hub, usb_power_hub.UsbPowerHub)
        self.assertIsInstance(hub, usb_reset.UsbResetCapable)
        self.mock_access.assert_called_once_with(
            "/dev/bus/usb/001/005", mock.ANY
        )

    def test_init_device_node_not_writable_raises_error(self) -> None:
        """Test init fails when the usbfs device node is not writable."""
        self.mock_access.return_value = False
        with self.assertRaisesRegex(
            usb_power_hub.UsbPowerHubError, "/dev/bus/usb/001/005"
        ):
            linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()

    def test_reset_success(self) -> None:
        """Test reset issues USBDEVFS_RESET on the device node."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        hub.reset()
        self.mock_reset.assert_called_once_with("/dev/bus/usb/001/005")

    def test_reset_reenumerated_success(self) -> None:
        """Test reset succeeds when the kernel re-enumerates the device."""
        self.mock_reset.return_value = True
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        hub.reset()
        self.mock_reset.assert_called_once_with("/dev/bus/usb/001/005")

    def test_reset_re_resolves_bus_id_and_device_node(self) -> None:
        """Test reset looks up the bus ID and device node again."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        with (
            mock.patch.object(hub, "_find_usb_bus_id", return_value="2-3"),
            mock.patch(
                "builtins.open", side_effect=_fake_usb_device_node_open(2, 7)
            ),
        ):
            hub.reset()
        self.assertEqual(hub._usb_bus_id, "2-3")
        self.mock_reset.assert_called_once_with("/dev/bus/usb/002/007")

    def test_reset_failure_raises_error(self) -> None:
        """Test a failed USBDEVFS_RESET raises UsbPowerHubError."""
        self.mock_reset.side_effect = OSError(16, "Device or resource busy")
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            hub.reset()

    def test_reset_invalid_devnum_raises_error(self) -> None:
        """Test a non-integer sysfs devnum raises UsbPowerHubError."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        with mock.patch(
            "builtins.open", mock.mock_open(read_data="not-a-number\n")
        ):
            with self.assertRaises(usb_power_hub.UsbPowerHubError):
                hub.reset()
        self.mock_reset.assert_not_called()

    def test_power_off_raises_error(self) -> None:
        """Test power_off is not supported."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        with self.assertRaisesRegex(usb_power_hub.UsbPowerHubError, "reset"):
            hub.power_off()
        self.mock_reset.assert_not_called()

    def test_power_on_is_noop(self) -> None:
        """Test power_on does nothing, as the link is never left down."""
        hub = linux_virtual_usb_hub.LinuxVirtualUsbResetPowerHub()
        hub.power_on()
        self.mock_reset.assert_not_called()


class ResetUsbLinkTests(unittest.TestCase):
    """Unit tests for usb_reset.reset_usb_link."""

    def test_resets_reset_capable_hub(self) -> None:
        """Test hubs that support reset() are reset, not power cycled."""

        class _ResetHub(usb_power_hub.UsbPowerHub, usb_reset.UsbResetCapable):
            power_off = mock.Mock()
            power_on = mock.Mock()
            reset = mock.Mock()

        hub = _ResetHub()
        usb_reset.reset_usb_link(hub, port=3, off_duration_sec=5)
        hub.reset.assert_called_once_with(port=3)
        hub.power_off.assert_not_called()
        hub.power_on.assert_not_called()

    @mock.patch("time.sleep")
    def test_power_cycles_other_hubs(self, mock_sleep: mock.Mock) -> None:
        """Test other hubs fall back to a power cycle."""
        hub = mock.create_autospec(usb_power_hub.UsbPowerHub, instance=True)
        usb_reset.reset_usb_link(hub, port=3, off_duration_sec=5)
        hub.assert_has_calls(
            [mock.call.power_off(port=3), mock.call.power_on(port=3)]
        )
        mock_sleep.assert_called_once_with(5)
