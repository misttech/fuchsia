# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""WLAN policy affordance implementation using Fuchsia Controller."""

from __future__ import annotations

import asyncio
import functools
import inspect
import logging
import pprint
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import datetime, timedelta

import fidl_fuchsia_wlan_device_service as f_wlan_device_service
import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_async_extension
from fuchsia_controller_py import Channel, ZxStatus

from honeydew import affordances_capable, errors
from honeydew.affordances.affordance import AsyncLazyReady, ensure_ready
from honeydew.affordances.connectivity.wlan.utils import errors as wlan_errors
from honeydew.affordances.connectivity.wlan.utils.types import (
    ClientStateSummary,
    ConnectionState,
    CountryCode,
    Credential,
    NetworkConfig,
    NetworkIdentifier,
    SecurityType,
)
from honeydew.affordances.connectivity.wlan.wlan_policy import wlan_policy
from honeydew.affordances.location.location import Location
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing.custom_types import FidlEndpoint

# List of required FIDLs for the WLAN Fuchsia Controller affordance.
_REQUIRED_CAPABILITIES = [
    "fuchsia.wlan.policy.ClientListener",
    "fuchsia.wlan.policy.ClientProvider",
]

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Fuchsia Controller proxies
_CLIENT_PROVIDER_PROXY = FidlEndpoint(
    "core/wlancfg", "fuchsia.wlan.policy.ClientProvider"
)
_CLIENT_LISTENER_PROXY = FidlEndpoint(
    "core/wlancfg", "fuchsia.wlan.policy.ClientListener"
)
_DEVICE_MONITOR_PROXY = FidlEndpoint(
    "core/wlandevicemonitor", "fuchsia.wlan.device.service.DeviceMonitor"
)

_SET_COUNTRY_CODE_TIMEOUT = timedelta(seconds=10)
_COUNTRY_CODE_CHECK_INTERVAL = timedelta(seconds=1)


async def collect_network_config_iterator(
    iterator: (f_wlan_policy.NetworkConfigIteratorClient),
    *,
    timeout: float
    | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
) -> (list[f_wlan_policy.NetworkConfigIteratorGetNextResponse]):
    """Collect all elements from a NetworkConfigIterator.

    Will check for errors during collection.

    Args:
        iterator: Iterator to collect elements from.
        timeout: timeout value.

    Returns:
        All elements collected from iterator.

    Raises:
        HoneydewWlanError: Error from WLAN stack.
    """
    elements = []
    while True:
        try:
            response = await asyncio.wait_for(iterator.get_next(), timeout)
        except ZxStatus as status:
            if status.raw() == ZxStatus.ZX_ERR_PEER_CLOSED:
                # The server closed the channel, signifying the end of elements.
                break
            raise wlan_errors.HoneydewWlanError(
                f"{type(iterator).__name__}.GetNext() transport error"
            ) from status

        elements.append(response)

    return elements


async def collect_scan_result_iterator(
    iterator: (f_wlan_policy.ScanResultIteratorClient),
    *,
    timeout: float
    | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
) -> (list[f_wlan_policy.ScanResultIteratorGetNextResponse]):
    """Collect all elements from a ScanResultIterator.

    Will check for errors during collection.

    Args:
        iterator: Iterator to collect elements from.
        timeout: timeout value.

    Returns:
        All elements collected from iterator.

    Raises:
        HoneydewWlanError: Error from WLAN stack.
    """
    elements = []
    while True:
        try:
            result = await asyncio.wait_for(iterator.get_next(), timeout)
        except ZxStatus as status:
            if status.raw() == ZxStatus.ZX_ERR_PEER_CLOSED:
                # The server closed the channel, signifying the end of elements.
                break
            raise wlan_errors.HoneydewWlanError(
                f"{type(iterator).__name__}.GetNext() transport error"
            ) from status

        try:
            response = result.unwrap()
        except AssertionError as e:
            if result.err is not None:
                raise wlan_errors.HoneydewWlanError(
                    f"{type(iterator).__name__}.GetNext() error: {f_wlan_policy.ScanErrorCode(result.err).name}"
                ) from e
            else:
                raise wlan_errors.HoneydewWlanError(
                    f"{type(iterator).__name__}.GetNext() framework error"
                ) from e
        elements.append(response)

    return elements


@dataclass
class ClientControllerState:
    proxy: f_wlan_policy.ClientControllerClient
    updates: asyncio.Queue[ClientStateSummary]
    # Keep the async task for fuchsia.wlan.policy/ClientStateUpdates so it
    # doesn't get garbage collected then cancelled.
    client_state_updates_server_task: asyncio.Task[None]


class WlanPolicy(wlan_policy.WlanPolicy, AsyncLazyReady):
    """WlanPolicy affordance implemented with Fuchsia Controller."""

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.RebootCapableDevice,
        fuchsia_device_close: affordances_capable.FuchsiaDeviceClose,
        location: Location,
    ) -> None:
        """Create a WlanPolicy Fuchsia Controller affordance.

        Args:
            device_name: Device name returned by `ffx target list`.
            ffx: FFX transport.
            fuchsia_controller: Fuchsia Controller transport.
            reboot_affordance: Object that implements RebootCapableDevice.
            fuchsia_device_close: Object that implements FuchsiaDeviceClose.
        """
        super().__init__()

        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx
        self._fc_transport = fuchsia_controller
        self._reboot_affordance = reboot_affordance
        self._fuchsia_device_close = fuchsia_device_close
        self._client_controller: ClientControllerState | None = None
        self._location = location

        self.verify_supported()

        @functools.wraps(self.make_ready)
        def make_ready() -> None:
            fuchsia_async_extension.get_loop().run_until_complete(
                self.make_ready()
            )

        self._reboot_affordance.register_for_on_device_boot(make_ready)

        @functools.wraps(self._close)
        def _close() -> None:
            fuchsia_async_extension.get_loop().run_until_complete(self._close())

        self._fuchsia_device_close.register_for_on_device_close(_close)

    async def make_ready(self) -> None:
        await super().make_ready()
        await self._create_client_controller()

    async def _create_client_controller(self) -> None:
        """Initializes the client controller.

        See fuchsia.wlan.policy/ClientProvider.GetController().

        Raises:
            HoneydewWlanError: Error from WLAN stack.
        """
        self._client_provider_proxy = f_wlan_policy.ClientProviderClient(
            self._fc_transport.connect_device_proxy(_CLIENT_PROVIDER_PROXY)
        )
        self._device_monitor_proxy = f_wlan_device_service.DeviceMonitorClient(
            self._fc_transport.connect_device_proxy(_DEVICE_MONITOR_PROXY)
        )

        if self._client_controller:
            self._client_controller.client_state_updates_server_task.cancel()
            self._client_controller = None

        controller_client, controller_server = Channel.create()
        client_controller_proxy = f_wlan_policy.ClientControllerClient(
            controller_client.take()
        )

        updates: asyncio.Queue[ClientStateSummary] = asyncio.Queue()

        updates_client, updates_server = Channel.create()
        client_state_updates_server = ClientStateUpdatesImpl(
            updates_server, updates
        )
        task = asyncio.create_task(client_state_updates_server.serve())

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientProvider.GetController()"
        )

        try:
            self._client_provider_proxy.get_controller(
                requests=controller_server.take(),
                updates=updates_client.take(),
            )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientProvider.GetController() error {status}"
            ) from status

        self._client_controller = ClientControllerState(
            proxy=client_controller_proxy,
            updates=updates,
            client_state_updates_server_task=task,
        )

    async def _close(self) -> None:
        """Release handle on client controller.

        This needs to be called on test class teardown otherwise the device may
        be left in an inoperable state where no other components or tests can
        access state-changing WLAN Policy APIs.

        This is idempotent and irreversible. No other methods should be called
        after this one.
        """
        if self._client_controller:
            self._client_controller.client_state_updates_server_task.cancel()
            try:
                await self._client_controller.client_state_updates_server_task
            except asyncio.CancelledError:
                pass
            self._client_controller = None

    def verify_supported(self) -> None:
        """Verifies that the WlanPolicy affordance using FuchsiaController is supported by the
        Fuchsia device.

        This method should be called in `__init__()` so that if this affordance was called on a
        Fuchsia device that does not support it, it will raise NotSupportedError.

        Raises:
            NotSupportedError: If affordance is not supported.
        """
        for capability in _REQUIRED_CAPABILITIES:
            # TODO(http://b/359342196): This is a maintenance burden; find a
            # better way to detect FIDL component capabilities.
            if capability not in self._ffx.run(
                ["component", "capability", capability],
                # TODO(b/474143046) update to JSON when ffx supports it
                machine=ffx_types.MachineFormat.RAW,
            ):
                _LOGGER.warning(
                    "All available WLAN component capabilities:\n%s",
                    self._ffx.run(
                        ["component", "capability", "fuchsia.wlan"],
                        # TODO(b/474143046) update to JSON when ffx supports it
                        machine=ffx_types.MachineFormat.RAW,
                    ),
                )
                raise errors.NotSupportedError(
                    f'Component capability "{capability}" not exposed by device '
                    f"{self._device_name}; this build of Fuchsia does not support the "
                    "WLAN FC affordance."
                )

    def set_country_code_sync(self, country_code: CountryCode) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.set_country_code(country_code)
        )

    @ensure_ready
    async def set_country_code(self, country_code: CountryCode) -> None:
        await self._set_country_code(country_code)

    async def _set_country_code(self, country_code: CountryCode) -> None:
        """Sets DUT country code."""
        _LOGGER.info("Setting DUT country code to %s...", country_code)
        self._location.set_region(country_code)
        _LOGGER.info(
            "Waiting for configuration of all PHYs with country code %s...",
            country_code,
        )

        phy_list = (await self._device_monitor_proxy.list_phys()).phy_list

        deadline = datetime.now() + _SET_COUNTRY_CODE_TIMEOUT
        while datetime.now() < deadline:
            phy_country_codes = [
                CountryCode.from_bytes(
                    bytes(
                        (
                            await self._device_monitor_proxy.get_country(
                                phy_id=phy_id
                            )
                        )
                        .unwrap()
                        .resp.alpha2
                    )
                )
                for phy_id in phy_list
            ]

            # TODO(https://fxbug.dev/469784448): USER_XZ is the equivalent of WORLDWIDE
            # on some devices.
            if country_code == CountryCode.USER_XZ:
                if all(
                    [CountryCode.WORLDWIDE == cc for cc in phy_country_codes]
                ):
                    # Mutate country_code to what was actually set in each PHY.
                    country_code = CountryCode.WORLDWIDE
                    break
            else:
                if all([country_code == cc for cc in phy_country_codes]):
                    break

            await asyncio.sleep(_COUNTRY_CODE_CHECK_INTERVAL.total_seconds())
        else:
            if country_code == CountryCode.WORLDWIDE:
                _LOGGER.warning(
                    "Failed to set %s. Trying %s.",
                    CountryCode.WORLDWIDE,
                    CountryCode.USER_XZ,
                )
                return await self._set_country_code(CountryCode.USER_XZ)
            else:
                raise RuntimeError(
                    f"Failed to set DUT country code to {country_code}."
                )
        _LOGGER.info(
            "All PHYs configured for new country code: %s", country_code
        )

    def connect_sync(
        self,
        target_ssid: str,
        security_type: SecurityType,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> f_wlan_policy.RequestStatus:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self.connect(target_ssid, security_type, timeout=timeout)
        )

    @ensure_ready
    async def connect(
        self,
        target_ssid: str,
        security_type: SecurityType,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> f_wlan_policy.RequestStatus:
        """Triggers connection to a network.

        Args:
            target_ssid: The network to connect to. Must have been previously
                saved in order for a successful connection to happen.
            security_type: The security protocol of the network.
            timeout: timeout value.

        Returns:
            A RequestStatus response to the connect request.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return value not a string.
            RuntimeError: Client controller has not been initialized.
        """
        assert self._client_controller is not None

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.Connect("
            'ssid="%s", type_="%s")',
            target_ssid,
            security_type,
        )

        try:
            resp = await asyncio.wait_for(
                self._client_controller.proxy.connect(
                    id_=NetworkIdentifier(target_ssid, security_type).to_fidl(),
                ),
                timeout,
            )
            return f_wlan_policy.RequestStatus(resp.status)
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.Connect() error {status}"
            ) from status

    def get_saved_networks_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> list[NetworkConfig]:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self.get_saved_networks(timeout=timeout)
        )

    @ensure_ready
    async def get_saved_networks(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> list[NetworkConfig]:
        """Gets networks saved on device.

        Returns:
            A list of NetworkConfigs.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return values not correct types.
            RuntimeError: Client controller has not been initialized.
        """
        assert self._client_controller is not None

        client, server = Channel.create()
        iterator = f_wlan_policy.NetworkConfigIteratorClient(client.take())

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.GetSavedNetworks()"
        )

        try:
            self._client_controller.proxy.get_saved_networks(
                iterator=server.take(),
            )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.GetSavedNetworks() error {status}"
            ) from status

        configs = []
        for resp in await collect_network_config_iterator(
            iterator, timeout=timeout
        ):
            for config in resp.configs:
                configs.append(NetworkConfig.from_fidl(config))
        return configs

    def get_status_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self.get_status(timeout=timeout)
        )

    async def get_status(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        """Gets the current client listener state immediately.

        This call will get a new, temporary update listener which will return
        the most recent state immediately. This will not effect the class's
        existing state listener channel.

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
        client_listener_proxy = f_wlan_policy.ClientListenerClient(
            self._fc_transport.connect_device_proxy(_CLIENT_LISTENER_PROXY)
        )

        updates: asyncio.Queue[ClientStateSummary] = asyncio.Queue()
        updates_client, updates_server = Channel.create()
        client_state_updates_server = ClientStateUpdatesImpl(
            updates_server, updates
        )
        task = asyncio.create_task(client_state_updates_server.serve())

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientListener.GetListener() for get_status"
        )
        try:
            client_listener_proxy.get_listener(
                updates=updates_client.take(),
            )
        except ZxStatus as status:
            task.cancel()
            raise wlan_errors.HoneydewWlanError(
                f"ClientListener.GetListener() error {status}"
            ) from status

        try:
            # Retrieve the most recent update. This should be sent immediately
            # after a new listener is registered.
            return await asyncio.wait_for(updates.get(), timeout)
        finally:
            # Clean up the temporary listener task
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass

    def get_update_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> ClientStateSummary:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self.get_update(timeout=timeout)
        )

    @ensure_ready
    async def get_update(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
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
        """
        assert self._client_controller is not None

        return await asyncio.wait_for(
            self._client_controller.updates.get(), timeout
        )

    async def _wait_on_update(
        self,
        f: Callable[[ClientStateSummary], bool | Awaitable[bool]],
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Waits for update.

        Args:
            f: boolian value.
            timeout: timeout value.

        Raises:
             HoneydewWlanError: Error from WLAN stack.
        """
        assert self._client_controller is not None

        client_state_summaries = []
        while True:
            try:
                client_state_summaries.append(
                    await asyncio.wait_for(
                        self._client_controller.updates.get(), timeout
                    )
                )
                result = f(client_state_summaries[-1])
                if inspect.isawaitable(result):
                    result = await result
                if result:
                    return
            except TimeoutError as e:
                raise wlan_errors.HoneydewWlanError(
                    f"Timeout out waiting for next update. Waited: {timeout}s."
                    f"Updates received:\n\n"
                    f"{pprint.pformat(client_state_summaries, indent=4)}"
                ) from e

    def wait_until_update_sync(
        self,
        expected_update: ClientStateSummary,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.wait_until_update(expected_update, timeout=timeout)
        )

    @ensure_ready
    async def wait_until_update(
        self,
        expected_update: ClientStateSummary,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Wait until the expected update.

        Raises:
            HoneydewWlanError: If expected update does not arrive by end of timeout.
        """

        try:
            await self._wait_on_update(
                lambda update: update == expected_update, timeout=timeout
            )
        except TimeoutError as e:
            raise wlan_errors.HoneydewWlanError(
                f"Never received {expected_update}."
            ) from e

    def remove_all_networks_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.remove_all_networks(timeout=timeout)
        )

    @ensure_ready
    async def remove_all_networks(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Deletes all saved networks on the device.

        Args:
            timeout: timeout value.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been initialized.
            TimeoutError: Operation takes longer than DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT.
            per network.
        """
        assert self._client_controller is not None

        for network in await self.get_saved_networks():
            await self.remove_network(
                target_ssid=network.ssid,
                security_type=network.security_type,
                target_pwd=network.credential_value,
                timeout=timeout,
            )

    def remove_network_sync(
        self,
        target_ssid: str,
        security_type: SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.remove_network(
                target_ssid, security_type, target_pwd, timeout=timeout
            )
        )

    @ensure_ready
    async def remove_network(
        self,
        target_ssid: str,
        security_type: SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Removes or "forgets" a network from saved networks.

        Args:
            target_ssid: The network to remove.
            security_type: The security protocol of the network.
            target_pwd: The credential being saved with the network. No password
                is equivalent to an empty string.
            timeout: timeout value.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been initialized.
            TimeoutError: Operation takes longer than DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT.
        """
        assert self._client_controller is not None

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.RemoveNetwork("
            'ssid="%s", type_="%s", credential="%s")',
            target_ssid,
            security_type,
            target_pwd,
        )

        try:
            res = await asyncio.wait_for(
                self._client_controller.proxy.remove_network(
                    config=f_wlan_policy.NetworkConfig(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(target_ssid.encode("utf-8")),
                            type_=security_type.to_fidl(),
                        ),
                        credential=Credential.from_password(
                            target_pwd
                        ).to_fidl(),
                    ),
                ),
                timeout,
            )
            if res.err:
                raise wlan_errors.HoneydewWlanError(
                    f"ClientController.RemoveNetwork() error {res.err}"
                )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.RemoveNetwork() ZxStatus error {status}"
            )

    def save_network_sync(
        self,
        target_ssid: str,
        security_type: SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.save_network(
                target_ssid, security_type, target_pwd, timeout=timeout
            )
        )

    @ensure_ready
    async def save_network(
        self,
        target_ssid: str,
        security_type: SecurityType,
        target_pwd: str | None = None,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Saves a network to the device.

        Args:
            target_ssid: The network to save.
            security_type: The security protocol of the network.
            target_pwd: The credential being saved with the network. No password
                is equivalent to an empty string.
            timeout: timeout value.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been initialized.
        """
        assert self._client_controller is not None

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.SaveNetwork("
            'ssid="%s", type_="%s", credential="%s")',
            target_ssid,
            security_type,
            target_pwd,
        )

        try:
            res = await asyncio.wait_for(
                self._client_controller.proxy.save_network(
                    config=f_wlan_policy.NetworkConfig(
                        id_=f_wlan_policy.NetworkIdentifier(
                            ssid=list(target_ssid.encode("utf-8")),
                            type_=security_type.to_fidl(),
                        ),
                        credential=Credential.from_password(
                            target_pwd
                        ).to_fidl(),
                    ),
                ),
                timeout,
            )
            if res.err:
                raise wlan_errors.HoneydewWlanError(
                    "ClientController.SaveNetworks() NetworkConfigChangeError "
                    f"{res.err}"
                )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.SaveNetwork() error {status}"
            ) from status

    def scan_for_networks_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> list[str]:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self.scan_for_networks(timeout=timeout)
        )

    @ensure_ready
    async def scan_for_networks(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> list[str]:
        """Scans for networks.

        Returns:
            A list of network SSIDs that can be connected to.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            TypeError: Return value not a list.
            RuntimeError: Client controller has not been initialized.
        """
        assert self._client_controller is not None

        client, server = Channel.create()
        iterator = f_wlan_policy.ScanResultIteratorClient(client.take())

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.ScanForNetworks()"
        )

        try:
            self._client_controller.proxy.scan_for_networks(
                iterator=server.take(),
            )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.ScanForNetworks() error {status}"
            ) from status

        scan_results = set()
        responses = await collect_scan_result_iterator(
            iterator, timeout=timeout
        )
        for r in responses:
            assert r.scan_results is not None, f"{r!r} missing scan_results"
            for scan_result in r.scan_results:
                assert (
                    scan_result.id_ is not None
                ), f"{scan_result!r} missing id"
                scan_results.add(bytes(scan_result.id_.ssid).decode("utf-8"))

        return list(scan_results)

    def set_new_update_listener_sync(self) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.set_new_update_listener()
        )

    @ensure_ready
    async def set_new_update_listener(self) -> None:
        """Sets the update listener stream of the facade to a new stream.

        This causes updates to be reset. Intended to be used between tests so
        that the behavior of updates in a test is independent from previous
        tests.

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: Client controller has not been initialized.
        """
        assert self._client_controller is not None

        _LOGGER.debug(
            "WLAN client state updates server is being restarted; unconsumed "
            "updates will be lost! This can be a source of race conditions if "
            "this is being called in the middle of a test that is expecting a "
            "contiguous stream of WLAN events."
        )

        # Replace the existing ClientStateUpdates server without giving up our
        # handle to ClientController. This is necessary since the ClientProvider
        # API is designed to only allow a single caller to make ClientController
        # calls which would impact WLAN state. If we lose our handle to the
        # ClientController, some other component on the system could take it.
        if self._client_controller.client_state_updates_server_task.cancel():
            try:
                await self._client_controller.client_state_updates_server_task
                raise RuntimeError(
                    "Expected cancellation of task to raise CancelledError"
                )
            except asyncio.exceptions.CancelledError:
                pass  # expected

        client_listener_proxy = f_wlan_policy.ClientListenerClient(
            self._fc_transport.connect_device_proxy(_CLIENT_LISTENER_PROXY)
        )

        updates: asyncio.Queue[ClientStateSummary] = asyncio.Queue()
        updates_client, updates_server = Channel.create()
        client_state_updates_server = ClientStateUpdatesImpl(
            updates_server, updates
        )
        task = asyncio.create_task(client_state_updates_server.serve())

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientListener.GetListener()"
        )

        try:
            client_listener_proxy.get_listener(
                updates=updates_client.take(),
            )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientListener.GetListener() error {status}"
            ) from status

        self._client_controller.updates = updates
        self._client_controller.client_state_updates_server_task = task

    def start_client_connections_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.start_client_connections(timeout=timeout)
        )

    @ensure_ready
    async def start_client_connections(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Enables device to initiate connections to networks.

        Either by auto-connecting to saved networks or acting on incoming calls
        triggering connections.

        See fuchsia.wlan.policy/ClientController.StartClientConnections().

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been created yet.
            TimeoutError: timeout.
        """
        assert self._client_controller is not None

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.StartClientConnections()"
        )

        try:
            resp = await asyncio.wait_for(
                self._client_controller.proxy.start_client_connections(),
                timeout,
            )
            status = f_wlan_policy.RequestStatus(resp.status)
            if status != f_wlan_policy.RequestStatus.ACKNOWLEDGED:
                raise wlan_errors.HoneydewWlanError(
                    "ClientController.StartClientConnections() returned "
                    f"request status {status.name}"
                )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.StartClientConnections() ZxStatus error {status}"
            )

    def stop_client_connections_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self.stop_client_connections(timeout=timeout)
        )

    @ensure_ready
    async def stop_client_connections(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        """Disables device for initiating connections to networks.

        Tears down any existing connections to WLAN networks and disables
        initiation of new connections.

        See fuchsia.wlan.policy/ClientController.StopClientConnections().

        Raises:
            HoneydewWlanError: Error from WLAN stack.
            RuntimeError: A client controller has not been created yet.
        """
        assert self._client_controller is not None

        _LOGGER.debug(
            "Calling fuchsia.wlan.policy/ClientController.StopClientConnections()"
        )

        try:
            resp = await asyncio.wait_for(
                self._client_controller.proxy.stop_client_connections(), timeout
            )
            status = f_wlan_policy.RequestStatus(resp.status)
            if status != f_wlan_policy.RequestStatus.ACKNOWLEDGED:
                raise wlan_errors.HoneydewWlanError(
                    f"ClientController.StopClientConnections() returned request status {status.name}"
                )
        except ZxStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientController.StopClientConnections() error {status}"
            ) from status

    def wait_for_no_connections_sync(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self.wait_for_no_connections(timeout=timeout)
        )

    async def wait_for_no_connections(
        self,
        *,
        timeout: float
        | None = wlan_policy.WlanPolicy.DEFAULT_WLAN_POLICY_OPERATION_TIMEOUT,
    ) -> None:
        await self.set_new_update_listener()
        connection_states = {
            ConnectionState.CONNECTING,
            ConnectionState.CONNECTED,
        }

        try:
            await self._wait_on_update(
                lambda update: not any(
                    n.connection_state in connection_states
                    for n in update.networks
                ),
                timeout=timeout,
            )
        except TimeoutError as e:
            raise wlan_errors.HoneydewWlanError(
                "Networks still connected."
            ) from e


class ClientStateUpdatesImpl(f_wlan_policy.ClientStateUpdatesServer):
    """Server to receive WLAN status changes.

    Receives updates for client connections and the associated network state
    These updates contain information about whether or not the device will
    attempt to connect to networks, saved network configuration change
    information, individual connection state information by NetworkIdentifier
    and connection attempt information.
    """

    def __init__(
        self, server: Channel, updates: asyncio.Queue[ClientStateSummary]
    ) -> None:
        super().__init__(server)
        self._updates = updates
        _LOGGER.debug("Started ClientStateUpdates server")

    async def on_client_state_update(
        self,
        request: f_wlan_policy.ClientStateUpdatesOnClientStateUpdateRequest,
    ) -> None:
        """Detected a change to the state or registered listeners.

        Args:
            request: Current summary of WLAN client state.
        """
        summary = ClientStateSummary.from_fidl(request.summary)
        _LOGGER.debug("OnClientStateUpdate called with %s", repr(summary))
        await self._updates.put(summary)
