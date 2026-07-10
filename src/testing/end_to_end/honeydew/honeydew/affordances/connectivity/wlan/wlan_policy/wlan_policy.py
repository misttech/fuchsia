# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for wlan policy affordance."""

import abc
from dataclasses import dataclass

import fidl_fuchsia_wlan_policy as f_wlan_policy

from honeydew.affordances import affordance
from honeydew.affordances.connectivity.wlan.utils.types import (
    ClientStateSummary,
    CountryCode,
    NetworkConfig,
)


class AsyncWlanPolicy(abc.ABC):
    """Abstract base class for an async WlanPolicy affordance."""

    DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT = 60

    @abc.abstractmethod
    async def set_country_code(self, country_code: CountryCode) -> None:
        """Set regulatory region and wait for wlancfg to change country code of each phy."""

    @abc.abstractmethod
    async def connect(
        self,
        target_ssid: str,
        security_type: f_wlan_policy.SecurityType,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Triggers connection to a network and blocks until connected.

        Args:
            target_ssid: The network to connect to. Must have been previously
                saved in order for a successful connection to happen.
            security_type: The security protocol of the network.
            timeout: timeout value.

        Raises:
            HoneydewWlanError: Error from WLAN stack, or if connect() FIDL call
                returns anything except RequestStatus.Acknowledged, or if connection
                failure.
            TypeError: Return value not a string.
        """

    @abc.abstractmethod
    async def get_saved_networks(
        self, *, timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT
    ) -> list[NetworkConfig]:
        """Gets networks saved on device.

        Returns:
            A list of NetworkConfigs.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return values not correct types.
        """

    @abc.abstractmethod
    async def get_status(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        """Gets the current client listener state immediately.

        Args:
            timeout: Timeout in seconds to wait for the get_status command to
                return.

        Returns:
            An update of connection status. If there is no error, the result is
            a WlanPolicyUpdate with a structure that matches the FIDL
            ClientStateSummary struct given for updates.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Reached timeout without any updates.
        """

    @abc.abstractmethod
    async def get_update(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        """Gets one client listener update.

        This call will return with an update immediately the
        first time the update listener is initialized by setting a new listener
        or by creating a client controller before setting a new listener.
        Subsequent calls will hang until there is a change since the last
        update call.

        Args:
            timeout: Timeout in seconds to wait for the get_update command to
                return. By default it is set to None (which means timeout is
                disabled)

        Returns:
            An update of connection status. If there is no error, the result is
            a WlanPolicyUpdate with a structure that matches the FIDL
            ClientStateSummary struct given for updates.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Reached timeout without any updates.
            TypeError: Return values not correct types.
        """

    @abc.abstractmethod
    async def wait_for_client_state(
        self,
        expected_state: f_wlan_policy.WlanClientState,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Waits until the client converges to expected state."""

    @abc.abstractmethod
    async def wait_for_network_state(
        self,
        ssid: str,
        expected_state: f_wlan_policy.ConnectionState,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> f_wlan_policy.ConnectionState:
        """Waits until the network converges to expected state."""

    @abc.abstractmethod
    async def remove_all_networks(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Deletes all saved networks on the device.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Operation takes longer than expected.
        """

    @abc.abstractmethod
    async def remove_network(
        self,
        target_ssid: str,
        security_type: f_wlan_policy.SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Removes or "forgets" a network from saved networks.

        Args:
            target_ssid: The network to remove.
            security_type: The security protocol of the network.
            target_pwd: The credential being saved with the network. No password
                is equivalent to an empty string.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Operation takes longer than expected.
        """

    @abc.abstractmethod
    async def save_network(
        self,
        target_ssid: str,
        security_type: f_wlan_policy.SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Saves a network to the device.

        Args:
            target_ssid: The network to save.
            security_type: The security protocol of the network.
            target_pwd: The credential being saved with the network. No password
                is equivalent to an empty string.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
        """

    @abc.abstractmethod
    async def scan_for_networks(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> list[str]:
        """Scans for networks.

        Returns:
            A list of network SSIDs that can be connected to.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return value not a list.
        """

    @abc.abstractmethod
    async def set_new_update_listener(self) -> None:
        """Sets the update listener stream of the facade to a new stream.

        This causes updates to be reset. Intended to be used between tests so
        that the behavior of updates in a test is independent from previous
        tests.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
        """

    @abc.abstractmethod
    async def start_client_connections(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Enables device to initiate connections to networks.

        Either by auto-connecting to saved networks or acting on incoming calls
        triggering connections.

        See fuchsia.wlan.policy/ClientController.StartClientConnections().

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been created yet
            TimeoutError: Operation takes longer than expected.
        """

    @abc.abstractmethod
    async def stop_client_connections(
        self,
        *,
        wait_for_confirmation: bool = True,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Disables device for initiating connections to networks.

        Tears down any existing connections to WLAN networks and disables
        initiation of new connections.

        See fuchsia.wlan.policy/ClientController.StopClientConnections().

        Args:
            wait_for_confirmation: Whether to wait for the client state to
                reach CONNECTIONS_DISABLED.
            timeout: Operation takes longer than expected.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been created yet
        """

    @abc.abstractmethod
    async def wait_for_no_connections(
        self, *, timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT
    ) -> None:
        """Waits until the WLAN network state is disconnected

        Raises:
            HoneydewWlanError: Failure to observe no connection within timeout.
        """

    @abc.abstractmethod
    async def ensure_clean_state(self) -> None:
        """Restarts client connections to start tests in a good state.

        For background, see bugs:
        - https://fxbug.dev/461881673
        - https://fxbug.dev/467783085
        """


class WlanPolicy(affordance.Affordance):
    """Abstract base class for WlanPolicy affordance."""

    DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT = 60

    @dataclass
    class PreservedState:
        saved_networks: list[NetworkConfig] | None
        client_connections_state: f_wlan_policy.WlanClientState | None

    # List all the public methods
    @abc.abstractmethod
    def set_country_code(self, country_code: CountryCode) -> None:
        """Set regulatory region and wait for wlancfg to change country code of each phy."""

    @abc.abstractmethod
    def connect(
        self,
        target_ssid: str,
        security_type: f_wlan_policy.SecurityType,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Triggers connection to a network and blocks until connected.

        Args:
            target_ssid: The network to connect to. Must have been previously
                saved in order for a successful connection to happen.
            security_type: The security protocol of the network.
            timeout: timeout value.

        Raises:
            HoneydewWlanError: Error from WLAN stack, or if connect() FIDL call
                returns anything except RequestStatus.Acknowledged, or if connection
                failure.
            TypeError: Return value not a string.
        """

    @abc.abstractmethod
    def get_saved_networks(
        self, *, timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT
    ) -> list[NetworkConfig]:
        """Gets networks saved on device.

        Returns:
            A list of NetworkConfigs.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return values not correct types.
        """

    @abc.abstractmethod
    def get_status(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        """Gets the current client listener state immediately.

        Args:
            timeout: Timeout in seconds to wait for the get_status command to
                return.

        Returns:
            An update of connection status. If there is no error, the result is
            a WlanPolicyUpdate with a structure that matches the FIDL
            ClientStateSummary struct given for updates.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Reached timeout without any updates.
        """

    @abc.abstractmethod
    def get_update(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        """Gets one client listener update.

        This call will return with an update immediately the
        first time the update listener is initialized by setting a new listener
        or by creating a client controller before setting a new listener.
        Subsequent calls will hang until there is a change since the last
        update call.

        Args:
            timeout: Timeout in seconds to wait for the get_update command to
                return. By default it is set to None (which means timeout is
                disabled)

        Returns:
            An update of connection status. If there is no error, the result is
            a WlanPolicyUpdate with a structure that matches the FIDL
            ClientStateSummary struct given for updates.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Reached timeout without any updates.
            TypeError: Return values not correct types.
        """

    @abc.abstractmethod
    def wait_for_client_state(
        self,
        expected_state: f_wlan_policy.WlanClientState,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Waits until the client converges to expected state."""

    @abc.abstractmethod
    def wait_for_network_state(
        self,
        ssid: str,
        expected_state: f_wlan_policy.ConnectionState,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> f_wlan_policy.ConnectionState:
        """Waits until the network converges to expected state."""

    @abc.abstractmethod
    def remove_all_networks(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Deletes all saved networks on the device.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Operation takes longer than expected.
        """

    @abc.abstractmethod
    def remove_network(
        self,
        target_ssid: str,
        security_type: f_wlan_policy.SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Removes or "forgets" a network from saved networks.

        Args:
            target_ssid: The network to remove.
            security_type: The security protocol of the network.
            target_pwd: The credential being saved with the network. No password
                is equivalent to an empty string.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TimeoutError: Operation takes longer than expected.
        """

    @abc.abstractmethod
    def save_network(
        self,
        target_ssid: str,
        security_type: f_wlan_policy.SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Saves a network to the device.

        Args:
            target_ssid: The network to save.
            security_type: The security protocol of the network.
            target_pwd: The credential being saved with the network. No password
                is equivalent to an empty string.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
        """

    @abc.abstractmethod
    def scan_for_networks(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> list[str]:
        """Scans for networks.

        Returns:
            A list of network SSIDs that can be connected to.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return value not a list.
        """

    @abc.abstractmethod
    def set_new_update_listener(self) -> None:
        """Sets the update listener stream of the facade to a new stream.

        This causes updates to be reset. Intended to be used between tests so
        that the behavior of updates in a test is independent from previous
        tests.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
        """

    @abc.abstractmethod
    def start_client_connections(
        self,
        *,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Enables device to initiate connections to networks.

        Either by auto-connecting to saved networks or acting on incoming calls
        triggering connections.

        See fuchsia.wlan.policy/ClientController.StartClientConnections().

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been created yet
            TimeoutError: Operation takes longer than expected.
        """

    @abc.abstractmethod
    def stop_client_connections(
        self,
        *,
        wait_for_confirmation: bool = True,
        timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Disables device for initiating connections to networks.

        Tears down any existing connections to WLAN networks and disables
        initiation of new connections.

        See fuchsia.wlan.policy/ClientController.StopClientConnections().

        Args:
            wait_for_confirmation: Whether to wait for the client state to
                reach CONNECTIONS_DISABLED.
            timeout: Operation takes longer than expected.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been created yet
        """

    @abc.abstractmethod
    def wait_for_no_connections(
        self, *, timeout: float | None = DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT
    ) -> None:
        """Waits until the WLAN network state is disconnected

        Raises:
            HoneydewWlanError: Failure to observe no connection within timeout.
        """
