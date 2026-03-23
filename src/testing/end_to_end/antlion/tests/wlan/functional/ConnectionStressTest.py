#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Script for testing WiFi connection and disconnection in a loop

"""

import logging
import time
from dataclasses import dataclass

from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from antlion.utils import rand_ascii_str
from mobly import asserts, signals, test_runner
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
    profile: str
    band: Band
    security: Security
    ap_ssid: str
    ap_password: str | None
    dut_ssid: str
    dut_password: str | None
    expect_associated: bool


class ConnectionStressTest(base_test.WifiBaseTest):
    # Default number of test iterations here.
    # Override using parameter in config file.
    # Eg: "connection_stress_test_iterations": "50"
    num_of_iterations = 10

    def pre_run(self) -> None:
        tests: list[TestParams] = []

        # Successful associate
        for profile in [
            "whirlwind",
            "whirlwind_11ab_legacy",
            "whirlwind_11ag_legacy",
        ]:
            for band in [Band.BAND_2G, Band.BAND_5G]:
                ssid = rand_ascii_str(10)
                tests.append(
                    TestParams(
                        profile=profile,
                        band=band,
                        security=Security.NONE,
                        ap_ssid=ssid,
                        ap_password=None,
                        dut_ssid=ssid,
                        dut_password=None,
                        expect_associated=True,
                    )
                )

        # Wrong SSID
        for band in [Band.BAND_2G, Band.BAND_5G]:
            ssid = rand_ascii_str(10)
            tests.append(
                TestParams(
                    profile="whirlwind",
                    band=band,
                    security=Security.NONE,
                    ap_ssid=ssid,
                    ap_password=None,
                    dut_ssid=f"wrong_{ssid}",
                    dut_password=None,
                    expect_associated=False,
                )
            )

        # Wrong password
        for band in [Band.BAND_2G, Band.BAND_5G]:
            ssid = rand_ascii_str(10)
            password = rand_ascii_str(20)
            tests.append(
                TestParams(
                    profile="whirlwind",
                    band=band,
                    security=Security.WPA2,
                    ap_ssid=ssid,
                    ap_password=password,
                    dut_ssid=ssid,
                    dut_password=f"wrong_{password}",
                    expect_associated=False,
                )
            )

        def test_name(test: TestParams) -> str:
            channel = "2g" if test.band == Band.BAND_2G else "5g"
            if test.expect_associated:
                return f"test_{test.profile}_{channel}"
            if test.ap_ssid != test.dut_ssid:
                return f"test_{test.profile}_{channel}_wrong_ssid"
            if test.ap_password != test.dut_password:
                return f"test_{test.profile}_{channel}_wrong_password"
            raise TypeError(f"Unknown name for {test}")

        self.generate_tests(
            self.connect_disconnect, test_name, [(t,) for t in tests]
        )

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        self.ssid = rand_ascii_str(10)

        self.dut = self.get_dut(AssociationMode.POLICY)

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

        self.num_of_iterations = int(
            self.user_params.get(
                "connection_stress_test_iterations", self.num_of_iterations
            )
        )
        self.log.info(f"iterations: {self.num_of_iterations}")

    def teardown_test(self) -> None:
        self.dut.reset_wifi()
        self.download_logs()
        if hasattr(self, "openwrt_ap"):
            self.openwrt_ap.stop_wifi()
        elif hasattr(self, "access_point"):
            self.access_point.stop_all_aps()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        if hasattr(self, "openwrt_ap"):
            self.openwrt_ap.stop_wifi()
        elif hasattr(self, "access_point"):
            self.access_point.stop_all_aps()

    def connect_disconnect(self, test: TestParams) -> None:
        """Helper to start an AP, connect DUT to it and disconnect

        Args:
            test: TestParams containing configuration
        """
        if hasattr(self, "openwrt_ap"):
            config = AccessPointConfig.generate(
                band=test.band,
                ssid=test.ap_ssid,
                password=test.ap_password,
                security=test.security,
            )
            self.openwrt_ap.configure_wifi(config)
            self.openwrt_ap.verify_wifi_status(band=test.band)
        elif hasattr(self, "access_point"):
            band = ConfigMapper.to_hostapd_band(test.band)
            security = ConfigMapper.to_hostapd_security(test.security)
            setup_ap(
                access_point=self.access_point,
                profile_name=test.profile,
                channel=band.default_channel(),
                ssid=test.ap_ssid,
                security=DeprecatedSecurity(
                    security_mode=security, password=test.ap_password
                ),
            )

        for iteration in range(0, self.num_of_iterations):
            associated = self.dut.associate(
                test.dut_ssid,
                target_pwd=test.dut_password,
                target_security=ConfigMapper.to_hostapd_security(test.security),
            )
            asserts.assert_equal(
                associated,
                test.expect_associated,
                (
                    f"Attempt {iteration}/{self.num_of_iterations}: "
                    f"associated={associated}, want {test.expect_associated}"
                ),
            )

            self.dut.disconnect()

            # Wait a second before trying again
            time.sleep(1)


if __name__ == "__main__":
    test_runner.main()
