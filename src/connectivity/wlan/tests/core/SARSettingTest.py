# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for SAR settings.
"""
import logging

logger = logging.getLogger(__name__)


import asyncio

import fidl_fuchsia_wlan_common_security as fidl_security
import fidl_fuchsia_wlan_device_service as fidl_device_svc
import fidl_fuchsia_wlan_internal as fidl_internal
from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from core_testing import base_test
from honeydew.typing.custom_types import FidlEndpoint
from mobly import asserts, signals, test_runner


class SARSettingTest(base_test.ConnectionBaseTestClassSync):
    def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[
                # Trailing comma is load-bearing: it declares this as a singleton
                # tuple: https://docs.python.org/3/library/stdtypes.html#tuple
                (scenario,)
                for scenario in fidl_internal.TxPowerScenario
            ],
        )

    def name_func(self, scenario: fidl_internal.TxPowerScenario) -> str:
        return f"test_connect_with_sar_{scenario}"

    def _test_logic(self, scenario: fidl_internal.TxPowerScenario) -> None:
        # Setup AP
        ssid: str = utils.rand_ascii_str(AP_SSID_LENGTH_2G)
        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=AP_DEFAULT_CHANNEL_2G,
            ssid=ssid,
            security=Security(security_mode=SecurityMode.OPEN),
        )

        device_monitor_proxy = fidl_device_svc.DeviceMonitorClient(
            self.fuchsia_device.fuchsia_controller.connect_device_proxy(
                FidlEndpoint(
                    "core/wlandevicemonitor",
                    "fuchsia.wlan.device.service.DeviceMonitor",
                )
            )
        )

        # Set the SAR scenario
        asyncio.run(
            device_monitor_proxy.set_tx_power_scenario(
                phy_id=self.phy_id,
                scenario=scenario,
            )
        ).unwrap()

        # Find the matching bss_description
        bss_descriptions = self.fuchsia_device.wlan_core.scan_for_bss_info()
        bss_description = None
        for ssid, descriptions in bss_descriptions.items():
            if ssid == ssid:
                bss_description = descriptions[0]
                break
        if bss_description is None:
            logger.warning("Scanned these SSIDs: %s", bss_descriptions.keys())
            raise signals.TestFailure(
                "Could not find BSS description for SSID: %s" % ssid
            )
        # Connect to the AP
        self.fuchsia_device.wlan_core.connect(
            ssid=ssid,
            password=None,
            bss_desc=bss_description,
            authentication=fidl_security.Authentication(
                protocol=fidl_security.Protocol.OPEN,
                credentials=None,
            ),
        )

        # confirm the SAR scenario is still set
        get_sar_resp = asyncio.run(
            device_monitor_proxy.get_tx_power_scenario(
                phy_id=self.phy_id,
            )
        ).unwrap()
        asserts.assert_equal(
            get_sar_resp.scenario,
            scenario,
        )


if __name__ == "__main__":
    test_runner.main()
