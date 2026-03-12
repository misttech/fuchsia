# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Bluetooth Avrcp affordance."""

import logging

import fuchsia_base_test
from mobly import asserts, test_runner

from honeydew.affordances.connectivity.bluetooth.utils import (
    types as bluetooth_types,
)
from honeydew.transports.sl4f.errors import Sl4fError

_LOGGER: logging.Logger = logging.getLogger(__name__)

BluetoothAvrcpCommand = bluetooth_types.BluetoothAvrcpCommand


class BluetoothAvrcpAffordanceTests(fuchsia_base_test.AsyncFuchsiaBaseTest):
    """BluetoothAvrcp affordance tests"""

    async def setup_class(self) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Assigns `device` variable with FuchsiaDevice object
        """
        await super().setup_class()
        self.device = self.fuchsia_devices[0]

    async def test_avrcp_init(self) -> None:
        """Test for Bluetooth.avrcp_init() method."""
        await self.device.bluetooth_avrcp.init_avrcp(target_id="0")

    async def test_list_received_requests(self) -> None:
        """Test for Bluetooth.list_received_requests() method."""
        res = await self.device.bluetooth_avrcp.list_received_requests()
        assert len(res) == 0

    async def test_publish_mock_player(self) -> None:
        """Test for Bluetooth.publish_mock_player() method."""
        await self.device.bluetooth_avrcp.publish_mock_player()

    async def test_send_avrcp_command(self) -> None:
        """Test for Bluetooth.send_avrcp_command() method."""
        # Currently fails sending commands since we only test single device
        with asserts.assert_raises(Sl4fError):
            await self.device.bluetooth_avrcp.send_avrcp_command(
                BluetoothAvrcpCommand.PLAY
            )

    async def test_stop_mock_player(self) -> None:
        """Test for Bluetooth.stop_mock_player() method"""
        await self.device.bluetooth_avrcp.stop_mock_player()


if __name__ == "__main__":
    test_runner.main()
