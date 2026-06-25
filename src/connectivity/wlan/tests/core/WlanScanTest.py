#!/usr/bin/env python3.4
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
"""
This test exercises basic scanning functionality to confirm expected behavior
related to wlan scanning
"""

import logging
from datetime import datetime

import fidl_fuchsia_wlan_internal as f_wlan_internal
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from mobly.records import TestResultRecord
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)


class WlanScanTest(base_test.WifiBaseTest):
    """WLAN scan test class.

    Test Bed Requirement:
    * One Fuchsia device
    * Several Wi-Fi networks visible to the device, including an open Wi-Fi
      network or a onHub/GoogleWifi
    """

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()

    def setup_class(self) -> None:
        super().setup_class()

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass(
                "Requires at least one access point and one Fuchsia device"
            )

        self.dut = self.fuchsia_devices[0]
        self.dut.configure_wlan(association_mechanism="drivers")

    def on_fail(self, record: TestResultRecord) -> None:
        self.on_device_fail(self.dut, record)
        self.dut.configure_wlan(association_mechanism="drivers")

    def teardown_test(self) -> None:
        self.dut.honeydew_fd.wlan_core_deprecated_sync.disconnect()
        if self.access_point:
            self.access_point.stop_all_aps()

    def teardown_class(self) -> None:
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()

    def test_scan_while_connected(self) -> None:
        """Connects to a specified network and initiates a scan."""
        ssid = AccessPointConfig.random_string(20)
        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
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
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=DEFAULT_2G_CHANNEL.number,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=DeprecatedSecurityMode.OPEN,
                    password=None,
                ),
            )

        authentication = f_wlan_internal.Authentication(
            protocol=f_wlan_internal.Protocol.OPEN, credentials=None
        )

        name = self.dut.honeydew_fd.device_name

        self.log.info('[%s] Scanning for ssid "%s"', name, ssid)
        scan_results = (
            self.dut.honeydew_fd.wlan_core_deprecated_sync.scan_for_bss_info()
        )
        asserts.assert_in(
            ssid, scan_results, f'Scan results did not include "{ssid}"'
        )
        target_bss = scan_results[ssid]
        asserts.assert_equal(
            len(target_bss),
            1,
            f'Expected 1 BSS for "{ssid}", got {len(target_bss)}',
        )

        self.log.info('[%s] Connecting to ssid "%s"', name, ssid)
        asserts.assert_true(
            self.dut.honeydew_fd.wlan_core_deprecated_sync.connect(
                ssid,
                target_bss[0],
                authentication,
            ),
            f"Expected connect to {ssid} to succeed",
        )

        self.log.info('[%s] Scanning while connected to "%s"', name, ssid)
        start_time = datetime.now()
        scan_results = (
            self.dut.honeydew_fd.wlan_core_deprecated_sync.scan_for_bss_info()
        )
        self.log.info("Scan contained %d results", len(scan_results))
        self.log.debug("Scan results: %s", scan_results)
        total_time_ms = (datetime.now() - start_time).total_seconds() * 1000
        self.log.info(f"Scan time: {total_time_ms:.2f} ms")

        asserts.assert_in(
            ssid, scan_results, f'Scan results did not include "{ssid}"'
        )


if __name__ == "__main__":
    test_runner.main()
