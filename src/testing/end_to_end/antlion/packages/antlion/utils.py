#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import ipaddress
import time
from typing import TYPE_CHECKING

import fuchsia_async_extension
from libs.proc.runner import CalledProcessError, Runner
from mobly import signals

if TYPE_CHECKING:
    from antlion.controllers.fuchsia_device import FuchsiaDevice
    from libs.ssh.connection import SshConnection

# Default max seconds it takes to Duplicate Address Detection to finish before
# assigning an IPv6 address.
DAD_TIMEOUT_SEC = 30


def get_interface_ip_addresses(
    comm_channel: SshConnection | FuchsiaDevice,
    interface: str,
) -> dict[str, list[str]]:
    """Gets all of the ip addresses, ipv4 and ipv6, associated with a
       particular interface name.

    Args:
        comm_channel: How to send commands to a device.  Can be ssh, etc.
            Must have the run function implemented.
        interface: The interface name on the device, ie eth0

    Returns:
        A list of dictionaries of the the various IP addresses:
            ipv4_private: Any 192.168, 172.16, 10, or 169.254 addresses
            ipv4_public: Any IPv4 public addresses
            ipv6_link_local: Any fe80:: addresses
            ipv6_private_local: Any fd00:: addresses
            ipv6_public: Any publicly routable addresses
    """
    # Local imports are used here to prevent cyclic dependency.
    from antlion.controllers.fuchsia_device import FuchsiaDevice
    from libs.ssh.connection import SshConnection

    addrs: list[str] = []

    if isinstance(comm_channel, SshConnection):
        ip = comm_channel.run(["ip", "-o", "addr", "show", interface])
        addrs = [
            addr.replace("/", " ").split()[3]
            for addr in ip.stdout.decode("utf-8").splitlines()
        ]
    elif isinstance(comm_channel, FuchsiaDevice):
        for iface in fuchsia_async_extension.get_loop().run_until_complete(
            comm_channel.honeydew_fd.netstack.list_interfaces()
        ):
            if iface.name != interface:
                continue
            for ipv4_address in iface.ipv4_addresses:
                addrs.append(str(ipv4_address))
            for ipv6_address in iface.ipv6_addresses:
                addrs.append(str(ipv6_address))
    else:
        raise ValueError("Unsupported method to send command to device.")

    ipv4_private_addresses = []
    ipv4_public_addresses = []
    ipv6_link_local_addresses = []
    ipv6_private_local_addresses = []
    ipv6_public_addresses = []

    for addr in addrs:
        on_device_ip = ipaddress.ip_address(addr)
        if on_device_ip.version == 4:
            if on_device_ip.is_private:
                ipv4_private_addresses.append(str(on_device_ip))
            elif on_device_ip.is_global or (
                # Carrier private doesn't have a property, so we check if
                # all other values are left unset.
                not on_device_ip.is_reserved
                and not on_device_ip.is_unspecified
                and not on_device_ip.is_link_local
                and not on_device_ip.is_loopback
                and not on_device_ip.is_multicast
            ):
                ipv4_public_addresses.append(str(on_device_ip))
        elif on_device_ip.version == 6:
            if on_device_ip.is_link_local:
                ipv6_link_local_addresses.append(str(on_device_ip))
            elif on_device_ip.is_private:
                ipv6_private_local_addresses.append(str(on_device_ip))
            elif on_device_ip.is_global:
                ipv6_public_addresses.append(str(on_device_ip))

    return {
        "ipv4_private": ipv4_private_addresses,
        "ipv4_public": ipv4_public_addresses,
        "ipv6_link_local": ipv6_link_local_addresses,
        "ipv6_private_local": ipv6_private_local_addresses,
        "ipv6_public": ipv6_public_addresses,
    }


class AddressTimeout(signals.TestError):
    pass


class MultipleAddresses(signals.TestError):
    pass


def get_addr(
    comm_channel: SshConnection | FuchsiaDevice,
    interface: str,
    addr_type: str = "ipv4_private",
    timeout_sec: int | None = None,
) -> str:
    """Get the requested type of IP address for an interface; if an address is
    not available, retry until the timeout has been reached.

    Args:
        addr_type: Type of address to get as defined by the return value of
            utils.get_interface_ip_addresses.
        timeout_sec: Seconds to wait to acquire an address if there isn't one
            already available. If fetching an IPv4 address, the default is 3
            seconds. If IPv6, the default is 30 seconds for Duplicate Address
            Detection.

    Returns:
        A string containing the requested address.

    Raises:
        TestAbortClass: timeout_sec is None and invalid addr_type
        AddressTimeout: No address is available after timeout_sec
        MultipleAddresses: Several addresses are available
    """
    if not timeout_sec:
        if "ipv4" in addr_type:
            timeout_sec = 3
        elif "ipv6" in addr_type:
            timeout_sec = DAD_TIMEOUT_SEC
        else:
            raise signals.TestAbortClass(f'Unknown addr_type "{addr_type}"')

    timeout = time.time() + timeout_sec
    while time.time() < timeout:
        ip_addrs = get_interface_ip_addresses(comm_channel, interface)[
            addr_type
        ]
        if len(ip_addrs) > 1:
            raise MultipleAddresses(
                f'Expected only one "{addr_type}" address, got {ip_addrs}'
            )
        elif len(ip_addrs) == 1:
            return ip_addrs[0]

    raise AddressTimeout(
        f'No available "{addr_type}" address after {timeout_sec}s'
    )


def get_interface_based_on_ip(runner: Runner, desired_ip_address: str) -> str:
    """Gets the interface for a particular IP

    Args:
        comm_channel: How to send commands to a device.  Can be ssh, adb serial,
            etc.  Must have the run function implemented.
        desired_ip_address: The IP address that is being looked for on a device.

    Returns:
        The name of the test interface.

    Raises:
        RuntimeError: when desired_ip_address is not found
    """

    desired_ip_address = desired_ip_address.split("%", 1)[0]
    ip = runner.run(["ip", "-o", "addr", "show"])
    for line in ip.stdout.decode("utf-8").splitlines():
        if desired_ip_address in line:
            return line.split()[1]
    raise RuntimeError(
        f'IP "{desired_ip_address}" not found in list:\n{ip.stdout.decode("utf-8")}'
    )


def renew_linux_ip_address(runner: Runner, interface: str) -> None:
    # Ensure the interface is not empty, so we don't accidentally target _all_ interfaces.
    if not interface:
        raise ValueError("Interface name must not be empty.")

    # If dhcpcd is managing this interface (common on Raspberry Pi OS), release
    # its lease so it stops managing the interface and does not race with dhclient.
    try:
        runner.run(f"sudo dhcpcd -k {interface}")
    except CalledProcessError:
        # dhcpcd may not be installed or running on this device.
        pass

    # Ensure interface is UP.
    runner.run(f"sudo ip link set {interface} up")
    # Release any existing lease on the DHCP server and stop the running
    # dhclient daemon to prevent multiple competing daemons.
    try:
        runner.run(f"sudo dhclient -r {interface}")
    except CalledProcessError:
        # No prior dhclient process or lease to release.
        pass
    # Flush existing IPv4 addresses to ensure stale leases from previous networks
    # are removed, preventing MultipleAddresses errors when a new lease is assigned.
    runner.run(f"sudo ip -4 addr flush dev {interface}")
    # Negotiate a fresh lease for the current network scope and start a new daemon.
    runner.run(f"sudo dhclient {interface}")
