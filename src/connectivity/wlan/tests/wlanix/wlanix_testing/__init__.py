# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Domain wrapper classes and testing utilities for wlanix E2E tests."""

from __future__ import annotations

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
    "Nl80211MulticastServer",
    "Nl80211MulticastServerContext",
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
