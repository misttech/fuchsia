#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import time
from dataclasses import dataclass

from antlion.controllers.fuchsia_lib.ssh import FuchsiaSSHProvider
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanError,
)
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectionState,
    DisconnectStatus,
    NetworkState,
    WlanClientState,
)
from honeydew.fuchsia_device.fuchsia_device import (
    FuchsiaDevice as HdFuchsiaDevice,
)
from mobly import logger, signals

SESSION_MANAGER_TIMEOUT_SEC = 10
FUCHSIA_DEFAULT_WLAN_CONFIGURE_RETRIES = 3
DEFAULT_GET_UPDATE_TIMEOUT = 60
DEFAULT_TIME_WAIT_FOR_CLIENT_CONNECTIONS_STATE = 5  # seconds


class WlanPolicyControllerError(signals.ControllerError):
    pass


@dataclass
class ClientState:
    state: str
    networks: list[dict[str, object]]


# TODO(http://b/309854439): Add a ClientStateWatcher and refactor tests to allow test
# developers more control when update listeners are set and the client update state is
# reset.
class WlanPolicyController:
    """Contains methods related to the wlan policy layer, to be used in the
    FuchsiaDevice object."""

    def __init__(
        self, honeydew: HdFuchsiaDevice, ssh: FuchsiaSSHProvider
    ) -> None:
        self.policy_configured = False
        self.honeydew = honeydew
        self.ssh = ssh
        self.log = logger.PrefixLoggerAdapter(
            logging.getLogger(),
            {
                logger.PrefixLoggerAdapter.EXTRA_KEY_LOG_PREFIX: f"[WlanPolicyController | {self.ssh.config.host_name}]",
            },
        )

    def configure_wlan(
        self,
        restart_client_connections: bool = True,
        clear_networks: bool = True,
        retries: int = FUCHSIA_DEFAULT_WLAN_CONFIGURE_RETRIES,
    ) -> None:
        """Sets up wlan policy layer.

        Args:
            restart_client_connections: whether to restart client connections.
            clear_networks: whether to clear all saved networks.
            retries: number of times to re-attempt to configure WLAN policy.
        """

        # We need to stop session manager to free control of
        # fuchsia.wlan.policy.ClientController, which can only be used by a
        # single caller at a time. Fuchsia Controller needs the ClientController
        # to trigger WLAN policy state changes. On eng builds the
        # session_manager can be restarted after being stopped during reboot so
        # we attempt killing the session manager process for 10 seconds.
        # See https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.wlan.policy/client_provider.fidl
        if b"cast_agent.cm" in self.ssh.run("ps").stdout:
            session_manager_expiration = (
                time.time() + SESSION_MANAGER_TIMEOUT_SEC
            )
            while time.time() < session_manager_expiration:
                self.ssh.stop_component(
                    "session_manager", is_cfv2_component=True
                )

        # Acquire control of policy layer
        for attempt in range(retries):
            try:
                self.honeydew.wlan_policy.create_client_controller()
                if clear_networks:
                    self.log.info(
                        "Removing any and all saved networks to run tests in a clean state."
                    )
                    self.honeydew.wlan_policy.remove_all_networks()

                # Optionally restart client connections to start tests in a good state. This should
                # prevent issues like scans still being in progress when tests start.
                if restart_client_connections:
                    self.log.info(
                        "Restarting client connections to run test in a clean state"
                    )
                    self.stop_client_connections_and_wait()
                self.honeydew.wlan_policy.start_client_connections()
                self.log.info(
                    "ACTS tests now have control of the WLAN policy layer."
                )
                self.policy_configured = True
                return
            except HoneydewWlanError as e:
                self.log.warning(
                    "Attempt %d/%d to configure WLAN policy failed: %s",
                    attempt + 1,
                    retries,
                    e,
                )
            except TimeoutError as e:
                raise WlanPolicyControllerError(
                    f"An operation timed out while configuring WLAN policy client controller: {e}."
                )
        raise WlanPolicyControllerError(
            f"Failed to configure WLAN policy client controller after {retries} retries."
        )

    def _deconfigure_wlan(self) -> None:
        self.honeydew.wlan_policy.stop_client_connections()
        self.policy_configured = False

    def stop_client_connections_and_wait(
        self, wait_time: int = DEFAULT_TIME_WAIT_FOR_CLIENT_CONNECTIONS_STATE
    ) -> None:
        """This function stops client connections if client connections are currently enabled,
        and waits for an update showing that the state has changed.
        """
        try:
            client = self.honeydew.wlan_policy.get_status(
                timeout=DEFAULT_TIME_WAIT_FOR_CLIENT_CONNECTIONS_STATE
            )
            if client.state != WlanClientState.CONNECTIONS_ENABLED:
                self.log.info(
                    "Client connections are not enabled, so they will not be disabled."
                )
                return
        except TimeoutError:
            # This update should basically be immediate because we get a new client state listener
            # channel, so this is unexpected
            self.log.warning(
                "Unexpectedly timed out getting client state. Proceeding to stop client connections"
            )

        self.honeydew.wlan_policy.stop_client_connections()
        try:
            self.wait_for_client_state(
                expected_state=WlanClientState.CONNECTIONS_DISABLED,
                timeout_sec=DEFAULT_TIME_WAIT_FOR_CLIENT_CONNECTIONS_STATE,
            )
        except TimeoutError:
            self.log.warning(
                "Timed out waiting for client connections disabled update. "
                "Will proceed with the test as normal anyway."
            )

    def clean_up(self) -> None:
        # It is possible for policy to have been configured before, but
        # deconfigured before test end. In this case, in must be setup
        # before removing networks. This will both set up the DUT and remove
        # networks.
        if not self.policy_configured:
            self.configure_wlan(
                clear_networks=True, restart_client_connections=False
            )

    def _find_network(
        self,
        ssid: str,
        networks: list[NetworkState],
    ) -> NetworkState | None:
        """Helper method to find network in list of network states.

        Args:
            ssid: The network name to look for.
            networks: The list of network states to look in.

        Returns:
            Network state of target ssid or None if not found in networks.
        """
        for network in networks:
            if network.network_identifier.ssid == ssid:
                return network
        return None

    def wait_for_network_state(
        self,
        ssid: str,
        expected_states: ConnectionState | set[ConnectionState],
        expected_status: DisconnectStatus | None = None,
        timeout_sec: int = DEFAULT_GET_UPDATE_TIMEOUT,
    ) -> ConnectionState:
        """Waits until the device returns with expected network state.

        Args:
            ssid: The network name to check the state of.
            expected_states: The network state or states we are expecting to see.
            expected_status: The disconnect status of the network. Only relevant when
                expected_state is FAILED or DISCONNECTED.
            timeout_sec: The number of seconds to wait for a update showing connection.

        Returns:
            Current network state if network converges on one of the expected states.

        Raises:
            TypeError: If DisconnectStatus provided with a CONNECTING or CONNECTED
                state.
            WlanPolicyControllerError: If no network is found before timeout or fails to
                converge to one of the expected states.
        """

        if not isinstance(expected_states, set):
            expected_states = {expected_states}

        if (
            expected_states
            == {ConnectionState.CONNECTING, ConnectionState.CONNECTED}
            or expected_states.issubset(
                {ConnectionState.CONNECTING, ConnectionState.CONNECTED}
            )
            and expected_status is not None
        ):
            raise TypeError(
                "Disconnect status not valid for CONNECTING or CONNECTED states."
            )

        self.honeydew.wlan_policy.set_new_update_listener()
        network: NetworkState | None = None

        end_time = time.time() + timeout_sec
        while time.time() < end_time:
            time_left = max(1.0, end_time - time.time())
            try:
                client = self.honeydew.wlan_policy.get_update(timeout=time_left)
            except TimeoutError as e:
                self.log.debug("Timeout waiting for WLAN state updates: %s", e)
                continue

            # If we don't find the network initially, wait and retry.
            network = self._find_network(ssid, client.networks)
            if network is None:
                self.log.debug(
                    f"{ssid} not found in client networks: {client.networks}"
                )
                continue

            if network.connection_state in expected_states:
                # Check optional disconnect status matches.
                if expected_status:
                    if network.disconnect_status is not expected_status:
                        raise WlanPolicyControllerError(
                            f"Disconnect status is not {expected_status}"
                        )
            elif network.connection_state is ConnectionState.CONNECTING:
                self.log.debug(f"Network {ssid} still attempting to connect.")
                continue
            else:
                raise WlanPolicyControllerError(
                    f'Expected network "{ssid}" to be in state {expected_states}, '
                    f"got {network.connection_state}"
                )

            # Successfully converged on expected state and status
            return network.connection_state

        if network is None:
            raise WlanPolicyControllerError(
                f"Timed out trying to find ssid: {ssid}"
            )
        raise WlanPolicyControllerError(
            f'Timed out waiting for "{ssid}" to reach state {expected_states} and '
            f"status {expected_status}"
        )

    def wait_for_client_state(
        self,
        expected_state: WlanClientState,
        timeout_sec: int = DEFAULT_GET_UPDATE_TIMEOUT,
    ) -> None:
        """Waits until the client converges to expected state.

        Args:
            expected_state: The client state we are waiting to see.
            timeout_sec: Duration to wait for the desired_state.

        Raises:
            WlanPolicyControllerError: If client still has not converged to expected
                state at end of timeout.
        """
        self.honeydew.wlan_policy.set_new_update_listener()

        last_err: TimeoutError | None = None
        end_time = time.time() + timeout_sec
        while time.time() < end_time:
            time_left = max(1, int(end_time - time.time()))
            try:
                client = self.honeydew.wlan_policy.get_update(timeout=time_left)
            except TimeoutError as e:
                last_err = e
                continue
            if client.state is not expected_state:
                # Continue getting updates.
                continue
            else:
                return
        else:
            self.log.error(
                f"Client state did not converge to the expected state: {expected_state}"
                f" Waited:{timeout_sec}s"
            )
            raise WlanPolicyControllerError from last_err
