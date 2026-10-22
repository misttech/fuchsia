# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.device_classes.smart_display.py."""

import unittest
from unittest import mock

from honeydew.device_classes import smart_display
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types


class SmartDisplayTests(unittest.TestCase):
    """Unit tests for honeydew.device_classes.smart_display.py."""

    def test_smart_display_is_a_fuchsia_device(self) -> None:
        """Test case to make sure SmartDisplay is a FuchsiaDevice."""
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
            obj = smart_display.SmartDisplay(
                device_info=custom_types.DeviceInfo(
                    name="fuchsia-smart-display",
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
            self.assertIsInstance(obj, fuchsia_device.FuchsiaDevice)
            self.assertIsInstance(obj, smart_display.SmartDisplay)


if __name__ == "__main__":
    unittest.main()
