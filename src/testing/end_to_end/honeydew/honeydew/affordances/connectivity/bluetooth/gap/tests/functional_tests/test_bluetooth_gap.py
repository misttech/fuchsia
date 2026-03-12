# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Bluetooth affordance."""

import logging

import fuchsia_base_test
from mobly import asserts, test_runner

from honeydew.affordances.connectivity.bluetooth.utils import types as bt_types

_LOGGER: logging.Logger = logging.getLogger(__name__)

BluetoothAcceptPairing = bt_types.BluetoothAcceptPairing
BluetoothConnectionType = bt_types.BluetoothConnectionType


class BluetoothGapAffordanceTests(fuchsia_base_test.AsyncFuchsiaBaseTest):
    """BluetoothGap affordance tests"""

    async def setup_class(self) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Assigns `device` variable with FuchsiaDevice object
        """
        await super().setup_class()
        self.device = self.fuchsia_devices[0]

    async def test_bluetooth_accept_pairing(self) -> None:
        """Test case for bluetooth.accept_pairing()"""

        input_mode = BluetoothAcceptPairing.DEFAULT_INPUT_MODE
        output_mode = BluetoothAcceptPairing.DEFAULT_OUTPUT_MODE

        await self.device.bluetooth_gap.accept_pairing(input_mode, output_mode)

    async def test_bluetooth_connect_device(self) -> None:
        """Test case for bluetooth.connect_device()"""

        identifier = 000000000
        transport = BluetoothConnectionType.CLASSIC
        await self.device.bluetooth_gap.connect_device(identifier, transport)

    async def test_bluetooth_forget_device(self) -> None:
        """Test case for bluetooth.forget_device()"""

        identifier = 000000000

        await self.device.bluetooth_gap.forget_device(identifier)

    async def test_get_active_adapter_address(self) -> None:
        """Test case for bluetooth.get_active_adapter_address()"""
        await self.device.bluetooth_gap.get_active_adapter_address()

    async def test_bluetooth_get_connected_devices(self) -> None:
        """Test case for bluetooth.get_connected_devices()"""

        res = await self.device.bluetooth_gap.get_connected_devices()
        asserts.assert_equal(res, [])

    async def test_bluetooth_get_known_remote_devices(self) -> None:
        """Test case for bluetooth.get_known_remote_devices()"""

        await self.device.bluetooth_gap.get_known_remote_devices()

    async def test_bluetooth_pair_device(self) -> None:
        """Test case for bluetooth.pair_device()"""

        identifier = 000000000
        transport = BluetoothConnectionType.CLASSIC

        await self.device.bluetooth_gap.pair_device(identifier, transport)

    async def test_bluetooth_request_discovery(self) -> None:
        """Test case for bluetooth.request_discovery()"""

        await self.device.bluetooth_gap.request_discovery(True)

    async def test_bluetooth_set_discoverable(self) -> None:
        """Test case for bluetooth_gap.set_discoverable()"""

        await self.device.bluetooth_gap.set_discoverable(True)


if __name__ == "__main__":
    test_runner.main()
