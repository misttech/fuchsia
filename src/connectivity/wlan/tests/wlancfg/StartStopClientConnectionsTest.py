# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_wlan_base_test
from antlion.controllers import access_point
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants, hostapd_security
from antlion.utils import rand_ascii_str
from honeydew.affordances.connectivity.wlan.utils.types import (
    ClientStateSummary,
    ConnectionState,
    DisconnectStatus,
    NetworkIdentifier,
    NetworkState,
    SecurityType,
    WlanClientState,
)
from mobly import asserts, signals, test_runner

logger = logging.getLogger(__name__)

SESSION_MANAGER_TIMEOUT_SEC = 10


class StartStopClientConnectionsTest(
    fuchsia_wlan_base_test.FuchsiaWlanBaseTest
):
    """Tests that we see the expected behavior with enabling and disabling
        client connections

    Test Bed Requirement:
    * One or more Fuchsia devices
    * One Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()

        if len(self.fuchsia_devices) < 1:
            raise EnvironmentError("No Fuchsia devices found.")
        self.device = self.fuchsia_devices[0]

        access_points = self.register_controller(
            access_point, required=True, min_number=1
        )
        if access_points is None or len(self.fuchsia_devices) < 1:
            raise EnvironmentError("No access points found.")
        assert len(access_points) == 1

        self.access_point = access_points[0]

        self.ssid = rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        self.password = rand_ascii_str(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        self.security_type = SecurityType.WPA2
        security = hostapd_security.Security(
            security_mode=hostapd_security.SecurityMode.WPA2,
            password=self.password,
        )

        self.access_point.stop_all_aps()
        setup_ap(
            self.access_point,
            "whirlwind",
            hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            self.ssid,
            security=security,
        )

        # Acquire control of policy layer
        max_attempts = 3
        for attempt in range(1, max_attempts + 1):
            try:
                self.device.wlan_policy.create_client_controller()
                self.device.wlan_policy.start_client_connections()
                logger.info("Acquired control of the WLAN policy layer.")
            except Exception as e:
                logger.warning(
                    "Attempt %d/%d to acquire WLAN policy failed: %s",
                    attempt,
                    max_attempts,
                    e,
                )
                if attempt == max_attempts:
                    signals.TestAbortClass(
                        f"Failed to acquire WLAN policy client controller after {max_attempts} attempts: {e}."
                    )

    def setup_test(self) -> None:
        super().setup_test()
        self.device.wlan_policy.remove_all_networks()
        self.device.wlan_policy.wait_for_no_connections()

    def teardown_class(self) -> None:
        self.device.wlan_policy.remove_all_networks()
        self.device.wlan_policy.wait_for_no_connections()

        self.access_point.stop_all_aps()
        super().teardown_class()

    def test_stop_client_connections_update(self) -> None:
        """Test that we can stop client connections.

        The fuchsia device always starts client connections during configure_wlan. We
        verify first that we are in a client connections enabled state.
        """
        self.device.wlan_policy.start_client_connections()
        self.device.wlan_policy.set_new_update_listener()
        self.device.wlan_policy.wait_until_update(
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[],
            ),
        )

        self.device.wlan_policy.stop_client_connections()
        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_DISABLED,
                networks=[],
            ),
        )

    def test_start_client_connections_update(self) -> None:
        """Test that we can start client connections."""
        self.device.wlan_policy.stop_client_connections()
        self.device.wlan_policy.set_new_update_listener()
        self.device.wlan_policy.wait_until_update(
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_DISABLED,
                networks=[],
            ),
            timeout=30,
        )

        self.device.wlan_policy.start_client_connections()
        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[],
            ),
        )

    def test_stop_client_connections_rejects_connections(self) -> None:
        """Test that if client connections are disabled connection attempts fail."""
        self.device.wlan_policy.start_client_connections()
        self.device.wlan_policy.set_new_update_listener()
        self.device.wlan_policy.wait_until_update(
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[],
            ),
        )

        self.device.wlan_policy.save_network(
            self.ssid, self.security_type, self.password
        )
        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    NetworkState(
                        network_identifier=NetworkIdentifier(
                            self.ssid, self.security_type
                        ),
                        connection_state=ConnectionState.CONNECTING,
                        disconnect_status=None,
                    )
                ],
            ),
        )

        # Stop connections interrupts connect attempt.
        self.device.wlan_policy.stop_client_connections()
        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_DISABLED,
                networks=[
                    NetworkState(
                        network_identifier=NetworkIdentifier(
                            self.ssid, self.security_type
                        ),
                        connection_state=ConnectionState.DISCONNECTED,
                        disconnect_status=DisconnectStatus.CONNECTION_STOPPED,
                    )
                ],
            ),
        )

        # Subsequent attempt to connect fails.
        status = self.device.wlan_policy.connect(self.ssid, self.security_type)
        assert (
            status is f_wlan_policy.RequestStatus.REJECTED_INCOMPATIBLE_MODE
        ), "Expected connection request rejected as incompatible."

    def test_start_stop_client_connections(self) -> None:
        """Test automated behavior when starting/stopping client connections.

        When starting and stopping the client connections the device should connect and
        disconnect from the saved network.
        """
        self.device.wlan_policy.stop_client_connections()
        self.device.wlan_policy.set_new_update_listener()
        self.device.wlan_policy.wait_until_update(
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_DISABLED,
                networks=[],
            ),
        )

        self.device.wlan_policy.save_network(
            self.ssid, self.security_type, self.password
        )
        logger.info(
            f'Saved network "{self.ssid}" with password "{self.password}" ({self.security_type})'
        )

        self.device.wlan_policy.start_client_connections()
        logger.info("WLAN client connections enabled, expecting auto-connect")

        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED, networks=[]
            ),
        )
        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    NetworkState(
                        network_identifier=NetworkIdentifier(
                            self.ssid, self.security_type
                        ),
                        connection_state=ConnectionState.CONNECTING,
                        disconnect_status=None,
                    )
                ],
            ),
            f'Expected auto-connect request to "{self.ssid}"',
        )
        asserts.assert_equal(
            self.device.wlan_policy.get_update(timeout=60),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    NetworkState(
                        network_identifier=NetworkIdentifier(
                            self.ssid, self.security_type
                        ),
                        connection_state=ConnectionState.CONNECTED,
                        disconnect_status=None,
                    )
                ],
            ),
            f'Expected auto-connect to "{self.ssid}" within 1 minute',
        )
        logger.info(f'Connected to network "{self.ssid}"')

        self.device.wlan_policy.stop_client_connections()
        logger.info("Stopped client connections")

        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    NetworkState(
                        network_identifier=NetworkIdentifier(
                            self.ssid, self.security_type
                        ),
                        connection_state=ConnectionState.DISCONNECTED,
                        disconnect_status=DisconnectStatus.CONNECTION_STOPPED,
                    )
                ],
            ),
            f'Expected auto-disconnect from "{self.ssid}"',
        )
        asserts.assert_equal(
            self.device.wlan_policy.get_update(),
            ClientStateSummary(
                state=WlanClientState.CONNECTIONS_DISABLED, networks=[]
            ),
        )


if __name__ == "__main__":
    test_runner.main()
