# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for fuchsia_base_test."""

import unittest
from unittest import mock

import fuchsia_base_test
from fuchsia_controller_py import ZxStatus
from honeydew import errors
from honeydew.auxiliary_devices.power_switch import power_switch
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from honeydew.typing import custom_types
from mobly import config_parser, signals

# pylint: disable=protected-access


class FuchsiaBaseTestTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for fuchsia_base_test.FuchsiaBaseTest."""

    def setUp(self) -> None:
        config = config_parser.TestRunConfig()
        config.testbed_name = "FakeTestbed"
        config.log_path = "/tmp"
        self.test_obj = fuchsia_base_test.FuchsiaBaseTest(config)
        self.devices = [
            mock.create_autospec(fuchsia_device.FuchsiaDevice, instance=True)
            for _ in range(2)
        ]
        self.test_obj.fuchsia_devices = self.devices

    async def test_log_message_to_devices_ignores_device_errors(self) -> None:
        """Test _log_message_to_devices() still logs to the other devices when
        logging to one of them fails."""
        self.devices[0].log_message_to_device.side_effect = ZxStatus(
            ZxStatus.ZX_ERR_PEER_CLOSED
        )

        await self.test_obj._log_message_to_devices(
            message="message", level=custom_types.LEVEL.INFO
        )

        for device in self.devices:
            device.log_message_to_device.assert_called_once_with(
                "message", custom_types.LEVEL.INFO
            )

    async def test_log_message_to_devices_propagates_unexpected_exceptions(
        self,
    ) -> None:
        """Test _log_message_to_devices() propagates unexpected exceptions
        (e.g. Mobly signals) raised while logging to a device."""
        self.devices[
            0
        ].log_message_to_device.side_effect = signals.TestAbortAll("abort")

        with self.assertRaises(signals.TestAbortAll):
            await self.test_obj._log_message_to_devices(
                message="message", level=custom_types.LEVEL.INFO
            )

        self.devices[0].log_message_to_device.assert_called_once()
        self.devices[1].log_message_to_device.assert_not_called()

    async def test_collect_snapshot_ignores_device_errors(self) -> None:
        """Test _collect_snapshot() still collects snapshots from the other
        devices when collecting from one of them fails."""
        self.devices[0].snapshot.side_effect = OSError("No space left")

        await self.test_obj._collect_snapshot(directory="/tmp/snapshots")

        for device in self.devices:
            device.snapshot.assert_called_once_with(directory="/tmp/snapshots")

    async def test_collect_snapshot_propagates_unexpected_exceptions(
        self,
    ) -> None:
        """Test _collect_snapshot() propagates unexpected exceptions (e.g.
        Mobly signals) raised while collecting a snapshot."""
        self.devices[0].snapshot.side_effect = signals.TestAbortAll("abort")

        with self.assertRaises(signals.TestAbortAll):
            await self.test_obj._collect_snapshot(directory="/tmp/snapshots")

        self.devices[0].snapshot.assert_called_once()
        self.devices[1].snapshot.assert_not_called()

    def _mock_power_switch_and_usb_power_hub(
        self,
    ) -> tuple[mock.Mock, mock.Mock]:
        """Makes all devices use the same mocked power switch and USB power
        hub, and returns them."""
        for device in self.devices:
            # No active tracing session to stop in teardown_class().
            device.tracing = mock.MagicMock()
            device.tracing.is_active.return_value = False
            # Auxiliary devices are only powered on for unreachable devices.
            device.ffx = mock.MagicMock()
            device.ffx.check_connection.side_effect = errors.HoneydewError(
                "unreachable"
            )
        switch = mock.create_autospec(power_switch.PowerSwitch, instance=True)
        hub = mock.create_autospec(usb_power_hub.UsbPowerHub, instance=True)
        self.enterContext(
            mock.patch.object(
                self.test_obj, "_lookup_power_switch", return_value=(switch, 1)
            )
        )
        self.enterContext(
            mock.patch.object(
                self.test_obj, "_lookup_usb_power_hub", return_value=(hub, 2)
            )
        )
        return switch, hub

    async def test_teardown_class_ignores_power_on_failures(self) -> None:
        """Test teardown_class() still powers on all devices when powering on
        fails."""
        switch, hub = self._mock_power_switch_and_usb_power_hub()
        switch.power_on.side_effect = power_switch.PowerSwitchError("error")
        hub.power_on.side_effect = usb_power_hub.UsbPowerHubError("error")

        await self.test_obj.teardown_class()

        self.assertEqual(switch.power_on.call_count, len(self.devices))
        self.assertEqual(hub.power_on.call_count, len(self.devices))

    async def test_teardown_class_propagates_unexpected_exceptions_from_power_switch(
        self,
    ) -> None:
        """Test teardown_class() propagates unexpected exceptions (e.g. Mobly
        signals) raised while powering on the power switch."""
        switch, hub = self._mock_power_switch_and_usb_power_hub()
        switch.power_on.side_effect = signals.TestAbortAll("abort")

        with self.assertRaises(signals.TestAbortAll):
            await self.test_obj.teardown_class()

        switch.power_on.assert_called_once_with(outlet=1)
        hub.power_on.assert_not_called()

    async def test_teardown_class_propagates_unexpected_exceptions_from_usb_power_hub(
        self,
    ) -> None:
        """Test teardown_class() propagates unexpected exceptions (e.g. Mobly
        signals) raised while powering on the USB power hub."""
        switch, hub = self._mock_power_switch_and_usb_power_hub()
        hub.power_on.side_effect = signals.TestAbortAll("abort")

        with self.assertRaises(signals.TestAbortAll):
            await self.test_obj.teardown_class()

        switch.power_on.assert_called_once_with(outlet=1)
        hub.power_on.assert_called_once_with(port=2)


if __name__ == "__main__":
    unittest.main()
