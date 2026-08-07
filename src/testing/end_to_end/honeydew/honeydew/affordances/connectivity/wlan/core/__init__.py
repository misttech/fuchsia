# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""WLAN Core affordance"""

import asyncio
import logging
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import timedelta

import fidl_fuchsia_wlan_common as f_wlan_common
import fidl_fuchsia_wlan_device_service as f_wlan_device_service
import fidl_fuchsia_wlan_ieee80211 as f_wlan_ieee80211
import fidl_fuchsia_wlan_internal as f_wlan_internal
import fidl_fuchsia_wlan_sme as f_wlan_sme
from fidl._client import FidlClient
from fuchsia_controller_py import FcTransportStatus, ZxStatus
from honeydew import affordances_capable, errors
from honeydew.affordances.affordance import AsyncLazyReady, ensure_ready
from honeydew.affordances.connectivity.wlan.utils import errors as wlan_errors
from honeydew.affordances.connectivity.wlan.utils.types import (
    BssDescriptionParser,
    CountryCode,
    WlanInterfaces,
)
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing.custom_types import FidlEndpoint, MacAddress

_LOGGER: logging.Logger = logging.getLogger(__name__)

_PAUSE_FOR_ADDITIONAL_PHY_DEVICES = timedelta(seconds=1)

# Fuchsia Controller proxies
_DEVICE_MONITOR_PROXY = FidlEndpoint(
    "core/wlandevicemonitor", "fuchsia.wlan.device.service.DeviceMonitor"
)
_REGULATORY_REGION_CONFIGURATOR_PROXY = FidlEndpoint(
    "core/regulatory_region",
    "fuchsia.location.namedplace.RegulatoryRegionConfigurator",
)


class WlanCore(AsyncLazyReady):
    """WLAN Core affordance"""

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.RebootCapableDevice,
        fuchsia_device_close: affordances_capable.FuchsiaDeviceClose,
    ) -> None:
        """Create an Async WLAN Core Fuchsia Controller affordance.

        Args:
            device_name: Device name returned by `ffx target list`.
            ffx: FFX transport.
            fuchsia_controller: Fuchsia Controller transport.
            reboot_affordance: Object that implements RebootCapableDevice.
            fuchsia_device_close: Object that implements FuchsiaDeviceClose.
        """
        AsyncLazyReady.__init__(self)

        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx
        self._fc_transport = fuchsia_controller
        self._reboot_affordance = reboot_affordance
        self._fuchsia_device_close = fuchsia_device_close

        self._reboot_affordance.register_for_on_device_boot(self.make_ready)

    async def make_ready(self) -> None:
        """Re-initializes connection to the WLAN stack."""
        self._device_monitor_proxy = f_wlan_device_service.DeviceMonitorClient(
            self._fc_transport.connect_device_proxy(_DEVICE_MONITOR_PROXY)
        )
        await super().make_ready()

    @ensure_ready
    async def connect(
        self,
        ssid: str,
        bss_desc: f_wlan_ieee80211.BssDescription,
        authentication: f_wlan_internal.Authentication,
    ) -> bool:
        """Trigger connection to a network.

        Args:
            ssid: The network to connect to.
            bss_desc: The basic service set for target network.
            authentication: Authentication to connect with.

        Returns:
            True on success otherwise false.

        Raises:
            HoneydewWlanError: Error from WLAN stack
            NetworkInterfaceNotFoundError: No client WLAN interface found.
            TypeError: When authentication is not provided.
        """
        iface_id = await self._get_first_sme(f_wlan_common.WlanMacRole.CLIENT)
        sme = await self._get_client_sme(iface_id)

        client, server = self._fc_transport.channel_create()
        connect_transaction_client = f_wlan_sme.ConnectTransactionClient(
            client.take()
        )

        req = f_wlan_sme.ConnectRequest(
            ssid=list(ssid.encode("utf-8")),
            bss_description=bss_desc,
            multiple_bss_candidates=False,  # only used for metrics, selected arbitrarily
            authentication=authentication,
            deprecated_scan_type=f_wlan_common.ScanType.ACTIVE,
        )

        # Run an event handler in the background before starting the connect.
        event_handler = ConnectTransactionEventHandler(
            connect_transaction_client
        )
        event_handler_task = asyncio.create_task(event_handler.serve())

        try:
            # Initiate the connect.
            sme.connect(req=req, txn=server.take())

            # Wait for the driver to finish connecting.
            on_connect_result_req = await asyncio.wait_for(
                event_handler.txn_queue.get(), timeout=60
            )
            assert isinstance(
                on_connect_result_req,
                f_wlan_sme.ConnectTransactionOnConnectResultRequest,
            )
            result = on_connect_result_req.result
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientSme.Connect() error {status}"
            ) from status
        except TimeoutError as e:
            raise wlan_errors.HoneydewWlanError(
                f'Timed out waiting for the WLAN SME to connect to "{ssid}"'
            ) from e
        finally:
            event_handler_task.cancel()
            try:
                await event_handler_task
            except asyncio.exceptions.CancelledError:
                pass  # expected

        # Verify the connection.
        if result.code != f_wlan_ieee80211.StatusCode.SUCCESS:
            code = f_wlan_ieee80211.StatusCode(result.code)
            raise wlan_errors.HoneydewWlanError(
                f'Failed to connect to "{ssid}", received {code.name}({code.value})'
            )
        if result.is_credential_rejected:
            raise wlan_errors.HoneydewWlanError(
                f'Failed to connect to "{ssid}", credentials were rejected.'
            )

        client_status = await self._status(sme)
        if client_status.connected:
            got_ssid = bytes(client_status.connected.ssid).decode("utf-8")
            if got_ssid != ssid:
                raise wlan_errors.HoneydewWlanError(
                    f'Connected to wrong network. Expected "{ssid}", got "{got_ssid}".'
                )
        else:
            raise wlan_errors.HoneydewWlanError(
                f"Expected ClientStatusResponse.connected, got {client_status}"
            )

        return True

    @ensure_ready
    async def create_iface(
        self,
        phy_id: int,
        role: f_wlan_common.WlanMacRole,
        sta_addr: str | None = None,
    ) -> int:
        """Create a new WLAN interface.

        Args:
            phy_id: The iface ID.
            role: The role of the new iface.
            sta_addr: MAC address for softAP iface.

        Returns:
            Iface id of newly created interface.

        Raises:
            HoneydewWlanError: Error from WLAN stack
            ValueError: Invalid MAC address
        """
        if sta_addr is None:
            sta_addr = "00:00:00:00:00:00"
            _LOGGER.warning(
                "No MAC provided in args of create_iface, using %s", sta_addr
            )

        try:
            create_iface_response = (
                await self._device_monitor_proxy.create_iface(
                    phy_id=phy_id,
                    role=role,
                    sta_address=bytes(MacAddress(sta_addr)),
                )
            ).unwrap()
        except (AssertionError, ZxStatus, FcTransportStatus) as e:
            raise wlan_errors.HoneydewWlanError(
                "DeviceMonitor.CreateIface() error"
            ) from e

        assert (
            create_iface_response.iface_id is not None
        ), f"{create_iface_response!r} missing iface_id"
        return create_iface_response.iface_id

    @ensure_ready
    async def destroy_iface(self, iface_id: int) -> None:
        """Destroy WLAN interface by ID.

        Args:
            iface_id: The interface to destroy.

        Raises:
            HoneydewWlanError: Error from WLAN stack
        """
        req = f_wlan_device_service.DestroyIfaceRequest(iface_id=iface_id)
        try:
            destroy_iface = await self._device_monitor_proxy.destroy_iface(
                req=req
            )
            if destroy_iface.status != FcTransportStatus.FC_OK:
                raise FcTransportStatus(destroy_iface.status)
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"DeviceMonitor.DestroyIface() error {status}"
            ) from status

    @ensure_ready
    async def disconnect(self) -> None:
        """Disconnect all client WLAN connections.

        Raises:
            HoneydewWlanError: Error from WLAN stack
        """
        iface_ids = await self.get_iface_id_list()

        for iface_id in iface_ids:
            info = await self.query_iface(iface_id)
            if info.role == f_wlan_common.WlanMacRole.CLIENT:
                sme = await self._get_client_sme(iface_id)
                try:
                    await sme.disconnect(
                        reason=f_wlan_sme.UserDisconnectReason.WLAN_SERVICE_UTIL_TESTING,
                    )
                except FcTransportStatus as status:
                    raise wlan_errors.HoneydewWlanError(
                        f"SmeClient.Disconnect() error {status}"
                    ) from status

    @ensure_ready
    async def get_country(self, phy_id: int) -> CountryCode:
        """Queries the currently configured country code from phy `phy_id`.

        Args:
            phy_id: A phy id that is present on the device.

        Returns:
            The currently configured country code from `phy_id`.

        Raises:
            HoneydewWlanError: DeviceMonitor.GetCountry error
        """
        try:
            get_country_response = (
                await self._device_monitor_proxy.get_country(phy_id=phy_id)
            ).unwrap()
        except (AssertionError, ZxStatus, FcTransportStatus) as e:
            raise wlan_errors.HoneydewWlanError(
                "DeviceMonitor.GetCountry() error"
            ) from e

        return CountryCode(
            bytes(get_country_response.resp.alpha2).decode("utf-8")
        )

    @ensure_ready
    async def set_country(self, phy_id: int, code: CountryCode) -> None:
        try:
            await self._device_monitor_proxy.set_country(
                req=f_wlan_device_service.SetCountryRequest(
                    phy_id=phy_id,
                    alpha2=[ord(c) for c in code],
                )
            )
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"DeviceMonitor.SetCountry() error {status}"
            ) from status

    @ensure_ready
    async def get_iface_id_list(self) -> Sequence[int]:
        """Get list of wlan iface IDs on device.

        Returns:
            A list of wlan iface IDs that are present on the device.

        Raises:
            HoneydewWlanError: DeviceMonitor.ListIfaces error
        """
        try:
            return (await self._device_monitor_proxy.list_ifaces()).iface_list
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"DeviceMonitor.ListIfaces() error {status}"
            ) from status

    @ensure_ready
    async def get_phy_id_list(self) -> Sequence[int]:
        """Get list of phy ids on device.

        Returns:
            A list of phy ids that is present on the device.

        Raises:
            HoneydewWlanError: DeviceMonitor.ListPhys error
        """
        try:
            return (await self._device_monitor_proxy.list_phys()).phy_list
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"DeviceMonitor.ListPhys() error {status}"
            ) from status

    @ensure_ready
    async def query_interfaces(self) -> WlanInterfaces:
        """Retrieves a QueryIfaceResponse for every WLAN interface on the device.

        Returns:
            WlanInterfaces containing a QueryIfaceResponse for every WLAN interface
            on the device.

        Raises:
            HoneydewWlanError: DeviceMonitor.ListIfaces or DeviceMonitor.QueryIface error
        """
        wlan_iface_ids = await self.get_iface_id_list()
        if not wlan_iface_ids:
            return WlanInterfaces(client={}, ap={})

        client: dict[MacAddress, f_wlan_device_service.QueryIfaceResponse] = {}
        ap: dict[MacAddress, f_wlan_device_service.QueryIfaceResponse] = {}
        for ids in wlan_iface_ids:
            result = await self.query_iface(ids)
            mac = MacAddress(bytes(result.sta_addr))
            match result.role:
                case f_wlan_common.WlanMacRole.CLIENT:
                    client[mac] = result
                case f_wlan_common.WlanMacRole.AP:
                    ap[mac] = result
                case _:
                    raise wlan_errors.HoneydewWlanError(
                        f'Unexpected WlanMacRole "{result.role}" for iface {id}'
                    )

        return WlanInterfaces(client=client, ap=ap)

    @ensure_ready
    async def query_iface(
        self, iface_id: int
    ) -> f_wlan_device_service.QueryIfaceResponse:
        """Retrieves interface info for given wlan iface id.

        Args:
            iface_id: The wlan interface id to get info from.

        Returns:
            QueryIfaceResponseWrapper from the SL4F server.

        Raises:
            HoneydewWlanError: DeviceMonitor.QueryIface error
        """
        try:
            return (
                (
                    await self._device_monitor_proxy.query_iface(
                        iface_id=iface_id
                    )
                )
                .unwrap()
                .resp
            )
        except (AssertionError, ZxStatus, FcTransportStatus) as e:
            raise wlan_errors.HoneydewWlanError(
                "DeviceMonitor.QueryIface() error"
            ) from e

    @ensure_ready
    async def scan_for_bss_info(
        self,
    ) -> dict[str, list[f_wlan_ieee80211.BssDescription]]:
        """Scans and returns BSS info.

        Returns:
            A dict mapping each seen SSID to a list of BSS Description IE
            blocks, one for each BSS observed in the network

        Raises:
            HoneydewWlanError: Error from WLAN stack
            NetworkInterfaceNotFoundError: No client WLAN interface found.
        """
        iface_id = await self._get_first_sme(f_wlan_common.WlanMacRole.CLIENT)
        client_sme = await self._get_client_sme(iface_id)

        # Perform a passive scan
        req = f_wlan_sme.ScanRequest(
            passive=f_wlan_sme.PassiveScanRequest(channels=[])
        )

        try:
            scan_for_controller_response = (
                await client_sme.scan_for_controller(req=req)
            ).unwrap()
        except (AssertionError, ZxStatus, FcTransportStatus) as e:
            raise wlan_errors.HoneydewWlanError(
                "ClientSme.ScanForController() error"
            ) from e

        results: dict[str, list[f_wlan_ieee80211.BssDescription]] = {}

        for scan_result in scan_for_controller_response.scan_results:
            desc = scan_result.bss_description
            ssid = BssDescriptionParser.ssid(desc)
            if ssid:
                if ssid in results:
                    results[ssid].append(desc)
                else:
                    results[ssid] = [desc]
            else:
                _LOGGER.warning(
                    "Scan result does not contain SSID: %s", scan_result
                )

        return results

    async def _get_first_sme(self, role: f_wlan_common.WlanMacRole) -> int:
        """Find a WLAN interface running with the specified role.

        Args:
            role: Desired mode of the WLAN interface.

        Raises:
            NetworkInterfaceNotFoundError: No WLAN interface found running with the
                specified role.

        Returns:
            ID of the first WLAN interface found running with the specified
            role. The others are discarded.
        """
        iface_ids = await self.get_iface_id_list()
        if len(iface_ids) == 0:
            raise wlan_errors.NetworkInterfaceNotFoundError(
                "No WLAN interface found"
            )

        for iface_id in iface_ids:
            info = await self.query_iface(iface_id)
            if info.role == role:
                return iface_id

        raise wlan_errors.NetworkInterfaceNotFoundError(
            f"WLAN interface with role {role} not found"
        )

    async def _get_client_sme(
        self, iface_id: int
    ) -> f_wlan_sme.ClientSmeClient:
        """Get a handle to ClientSme for performing SME actions.

        Args:
            iface_id: The wlan interface id to connect with

        Raises:
            HoneydewWlanError: DeviceMonitor.QueryIface error

        Returns:
            Client-side handle to the fuchsia.wlan.sme.ClientSme protocol, used
            for performing driver-layer actions on the underlying WLAN hardware.
        """
        client, server = self._fc_transport.channel_create()
        sme_client = f_wlan_sme.ClientSmeClient(client)

        try:
            await self._device_monitor_proxy.get_client_sme(
                iface_id=iface_id, sme_server=server.take()
            )
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"DeviceMonitor.GetClientSme() error {status}"
            ) from status

        return sme_client

    @ensure_ready
    async def status(self) -> f_wlan_sme.ClientStatusResponse:
        """Request connection status

        Returns:
            fuchsia.wlan.sme/ClientStatusResponse FIDL union.

        Raises:
            HoneydewWlanError: Error from WLAN stack
            NetworkInterfaceNotFoundError: No client WLAN interface found.
            TypeError: If any of the return values are not of the expected type.
        """
        iface_id = await self._get_first_sme(f_wlan_common.WlanMacRole.CLIENT)
        sme = await self._get_client_sme(iface_id)
        return await self._status(sme)

    async def _status(
        self, sme: f_wlan_sme.ClientSmeClient
    ) -> f_wlan_sme.ClientStatusResponse:
        try:
            resp = await sme.status()
        except FcTransportStatus as status:
            raise wlan_errors.HoneydewWlanError(
                f"ClientSme.Status() error {status}"
            ) from status

        return resp.resp

    @ensure_ready
    async def ensure_single_phy(self) -> int:
        """Asserts there is only one PHY device and returns its ID.

        Returns:
            The phy_id of the single detected PHY.

        Raises:
            HoneydewWlanError: DeviceWatcher failed to report a phy or detected second phy.
        """
        proxy, server = self._fc_transport.channel_create()
        device_watcher_client = f_wlan_device_service.DeviceWatcherClient(
            proxy.take()
        )
        self._device_monitor_proxy.watch_devices(watcher=server.take())

        phy_id = None
        async with DeviceWatcherEventHandler(device_watcher_client) as ctx:
            while True:
                try:
                    next_txn = await asyncio.wait_for(
                        ctx.txn_queue.get(),
                        timeout=_PAUSE_FOR_ADDITIONAL_PHY_DEVICES.total_seconds(),
                    )
                    if isinstance(
                        next_txn,
                        f_wlan_device_service.DeviceWatcherOnPhyAddedRequest,
                    ):
                        if phy_id is not None:
                            raise wlan_errors.HoneydewWlanError(
                                "Detected second phy device."
                            )
                        phy_id = next_txn.phy_id
                    elif isinstance(
                        next_txn,
                        f_wlan_device_service.DeviceWatcherOnIfaceAddedRequest,
                    ):
                        _LOGGER.info(
                            "Ignoring notification of existing iface %s",
                            next_txn.iface_id,
                        )
                    else:
                        raise wlan_errors.HoneydewWlanError(
                            f"Expected OnPhyAdded or OnIfaceAdded, but received: {next_txn}"
                        )
                except TimeoutError:
                    _LOGGER.debug("Assuming all DeviceWatcher events observed.")
                    break

        if phy_id is None:
            raise wlan_errors.HoneydewWlanError(
                "DeviceWatcher failed to report a phy."
            )
        return phy_id


@dataclass
class ConnectTransactionContext:
    txn_queue: asyncio.Queue[
        f_wlan_sme.ConnectTransactionOnConnectResultRequest
        | f_wlan_sme.ConnectTransactionOnDisconnectRequest
        | f_wlan_sme.ConnectTransactionOnRoamResultRequest
        | f_wlan_sme.ConnectTransactionOnSignalReportRequest
        | f_wlan_sme.ConnectTransactionOnChannelSwitchedRequest
    ]


class ConnectTransactionEventHandler(f_wlan_sme.ConnectTransactionEventHandler):
    """Event handler for ClientSme.Connect()."""

    def __init__(
        self,
        client: FidlClient,
    ) -> None:
        super().__init__(client)
        self.txn_queue: asyncio.Queue[
            f_wlan_sme.ConnectTransactionOnConnectResultRequest
            | f_wlan_sme.ConnectTransactionOnDisconnectRequest
            | f_wlan_sme.ConnectTransactionOnRoamResultRequest
            | f_wlan_sme.ConnectTransactionOnSignalReportRequest
            | f_wlan_sme.ConnectTransactionOnChannelSwitchedRequest
        ] = asyncio.Queue()
        self.server_task: asyncio.Task[None] | None = None

    def on_connect_result(
        self, request: f_wlan_sme.ConnectTransactionOnConnectResultRequest
    ) -> None:
        """Return the result of the initial connection request or later
        SME-initiated reconnection."""
        _LOGGER.debug(
            "ConnectTransaction.OnConnectResult() called with %s", request
        )
        self.txn_queue.put_nowait(request)

    def on_disconnect(
        self, request: f_wlan_sme.ConnectTransactionOnDisconnectRequest
    ) -> None:
        """Notify that the client has disconnected."""
        _LOGGER.debug(
            "ConnectTransaction.OnDisconnect() called with %s", request
        )
        self.txn_queue.put_nowait(request)

    def on_roam_result(
        self, request: f_wlan_sme.ConnectTransactionOnRoamResultRequest
    ) -> None:
        """Report the result of a roam attempt."""
        _LOGGER.debug(
            "ConnectTransaction.OnRoamResult() called with %s", request
        )
        self.txn_queue.put_nowait(request)

    def on_signal_report(
        self, request: f_wlan_sme.ConnectTransactionOnSignalReportRequest
    ) -> None:
        """Give an update of the latest signal report."""
        _LOGGER.debug(
            "ConnectTransaction.OnSignalReport() called with %s", request
        )
        self.txn_queue.put_nowait(request)

    def on_channel_switched(
        self, request: f_wlan_sme.ConnectTransactionOnChannelSwitchedRequest
    ) -> None:
        """Give an update of the channel switching."""
        _LOGGER.debug(
            "ConnectTransaction.OnChannelSwitched() called with %s", request
        )
        self.txn_queue.put_nowait(request)

    async def __aenter__(self) -> ConnectTransactionContext:
        self.server_task = asyncio.create_task(self.serve())
        return ConnectTransactionContext(txn_queue=self.txn_queue)

    async def __aexit__(self, *args: object) -> None:
        if self.server_task is not None:
            self.server_task.cancel()
            try:
                await self.server_task
            except asyncio.CancelledError:
                pass


@dataclass
class DeviceWatcherContext:
    txn_queue: asyncio.Queue[
        f_wlan_device_service.DeviceWatcherOnPhyAddedRequest
        | f_wlan_device_service.DeviceWatcherOnPhyRemovedRequest
        | f_wlan_device_service.DeviceWatcherOnIfaceAddedRequest
        | f_wlan_device_service.DeviceWatcherOnIfaceRemovedRequest
    ]


class DeviceWatcherEventHandler(
    f_wlan_device_service.DeviceWatcherEventHandler
):
    """Event handler for DeviceWatcher."""

    def __init__(
        self,
        client: FidlClient,
    ) -> None:
        self.client = client
        self.server_task: asyncio.Task[None] | None = None

    def on_phy_added(
        self, request: f_wlan_device_service.DeviceWatcherOnPhyAddedRequest
    ) -> None:
        _LOGGER.debug("DeviceWatcher.OnPhyAdded() called with %s", request)
        self.txn_queue.put_nowait(request)

    def on_phy_removed(
        self, request: f_wlan_device_service.DeviceWatcherOnPhyRemovedRequest
    ) -> None:
        _LOGGER.debug("DeviceWatcher.OnPhyRemoved() called with %s", request)
        self.txn_queue.put_nowait(request)

    def on_iface_added(
        self, request: f_wlan_device_service.DeviceWatcherOnIfaceAddedRequest
    ) -> None:
        _LOGGER.debug("DeviceWatcher.OnPhyAdded() called with %s", request)
        self.txn_queue.put_nowait(request)

    def on_iface_removed(
        self, request: f_wlan_device_service.DeviceWatcherOnIfaceRemovedRequest
    ) -> None:
        _LOGGER.debug("DeviceWatcher.OnIfaceRemoved() called with %s", request)
        self.txn_queue.put_nowait(request)

    async def __aenter__(self) -> DeviceWatcherContext:
        super().__init__(client=self.client)
        self.txn_queue: asyncio.Queue[
            f_wlan_device_service.DeviceWatcherOnPhyAddedRequest
            | f_wlan_device_service.DeviceWatcherOnPhyRemovedRequest
            | f_wlan_device_service.DeviceWatcherOnIfaceAddedRequest
            | f_wlan_device_service.DeviceWatcherOnIfaceRemovedRequest
        ] = asyncio.Queue()
        self.server_task = asyncio.create_task(self.serve())
        return DeviceWatcherContext(txn_queue=self.txn_queue)

    async def __aexit__(self, *args: object) -> None:
        if self.server_task is not None:
            self.server_task.cancel()
            try:
                await self.server_task
            except asyncio.CancelledError:
                pass
