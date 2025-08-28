#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.controllers.fuchsia_lib.lib_controllers.wlan_policy_controller import (
    WlanPolicyControllerError,
)
from antlion.test_utils.wifi import base_test
from antlion.utils import rand_ascii_str, rand_hex_str
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanError,
)
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectionState,
    NetworkConfig,
    SecurityType,
    WlanClientState,
)
from mobly import asserts, signals, test_runner

PSK_LEN = 64
CREDENTIAL_TYPE_PSK = "Psk"
CREDENTIAL_TYPE_NONE = "None"
CREDENTIAL_TYPE_PASSWORD = "Password"
CREDENTIAL_VALUE_NONE = ""


class SavedNetworksTest(base_test.WifiBaseTest):
    """WLAN policy commands test class.

    A test that saves various networks and verifies the behavior of save, get, and
    remove through the ClientController API of WLAN policy.

    Test Bed Requirement:
    * One or more Fuchsia devices
    * One Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        # Keep track of whether we have started an access point in a test
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
        self.access_points[0].stop_all_aps()

    def teardown_class(self) -> None:
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.remove_all_networks()
        self.access_points[0].stop_all_aps()

    def _has_saved_network(
        self, fd: FuchsiaDevice, network: NetworkConfig
    ) -> bool:
        """Verify that the network is present in saved networks.

        Args:
            fd: Fuchsia device to run on.
            network: Network to check for.

        Returns:
            True if network is found in saved networks, otherwise False.
        """
        networks: list[
            NetworkConfig
        ] = fd.honeydew_fd.wlan_policy.get_saved_networks()
        if network in networks:
            return True
        else:
            return False

    def _start_ap(
        self,
        ssid: str,
        security_type: SecurityMode,
        password: str | None = None,
    ) -> None:
        """Starts an access point.

        Args:
            ssid: The SSID of the network to broadcast
            security_type: The security type of the network to be broadcasted
            password: The password to connect to the broadcasted network. The password
                is ignored if security type is none.

        Raises:
            EnvironmentError if it fails to set up AP for test.
        """
        # Put together the security configuration of the network to be broadcasted.
        security = Security(security_mode=security_type, password=password)

        if len(self.access_points) > 0:
            # Create an AP with default values other than the specified values.
            setup_ap(
                self.access_points[0],
                "whirlwind",
                hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid,
                security=security,
            )
        else:
            self.log.error(
                "No access point available for test, please check config"
            )
            raise EnvironmentError("Failed to set up AP for test")

    def test_open_network_with_password(self) -> None:
        """Save an open network with a password and verify that it fails to save."""
        test_network = NetworkConfig(
            rand_ascii_str(10),
            SecurityType.NONE,
            CREDENTIAL_TYPE_NONE,
            rand_ascii_str(8),
        )

        for fd in self.fuchsia_devices:
            try:
                fd.honeydew_fd.wlan_policy.save_network(
                    test_network.ssid,
                    test_network.security_type,
                    test_network.credential_value,
                )
                asserts.fail("Unexpectedly succeeded to save network")
            except HoneydewWlanError:
                networks = fd.honeydew_fd.wlan_policy.get_saved_networks()
                if test_network in networks:
                    asserts.fail("Got an unexpected saved network")
                # Successfully failed to save network.
                return

            asserts.fail("Failed to get error saving bad network")

    def test_open_network(self) -> None:
        """Save an open network and verify presence."""
        test_network = NetworkConfig(
            rand_ascii_str(10),
            SecurityType.NONE,
            CREDENTIAL_TYPE_NONE,
            CREDENTIAL_VALUE_NONE,
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

    def test_network_with_psk(self) -> None:
        """Save a network with a PSK and verify presence.

        PSK are translated from hex to bytes when saved, and when returned by
        get_saved_networks it will be lower case.
        """
        test_network = NetworkConfig(
            rand_ascii_str(11),
            SecurityType.WPA2,
            CREDENTIAL_TYPE_PSK,
            rand_hex_str(PSK_LEN).lower(),
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

    def test_wep_network(self) -> None:
        """Save a wep network and verify presence."""
        test_network = NetworkConfig(
            rand_ascii_str(12),
            SecurityType.WEP,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(13),
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

    def test_wpa2_network(self) -> None:
        """Save a wpa2 network and verify presence."""
        test_network = NetworkConfig(
            rand_ascii_str(9),
            SecurityType.WPA2,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(15),
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

    def test_wpa_network(self) -> None:
        """Save a wpa network and verify presence."""
        test_network = NetworkConfig(
            rand_ascii_str(16),
            SecurityType.WPA,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(9),
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

    def test_wpa3_network(self) -> None:
        """Save a wpa3 network and verify presence."""
        test_network = NetworkConfig(
            rand_ascii_str(9),
            SecurityType.WPA3,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(15),
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

    def test_save_network_persists(self) -> None:
        """Save a network and verify after reboot network is present."""
        test_network = NetworkConfig(
            rand_ascii_str(10),
            SecurityType.WPA2,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(10),
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )

            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network not present")

            fd.reboot()

            if not self._has_saved_network(fd, test_network):
                asserts.fail("Saved network did not persist through reboot")

    def test_same_ssid_diff_security(self) -> None:
        """Save two networks with the same ssids but different security types.

        Both networks should be saved and present in network state since they have
        different security types and therefore different network identifiers.
        """
        ssid = rand_ascii_str(19)
        test_network_wpa2 = NetworkConfig(
            ssid,
            SecurityType.WPA2,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(12),
        )
        test_network_open = NetworkConfig(
            ssid,
            SecurityType.NONE,
            CREDENTIAL_TYPE_NONE,
            CREDENTIAL_VALUE_NONE,
        )

        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.save_network(
                test_network_wpa2.ssid,
                test_network_wpa2.security_type,
                test_network_wpa2.credential_value,
            )

            fd.honeydew_fd.wlan_policy.save_network(
                test_network_open.ssid,
                test_network_open.security_type,
                test_network_open.credential_value,
            )

            if not (
                self._has_saved_network(fd, test_network_wpa2)
                and self._has_saved_network(fd, test_network_open)
            ):
                asserts.fail("Both saved networks not present")

    def test_remove_disconnects(self) -> None:
        """Connect to network, remove it while still connected, and verify disconnect.

        This test requires a wpa2 network. Remove all other networks first so that we
        don't auto connect to them.
        """
        test_network = NetworkConfig(
            rand_ascii_str(10),
            SecurityType.WPA2,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(10),
        )

        self._start_ap(
            test_network.ssid, SecurityMode.WPA2, test_network.credential_value
        )

        for fd in self.fuchsia_devices:
            fd.wlan_policy_controller.wait_for_no_connections()
            # Make sure client connections are enabled
            fd.honeydew_fd.wlan_policy.start_client_connections()
            fd.wlan_policy_controller.wait_for_client_state(
                WlanClientState.CONNECTIONS_ENABLED
            )
            # Save and verify we connect to network
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )

            fd.wlan_policy_controller.wait_for_network_state(
                test_network.ssid, ConnectionState.CONNECTED
            )
            # Remove network and verify we disconnect
            fd.honeydew_fd.wlan_policy.remove_network(
                test_network.ssid,
                test_network.security_type,
                test_network.credential_value,
            )
            try:
                fd.wlan_policy_controller.wait_for_no_connections()
            except WlanPolicyControllerError as e:
                raise signals.TestFailure("Failed to remove network") from e

    def test_auto_connect_open(self) -> None:
        """Save an open network and verify it auto connects.

        Start up AP with an open network and verify that the client auto connects to
        that network after we save it.
        """
        test_network = NetworkConfig(
            rand_ascii_str(10),
            SecurityType.NONE,
            CREDENTIAL_TYPE_NONE,
            CREDENTIAL_VALUE_NONE,
        )

        self._start_ap(
            test_network.ssid, SecurityMode.OPEN, test_network.credential_value
        )

        for fd in self.fuchsia_devices:
            fd.wlan_policy_controller.wait_for_no_connections()
            # Make sure client connections are enabled
            fd.honeydew_fd.wlan_policy.start_client_connections()
            fd.wlan_policy_controller.wait_for_client_state(
                WlanClientState.CONNECTIONS_ENABLED
            )
            # Save the network and make sure that we see the device auto connect to it.
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid, test_network.security_type
            )
            try:
                fd.wlan_policy_controller.wait_for_network_state(
                    test_network.ssid, ConnectionState.CONNECTED
                )
            except WlanPolicyControllerError as e:
                raise signals.TestFailure(
                    "network is not in connected state"
                ) from e

    def test_auto_connect_wpa3(self) -> None:
        """Save an wpa3 network and verify it auto connects.

        Start up AP with a wpa3 network and verify that the client auto connects to
        that network after we save it.
        """
        test_network = NetworkConfig(
            rand_ascii_str(10),
            SecurityType.WPA3,
            CREDENTIAL_TYPE_PASSWORD,
            rand_ascii_str(10),
        )

        self._start_ap(
            test_network.ssid, SecurityMode.WPA3, test_network.credential_value
        )

        for fd in self.fuchsia_devices:
            fd.wlan_policy_controller.wait_for_no_connections()
            # Make sure client connections are enabled
            fd.honeydew_fd.wlan_policy.start_client_connections()
            fd.wlan_policy_controller.wait_for_client_state(
                WlanClientState.CONNECTIONS_ENABLED
            )
            # Save the network and make sure that we see the device auto connect to it.
            fd.honeydew_fd.wlan_policy.save_network(
                test_network.ssid,
                SecurityType.WPA3,
                test_network.credential_value,
            )
            try:
                fd.wlan_policy_controller.wait_for_network_state(
                    test_network.ssid, ConnectionState.CONNECTED
                )
            except WlanPolicyControllerError as e:
                raise signals.TestFailure(
                    "network is not in connected state"
                ) from e


if __name__ == "__main__":
    test_runner.main()
