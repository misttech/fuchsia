# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Test that the device has exactly one phy.

This test could assert at least one phy; however, it's likely an error today
if there is more than one phy listed.
"""
import logging

logger = logging.getLogger(__name__)


import fidl_fuchsia_wlan_common as fw_common
from core_testing import base_test
from mobly import asserts, test_runner


class CreateApIfaceWithNullMacTest(base_test.CoreBaseTestClass):
    async def test_create_ap_iface_with_null_mac(self) -> None:
        create_iface_response = (
            await self.test_kit.device_monitor.create_iface(
                phy_id=self.test_kit.phy_id,
                role=fw_common.WlanMacRole.AP,
                sta_address=[0, 0, 0, 0, 0, 0],
            )
        ).unwrap()
        assert (
            create_iface_response.iface_id is not None
        ), "DeviceMonitor.CreateIface() response is missing a iface_id"
        iface_id = create_iface_response.iface_id

        query_iface_response = (
            (await self.test_kit.device_monitor.query_iface(iface_id=iface_id))
            .unwrap()
            .resp
        )
        asserts.assert_equal(iface_id, query_iface_response.id_)
        asserts.assert_equal(self.test_kit.phy_id, query_iface_response.phy_id)
        asserts.assert_equal(
            fw_common.WlanMacRole.AP, query_iface_response.role
        )
        asserts.assert_not_equal(
            [0, 0, 0, 0, 0, 0], query_iface_response.sta_addr
        )


if __name__ == "__main__":
    test_runner.main()
