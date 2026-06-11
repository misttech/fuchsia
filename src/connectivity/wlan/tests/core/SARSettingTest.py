# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for SAR settings.
"""
import logging

from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)

logger = logging.getLogger(__name__)


import fidl_fuchsia_wlan_device_service as fidl_device_svc
import fidl_fuchsia_wlan_internal as fidl_security
import fidl_fuchsia_wlan_internal as fidl_internal
from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from core_testing import base_test
from honeydew.typing.custom_types import FidlEndpoint
from mobly import asserts, signals, test_runner


class SARSettingTest(base_test.ConnectionBaseTestClass):
    async def pre_run(self) -> None:
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

    async def _test_logic(
        self, scenario: fidl_internal.TxPowerScenario
    ) -> None:
        # Setup AP
        ssid: str = utils.rand_ascii_str(AP_SSID_LENGTH_2G)
        if not self.test_kit.access_point:
            raise signals.TestAbortClass(
                "No access point configured for this test."
            )
        if isinstance(self.test_kit.access_point, OpenWrtAP):
            self.test_kit.access_point.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    security=SecurityOpen(),
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(self.test_kit.access_point, AccessPoint):
            setup_ap(
                access_point=self.test_kit.access_point,
                profile_name="whirlwind",
                channel=AP_DEFAULT_CHANNEL_2G,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=DeprecatedSecurityMode.OPEN
                ),
            )

        device_monitor_proxy = fidl_device_svc.DeviceMonitorClient(
            self.dut.fuchsia_controller.connect_device_proxy(
                FidlEndpoint(
                    "core/wlandevicemonitor",
                    "fuchsia.wlan.device.service.DeviceMonitor",
                )
            )
        )

        # Set the SAR scenario
        (
            await device_monitor_proxy.set_tx_power_scenario(
                phy_id=self.test_kit.phy_id,
                scenario=scenario,
            )
        ).unwrap()

        # Find the matching bss_description
        scan_results = await self.dut.wlan_core.scan_for_bss_info()
        try:
            bss_description = scan_results[ssid][0]
        except KeyError:
            logger.warning("Scanned these SSIDs: %s", scan_results.keys())
            raise signals.TestFailure(
                "Could not find BSS description for SSID: %s" % ssid
            )

        # Connect to the AP
        await self.dut.wlan_core.connect(
            ssid=ssid,
            bss_desc=bss_description,
            authentication=fidl_security.Authentication(
                protocol=fidl_security.Protocol.OPEN,
                credentials=None,
            ),
        )

        # confirm the SAR scenario is still set
        get_sar_resp = (
            await device_monitor_proxy.get_tx_power_scenario(
                phy_id=self.test_kit.phy_id,
            )
        ).unwrap()
        asserts.assert_equal(
            get_sar_resp.scenario,
            scenario,
        )


if __name__ == "__main__":
    test_runner.main()
