# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Test that the WifiChip can be restarted.
"""

import asyncio
import logging
from typing import Any

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
from mobly import signals, test_runner
from wlanix_testing import WifiEventCallbackServer, base_test


class WifiChipRestartTest(base_test.WifiChipBaseTestClass):
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, allow_ifaces_between_tests=True, **kwargs)

    async def test_trigger_subsystem_restart(self) -> None:
        """Test calling TriggerSubsystemRestart API."""

        # Connect to Wifi to register callback
        (
            proxy,
            server,
        ) = self.dut.fuchsia_controller.channel_create()
        self.wlanix_proxy.get_wifi(wifi=server.take())
        wifi_proxy = fidl_wlanix.WifiClient(proxy)

        # Register callback
        (
            callback_client_end,
            callback_server_end,
        ) = self.dut.fuchsia_controller.channel_create()
        async with WifiEventCallbackServer(
            callback_client_end, callback_server_end
        ) as ctx:
            wifi_proxy.register_event_callback(
                callback=ctx.callback_channel.take()
            )

            # Ensure the event callback registration is fully processed
            # by making a two-way call on the same channel before triggering restart.
            (await wifi_proxy.get_state()).unwrap()

            logging.info("Triggering subsystem restart...")
            # Trigger restart
            (await self.wifi_chip_proxy.trigger_subsystem_restart()).unwrap()
            logging.info("Restart triggered.")

            # Wait for subsystem restart
            try:
                await asyncio.wait_for(
                    ctx.subsystem_restart_received.wait(),
                    timeout=30,
                )
                logging.info("OnSubsystemRestart event received.")
            except asyncio.TimeoutError as e:
                raise signals.TestFailure(
                    "Timed out waiting for OnSubsystemRestart event"
                ) from e


if __name__ == "__main__":
    test_runner.main()
