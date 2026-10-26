# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Domain wrapper classes and testing utilities for wlanix E2E tests."""

import asyncio
import logging
from collections.abc import Sequence

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing.custom_types import FidlEndpoint
from mobly import signals
from mobly.asserts import (
    assert_equal,
    assert_false,
    assert_true,
)
from openwrt_access_point.lib.access_point_config import (
    Security,
    SecurityWpa3,
)

from .fidl_servers import (
    Nl80211MulticastServer,
    Nl80211MulticastServerContext,
    SupplicantStaIfaceCallbackContext,
    SupplicantStaIfaceCallbackServer,
    WifiEventCallbackContext,
    WifiEventCallbackServer,
)
from .nl80211 import (
    NL80211_ATTR_BSS,
    NL80211_ATTR_IFINDEX,
    NL80211_ATTR_IFNAME,
    NL80211_ATTR_MAC,
    NL80211_ATTR_SCAN_FREQUENCIES,
    NL80211_ATTR_SCAN_SSIDS,
    NL80211_ATTR_SCHED_SCAN_INTERVAL,
    NL80211_ATTR_SCHED_SCAN_MATCH,
    NL80211_ATTR_SCHED_SCAN_PLANS,
    NL80211_ATTR_WIPHY,
    NL80211_BSS_INFORMATION_ELEMENTS,
    NL80211_CMD_GET_INTERFACE,
    NL80211_CMD_GET_SCAN,
    NL80211_CMD_NEW_INTERFACE,
    NL80211_CMD_NEW_SCAN_RESULTS,
    NL80211_CMD_SCHED_SCAN_RESULTS,
    NL80211_CMD_START_SCHED_SCAN,
    NL80211_CMD_STOP_SCHED_SCAN,
    NL80211_CMD_TRIGGER_SCAN,
    NL80211_SCHED_SCAN_MATCH_ATTR_RSSI,
    NL80211_SCHED_SCAN_MATCH_ATTR_SSID,
    NL80211_SCHED_SCAN_PLAN_INTERVAL,
    NL80211_SCHED_SCAN_PLAN_ITERATIONS,
    Nl80211,
    build_nla,
    parse_netlink_attributes,
    verify_new_interface_response,
)

logger = logging.getLogger(__name__)

__all__ = [
    "NL80211_ATTR_BSS",
    "NL80211_ATTR_IFINDEX",
    "NL80211_ATTR_IFNAME",
    "NL80211_ATTR_MAC",
    "NL80211_ATTR_SCAN_FREQUENCIES",
    "NL80211_ATTR_SCAN_SSIDS",
    "NL80211_ATTR_SCHED_SCAN_INTERVAL",
    "NL80211_ATTR_SCHED_SCAN_MATCH",
    "NL80211_ATTR_SCHED_SCAN_PLANS",
    "NL80211_ATTR_WIPHY",
    "NL80211_BSS_INFORMATION_ELEMENTS",
    "NL80211_CMD_GET_INTERFACE",
    "NL80211_CMD_GET_SCAN",
    "NL80211_CMD_NEW_INTERFACE",
    "NL80211_CMD_NEW_SCAN_RESULTS",
    "NL80211_CMD_SCHED_SCAN_RESULTS",
    "NL80211_CMD_START_SCHED_SCAN",
    "NL80211_CMD_STOP_SCHED_SCAN",
    "NL80211_CMD_TRIGGER_SCAN",
    "NL80211_SCHED_SCAN_MATCH_ATTR_RSSI",
    "NL80211_SCHED_SCAN_MATCH_ATTR_SSID",
    "NL80211_SCHED_SCAN_PLAN_INTERVAL",
    "NL80211_SCHED_SCAN_PLAN_ITERATIONS",
    "Nl80211",
    "Nl80211MulticastServer",
    "Nl80211MulticastServerContext",
    "StaIface",
    "SupplicantStaIfaceCallbackContext",
    "SupplicantStaIfaceCallbackServer",
    "WifiChip",
    "WifiEventCallbackContext",
    "WifiEventCallbackServer",
    "Wlanix",
    "build_nla",
    "parse_netlink_attributes",
    "verify_new_interface_response",
]


class StaIface:
    """Represents a STA interface created via WifiChip.CreateStaIface."""

    def __init__(
        self,
        name: str,
        chip: WifiChip,
        wifi_sta_iface: fidl_wlanix.WifiStaIfaceClient,
        supplicant_sta_iface: fidl_wlanix.SupplicantStaIfaceClient | None,
        nl80211: Nl80211 | None,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        self.name = name
        self._chip = chip
        self.wifi_sta_iface = wifi_sta_iface
        self._supplicant_sta_iface = supplicant_sta_iface
        self._nl80211 = nl80211
        self._fc_transport = fuchsia_controller

    @property
    def supplicant_sta_iface(self) -> fidl_wlanix.SupplicantStaIfaceClient:
        if self._supplicant_sta_iface is None:
            self._supplicant_sta_iface = (
                self._chip.wlanix.add_supplicant_sta_iface(self.name)
            )
        return self._supplicant_sta_iface

    @property
    def nl80211(self) -> Nl80211:
        if self._nl80211 is None:
            self._nl80211 = self._chip.wlanix.get_nl80211()
        return self._nl80211

    async def get_name(self) -> str:
        """Queries WifiStaIface.GetName() and returns the interface name."""
        response = (await self.wifi_sta_iface.get_name()).unwrap()
        assert (
            response.iface_name is not None
        ), "WifiStaIface.GetName() response is missing an iface_name value"
        return response.iface_name

    async def query_nl80211_attrs(self) -> dict[int, bytes]:
        """Queries NL80211_CMD_GET_INTERFACE and returns the parsed attributes."""
        return await self.nl80211.query_interface_attrs()

    async def get_iface_index(self) -> int:
        """Queries NL80211_CMD_GET_INTERFACE and returns NL80211_ATTR_IFINDEX."""
        return await self.nl80211.get_iface_index()

    async def trigger_scan(self, timeout: float = 20.0) -> None:
        """Queries the interface index, triggers an nl80211 scan, and waits for results."""
        iface_index = await self.get_iface_index()
        await self.nl80211.trigger_scan(iface_index, timeout=timeout)

    async def connect(
        self,
        ssid: str,
        security: Security,
        password: str | None = None,
        timeout: float = 30.0,
    ) -> None:
        """Connects to the specified network via SupplicantStaIface and waits for completion."""
        client, server = self._fc_transport.channel_create()
        async with SupplicantStaIfaceCallbackServer(client, server) as ctx:
            state_change_queue = ctx.state_change_queue
            callback_channel = ctx.callback_channel

            self.supplicant_sta_iface.register_callback(
                callback=callback_channel.take()
            )

            proxy, net_server = self._fc_transport.channel_create()
            self.supplicant_sta_iface.add_network(network=net_server.take())
            supplicant_sta_network_proxy = (
                fidl_wlanix.SupplicantStaNetworkClient(proxy)
            )

            supplicant_sta_network_proxy.set_ssid(
                ssid=list(ssid.encode("ascii"))
            )
            if password:
                if isinstance(security, SecurityWpa3):
                    supplicant_sta_network_proxy.set_sae_password(
                        password=list(password.encode("ascii"))
                    )
                else:
                    supplicant_sta_network_proxy.set_psk_passphrase(
                        passphrase=list(password.encode("ascii"))
                    )

            try:
                (await supplicant_sta_network_proxy.select()).unwrap()
            except AssertionError as e:
                raise signals.TestFailure(
                    f'Failed to connect to "{ssid}" with {security}'
                ) from e

            try:
                state_change = await asyncio.wait_for(
                    state_change_queue.get(), timeout=timeout
                )
            except TimeoutError as e:
                raise signals.TestFailure(
                    f'Timed out waiting for connection state change for "{ssid}"'
                ) from e

            assert isinstance(
                state_change,
                fidl_wlanix.SupplicantStaIfaceCallbackOnStateChangedRequest,
            ), f"Expected OnStateChanged. Got {state_change!r}"
            assert_equal(
                state_change.new_state,
                fidl_wlanix.StaIfaceCallbackState.COMPLETED,
            )
            assert_true(
                state_change_queue.empty(),
                "Unexpectedly received additional callback messages.",
            )
            logger.info('Successfully connected to "%s"!', ssid)

    async def scan_and_connect(
        self,
        ssid: str,
        security: Security,
        password: str | None = None,
        scan_timeout: float = 20.0,
        connect_timeout: float = 30.0,
    ) -> None:
        """Triggers an nl80211 scan and connects to the specified network via SupplicantStaIface."""
        await self.trigger_scan(timeout=scan_timeout)
        await self.connect(
            ssid=ssid,
            security=security,
            password=password,
            timeout=connect_timeout,
        )

    async def disconnect(self) -> None:
        """Disconnects the supplicant interface."""
        await self.supplicant_sta_iface.disconnect()

    async def remove(self) -> None:
        """Removes this STA interface from its parent WifiChip."""
        await self._chip.remove_sta_iface(self.name)


class WifiChip:
    """Represents a WifiChip managed by wlanix."""

    def __init__(
        self,
        chip_id: int,
        proxy: fidl_wlanix.WifiChipClient,
        wlanix: Wlanix,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        self.id = chip_id
        self.proxy = proxy
        self.wlanix = wlanix
        self._fc_transport = fuchsia_controller

    async def get_sta_iface_names(self) -> list[str]:
        """Returns the list of active STA interface names on this chip."""
        response = (await self.proxy.get_sta_iface_names()).unwrap()
        assert (
            response.iface_names is not None
        ), "WifiChip.GetStaIfaceNames() response is missing an iface_names value"
        return list(response.iface_names)

    async def create_sta_iface(
        self, *, attach_supplicant: bool = True
    ) -> StaIface:
        """Creates a STA interface on this chip and returns a StaIface wrapper."""
        proxy, server = self._fc_transport.channel_create()
        (await self.proxy.create_sta_iface(iface=server.take())).unwrap()
        wifi_sta_iface = fidl_wlanix.WifiStaIfaceClient(proxy)

        get_name_response = (await wifi_sta_iface.get_name()).unwrap()
        assert (
            get_name_response.iface_name is not None
        ), "WifiStaIface.GetName() response is missing an iface_name value"
        iface_name = get_name_response.iface_name

        supplicant_sta_iface: fidl_wlanix.SupplicantStaIfaceClient | None = None
        if attach_supplicant:
            supplicant_sta_iface = self.wlanix.add_supplicant_sta_iface(
                iface_name
            )

        return StaIface(
            name=iface_name,
            chip=self,
            wifi_sta_iface=wifi_sta_iface,
            supplicant_sta_iface=supplicant_sta_iface,
            nl80211=None,
            fuchsia_controller=self._fc_transport,
        )

    async def remove_sta_iface(self, iface_name: str) -> None:
        """Removes the specified STA interface and verifies it is no longer present."""
        (await self.proxy.remove_sta_iface(iface_name=iface_name)).unwrap()
        iface_names = await self.get_sta_iface_names()
        assert_false(
            iface_name in iface_names,
            f"Iface {iface_name} still exists after removal",
        )

    async def destroy_all_sta_ifaces(self) -> None:
        """Removes all active STA interfaces on this chip and verifies none remain."""
        iface_names = await self.get_sta_iface_names()
        for iface_name in iface_names:
            logger.info("Removing STA interface %s", iface_name)
            await self.remove_sta_iface(iface_name)

        remaining = await self.get_sta_iface_names()
        assert_equal(
            len(remaining),
            0,
            f"WifiChip should have no remaining STA ifaces, got {remaining}",
        )

    async def trigger_subsystem_restart_and_wait(
        self, timeout: float = 30.0
    ) -> None:
        """Triggers a subsystem restart and waits for the OnSubsystemRestart callback event."""
        wifi_proxy = self.wlanix.get_wifi()
        client, server = self._fc_transport.channel_create()
        async with WifiEventCallbackServer(client, server) as ctx:
            wifi_proxy.register_event_callback(
                callback=ctx.callback_channel.take()
            )
            # Ensure the event callback registration is fully processed
            # by making a two-way call on the same channel before triggering restart.
            (await wifi_proxy.get_state()).unwrap()

            logger.info("Triggering subsystem restart...")
            (await self.proxy.trigger_subsystem_restart()).unwrap()
            logger.info("Restart triggered.")

            try:
                await asyncio.wait_for(
                    ctx.subsystem_restart_received.wait(),
                    timeout=timeout,
                )
                logger.info("OnSubsystemRestart event received.")
            except TimeoutError as e:
                raise signals.TestFailure(
                    "Timed out waiting for OnSubsystemRestart event"
                ) from e


class Wlanix:
    """Top-level helper for interacting with fuchsia.wlan.wlanix on a FuchsiaDevice."""

    def __init__(self, dut: fuchsia_device.FuchsiaDevice) -> None:
        self._dut = dut
        self._fc_transport = dut.fuchsia_controller
        self.proxy = fidl_wlanix.WlanixClient(
            self._fc_transport.connect_device_proxy(
                FidlEndpoint("core/wlanix", "fuchsia.wlan.wlanix.Wlanix")
            )
        )

    def get_wifi(self) -> fidl_wlanix.WifiClient:
        """Opens a new fuchsia.wlan.wlanix/Wifi channel."""
        proxy, server = self._fc_transport.channel_create()
        self.proxy.get_wifi(wifi=server.take())
        return fidl_wlanix.WifiClient(proxy)

    def get_supplicant(self) -> fidl_wlanix.SupplicantClient:
        """Opens a new fuchsia.wlan.wlanix/Supplicant channel."""
        proxy, server = self._fc_transport.channel_create()
        self.proxy.get_supplicant(supplicant=server.take())
        return fidl_wlanix.SupplicantClient(proxy)

    def add_supplicant_sta_iface(
        self, iface_name: str
    ) -> fidl_wlanix.SupplicantStaIfaceClient:
        """Attaches a SupplicantStaIface for the given interface name."""
        supplicant_proxy = self.get_supplicant()
        proxy, server = self._fc_transport.channel_create()
        supplicant_proxy.add_sta_interface(
            iface=server.take(),
            iface_name=iface_name,
        )
        return fidl_wlanix.SupplicantStaIfaceClient(proxy)

    def get_nl80211(self) -> Nl80211:
        """Opens a new fuchsia.wlan.wlanix/Nl80211 channel wrapped in an Nl80211 helper."""
        proxy, server = self._fc_transport.channel_create()
        self.proxy.get_nl80211(nl80211=server.take())
        return Nl80211(fidl_wlanix.Nl80211Client(proxy), self._fc_transport)

    async def start_wifi(self) -> fidl_wlanix.WifiClient:
        """Opens a Wifi channel, calls Start(), and verifies WiFi is started."""
        wifi_proxy = self.get_wifi()
        (await wifi_proxy.start()).unwrap()
        state_response = (await wifi_proxy.get_state()).unwrap()
        assert (
            state_response.is_started
        ), "WiFi state should be started after calling start()"
        return wifi_proxy

    async def get_chip_ids(self, *, start_wifi: bool = False) -> Sequence[int]:
        """Queries Wifi.GetChipIds() and returns the list of chip IDs."""
        wifi_proxy = await self.start_wifi() if start_wifi else self.get_wifi()
        response = (await wifi_proxy.get_chip_ids()).unwrap()
        assert (
            response.chip_ids is not None
        ), "Wifi.GetChipIds() response is missing a chip_ids value"
        return list(response.chip_ids)

    async def ensure_single_chip(self, *, start_wifi: bool = True) -> WifiChip:
        """Starts WiFi (by default), asserts a single WifiChip exists, and returns it."""
        wifi_proxy = await self.start_wifi() if start_wifi else self.get_wifi()
        response = (await wifi_proxy.get_chip_ids()).unwrap()
        assert (
            response.chip_ids is not None
        ), "Wifi.GetChipIds() response is missing a chip_ids value"
        assert_equal(
            len(response.chip_ids),
            1,
            "Wifi.GetChipIds() should return exactly one chip_id.",
        )

        chip_id = response.chip_ids[0]
        proxy, server = self._fc_transport.channel_create()
        (
            await wifi_proxy.get_chip(chip_id=chip_id, chip=server.take())
        ).unwrap()
        return WifiChip(
            chip_id=chip_id,
            proxy=fidl_wlanix.WifiChipClient(proxy),
            wlanix=self,
            fuchsia_controller=self._fc_transport,
        )
