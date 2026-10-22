# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.device_classes.workbench.py."""

import unittest
from unittest import mock

from honeydew import errors
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_using_usb_cli,
)
from honeydew.device_classes import workbench
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types


class WorkbenchTests(unittest.TestCase):
    """Unit tests for honeydew.device_classes.workbench.py."""

    def setUp(self) -> None:
        super().setUp()
        with (
            mock.patch.object(
                ffx.FFX,
                "_check_running_monitor",
                return_value=False,
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "create_context",
                autospec=True,
            ),
            mock.patch.object(
                ffx.FFX,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "check_connection",
                autospec=True,
            ),
        ):
            self.wb_obj = workbench.Workbench(
                device_info=custom_types.DeviceInfo(
                    name="fuchsia-workbench",
                    serial_number=None,
                    ip_port=None,
                    serial_socket=None,
                ),
                ffx_config_data=ffx_config.FfxConfigData(
                    logs_dir="/tmp/logs",
                    binary_path="/bin/ffx",
                    logs_level="debug",
                    enable_usb=False,
                    usb_socket_path=None,
                    usb_driver_autostart=False,
                    subtools_search_path=None,
                    proxy_timeout_secs=None,
                    ssh_keepalive_timeout=None,
                    emu_instance_dir=None,
                    ssh_private_keys=None,
                    ssh_public_keys=None,
                    shared_data="/tmp/shared_data",
                ),
            )

    def test_workbench_is_a_fuchsia_device(self) -> None:
        """Test case to make sure Workbench is a FuchsiaDevice."""
        self.assertIsInstance(self.wb_obj, fuchsia_device.FuchsiaDevice)
        self.assertIsInstance(self.wb_obj, workbench.Workbench)

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "verify_supported",
        autospec=True,
    )
    def test_system_power_state_controller(
        self,
        mock_verify_supported: mock.Mock,
    ) -> None:
        """Test case for Workbench.system_power_state_controller."""
        self.assertIsInstance(
            self.wb_obj.system_power_state_controller,
            system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        )
        mock_verify_supported.assert_called_once()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "verify_supported",
        side_effect=errors.NotSupportedError("Not supported"),
        autospec=True,
    )
    def test_system_power_state_controller_not_supported(
        self,
        mock_verify_supported: mock.Mock,
    ) -> None:
        """Test case for Workbench.system_power_state_controller raising NotSupportedError."""
        with self.assertRaises(errors.NotSupportedError):
            _ = self.wb_obj.system_power_state_controller
        mock_verify_supported.assert_called_once()


if __name__ == "__main__":
    unittest.main()
