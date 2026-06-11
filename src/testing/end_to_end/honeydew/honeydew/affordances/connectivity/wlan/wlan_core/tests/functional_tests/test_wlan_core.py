# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for wlan affordance."""

import time

import fidl_fuchsia_wlan_common as f_wlan_common
import fidl_fuchsia_wlan_internal as f_wlan_internal
import fuchsia_wlan_base_test
from antlion.controllers import access_point
from antlion.controllers.ap_lib import hostapd_constants
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)

from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.affordances.connectivity.wlan.utils.types import ClientStatusIdle


class WlanCoreTests(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Wlan_core affordance tests"""

    async def setup_class(self) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Assigns `device` variable with FuchsiaDevice object
            * Assigns `access_point` variable with AccessPoint object
            * Assigns `openwrt_ap` variable with OpenWrtAP object
        """
        await super().setup_class()

        await self.wait_for_interface(self.dut.netstack, PortClass.WLAN_CLIENT)

    async def test_iface_methods(self) -> None:
        """Test case for device wlan_core iface methods.

        This test gets the list of phy IDs present, creates an iface, then checks that
        is exists by querying it, then calls destroy on the iface, and finally gets the
        iface ID list to check that the created iface has been successfully destroyed.

        This test case calls the following wlan_core methods:
            * wlan_core.get_phy_id_list()
            * wlan_core.get_iface_id_list()
            * wlan_core.create_iface()
            * wlan_core.query_iface()
            * wlan_core.destroy_iface()
        """
        # We check here to make sure the device is running a softmac WLAN driver.
        # If not, we run basic tests without create_iface().
        # TODO(b/328500376): Add WLAN affordance method for this or remove if not
        # needed.
        driver_list = self.dut.ffx.run(["driver", "list"])
        if driver_list.find("iwlwifi") != -1:
            phy_ids = await self.dut.wlan_core.get_phy_id_list()
            iface_ids = await self.dut.wlan_core.get_iface_id_list()

            iface_id = await self.dut.wlan_core.create_iface(
                phy_id=phy_ids[0], role=f_wlan_common.WlanMacRole.CLIENT
            )

            query_resp = await self.dut.wlan_core.query_iface(iface_id)
            asserts.assert_equal(
                query_resp.role, f_wlan_common.WlanMacRole.CLIENT
            )
            asserts.assert_equal(query_resp.id_, iface_id)
            asserts.assert_equal(query_resp.phy_id, phy_ids[0])

            await self.dut.wlan_core.destroy_iface(iface_id)
            expected_iface_ids = await self.dut.wlan_core.get_iface_id_list()

            asserts.assert_equal(iface_ids, expected_iface_ids)
        else:
            phy_ids = await self.dut.wlan_core.get_phy_id_list()
            iface_ids = await self.dut.wlan_core.get_iface_id_list()
            if iface_ids:
                await self.dut.wlan_core.destroy_iface(iface_ids[0])

    async def test_phy_response_to_country_code_change(self) -> None:
        """Tests that all phys respond to a country code setting change."""

        async def check_country_code(
            phy_id: int, country_code: str, timeout: float
        ) -> None:
            deadline = time.time() + timeout
            while True:
                last_get_country_response = (
                    await self.dut.wlan_core.get_country(phy_id)
                )
                if last_get_country_response == country_code:
                    return
                if time.time() > deadline:
                    raise signals.TestFailure(
                        f"""Failed to observe current country code setting {country_code} on phy {phy_id}.
                            phy {phy_id} country code is still {last_get_country_response} after {timeout} seconds."""
                    )
                time.sleep(1)

        await self.dut.location.set_region("US")
        for phy_id in await self.dut.wlan_core.get_phy_id_list():
            await check_country_code(phy_id, "US", 10)

        await self.dut.location.set_region("WW")
        for phy_id in await self.dut.wlan_core.get_phy_id_list():
            await check_country_code(phy_id, "WW", 10)

    async def test_scan_and_connect(self) -> None:
        """Test case for scanning, connecting and disconnecting to a network.

        This test sets up an access point with a network, then scans and connects to
        that network. If the connect returns true we know it was successful. The test
        then calls disconnect and checks that the status is idle.

        This test case calls the following wlan methods:
            * wlan.scan_for_bss_info()
            * wlan.connect()
            * wlan.disconnect()
            * wlan.status()
        """
        if not self.openwrt_ap and not self.access_point:
            raise signals.TestSkip("Access point required for this test")

        test_ssid = AccessPointConfig.random_string()
        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=test_ssid,
                                security=SecurityOpen(),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            assert self.access_point is not None
            access_point.setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=test_ssid,
            )

        end_time = time.time() + 30
        while time.time() < end_time:
            iface_ids = await self.dut.wlan_core.get_iface_id_list()
            if len(iface_ids) > 0:
                break
        else:
            asserts.fail("No iface ids present")

        bss_scan_response = await self.dut.wlan_core.scan_for_bss_info()
        bss_desc_for_ssid = bss_scan_response.get(test_ssid)
        if bss_desc_for_ssid:
            asserts.assert_true(
                await self.dut.wlan_core.connect(
                    ssid=test_ssid,
                    bss_desc=bss_desc_for_ssid[0],
                    authentication=f_wlan_internal.Authentication(
                        f_wlan_internal.Protocol.OPEN, None
                    ),
                ),
                "Failed to connect.",
            )
        else:
            asserts.fail("Scan did not find bss descriptions for test ssid")

        await self.dut.wlan_core.disconnect()
        status = await self.dut.wlan_core.status()
        if status == ClientStatusIdle():
            return
        asserts.fail(
            f"Status did not return to idle after disconnect: {status}"
        )


if __name__ == "__main__":
    test_runner.main()
