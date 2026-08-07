# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

import fidl_fuchsia_wlan_device_service as f_wlan_device_service
from core_testing import base_test
from honeydew.affordances.connectivity.wlan.utils.types import CountryCode
from honeydew.typing.custom_types import FidlEndpoint
from mobly import asserts, test_runner

logger = logging.getLogger(__name__)


class CountryCodeTest(base_test.CoreBaseTestClass):
    async def test_phy_response_to_country_code_change(self) -> None:
        fc = self.dut.fuchsia_controller
        proxy = f_wlan_device_service.DeviceMonitorClient(
            fc.connect_device_proxy(
                FidlEndpoint(
                    "core/wlandevicemonitor",
                    "fuchsia.wlan.device.service.DeviceMonitor",
                )
            )
        )

        async def set_country_code(
            phy_id: int,
            goal_country_code: str,
        ) -> None:
            logger.info(
                f"Setting country code on PHY {phy_id} to {goal_country_code} via DeviceMonitor..."
            )
            set_country_response = await proxy.set_country(
                req=f_wlan_device_service.SetCountryRequest(
                    phy_id=phy_id,
                    alpha2=list(goal_country_code.encode("ascii")),
                )
            )
            asserts.assert_equal(
                set_country_response.status,
                0,
                f"Failed to set country code on PHY {phy_id}. Status: {set_country_response.status}",
            )

            # Immediately verify it was set
            get_country_response = (
                await proxy.get_country(phy_id=phy_id)
            ).unwrap()
            current_country_code = bytes(
                get_country_response.resp.alpha2
            ).decode("ascii")

            asserts.assert_equal(
                str(current_country_code),
                goal_country_code,
                f"Wrong country code on PHY {phy_id}.",
            )

        for phy_id in await self.dut.wlan_core.get_phy_id_list():
            # Get the original country code
            get_country_response = (
                await proxy.get_country(phy_id=phy_id)
            ).unwrap()
            original_country_code = bytes(
                get_country_response.resp.alpha2
            ).decode("ascii")
            logger.info(
                f"Original country code on PHY {phy_id} is {original_country_code}."
            )

            # Pick a different country code
            new_country_code = [
                cc for cc in CountryCode if cc != original_country_code
            ][0]

            # Set to the new valid country code
            await set_country_code(phy_id, new_country_code)

            # Do not restore the original country code. Tests that
            # require setting a particular country code should set the
            # country code themselves. Otherwise, tests should assume
            # only globally supported channels are available, and
            # those channels are available, by definition, regardless
            # of the country code setting.


if __name__ == "__main__":
    test_runner.main()
