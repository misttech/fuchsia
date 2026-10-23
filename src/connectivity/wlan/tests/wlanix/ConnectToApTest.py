# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for connecting to an access point.
"""

import logging

import fuchsia_wlan_base_test
import wlanix_testing
from legacy_access_point.access_point import AccessPoint, setup_ap
from legacy_access_point.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from mobly import signals, test_runner
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWpa2,
    SecurityWpa3,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

logger = logging.getLogger(__name__)


class ConnectToApTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    wlanix: wlanix_testing.Wlanix
    chip: wlanix_testing.WifiChip

    async def setup_class(self) -> None:
        await super().setup_class()
        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("Requires at least one access point")
        if isinstance(self.access_point, AccessPoint):
            self.access_point.stop_all_aps()
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.chip.destroy_all_sta_ifaces()

    async def teardown_test(self) -> None:
        try:
            await self.chip.destroy_all_sta_ifaces()
        finally:
            if isinstance(self.access_point, AccessPoint):
                self.access_point.stop_all_aps()
            await super().teardown_test()

    async def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[
                (SecurityOpen(), None),
                (SecurityWpa2(), AccessPointConfig.random_string()),
                (SecurityWpa3(), AccessPointConfig.random_string()),
            ],
        )

    def name_func(self, security: Security, password: str | None) -> str:
        return f"test_successfully_connect_to_ap_{security}"

    async def _test_logic(
        self, security: Security, password: str | None
    ) -> None:
        iface = await self.chip.create_sta_iface()
        ssid = AccessPointConfig.random_string(AP_SSID_LENGTH_2G)

        if self.openwrt_ap:
            self.openwrt_ap.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password,
                                    security=security,
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(self.access_point, AccessPoint):
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=AP_DEFAULT_CHANNEL_2G,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                ),
            )

        await iface.scan_and_connect(
            ssid=ssid, security=security, password=password
        )


if __name__ == "__main__":
    test_runner.main()
