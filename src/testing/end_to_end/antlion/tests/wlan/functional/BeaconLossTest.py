#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Script for testing WiFi recovery after rebooting the AP.

Override default number of iterations using the following
parameter in the test config file.

"beacon_loss_test_iterations": "5"
"""

import logging
import time

from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from antlion.utils import rand_ascii_str
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord


class BeaconLossTest(base_test.WifiBaseTest):
    # Default number of test iterations here.
    # Override using parameter in config file.
    # Eg: "beacon_loss_test_iterations": "10"
    num_of_iterations = 5

    # Time to wait for AP to startup
    wait_ap_startup_s = 15

    # Default wait time in seconds for the AP radio to turn back on
    wait_to_connect_after_ap_txon_s = 5

    # Time to wait for device to disconnect after AP radio of
    wait_after_ap_txoff_s = 15

    # Time to wait for device to complete connection setup after
    # given an associate command
    wait_client_connection_setup_s = 15

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        self.ssid = rand_ascii_str(10)

        self.dut = self.get_dut(AssociationMode.POLICY)

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]

        self.num_of_iterations = int(
            self.user_params.get(
                "beacon_loss_test_iterations", self.num_of_iterations
            )
        )
        self.in_use_interface: str | None = None

    def teardown_test(self) -> None:
        self.dut.disconnect()
        self.dut.reset_wifi()
        # ensure radio is on, in case the test failed while the radio was off
        if self.in_use_interface:
            self.access_point.iwconfig.ap_iwconfig(
                self.in_use_interface, "txpower on"
            )
        self.download_logs()
        self.access_point.stop_all_aps()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        self.access_point.stop_all_aps()

    def beacon_loss(self, channel: int) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=channel,
            ssid=self.ssid,
        )
        time.sleep(self.wait_ap_startup_s)
        if channel > 14:
            self.in_use_interface = self.access_point.wlan_5g
        else:
            self.in_use_interface = self.access_point.wlan_2g

        # TODO(b/144505723): [ACTS] update BeaconLossTest.py to handle client
        # roaming, saved networks, etc.
        self.log.info("sending associate command for ssid %s", self.ssid)
        self.dut.associate(self.ssid, SecurityMode.OPEN)

        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")

        time.sleep(self.wait_client_connection_setup_s)

        for _ in range(0, self.num_of_iterations):
            # Turn off AP radio
            self.log.info("turning off radio")
            self.access_point.iwconfig.ap_iwconfig(
                self.in_use_interface, "txpower off"
            )
            time.sleep(self.wait_after_ap_txoff_s)

            # Did we disconnect from AP?
            asserts.assert_false(
                self.dut.is_connected(), "Failed to disconnect."
            )

            # Turn on AP radio
            self.log.info("turning on radio")
            self.access_point.iwconfig.ap_iwconfig(
                self.in_use_interface, "txpower on"
            )
            time.sleep(self.wait_to_connect_after_ap_txon_s)

            # Tell the client to connect
            self.log.info(f"sending associate command for ssid {self.ssid}")
            self.dut.associate(self.ssid, SecurityMode.OPEN)
            time.sleep(self.wait_client_connection_setup_s)

            # Did we connect back to WiFi?
            asserts.assert_true(
                self.dut.is_connected(), "Failed to connect back."
            )

    def test_beacon_loss_2g(self) -> None:
        self.beacon_loss(channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G)

    def test_beacon_loss_5g(self) -> None:
        self.beacon_loss(channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G)


if __name__ == "__main__":
    test_runner.main()
