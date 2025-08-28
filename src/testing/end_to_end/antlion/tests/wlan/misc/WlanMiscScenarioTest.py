#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord


class WlanMiscScenarioTest(base_test.WifiBaseTest):
    """Random scenario tests, usually to reproduce certain bugs, that do not
    fit into a specific test category, but should still be run in CI to catch
    regressions.
    """

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        self.dut = self.get_dut(AssociationMode.POLICY)

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]

    def teardown_class(self) -> None:
        self.dut.disconnect()
        self.access_point.stop_all_aps()

    def teardown_test(self) -> None:
        self.dut.disconnect()
        self.download_logs()
        self.access_point.stop_all_aps()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        self.dut.disconnect()
        self.access_point.stop_all_aps()

    def test_connect_to_wpa2_after_wpa3_rejection(self) -> None:
        """Test association to non-WPA3 network after receiving a WPA3
        rejection, which was triggering a firmware hang.

        Bug: https://bugs.fuchsia.dev/p/fuchsia/issues/detail?id=71233
        """
        # Setup a WPA3 network
        wpa3_ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_5G)
        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=wpa3_ssid,
            security=Security(
                security_mode=SecurityMode.WPA3,
                password=generate_random_password(SecurityMode.WPA3),
            ),
        )
        # Attempt to associate with wrong password, expecting failure
        self.log.info("Attempting to associate WPA3 with wrong password.")
        asserts.assert_false(
            self.dut.associate(
                wpa3_ssid, SecurityMode.WPA3, target_pwd="wrongpass"
            ),
            "Associated with WPA3 network using the wrong password",
        )

        self.access_point.stop_all_aps()

        # Setup a WPA2 Network
        wpa2_ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_5G)
        wpa2_password = generate_random_password(SecurityMode.WPA2)
        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=wpa2_ssid,
            security=Security(
                security_mode=SecurityMode.WPA2, password=wpa2_password
            ),
        )

        # Attempt to associate, expecting success
        self.log.info("Attempting to associate with WPA2 network.")
        asserts.assert_true(
            self.dut.associate(
                wpa2_ssid,
                SecurityMode.WPA2,
                target_pwd=wpa2_password,
            ),
            "Failed to associate with WPA2 network after a WPA3 rejection.",
        )


if __name__ == "__main__":
    test_runner.main()
