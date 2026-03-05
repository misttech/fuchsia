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

import itertools
import logging
from dataclasses import dataclass
from datetime import datetime

import fidl_fuchsia_wlan_common_security as fidl_security
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_constants import BandType
from antlion.controllers.ap_lib.hostapd_security import (
    Security as HostapdSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as HostapdSecurityMode,
)
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from mobly.records import TestResultRecord
from mobly_controller.openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    Security,
)
from mobly_controller.openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)


@dataclass
class TestParams:
    band: Band
    security: Security


class WlanScanTest(base_test.WifiBaseTest):
    """WLAN scan test class.

    Test Bed Requirement:
    * One or more Fuchsia devices
    * Several Wi-Fi networks visible to the device, including an open Wi-Fi
      network or a onHub/GoogleWifi
    """

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()

    def pre_run(self) -> None:
        test_params: list[tuple[TestParams]] = []
        for (
            band,
            security,
        ) in itertools.product(
            # BandType,
            # [SecurityMode.OPEN, SecurityMode.WPA2],
            #
            # TODO(https://github.com/python/mypy/issues/14688): Replace the code below
            # with the commented code above once the bug affecting StrEnum resolves.
            [e for e in Band],
            [Security.NONE, Security.WPA2],
        ):
            test_params.append(
                (
                    TestParams(
                        band,
                        security,
                    ),
                )
            )

        def generate_test_name(t: TestParams) -> str:
            return (
                "test_scan_while_connected"
                f"_{t.security}_open_network"
                f"_{t.band}"
            )

        self.generate_tests(
            test_logic=self.scan_while_connected,
            name_func=generate_test_name,
            arg_sets=test_params,
        )

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

        for fd in self.fuchsia_devices:
            fd.configure_wlan(association_mechanism="drivers")

    def on_fail(self, record: TestResultRecord) -> None:
        for fd in self.fuchsia_devices:
            self.on_device_fail(fd, record)
            fd.configure_wlan(association_mechanism="drivers")

    def teardown_test(self) -> None:
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_core.disconnect()
        if hasattr(self, "access_point"):
            self.access_point.stop_all_aps()

    def teardown_class(self) -> None:
        self.download_logs()
        if hasattr(self, "access_point"):
            self.access_point.stop_all_aps()

    def scan_while_connected(self, t: TestParams) -> None:
        """Connects to as specified network and initiates a scan."""
        ssid = AccessPointConfig.random_string(20)
        password = (
            AccessPointConfig.random_string(10)
            if t.security == Security.WPA2
            else None
        )
        if self.openwrt_ap:
            config = AccessPointConfig.generate(
                band=t.band,
                ssid=ssid,
                password=password,
                security=t.security,
            )
            self.openwrt_ap.configure_wifi(config)
            self.openwrt_ap.verify_wifi_status(band=t.band)
        elif self.access_point:
            band = ConfigMapper.to_hostapd_band(t.band)
            security = ConfigMapper.to_hostapd_security(t.security)
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=band.default_channel(),
                ssid=ssid,
                security=HostapdSecurity(
                    security_mode=security,
                    password=password,
                ),
            )

        if t.security == Security.NONE:
            protocol = fidl_security.Protocol.OPEN
            credentials = None
        elif t.security == Security.WPA2:
            if password is None:
                raise signals.TestError("Password is required for WPA2")
            protocol = fidl_security.Protocol.WPA2_PERSONAL
            credentials = fidl_security.Credentials(
                wpa=fidl_security.WpaCredentials(
                    passphrase=(list(password.encode("ascii")))
                )
            )
        else:
            raise signals.TestFailure(f"Unhandled security mode {t.security}")
        authentication = fidl_security.Authentication(
            protocol=protocol, credentials=credentials
        )

        for fd in self.fuchsia_devices:
            name = fd.honeydew_fd.device_name

            self.log.info('[%s] Scanning for ssid "%s"', name, ssid)
            scan_results = fd.honeydew_fd.wlan_core.scan_for_bss_info()
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
                fd.honeydew_fd.wlan_core.connect(
                    ssid,
                    target_bss[0],
                    authentication,
                ),
                f"Expected connect to {ssid} to succeed",
            )

            self.log.info('[%s] Scanning while connected to "%s"', name, ssid)
            self.basic_scan_request(fd, ssid)

    def basic_scan_request(self, fd: FuchsiaDevice, ssid: str) -> None:
        """Verify ssid is discoverable.

        Args:
            fd: A fuchsia device
            ssid: ssid of network to validate is in scan results
        """
        start_time = datetime.now()
        scan_results = fd.honeydew_fd.wlan_core.scan_for_bss_info()
        self.log.info("Scan contained %d results", len(scan_results))
        self.log.debug("Scan results: %s", scan_results)
        total_time_ms = (datetime.now() - start_time).total_seconds() * 1000
        self.log.info(f"Scan time: {total_time_ms:.2f} ms")

        asserts.assert_in(
            ssid, scan_results, f'Scan results did not include "{ssid}"'
        )

    def test_basic_scan_request(self) -> None:
        """Verify a general scan trigger returns at least one result"""
        ssid = AccessPointConfig.random_string(20)
        if self.openwrt_ap:
            config = AccessPointConfig.generate(
                band=Band.BAND_2G,
                ssid=ssid,
                security=Security.NONE,
            )
            self.openwrt_ap.configure_wifi(config)
            self.openwrt_ap.verify_wifi_status(band=Band.BAND_2G)
        elif self.access_point:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=BandType.BAND_2G.default_channel(),
                ssid=ssid,
                security=HostapdSecurity(
                    security_mode=HostapdSecurityMode.OPEN,
                    password=None,
                ),
            )
        for fd in self.fuchsia_devices:
            self.basic_scan_request(fd, ssid)


if __name__ == "__main__":
    test_runner.main()
