#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import time

from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.fuchsia_lib.lib_controllers.wlan_policy_controller import (
    WlanPolicyControllerError,
)
from antlion.test_utils.wifi import base_test
from antlion.utils import rand_ascii_str
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectionState,
    SecurityType,
    WlanClientState,
)
from mobly import signals, test_runner

# These tests should have a longer timeout for connecting than normal connect
# tests because the device should probabilistically perform active scans for
# hidden networks. Multiple scans are necessary to verify a very low chance of
# random failure.
TIME_WAIT_FOR_CONNECT = 90
TIME_ATTEMPT_SCANS = 90


class HiddenNetworksTest(base_test.WifiBaseTest):
    """Tests that WLAN Policy will detect hidden networks

    Test Bed Requirement:
    * One or more Fuchsia devices
    * One Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        # Start an AP with a hidden network
        self.access_point = self.access_points[0]
        self.hidden_ssid = rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        self.hidden_password = rand_ascii_str(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        self.access_point.stop_all_aps()
        setup_ap(
            self.access_point,
            "whirlwind",
            hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            self.hidden_ssid,
            hidden=True,
            security=Security(
                security_mode=SecurityMode.WPA2,
                password=self.hidden_password,
            ),
        )

        if len(self.fuchsia_devices) < 1:
            raise EnvironmentError("No Fuchsia devices found.")
        for fd in self.fuchsia_devices:
            fd.configure_wlan(
                association_mechanism="policy", preserve_saved_networks=True
            )

    def setup_test(self) -> None:
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.remove_all_networks()
            fd.wlan_policy_controller.wait_for_no_connections()

    def teardown_class(self) -> None:
        self.access_point.stop_all_aps()

    # Tests

    def test_scan_hidden_networks(self) -> None:
        """Probabilistic test to see if we can see hidden networks with a scan.

        Scan a few times and check that we see the hidden networks in the results at
        least once. We stop client connections to not trigger a connect when saving,
        which would interfere with requested scans.

        Raises:
            TestFailure if we fail to see hidden network in scans before timing out.
        """
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.stop_client_connections()
            fd.wlan_policy_controller.wait_for_client_state(
                WlanClientState.CONNECTIONS_DISABLED
            )
            fd.honeydew_fd.wlan_policy.save_network(
                self.hidden_ssid, SecurityType.WPA2, self.hidden_password
            )
            fd.honeydew_fd.wlan_policy.start_client_connections()
            start_time = time.time()
            num_performed_scans = 0

            while time.time() < start_time + TIME_ATTEMPT_SCANS:
                num_performed_scans = num_performed_scans + 1
                scan_result = fd.honeydew_fd.wlan_policy.scan_for_networks()

                if self.hidden_ssid in scan_result:
                    self.log.info(
                        f"SSID of hidden network seen after {num_performed_scans} scans"
                    )
                    return
                # Don't overload SL4F with scan requests
                time.sleep(1)

            self.log.error(
                f"Failed to see SSID after {num_performed_scans} scans"
            )
            raise signals.TestFailure("Failed to see hidden network in scans")

    def test_auto_connect_hidden_on_startup(self) -> None:
        """Test auto connect on startup.

        This test checks that if we are not connected to anything but have a hidden
        network saved, we will eventually actively scan for it and connect.

        Raises:
            TestFailure if the client fails to auto connect to the hidden network.
        """
        # Start up AP with an open network with a random SSID

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.stop_client_connections()
            fd.honeydew_fd.wlan_policy.save_network(
                self.hidden_ssid, SecurityType.WPA2, self.hidden_password
            )

            # Reboot the device and check that it auto connects.
            fd.reboot()
            try:
                fd.wlan_policy_controller.wait_for_network_state(
                    self.hidden_ssid,
                    ConnectionState.CONNECTED,
                    timeout_sec=TIME_WAIT_FOR_CONNECT,
                )
            except WlanPolicyControllerError as e:
                raise signals.TestFailure(
                    "Failed to auto connect to hidden network on startup"
                ) from e

    def test_auto_connect_hidden_on_save(self) -> None:
        """Test auto connect to hidden network on save.

        This test checks that if we save a hidden network and are not connected to
        anything, the device will connect to the hidden network that was just saved.

        Raises:
            TestFailure if client fails to auto connect to a hidden network after saving
            it.
        """
        for fd in self.fuchsia_devices:
            fd.wlan_policy_controller.wait_for_no_connections()
            fd.honeydew_fd.wlan_policy.save_network(
                self.hidden_ssid, SecurityType.WPA2, self.hidden_password
            )
            try:
                fd.wlan_policy_controller.wait_for_network_state(
                    self.hidden_ssid,
                    ConnectionState.CONNECTED,
                    timeout_sec=TIME_WAIT_FOR_CONNECT,
                )
            except WlanPolicyControllerError as e:
                raise signals.TestFailure(
                    "Failed to auto connect to hidden network on save"
                ) from e


if __name__ == "__main__":
    test_runner.main()
