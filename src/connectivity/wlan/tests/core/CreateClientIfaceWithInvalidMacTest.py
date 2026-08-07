# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import logging

import fidl_fuchsia_wlan_common as fw_common
from core_testing import base_test
from honeydew.typing.custom_types import MacAddress
from mobly import asserts, test_runner

logger = logging.getLogger(__name__)


class CreateClientIfaceWithInvalidMacTest(base_test.CoreBaseTestClass):
    async def test_create_client_iface_with_universal_unicast_mac(self) -> None:
        mac = (
            MacAddress.random()
            .with_universally_administered_bit()
            .with_unicast_bit()
        )

        logger.info(
            f"Attempting to create client iface with universally administered unicast MAC {mac}..."
        )

        create_iface_result = await self.test_kit.device_monitor.create_iface(
            phy_id=self.test_kit.phy_id,
            role=fw_common.WlanMacRole.CLIENT,
            sta_address=bytes(mac),
        )

        if create_iface_result.err is None:
            asserts.fail(
                f"CreateIface successfully created an interface with an invalid MAC address: {create_iface_result}"
            )

        logger.info(
            f"CreateIface correctly rejected the invalid MAC with error: {create_iface_result}"
        )

    async def test_create_client_iface_with_local_multicast_mac(
        self,
    ) -> None:
        mac = (
            MacAddress.random()
            .with_locally_administered_bit()
            .with_multicast_bit()
        )

        logger.info(
            f"Attempting to create client iface with locally administered multicast MAC {mac}..."
        )

        create_iface_result = await self.test_kit.device_monitor.create_iface(
            phy_id=self.test_kit.phy_id,
            role=fw_common.WlanMacRole.CLIENT,
            sta_address=bytes(mac),
        )

        if create_iface_result.err is None:
            asserts.fail(
                f"CreateIface successfully created an interface with an invalid MAC address: {create_iface_result}"
            )

        logger.info(
            f"CreateIface correctly rejected the invalid MAC with error: {create_iface_result}"
        )

    async def test_create_client_iface_with_universal_multicast_mac(
        self,
    ) -> None:
        mac = (
            MacAddress.random()
            .with_universally_administered_bit()
            .with_multicast_bit()
        )

        logger.info(
            f"Attempting to create client iface with universally administered multicast MAC {mac}..."
        )

        create_iface_result = await self.test_kit.device_monitor.create_iface(
            phy_id=self.test_kit.phy_id,
            role=fw_common.WlanMacRole.CLIENT,
            sta_address=bytes(mac),
        )

        if create_iface_result.err is None:
            asserts.fail(
                f"CreateIface successfully created an interface with an invalid MAC address: {create_iface_result}"
            )

        logger.info(
            f"CreateIface correctly rejected the invalid MAC with error: {create_iface_result}"
        )


if __name__ == "__main__":
    test_runner.main()
