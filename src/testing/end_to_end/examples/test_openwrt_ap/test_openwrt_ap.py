# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""OpenWRT AP test for Lacewing."""

import asyncio
import logging

import fidl_fuchsia_wlan_common_security as f_wlan_common_security
import fuchsia_base_test
from honeydew.affordances.connectivity.wlan.utils.types import ClientStatusIdle
from mobly import asserts, signals, test_runner
from mobly_controller import openwrt_access_point
from mobly_controller.openwrt_access_point import OpenWrtAP
from mobly_controller.openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    Security,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)


class OpenWrtAPScanConnectTest(fuchsia_base_test.AsyncFuchsiaBaseTest):
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

        self.device = self.fuchsia_devices[0]

        self.openwrt_ap = self.openwrt_aps[0]

    async def _test_scan_and_connect(
        self, wifi_config: AccessPointConfig
    ) -> None:
        """Helper to test scanning and connecting for a given Wi-Fi config."""
        self.openwrt_ap.configure_wifi(wifi_config)
        asserts.assert_true(
            self.openwrt_ap.verify_wifi_status(band=wifi_config.band),
            "WiFi failed to start.",
        )

        self.log.info("Starting scan for SSID: %s", wifi_config.ssid)
        bss_desc_for_ssid = None
        for attempt in range(3):  # Retry up to 3 times
            bss_scan_response = await self.device.wlan_core.scan_for_bss_info()
            bss_desc_for_ssid = bss_scan_response.get(wifi_config.ssid)
            if bss_desc_for_ssid and len(bss_desc_for_ssid) > 0:
                break
            self.log.info(
                f"SSID {wifi_config.ssid} not found on attempt {attempt + 1}, retrying..."
            )

            await asyncio.sleep(2)

        self.log.info("Found SSID: %s", wifi_config.ssid)

        # TODO: https://fxbug.dev/487800358 - Create and use to_fidl() function.
        if wifi_config.security == Security.WPA2:
            if not wifi_config.password:
                raise signals.TestFailure(
                    "Password must be provided for WPA2 security"
                )
            authentication = f_wlan_common_security.Authentication(
                f_wlan_common_security.Protocol.WPA2_PERSONAL,
                f_wlan_common_security.Credentials(
                    wpa=f_wlan_common_security.WpaCredentials(
                        passphrase=wifi_config.password.encode("utf-8")
                    )
                ),
            )
        elif wifi_config.security in (Security.WPA3, Security.WPA2_WPA3):
            if not wifi_config.password:
                raise signals.TestFailure(
                    "Password must be provided for WPA3 security"
                )
            authentication = f_wlan_common_security.Authentication(
                f_wlan_common_security.Protocol.WPA3_PERSONAL,
                f_wlan_common_security.Credentials(
                    wpa=f_wlan_common_security.WpaCredentials(
                        passphrase=wifi_config.password.encode("utf-8")
                    )
                ),
            )
        else:
            authentication = f_wlan_common_security.Authentication(
                f_wlan_common_security.Protocol.OPEN, None
            )

        if bss_desc_for_ssid and len(bss_desc_for_ssid) > 0:
            success = await self.device.wlan_core.connect(
                ssid=wifi_config.ssid,
                bss_desc=bss_desc_for_ssid[0],
                authentication=authentication,
            )
            asserts.assert_true(success, "Failed to connect.")
        else:
            asserts.fail(
                f"SSID {wifi_config.ssid} not found in bss descriptions."
            )

        await self.device.wlan_core.disconnect()
        status = await self.device.wlan_core.status()
        if status == ClientStatusIdle():
            return
        asserts.fail(
            f"Status did not return to idle after disconnect: {status}"
        )

    async def test_scan_and_connect_2g(self) -> None:
        """Test case for scanning and connecting to a 2G OpenWRT AP."""
        await self._test_scan_and_connect(
            AccessPointConfig.generate(
                ssid=AccessPointConfig.random_string(),
                security=Security.NONE,
                band=Band.BAND_2G,
            )
        )

    async def test_scan_and_connect_5g(self) -> None:
        """Test case for scanning and connecting to a 5G OpenWRT AP."""
        await self._test_scan_and_connect(
            AccessPointConfig.generate(
                ssid=AccessPointConfig.random_string(),
                security=Security.NONE,
                band=Band.BAND_5G,
            )
        )

    async def test_scan_and_connect_wpa2(self) -> None:
        """Test case for scanning and connecting to a WPA2 network."""
        await self._test_scan_and_connect(
            AccessPointConfig.generate(
                ssid=AccessPointConfig.random_string(),
                password=AccessPointConfig.random_string(16),
                security=Security.WPA2,
                band=Band.BAND_2G,
            )
        )

    async def test_scan_and_connect_wpa3(self) -> None:
        """Test case for scanning and connecting to a WPA3 network."""
        await self._test_scan_and_connect(
            AccessPointConfig.generate(
                ssid=AccessPointConfig.random_string(),
                password=AccessPointConfig.random_string(16),
                security=Security.WPA2_WPA3,
                band=Band.BAND_2G,
            )
        )


if __name__ == "__main__":
    test_runner.main()
