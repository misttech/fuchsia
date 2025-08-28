#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import time

import fidl_fuchsia_wlan_policy as f_wlan_policy
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants, hostapd_security
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.wifi import base_test
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


class StartStopClientConnectionsTest(base_test.WifiBaseTest):
    """Tests that we see the expected behavior with enabling and disabling
        client connections

    Test Bed Requirement:
    * One or more Fuchsia devices
    * One Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        # Start an AP with a hidden network
        self.ssid = rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        self.access_point = self.access_points[0]
        self.password = rand_ascii_str(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        self.security_type = SecurityType.WPA2
        security = hostapd_security.Security(
            security_mode=hostapd_security.SecurityMode.WPA2,
            password=self.password,
        )

        self.access_point.stop_all_aps()
        # TODO(63719) use varying values for AP that shouldn't affect the test.
        setup_ap(
            self.access_point,
            "whirlwind",
            hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            self.ssid,
            security=security,
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
        self.download_logs()
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.remove_all_networks()
            fd.wlan_policy_controller.wait_for_no_connections()
        self.access_point.stop_all_aps()
        super().teardown_class()

    def _wait_until_update(
        self,
        fd: FuchsiaDevice,
        expected: ClientStateSummary,
        timeout_sec: float = 30,
    ) -> None:
        """Wait until the expected update.

        Args:
            fd: Fuchsia device to check.
            expected: Expected state.
            timeout_sec: Timeout in seconds.

        Raises:
            signals.TestFailure: If we don't get the expected update within
                timeout_sec.
        """
        result: ClientStateSummary | None = None
        timeout = time.time() + timeout_sec

        while True:
            time_left = timeout - time.time()
            try:
                if time_left < 0:
                    raise TimeoutError()
                result = fd.honeydew_fd.wlan_policy.get_update(time_left)
            except TimeoutError as e:
                raise signals.TestFailure(
                    f'want "{expected}" within {timeout_sec}s, got {result}'
                ) from e
            if result == expected:
                return

    def test_stop_client_connections_update(self) -> None:
        """Test that we can stop client connections.

        The fuchsia device always starts client connections during configure_wlan. We
        verify first that we are in a client connections enabled state.
        """
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.start_client_connections()
            fd.honeydew_fd.wlan_policy.set_new_update_listener()
            self._wait_until_update(
                fd,
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_ENABLED,
                    networks=[],
                ),
            )

            fd.honeydew_fd.wlan_policy.stop_client_connections()
            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_DISABLED,
                    networks=[],
                ),
            )

    def test_start_client_connections_update(self) -> None:
        """Test that we can start client connections."""
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.stop_client_connections()
            fd.honeydew_fd.wlan_policy.set_new_update_listener()
            self._wait_until_update(
                fd,
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_DISABLED,
                    networks=[],
                ),
            )

            fd.honeydew_fd.wlan_policy.start_client_connections()
            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_ENABLED,
                    networks=[],
                ),
            )

    def test_stop_client_connections_rejects_connections(self) -> None:
        """Test that if client connections are disabled connection attempts fail."""
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.start_client_connections()
            fd.honeydew_fd.wlan_policy.set_new_update_listener()
            self._wait_until_update(
                fd,
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_ENABLED,
                    networks=[],
                ),
            )

            fd.honeydew_fd.wlan_policy.save_network(
                self.ssid, self.security_type, self.password
            )
            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
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
            fd.honeydew_fd.wlan_policy.stop_client_connections()
            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
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
            status = fd.honeydew_fd.wlan_policy.connect(
                self.ssid, self.security_type
            )
            assert (
                status is f_wlan_policy.RequestStatus.REJECTED_INCOMPATIBLE_MODE
            ), "Expected connection request rejected as incompatible."

    def test_start_stop_client_connections(self) -> None:
        """Test automated behavior when starting/stopping client connections.

        When starting and stopping the client connections the device should connect and
        disconnect from the saved network.
        """
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.stop_client_connections()
            fd.honeydew_fd.wlan_policy.set_new_update_listener()
            self._wait_until_update(
                fd,
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_DISABLED,
                    networks=[],
                ),
            )

            fd.honeydew_fd.wlan_policy.save_network(
                self.ssid, self.security_type, self.password
            )
            self.log.info(
                f'Saved network "{self.ssid}" with password "{self.password}" ({self.security_type})'
            )

            fd.honeydew_fd.wlan_policy.start_client_connections()
            self.log.info(
                "WLAN client connections enabled, expecting auto-connect"
            )

            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_ENABLED, networks=[]
                ),
            )
            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
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
                fd.honeydew_fd.wlan_policy.get_update(timeout=60),
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
            self.log.info(f'Connected to network "{self.ssid}"')

            fd.honeydew_fd.wlan_policy.stop_client_connections()
            self.log.info("Stopped client connections")

            asserts.assert_equal(
                fd.honeydew_fd.wlan_policy.get_update(),
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
                fd.honeydew_fd.wlan_policy.get_update(),
                ClientStateSummary(
                    state=WlanClientState.CONNECTIONS_DISABLED, networks=[]
                ),
            )


if __name__ == "__main__":
    test_runner.main()
