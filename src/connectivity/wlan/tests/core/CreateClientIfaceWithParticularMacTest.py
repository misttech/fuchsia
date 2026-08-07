# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

import fidl_fuchsia_wlan_common as fw_common
from core_testing import base_test
from honeydew.affordances.connectivity.wlan.utils.types import MacAddress
from mobly import asserts, test_runner

logger = logging.getLogger(__name__)


class CreateClientIfaceWithParticularMacTest(base_test.CoreBaseTestClass):
    async def test_create_client_iface_with_particular_mac(self) -> None:
        # Generate a valid randomized MAC to set in the driver
        random_sta_addr = (
            MacAddress.random()
            .with_unicast_bit()
            .with_locally_administered_bit()
        )

        logger.info(f"Creating client iface with MAC {random_sta_addr}")
        create_iface_response = (
            await self.test_kit.device_monitor.create_iface(
                phy_id=self.test_kit.phy_id,
                role=fw_common.WlanMacRole.CLIENT,
                sta_address=bytes(random_sta_addr),
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
            fw_common.WlanMacRole.CLIENT, query_iface_response.role
        )
        asserts.assert_equal(
            random_sta_addr,
            MacAddress(bytes(query_iface_response.sta_addr)),
        )


if __name__ == "__main__":
    test_runner.main()
