# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging

import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from mobly import asserts, test_runner

logger = logging.getLogger(__name__)


class PhyResetTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    phy: wlan_core.Phy

    async def setup_class(self) -> None:
        await super().setup_class()
        self.phy = await self.dut.wlan_core.ensure_single_phy()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def test_reset_phy_deletes_existing_iface(self) -> None:
        iface = await self.phy.create_client_iface()
        ifaces_before = (await self.phy.device_monitor.list_ifaces()).iface_list
        asserts.assert_in(iface.id, ifaces_before)

        (await self.phy.device_monitor.reset(phy_id=self.phy.id)).unwrap()

        iface_removal_timeout = 10.0
        poll_interval = 0.25
        for _ in range(int(iface_removal_timeout / poll_interval)):
            ifaces_after = (
                await self.phy.device_monitor.list_ifaces()
            ).iface_list
            if len(ifaces_after) == 0:
                break
            await asyncio.sleep(poll_interval)
        else:
            ifaces_after = (
                await self.phy.device_monitor.list_ifaces()
            ).iface_list
            asserts.assert_equal(
                ifaces_after,
                [],
                "Expected all existing interfaces to be deleted after PHY reset",
            )

        new_iface = await self.phy.create_client_iface()
        query_iface_response = await new_iface.query()
        asserts.assert_equal(new_iface.id, query_iface_response.id_)
        asserts.assert_equal(self.phy.id, query_iface_response.phy_id)


if __name__ == "__main__":
    test_runner.main()
