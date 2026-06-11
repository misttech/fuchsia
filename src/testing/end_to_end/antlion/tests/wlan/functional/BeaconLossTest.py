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
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.utils import rand_ascii_str
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)


class BeaconLossTest(base_test.WifiBaseTest):
    MAX_ASSOCIATE_ATTEMPTS = 2
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

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

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
            if self.access_point:
                self.access_point.iwconfig.ap_iwconfig(
                    self.in_use_interface, "txpower on"
                )
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()

    def _associate_dut_with_retry(self, ssid: str) -> None:
        for i in range(self.MAX_ASSOCIATE_ATTEMPTS):
            try:
                self.log.debug(
                    f"Attempt {i + 1}/{self.MAX_ASSOCIATE_ATTEMPTS} to associate with SSID: {ssid}"
                )
                self.dut.associate(ssid, SecurityMode.OPEN)
                time.sleep(self.wait_client_connection_setup_s)
                if self.dut.is_connected():
                    self.log.info(
                        f"Successfully associated and connected to SSID: {ssid}"
                    )
                    return
                else:
                    retry_message = (
                        "Retrying..."
                        if i < self.MAX_ASSOCIATE_ATTEMPTS - 1
                        else "Retries exhausted."
                    )
                    self.log.warning(
                        f"DUT failed to connect on attempt {i + 1}/{self.MAX_ASSOCIATE_ATTEMPTS}. {retry_message}"
                    )
            except Exception as e:
                retry_message = (
                    "Retrying..."
                    if i < self.MAX_ASSOCIATE_ATTEMPTS - 1
                    else "Retries exhausted."
                )
                self.log.warning(
                    f"Exception occurred on association attempt {i + 1}/{self.MAX_ASSOCIATE_ATTEMPTS}: {e}. {retry_message}"
                )

        raise signals.TestError(
            f"Failed to associate and connect to SSID {ssid} after {self.MAX_ASSOCIATE_ATTEMPTS} attempts."
        )

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        if self.access_point:
            self.access_point.stop_all_aps()

    def beacon_loss(self, channel: BssChannel) -> None:
        band = channel.band

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=channel,
                        bss_settings=[
                            BssSettings(
                                ssid=self.ssid,
                                security=SecurityOpen(),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
            self.in_use_interface = (
                self.openwrt_ap.wlan_5g_interface
                if band == Band.BAND_5G
                else self.openwrt_ap.wlan_2g_interface
            )
        elif self.access_point:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=channel.number,
                ssid=self.ssid,
            )
            if channel.number > 14:
                self.in_use_interface = self.access_point.wlan_5g
            else:
                self.in_use_interface = self.access_point.wlan_2g

        assert self.in_use_interface is not None
        time.sleep(self.wait_ap_startup_s)
        self.log.info(
            f"Initial association with SSID: {self.ssid} on channel {channel}"
        )
        self._associate_dut_with_retry(self.ssid)

        for i in range(self.num_of_iterations):
            self.log.info(
                f"Iteration {i + 1}/{self.num_of_iterations}: Testing beacon loss on interface {self.in_use_interface}"
            )
            # Turn off AP radio
            self.log.info(
                f"Turning off AP radio for interface: {self.in_use_interface}"
            )
            if self.openwrt_ap:
                self.openwrt_ap.set_txpower(self.in_use_interface, 0)
            elif self.access_point:
                self.access_point.iwconfig.ap_iwconfig(
                    self.in_use_interface, "txpower off"
                )
            time.sleep(self.wait_after_ap_txoff_s)

            # Verify disconnection from AP
            asserts.assert_false(
                self.dut.is_connected(),
                f"DUT failed to disconnect from {self.ssid} after AP radio off",
            )
            self.log.info(f"DUT successfully disconnected from {self.ssid}")

            # Turn on AP radio
            self.log.info(
                f"Turning on AP radio for interface: {self.in_use_interface}"
            )
            assert self.in_use_interface is not None
            if self.openwrt_ap:
                self.openwrt_ap.reset_txpower(self.in_use_interface)
            elif self.access_point:
                self.access_point.iwconfig.ap_iwconfig(
                    self.in_use_interface, "txpower on"
                )
            time.sleep(self.wait_to_connect_after_ap_txon_s)

            # Initiate reconnection
            self.log.info(f"Sending associate command for SSID {self.ssid}")
            self.dut.associate(self.ssid, SecurityMode.OPEN)
            time.sleep(self.wait_client_connection_setup_s)

            # Verify reconnection
            asserts.assert_true(
                self.dut.is_connected(),
                f"Failed to connect back to {self.ssid}",
            )
            self.log.info(f"DUT successfully reconnected to {self.ssid}")

    def test_beacon_loss_2g(self) -> None:
        self.beacon_loss(channel=DEFAULT_2G_CHANNEL)

    def test_beacon_loss_5g(self) -> None:
        self.beacon_loss(channel=DEFAULT_5G_CHANNEL)


if __name__ == "__main__":
    test_runner.main()
