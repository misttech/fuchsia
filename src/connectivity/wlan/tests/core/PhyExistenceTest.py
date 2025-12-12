# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Test that the device has exactly one phy.

This test could assert at least one phy; however, it's likely an error today
if there is more than one phy listed.
"""
import asyncio
import logging

logger = logging.getLogger(__name__)


from datetime import timedelta

import fidl_fuchsia_wlan_device_service as fidl_device_service
from core_testing import base_test
from core_testing.handlers import DeviceWatcherEventHandler
from fuchsia_controller_py import Channel
from fuchsia_controller_py.wrappers import asyncmethod
from mobly import test_runner
from mobly.asserts import assert_equal, fail

PAUSE_FOR_ADDITIONAL_PHY_DEVICES = timedelta(seconds=5)


class PhyExistenceTest(base_test.CoreBaseTestClass):
    @asyncmethod
    async def test_get_phy_ids(self) -> None:
        proxy, server = Channel.create()

        # Wait for first phy device to appear, and assert no additional
        # phy devices are added after a brief pause.
        self.device_monitor_proxy.watch_devices(watcher=server.take())
        async with DeviceWatcherEventHandler(
            client=fidl_device_service.DeviceWatcherClient(proxy.take()),
            verbose=True,
        ) as ctx:
            next_txn = await ctx.txn_queue.get()
            match type(next_txn):
                case fidl_device_service.DeviceWatcherOnPhyAddedRequest:
                    pass
                case _:
                    fail(f"Expected OnPhyAdded, but received: {next_txn}")

            logger.info(
                f"Pausing {PAUSE_FOR_ADDITIONAL_PHY_DEVICES} seconds for additional phy devices to appear, if any."
            )
            await asyncio.sleep(PAUSE_FOR_ADDITIONAL_PHY_DEVICES.seconds)
            assert (
                ctx.txn_queue.empty()
            ), "Unexpectedly received additional callback messages."

        list_phys_response = await self.device_monitor_proxy.list_phys()
        assert (
            list_phys_response.phy_list is not None
        ), "DeviceMonitor.ListPhys() response is missing a phy_list value"
        assert_equal(
            len(list_phys_response.phy_list),
            1,
            "DeviceMonitor.ListPhys() should return exactly one phy_id.",
        )


if __name__ == "__main__":
    test_runner.main()
