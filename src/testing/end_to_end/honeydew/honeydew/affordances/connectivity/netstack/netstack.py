# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for netstack affordance."""

import abc

from honeydew.affordances import affordance
from honeydew.affordances.connectivity.netstack.types import (
    InterfaceProperties,
    PingResult,
)


class AsyncNetstack(abc.ABC):
    """Abstract base class for an async Netstack affordance."""

    @abc.abstractmethod
    async def list_interfaces(self) -> list[InterfaceProperties]:
        """List interfaces.

        Returns:
            Information on all interfaces on the device.

        Raises:
            HoneydewNetstackError: Error from the netstack.
        """

    @abc.abstractmethod
    async def ping(
        self,
        dest_ip: str,
        *,
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
        additional_ping_params: str | None = None,
    ) -> PingResult:
        """Send ICMP echo requests to a destination.

        Args:
            dest_ip: Destination IP address or hostname.
            count: Number of packets to send.
            interval: Interval between packets in milliseconds.
            timeout: Timeout for each packet in milliseconds.
            size: Packet size in bytes.
            additional_ping_params: Additional parameters to pass to the ping command.

        Returns:
            Result of the ping operation.

        Raises:
            HoneydewNetstackError: Error executing ping.
        """


class Netstack(affordance.Affordance):
    """Abstract base class for Netstack affordance."""

    # List all the public methods
    @abc.abstractmethod
    def list_interfaces(self) -> list[InterfaceProperties]:
        """List interfaces.

        Returns:
            Information on all interfaces on the device.

        Raises:
            HoneydewNetstackError: Error from the netstack.
        """

    @abc.abstractmethod
    def ping(
        self,
        dest_ip: str,
        *,
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
        additional_ping_params: str | None = None,
    ) -> PingResult:
        """Send ICMP echo requests to a destination.

        Args:
            dest_ip: Destination IP address or hostname.
            count: Number of packets to send.
            interval: Interval between packets in milliseconds.
            timeout: Timeout for each packet in milliseconds.
            size: Packet size in bytes.
            additional_ping_params: Additional parameters to pass to the ping command.

        Returns:
            Result of the ping operation.

        Raises:
            HoneydewNetstackError: Error executing ping.
        """
