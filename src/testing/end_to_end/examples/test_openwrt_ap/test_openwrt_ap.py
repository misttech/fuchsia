# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""OpenWRT AP test for Lacewing."""

import asyncio
import logging

import fidl_fuchsia_wlan_internal as f_wlan_internal
import fuchsia_base_test
import openwrt_access_point
from honeydew.affordances.connectivity.wlan.utils.types import ClientStatusIdle
from mobly import asserts, signals, test_runner
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityOpen,
    SecurityWpa2,
    SecurityWpa2Wpa3Mixed,
    SecurityWpa3,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)


class OpenWrtAPScanConnectTest(fuchsia_base_test.FuchsiaBaseTest):
    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        self.log = logging.getLogger()

        if not self.fuchsia_devices:
            raise signals.TestAbortClass(
                "At least one Fuchsia device is required"
            )
        self.openwrt_aps: list[
            OpenWrtAP
        ] | None = await self.register_controller(openwrt_access_point)
        if not self.fuchsia_devices:
            raise signals.TestAbortClass(
                "At least one Fuchsia device is required"
            )
        if not self.openwrt_aps:
            raise signals.TestAbortClass(
                "At least one OpenWRT access point is required"
            )

        self.openwrt_ap = self.openwrt_aps[0]

    async def _test_scan_and_connect(
        self, wifi_config: AccessPointConfig
    ) -> None:
        """Helper to test scanning and connecting for a given Wi-Fi config."""
        assert (
            wifi_config.radios
        ), "Expected at least one Radio configuration, but found None."
        assert wifi_config.radios[
            0
        ].bss_settings, (
            "Expected at least one BSS configuration, but found None."
        )
        bss_settings = wifi_config.radios[0].bss_settings[0]

        self.openwrt_ap.configure_wifi(wifi_config)

        self.log.info("Starting scan for SSID: %s", bss_settings.ssid)
        bss_desc_for_ssid = None
        for attempt in range(3):  # Retry up to 3 times
            bss_scan_response = await self.dut.wlan_core.scan_for_bss_info()
            bss_desc_for_ssid = bss_scan_response.get(bss_settings.ssid)
            if bss_desc_for_ssid and len(bss_desc_for_ssid) > 0:
                break
            self.log.info(
                f"SSID {bss_settings.ssid} not found on attempt {attempt + 1}, retrying..."
            )

            await asyncio.sleep(2)

        self.log.info("Found SSID: %s", bss_settings.ssid)

        # TODO: https://fxbug.dev/487800358 - Create and use to_fidl() function.
        if isinstance(bss_settings.security, SecurityWpa2):
            if not bss_settings.password:
                raise signals.TestFailure(
                    "Password must be provided for WPA2 security"
                )
            authentication = f_wlan_internal.Authentication(
                f_wlan_internal.Protocol.WPA2_PERSONAL,
                f_wlan_internal.Credentials(
                    wpa=f_wlan_internal.WpaCredentials(
                        passphrase=bss_settings.password.encode("utf-8")
                    )
                ),
            )
        elif isinstance(
            bss_settings.security, (SecurityWpa3, SecurityWpa2Wpa3Mixed)
        ):
            if not bss_settings.password:
                raise signals.TestFailure(
                    "Password must be provided for WPA3 security"
                )
            authentication = f_wlan_internal.Authentication(
                f_wlan_internal.Protocol.WPA3_PERSONAL,
                f_wlan_internal.Credentials(
                    wpa=f_wlan_internal.WpaCredentials(
                        passphrase=bss_settings.password.encode("utf-8")
                    )
                ),
            )
        else:
            authentication = f_wlan_internal.Authentication(
                f_wlan_internal.Protocol.OPEN, None
            )

        if bss_desc_for_ssid and len(bss_desc_for_ssid) > 0:
            success = await self.dut.wlan_core.connect(
                ssid=bss_settings.ssid,
                bss_desc=bss_desc_for_ssid[0],
                authentication=authentication,
            )
            asserts.assert_true(success, "Failed to connect.")
        else:
            asserts.fail(
                f"SSID {bss_settings.ssid} not found in bss descriptions."
            )

        await self.dut.wlan_core.disconnect()
        status = await self.dut.wlan_core.status()
        if status == ClientStatusIdle():
            return
        asserts.fail(
            f"Status did not return to idle after disconnect: {status}"
        )

    async def test_scan_and_connect_2g(self) -> None:
        """Test case for scanning and connecting to a 2G OpenWRT AP."""
        await self._test_scan_and_connect(
            AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=AccessPointConfig.random_string(),
                                security=SecurityOpen(),
                            )
                        ],
                    )
                ]
            )
        )

    async def test_scan_and_connect_5g(self) -> None:
        """Test case for scanning and connecting to a 5G OpenWRT AP."""
        await self._test_scan_and_connect(
            AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_5G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=AccessPointConfig.random_string(),
                                security=SecurityOpen(),
                            )
                        ],
                    )
                ]
            )
        )

    async def test_scan_and_connect_wpa2(self) -> None:
        """Test case for scanning and connecting to a WPA2 network."""
        await self._test_scan_and_connect(
            AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=AccessPointConfig.random_string(),
                                security=SecurityWpa2(),
                                password=AccessPointConfig.random_string(16),
                            )
                        ],
                    )
                ]
            )
        )

    async def test_scan_and_connect_wpa3(self) -> None:
        """Test case for scanning and connecting to a WPA3 network."""
        await self._test_scan_and_connect(
            AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=AccessPointConfig.random_string(),
                                security=SecurityWpa2Wpa3Mixed(),
                                password=AccessPointConfig.random_string(16),
                            )
                        ],
                    )
                ]
            )
        )


if __name__ == "__main__":
    test_runner.main()
