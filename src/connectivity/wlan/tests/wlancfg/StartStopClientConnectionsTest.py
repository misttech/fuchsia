# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_wlan_base_test
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanRequestRejectedError,
)
from legacy_access_point.access_point import setup_ap
from legacy_access_point.ap_lib import hostapd_constants, hostapd_security
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityWpa2,
)

logger = logging.getLogger(__name__)


class StartStopClientConnectionsTest(
    fuchsia_wlan_base_test.FuchsiaWlanBaseTest
):
    """Tests that we see the expected behavior with enabling and disabling
        client connections

    Test Bed Requirement:
    * One or more Fuchsia devices
    * One Access Point
    """

    async def setup_class(self) -> None:
        await super().setup_class()

        if not self.openwrt_aps and not self.access_points:
            raise signals.TestError("Requires at least one access point.")

        self.ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        self.password = AccessPointConfig.random_string(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        self.security_type = f_wlan_policy.SecurityType.WPA2

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=self.ssid,
                                security=SecurityWpa2(),
                                password=self.password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            security = hostapd_security.Security(
                security_mode=hostapd_security.SecurityMode.WPA2,
                password=self.password,
            )
            self.access_point.stop_all_aps()
            setup_ap(
                self.access_point,
                "whirlwind",
                hostapd_constants.AP_DEFAULT_WW_COMPATIBLE_CHANNEL,
                self.ssid,
                security=security,
            )

        # Acquire control of policy layer
        max_attempts = 3
        for attempt in range(1, max_attempts + 1):
            try:
                await self.dut.wlan_policy.start_client_connections()
                logger.info("Acquired control of the WLAN policy layer.")
                break
            except Exception as e:
                logger.warning(
                    "Attempt %d/%d to acquire WLAN policy failed: %s",
                    attempt,
                    max_attempts,
                    e,
                )
                if attempt == max_attempts:
                    raise signals.TestError(
                        f"Failed to acquire WLAN policy client controller after {max_attempts} attempts: {e}."
                    )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_policy.remove_all_networks()
        await self.dut.wlan_policy.wait_for_no_connections()

    async def teardown_class(self) -> None:
        await self.dut.wlan_policy.remove_all_networks()
        await self.dut.wlan_policy.wait_for_no_connections()

        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_class()

    async def test_stop_client_connections_update(self) -> None:
        """Test that we can stop client connections.

        The fuchsia device always starts client connections during configure_wlan. We
        verify first that we are in a client connections enabled state.
        """
        await self.dut.wlan_policy.start_client_connections()
        await self.dut.wlan_policy.wait_for_client_state(
            f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED
        )

        await self.dut.wlan_policy.stop_client_connections(
            wait_for_confirmation=False
        )
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_DISABLED,
                networks=[],
            ),
        )

    async def test_start_client_connections_update(self) -> None:
        """Test that we can start client connections."""
        await self.dut.wlan_policy.stop_client_connections(
            wait_for_confirmation=True
        )

        await self.dut.wlan_policy.start_client_connections()
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED,
                networks=[],
            ),
        )

    async def test_stop_client_connections_rejects_connections(self) -> None:
        """Test that if client connections are disabled connection attempts fail."""
        await self.dut.wlan_policy.start_client_connections()
        await self.dut.wlan_policy.wait_for_client_state(
            f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED
        )

        await self.dut.wlan_policy.save_network(
            self.ssid, self.security_type, self.password
        )
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    f_wlan_policy.NetworkState(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(self.ssid.encode("utf-8")),
                            type_=self.security_type,
                        ),
                        state=f_wlan_policy.ConnectionState.CONNECTING,
                        status=None,
                    )
                ],
            ),
        )

        # Stop connections interrupts connect attempt.
        await self.dut.wlan_policy.stop_client_connections(
            wait_for_confirmation=False
        )
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_DISABLED,
                networks=[
                    f_wlan_policy.NetworkState(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(self.ssid.encode("utf-8")),
                            type_=self.security_type,
                        ),
                        state=f_wlan_policy.ConnectionState.DISCONNECTED,
                        status=f_wlan_policy.DisconnectStatus.CONNECTION_STOPPED,
                    )
                ],
            ),
        )

        # Subsequent attempt to connect fails.
        with asserts.assert_raises(HoneydewWlanRequestRejectedError) as context:
            await self.dut.wlan_policy.connect(self.ssid, self.security_type)
        asserts.assert_equal(
            context.exception.reason,
            f_wlan_policy.RequestStatus.REJECTED_INCOMPATIBLE_MODE,
        )

    async def test_start_stop_client_connections(self) -> None:
        """Test automated behavior when starting/stopping client connections.

        When starting and stopping the client connections the device should connect and
        disconnect from the saved network.
        """
        await self.dut.wlan_policy.stop_client_connections(
            wait_for_confirmation=True
        )

        await self.dut.wlan_policy.save_network(
            self.ssid, self.security_type, self.password
        )
        logger.info(
            f'Saved network "{self.ssid}" with password "{self.password}" ({self.security_type})'
        )

        await self.dut.wlan_policy.start_client_connections()
        logger.info("WLAN client connections enabled, expecting auto-connect")

        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED,
                networks=[],
            ),
        )
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    f_wlan_policy.NetworkState(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(self.ssid.encode("utf-8")),
                            type_=self.security_type,
                        ),
                        state=f_wlan_policy.ConnectionState.CONNECTING,
                        status=None,
                    )
                ],
            ),
            f'Expected auto-connect request to "{self.ssid}"',
        )
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(timeout=60),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    f_wlan_policy.NetworkState(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(self.ssid.encode("utf-8")),
                            type_=self.security_type,
                        ),
                        state=f_wlan_policy.ConnectionState.CONNECTED,
                        status=None,
                    )
                ],
            ),
            f'Expected auto-connect to "{self.ssid}" within 1 minute',
        )
        logger.info(f'Connected to network "{self.ssid}"')

        await self.dut.wlan_policy.stop_client_connections(
            wait_for_confirmation=False
        )
        logger.info("Stopped client connections")

        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED,
                networks=[
                    f_wlan_policy.NetworkState(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(self.ssid.encode("utf-8")),
                            type_=self.security_type,
                        ),
                        state=f_wlan_policy.ConnectionState.DISCONNECTED,
                        status=f_wlan_policy.DisconnectStatus.CONNECTION_STOPPED,
                    )
                ],
            ),
            f'Expected auto-disconnect from "{self.ssid}"',
        )
        asserts.assert_equal(
            await self.dut.wlan_policy.get_update(),
            f_wlan_policy.ClientStateSummary(
                state=f_wlan_policy.WlanClientState.CONNECTIONS_DISABLED,
                networks=[],
            ),
        )


if __name__ == "__main__":
    test_runner.main()
