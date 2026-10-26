#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import ipaddress
import time
from enum import StrEnum

from libs.ssh.connection import SshConnection

# Default max seconds it takes to Duplicate Address Detection to finish before
# assigning an IPv6 address.
DAD_TIMEOUT_SEC = 30


class AddrType(StrEnum):
    ipv4_private = "ipv4_private"
    """Any 192.168, 172.16, 10, or 169.254 addresses"""
    ipv4_public = "ipv4_public"
    """Any IPv4 public addresses"""
    ipv6_link_local = "ipv6_link_local"
    """Any fe80:: addresses"""
    ipv6_private_local = "ipv6_private_local"
    """Any fd00:: addresses"""
    ipv6_public = "ipv6_public"
    """Any publicly routable addresses"""


def get_interface_ip_addresses(
    comm_channel: SshConnection,
    interface: str,
) -> dict[AddrType, list[str]]:
    """Gets all of the ip addresses, ipv4 and ipv6, associated with a
       particular interface name.

    Args:
        comm_channel: How to send commands to a device. Must be an SshConnection.
        interface: The interface name on the device, ie eth0

    Returns:
        A dictionary of lists of the various IP addresses:
            ipv4_private: Any 192.168, 172.16, 10, or 169.254 addresses
            ipv4_public: Any IPv4 public addresses
            ipv6_link_local: Any fe80:: addresses
            ipv6_private_local: Any fd00:: addresses
            ipv6_public: Any publicly routable addresses
    """
    ip = comm_channel.run(["ip", "-o", "addr", "show", interface])
    addrs = [
        addr.replace("/", " ").split()[3]
        for addr in ip.stdout.decode("utf-8").splitlines()
        if len(addr.split()) > 3
    ]

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
        AddrType.ipv4_private: ipv4_private_addresses,
        AddrType.ipv4_public: ipv4_public_addresses,
        AddrType.ipv6_link_local: ipv6_link_local_addresses,
        AddrType.ipv6_private_local: ipv6_private_local_addresses,
        AddrType.ipv6_public: ipv6_public_addresses,
    }


class AddressTimeout(TimeoutError):
    pass


class MultipleAddresses(ValueError):
    pass


def get_addr(
    comm_channel: SshConnection,
    interface: str,
    addr_type: AddrType = AddrType.ipv4_private,
    timeout_sec: int | None = None,
) -> str:
    """Get the requested type of IP address for an interface; if an address is
    not available, retry until the timeout has been reached.

    Args:
        comm_channel: How to send commands to a device.
        interface: The interface name on the device, ie eth0.
        addr_type: Type of address to get as defined by the return value of
            get_interface_ip_addresses.
        timeout_sec: Seconds to wait to acquire an address if there isn't one
            already available. If fetching an IPv4 address, the default is 3
            seconds. If IPv6, the default is 30 seconds for Duplicate Address
            Detection.

    Returns:
        A string containing the requested address.

    Raises:
        ValueError: Invalid addr_type
        AddressTimeout: No address is available after timeout_sec
        MultipleAddresses: Several addresses are available
    """
    if not timeout_sec:
        if "ipv4" in addr_type:
            timeout_sec = 3
        elif "ipv6" in addr_type:
            timeout_sec = DAD_TIMEOUT_SEC
        else:
            raise ValueError(f'Unknown addr_type "{addr_type}"')

    timeout = time.time() + timeout_sec
    while time.time() < timeout:
        addrs_dict = get_interface_ip_addresses(comm_channel, interface)
        if addr_type not in addrs_dict:
            raise ValueError(f'Unknown addr_type "{addr_type}"')
        ip_addrs = addrs_dict[addr_type]
        if len(ip_addrs) > 1:
            raise MultipleAddresses(
                f'Expected only one "{addr_type}" address, got {ip_addrs}'
            )
        elif len(ip_addrs) == 1:
            return ip_addrs[0]

    raise AddressTimeout(
        f'No available "{addr_type}" address after {timeout_sec}s'
    )
