# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""WLAN policy affordance implementation using Fuchsia Controller."""

from __future__ import annotations

import logging

import fidl_fuchsia_net_interfaces as f_net_interfaces
import fidl_fuchsia_net_root as f_net_root
import fuchsia_async_extension
from fuchsia_controller_py import FcTransportStatus, ZxStatus

from honeydew import affordances_capable, errors
from honeydew.affordances.connectivity.netstack import netstack
from honeydew.affordances.connectivity.netstack.errors import (
    HoneydewNetstackError,
)
from honeydew.affordances.connectivity.netstack.types import InterfaceProperties
from honeydew.affordances.connectivity.wlan.utils.types import MacAddress
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing.custom_types import FidlEndpoint

# List of required FIDLs for this affordance.
_REQUIRED_CAPABILITIES = [
    "fuchsia.net.interfaces.State",
]

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Fuchsia Controller proxies
_STATE_PROXY = FidlEndpoint(
    "core/network/netstack", "fuchsia.net.interfaces.State"
)
_INTERFACES_PROXY = FidlEndpoint(
    "core/network/netstack", "fuchsia.net.root.Interfaces"
)


class AsyncNetstackUsingFc(netstack.AsyncNetstack):
    """Async netstack affordance implemented with Fuchsia Controller."""

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.AsyncRebootCapableDevice,
    ) -> None:
        """Create an Async Netstack Fuchsia Controller affordance.

        Args:
            device_name: Device name returned by `ffx target list`.
            ffx: FFX transport.
            fuchsia_controller: Fuchsia Controller transport.
            reboot_affordance: Object that implements AsyncRebootCapableDevice.
        """
        super().__init__()

        self._fc_transport = fuchsia_controller
        self._reboot_affordance = reboot_affordance
        self.device = device_name
        self.ffx = ffx

        self.verify_supported()

        self._connect_proxy()
        self._reboot_affordance.register_for_on_device_boot(self._connect_proxy)

    def verify_supported(self) -> None:
        """Check if Netstack is supported on the DUT.

        Raises:
            NotSupportedError: Netstack affordance is not supported by Fuchsia device.
        """
        for capability in _REQUIRED_CAPABILITIES:
            # TODO(http://b/359342196): This is a maintenance burden; find a
            # better way to detect FIDL component capabilities.
            if capability not in self.ffx.run(
                ["component", "capability", capability],
                # TODO(b/474143046) update to JSON when ffx supports it
                machine=ffx_types.MachineFormat.RAW,
            ):
                _LOGGER.warning(
                    "All available netstack component capabilities:\n%s",
                    self.ffx.run(
                        ["component", "capability", "fuchsia.net"],
                        # TODO(b/474143046) update to JSON when ffx supports it
                        machine=ffx_types.MachineFormat.RAW,
                    ),
                )
                raise errors.NotSupportedError(
                    f'Component capability "{capability}" not exposed by device '
                    f"{self.device}; this build of Fuchsia does not support the "
                    "Netstack FC affordance."
                )

    def _connect_proxy(self) -> None:
        """Re-initializes connection to the Netstack."""
        self._state_proxy = f_net_interfaces.StateClient(
            self._fc_transport.connect_device_proxy(_STATE_PROXY)
        )
        self._interfaces_proxy = f_net_root.InterfacesClient(
            self._fc_transport.connect_device_proxy(_INTERFACES_PROXY)
        )

    async def list_interfaces(self) -> list[InterfaceProperties]:
        """List interfaces.

        Returns:
            Information on all interfaces on the device.

        Raises:
            HoneydewNetstackError: Error from the netstack.
            TypeError: Received invalid Watcher events from netstack.
        """
        client, server = self._fc_transport.channel_create()
        watcher = f_net_interfaces.WatcherClient(client.take())

        try:
            self._state_proxy.get_watcher(
                options=f_net_interfaces.WatcherOptions(
                    address_properties_interest=None,
                    # When an IP address is undergoing DAD, it cannot yet be used.
                    # Ask netstack to avoid reporting the address until it is
                    # actually ready to use.
                    include_non_assigned_addresses=False,
                ),
                watcher=server.take(),
            )
        except FcTransportStatus as status:
            raise HoneydewNetstackError(
                f"State.GetWatcher() error {status}"
            ) from status

        properties: list[InterfaceProperties] = []

        while True:
            try:
                resp = await watcher.watch()
            except FcTransportStatus as status:
                raise HoneydewNetstackError(
                    f"Watcher.Watch() error {status}"
                ) from status

            event = resp.event
            if event.existing:
                assert (
                    event.existing.id_ is not None
                ), f"{event.existing!r} missing id"
                try:
                    try:
                        get_mac_response = (
                            await self._interfaces_proxy.get_mac(
                                id_=event.existing.id_
                            )
                        ).unwrap()
                    except AssertionError:
                        _LOGGER.debug(
                            'Failed to find the MAC of interface "%s" (%s); '
                            "it no longer exists",
                            event.existing.name,
                            event.existing.id_,
                        )
                        continue  # this is fine and sometimes even expected

                    mac = (
                        MacAddress.from_bytes(
                            bytes(get_mac_response.mac.octets)
                        )
                        if get_mac_response.mac
                        else None
                    )
                except (FcTransportStatus, ZxStatus) as status:
                    raise HoneydewNetstackError(
                        f"Interfaces.GetMac() error {status}"
                    ) from status

                properties.append(
                    InterfaceProperties.from_fidl(event.existing, mac)
                )
            elif event.idle:
                # No more information readily available.
                break
            else:
                raise HoneydewNetstackError(
                    "Received invalid Watcher event from netstack. "
                    f"Expected existing or idle events, got {event}"
                )

        return properties


class NetstackUsingFc(netstack.Netstack):
    """Netstack affordance implemented with Fuchsia Controller."""

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.RebootCapableDevice,
    ) -> None:
        """Create a Netstack Fuchsia Controller affordance.

        Args:
            device_name: Device name returned by `ffx target list`.
            ffx: FFX transport.
            fuchsia_controller: Fuchsia Controller transport.
            reboot_affordance: Object that implements RebootCapableDevice.
        """
        self._inner = AsyncNetstackUsingFc(
            device_name=device_name,
            ffx=ffx,
            fuchsia_controller=fuchsia_controller,
            reboot_affordance=reboot_affordance.as_async(),
        )

    def verify_supported(self) -> None:
        """Check if Netstack is supported on the DUT.

        Raises:
            NotSupportedError: Netstack affordance is not supported by Fuchsia device.
        """
        self._inner.verify_supported()

    def list_interfaces(self) -> list[InterfaceProperties]:
        """List interfaces.

        Returns:
            Information on all interfaces on the device.

        Raises:
            HoneydewNetstackError: Error from the netstack.
        """
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.list_interfaces()
        )

    def as_async(self) -> AsyncNetstackUsingFc:
        """Returns the async version of Netstack."""
        return self._inner
