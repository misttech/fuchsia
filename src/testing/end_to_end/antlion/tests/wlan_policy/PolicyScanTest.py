#!/usr/bin/env python3.4
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#

import logging

from antlion.controllers.ap_lib import (
    hostapd_ap_preset,
    hostapd_bss_settings,
    hostapd_constants,
    hostapd_security,
)
from antlion.test_utils.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectionState,
    SecurityType,
)
from mobly import asserts, signals, test_runner


class PolicyScanTest(base_test.WifiBaseTest):
    """WLAN policy scan test class.

    This test exercises the scan functionality for the WLAN Policy API.

    Test Bed Requirement:
    * One or more Fuchsia devices
    * One Whirlwind Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()

        if len(self.fuchsia_devices) < 1:
            raise signals.TestFailure("No fuchsia devices found.")
        for fd in self.fuchsia_devices:
            fd.configure_wlan(
                association_mechanism="policy", preserve_saved_networks=True
            )
        if len(self.access_points) < 1:
            raise signals.TestFailure("No access points found.")
        # Prepare the AP
        self.access_point = self.access_points[0]
        self.access_point.stop_all_aps()
        # Generate network params.
        bss_settings_2g: list[hostapd_bss_settings.BssSettings] = []
        bss_settings_5g: list[hostapd_bss_settings.BssSettings] = []
        open_network = self.get_open_network(False, [])
        self.open_network_2g = open_network["2g"]
        self.open_network_5g = open_network["5g"]
        wpa2_settings = self.get_psk_network(False, [])
        self.wpa2_network_2g = wpa2_settings["2g"]
        self.wpa2_network_5g = wpa2_settings["5g"]
        bss_settings_2g.append(
            hostapd_bss_settings.BssSettings(
                name=self.wpa2_network_2g["SSID"],
                ssid=self.wpa2_network_2g["SSID"],
                security=hostapd_security.Security(
                    security_mode=self.wpa2_network_2g["security"],
                    password=self.wpa2_network_2g["password"],
                ),
            )
        )
        bss_settings_5g.append(
            hostapd_bss_settings.BssSettings(
                name=self.wpa2_network_5g["SSID"],
                ssid=self.wpa2_network_5g["SSID"],
                security=hostapd_security.Security(
                    security_mode=self.wpa2_network_5g["security"],
                    password=self.wpa2_network_5g["password"],
                ),
            )
        )
        self.ap_2g = hostapd_ap_preset.create_ap_preset(
            iface_wlan_2g=self.access_points[0].wlan_2g,
            iface_wlan_5g=self.access_points[0].wlan_5g,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            bss_settings=bss_settings_2g,
        )
        self.ap_5g = hostapd_ap_preset.create_ap_preset(
            iface_wlan_2g=self.access_points[0].wlan_2g,
            iface_wlan_5g=self.access_points[0].wlan_5g,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            bss_settings=bss_settings_5g,
        )
        # Start the networks
        self.access_point.start_ap(hostapd_config=self.ap_2g)
        self.access_point.start_ap(hostapd_config=self.ap_5g)
        # List of test SSIDs started by APs
        self.all_ssids = [
            self.open_network_2g["SSID"],
            self.wpa2_network_2g["SSID"],
            self.open_network_5g["SSID"],
            self.wpa2_network_5g["SSID"],
        ]

    def setup_test(self) -> None:
        super().setup_test()
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.remove_all_networks()
            fd.wlan_policy_controller.wait_for_no_connections()

    def teardown_test(self) -> None:
        self.download_logs()
        super().teardown_test()

    def _assert_network_is_in_results(
        self, scan_results: list[str], ssid: str
    ) -> None:
        """Verified scan results contain a specified network

        Args:
            scan_results: Scan results from a fuchsia Policy API scan.
            ssid: SSID for network that should be in the results.

        Raises:
            signals.TestFailure: if the network is not present in the scan results
        """
        asserts.assert_true(
            ssid in scan_results,
            f'Network "{ssid}" was not found in scan results: {scan_results}',
        )

    def test_basic_scan_request(self) -> None:
        """Verify a scan returns all expected networks"""
        for fd in self.fuchsia_devices:
            scan_results = fd.honeydew_fd.wlan_policy.scan_for_networks()
            if len(scan_results) == 0:
                raise signals.TestFailure("Scan did not find any networks")
            for ssid in self.all_ssids:
                self._assert_network_is_in_results(scan_results, ssid)

    def test_scan_while_connected_open_network_2g(self) -> None:
        """Connect to an open 2g network and perform a scan"""
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                self.open_network_2g["SSID"],
                SecurityType(
                    self.open_network_2g["security"].fuchsia_security_type()
                ),
                self.open_network_2g["password"],
            )
            fd.honeydew_fd.wlan_policy.connect(
                self.open_network_2g["SSID"],
                SecurityType(
                    self.open_network_2g["security"].fuchsia_security_type()
                ),
            )
            fd.wlan_policy_controller.wait_for_network_state(
                self.open_network_2g["SSID"], ConnectionState.CONNECTED
            )

            scan_results = fd.honeydew_fd.wlan_policy.scan_for_networks()
            for ssid in self.all_ssids:
                self._assert_network_is_in_results(scan_results, ssid)

    def test_scan_while_connected_wpa2_network_2g(self) -> None:
        """Connect to a WPA2 2g network and perform a scan"""
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                self.wpa2_network_2g["SSID"],
                SecurityType(
                    self.wpa2_network_2g["security"].fuchsia_security_type()
                ),
                self.wpa2_network_2g["password"],
            )
            fd.honeydew_fd.wlan_policy.connect(
                self.wpa2_network_2g["SSID"],
                SecurityType(
                    self.wpa2_network_2g["security"].fuchsia_security_type()
                ),
            )
            fd.wlan_policy_controller.wait_for_network_state(
                self.wpa2_network_2g["SSID"], ConnectionState.CONNECTED
            )

            scan_results = fd.honeydew_fd.wlan_policy.scan_for_networks()
            for ssid in self.all_ssids:
                self._assert_network_is_in_results(scan_results, ssid)

    def test_scan_while_connected_open_network_5g(self) -> None:
        """Connect to an open 5g network and perform a scan"""
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                self.open_network_5g["SSID"],
                SecurityType(
                    self.open_network_5g["security"].fuchsia_security_type()
                ),
                self.open_network_5g["password"],
            )
            fd.honeydew_fd.wlan_policy.connect(
                self.open_network_5g["SSID"],
                SecurityType(
                    self.open_network_5g["security"].fuchsia_security_type()
                ),
            )
            fd.wlan_policy_controller.wait_for_network_state(
                self.open_network_5g["SSID"], ConnectionState.CONNECTED
            )

            scan_results = fd.honeydew_fd.wlan_policy.scan_for_networks()
            for ssid in self.all_ssids:
                self._assert_network_is_in_results(scan_results, ssid)

    def test_scan_while_connected_wpa2_network_5g(self) -> None:
        """Connect to a WPA2 5g network and perform a scan"""
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                self.wpa2_network_5g["SSID"],
                SecurityType(
                    self.wpa2_network_5g["security"].fuchsia_security_type()
                ),
                self.wpa2_network_5g["password"],
            )
            fd.honeydew_fd.wlan_policy.connect(
                self.wpa2_network_5g["SSID"],
                SecurityType(
                    self.wpa2_network_5g["security"].fuchsia_security_type()
                ),
            )
            fd.wlan_policy_controller.wait_for_network_state(
                self.wpa2_network_5g["SSID"], ConnectionState.CONNECTED
            )

            scan_results = fd.honeydew_fd.wlan_policy.scan_for_networks()
            for ssid in self.all_ssids:
                self._assert_network_is_in_results(scan_results, ssid)


if __name__ == "__main__":
    test_runner.main()
