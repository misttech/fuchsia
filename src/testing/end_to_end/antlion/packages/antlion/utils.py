#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import ipaddress
import logging
import platform
import random
import re
import socket
import string
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import TYPE_CHECKING

import fuchsia_async_extension
from libs.proc import job
from libs.proc.runner import CalledProcessError, Runner
from mobly import signals

if TYPE_CHECKING:
    from antlion.controllers.fuchsia_device import FuchsiaDevice
    from libs.ssh.connection import SshConnection

# All Fuchsia devices use this suffix for link-local mDNS host names.
FUCHSIA_MDNS_TYPE = "_fuchsia._udp.local."

# Default max seconds it takes to Duplicate Address Detection to finish before
# assigning an IPv6 address.
DAD_TIMEOUT_SEC = 30


def rand_hex_str(length: int) -> str:
    """Generates a random string of specified length, composed of hex digits

    Args:
        length: The number of characters in the string.

    Returns:
        The random string generated.
    """
    letters = [random.choice(string.hexdigits) for i in range(length)]
    return "".join(letters)


def is_valid_ipv4_address(address: str) -> bool:
    try:
        socket.inet_pton(socket.AF_INET, address)
    except AttributeError:  # no inet_pton here, sorry
        try:
            socket.inet_aton(address)
        except socket.error:
            return False
        return address.count(".") == 3
    except socket.error:  # not a valid address
        return False

    return True


def is_valid_ipv6_address(address: str) -> bool:
    if "%" in address:
        address = address.split("%")[0]
    try:
        socket.inet_pton(socket.AF_INET6, address)
    except socket.error:  # not a valid address
        return False
    return True


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


def get_ping_command(
    dest_ip: str,
    count: int = 3,
    interval: int = 1000,
    timeout: int = 1000,
    size: int = 56,
    os_type: str = "Linux",
    additional_ping_params: str = "",
) -> str:
    """Builds ping command string based on address type, os, and params.

    Args:
        dest_ip: string, address to ping (ipv4 or ipv6)
        count: int, number of requests to send
        interval: int, time in seconds between requests
        timeout: int, time in seconds to wait for response
        size: int, number of bytes to send,
        os_type: string, os type of the source device (supports 'Linux',
            'Darwin')
        additional_ping_params: string, command option flags to
            append to the command string

    Returns:
        The ping command.
    """
    if is_valid_ipv4_address(dest_ip):
        ping_binary = "ping"
    elif is_valid_ipv6_address(dest_ip):
        ping_binary = "ping6"
    else:
        raise ValueError(f"Invalid ip addr: {dest_ip}")

    if os_type == "Darwin":
        if is_valid_ipv6_address(dest_ip):
            # ping6 on MacOS doesn't support timeout
            logging.debug(
                "Ignoring timeout, as ping6 on MacOS does not support it."
            )
            timeout_flag = []
        else:
            timeout_flag = ["-t", str(timeout / 1000)]
    elif os_type == "Linux":
        timeout_flag = ["-W", str(timeout / 1000)]
    else:
        raise ValueError("Invalid OS.  Only Linux and MacOS are supported.")

    ping_cmd = [
        ping_binary,
        *timeout_flag,
        "-c",
        str(count),
        "-i",
        str(interval / 1000),
        "-s",
        str(size),
        additional_ping_params,
        dest_ip,
    ]
    return " ".join(ping_cmd)


def ping(
    comm_channel: Runner,
    dest_ip: str,
    count: int = 3,
    interval: int = 1000,
    timeout: int = 1000,
    size: int = 56,
    additional_ping_params: str = "",
) -> PingResult:
    """Generic linux ping function, supports local (acts.libs.proc.job) and
    SshConnections (acts.libs.proc.job over ssh) to Linux based OSs and MacOS.

    NOTES: This will work with Android over SSH, but does not function over ADB
    as that has a unique return format.

    Args:
        comm_channel: communication channel over which to send ping command.
            Must have 'run' function that returns at least command, stdout,
            stderr, and exit_status (see acts.libs.proc.job)
        dest_ip: address to ping (ipv4 or ipv6)
        count: int, number of packets to send
        interval: int, time in milliseconds between pings
        timeout: int, time in milliseconds to wait for response
        size: int, size of packets in bytes
        additional_ping_params: string, command option flags to
            append to the command string

    Returns:
        Dict containing:
            command: string
            exit_status: int (0 or 1)
            stdout: string
            stderr: string
            transmitted: int, number of packets transmitted
            received: int, number of packets received
            packet_loss: int, percentage packet loss
            time: int, time of ping command execution (in milliseconds)
            rtt_min: float, minimum round trip time
            rtt_avg: float, average round trip time
            rtt_max: float, maximum round trip time
            rtt_mdev: float, round trip time standard deviation

        Any values that cannot be parsed are left as None
    """
    from libs.ssh.connection import SshConnection

    is_local = comm_channel == job  # type: ignore # Blanket ignore to enable mypy
    os_type = platform.system() if is_local else "Linux"
    ping_cmd = get_ping_command(
        dest_ip,
        count=count,
        interval=interval,
        timeout=timeout,
        size=size,
        os_type=os_type,
        additional_ping_params=additional_ping_params,
    )

    if isinstance(comm_channel, SshConnection) or is_local:
        logging.debug(
            "Running ping with parameters (count: %s, interval: %s, "
            "timeout: %s, size: %s)",
            count,
            interval,
            timeout,
            size,
        )
        try:
            ping_result: (
                subprocess.CompletedProcess[bytes] | CalledProcessError
            ) = comm_channel.run(ping_cmd)
        except CalledProcessError as e:
            ping_result = e
    else:
        raise ValueError(f"Unsupported comm_channel: {type(comm_channel)}")

    stdout = ping_result.stdout.decode("utf-8")
    stderr = ping_result.stderr.decode("utf-8")
    raw_output = stdout
    if stderr:
        raw_output += "\n" + stderr

    summary = re.search(
        "([0-9]+) packets transmitted.*?([0-9]+) received.*?([0-9]+)% packet "
        "loss.*?time ([0-9]+)",
        stdout,
    )
    rtt_stats = re.search(
        "= ([0-9.]+)/([0-9.]+)/([0-9.]+)/([0-9.]+)",
        stdout,
    )
    return PingResult(
        raw_output=raw_output,
        success=ping_result.returncode == 0,
        requested=count,
        transmitted=int(summary.group(1)) if summary else 0,
        received=int(summary.group(2)) if summary else 0,
        time_ms=float(summary.group(4)) / 1000 if summary else None,
        rtt_min_ms=float(rtt_stats.group(1)) if rtt_stats else None,
        rtt_avg_ms=float(rtt_stats.group(2)) if rtt_stats else None,
        rtt_max_ms=float(rtt_stats.group(3)) if rtt_stats else None,
        rtt_mdev_ms=float(rtt_stats.group(4)) if rtt_stats else None,
    )


@dataclass
class PingResult:
    raw_output: str
    success: bool
    requested: int
    transmitted: int
    received: int
    time_ms: float | None
    rtt_min_ms: float | None
    rtt_avg_ms: float | None
    rtt_max_ms: float | None
    rtt_mdev_ms: float | None

    @property
    def any_pings_received(self) -> bool:
        """True if at least one ping was received."""
        return self.received > 0

    @property
    def all_pings_received(self) -> bool:
        """True if all requested pings were received."""
        return (
            self.received == self.requested
            and self.transmitted == self.requested
        )


def ip_in_subnet(ip: str, subnet: str) -> bool:
    """Validate that ip is in a given subnet.

    Args:
        ip: string, ip address to verify (eg. '192.168.42.158')
        subnet: string, subnet to check (eg. '192.168.42.0/24')

    Returns:
        True, if ip in subnet, else False
    """
    return ipaddress.ip_address(ip) in ipaddress.ip_network(subnet)


def get_fuchsia_mdns_ipv6_address(device_mdns_name: str) -> None | str:
    """Finds the IPv6 link-local address of a Fuchsia device matching a mDNS
    name.

    Args:
        device_mdns_name: name of Fuchsia device (e.g. gig-clone-sugar-slash)

    Returns:
        string, IPv6 link-local address
    """
    import psutil
    from zeroconf import IPVersion, Zeroconf

    if not device_mdns_name:
        return None

    def mdns_query(interface: str, address: str) -> None | str:
        logging.info(
            f'Sending mDNS query for device "{device_mdns_name}" using "{address}"'
        )
        try:
            zeroconf = Zeroconf(
                ip_version=IPVersion.V6Only, interfaces=[address]
            )
        except RuntimeError as e:
            if "No adapter found for IP address" in e.args[0]:
                # Most likely, a device went offline and its control
                # interface was deleted. This is acceptable since the
                # device that went offline isn't guaranteed to be the
                # device we're searching for.
                logging.warning(f'No adapter found for "{address}"')
                return None
            raise

        device_records = zeroconf.get_service_info(
            FUCHSIA_MDNS_TYPE, f"{device_mdns_name}.{FUCHSIA_MDNS_TYPE}"
        )

        if device_records:
            for device_address in device_records.parsed_addresses():
                device_ip_address = ipaddress.ip_address(device_address)
                scoped_address = f"{device_address}%{interface}"
                if (
                    device_ip_address.version == 6
                    and device_ip_address.is_link_local
                    and ping(job, dest_ip=scoped_address).success  # type: ignore # Blanket ignore to enable mypy
                ):
                    logging.info(
                        f'Found device "{device_mdns_name}" at "{scoped_address}"'
                    )
                    zeroconf.close()
                    del zeroconf
                    return scoped_address

        zeroconf.close()
        del zeroconf
        return None

    with ThreadPoolExecutor() as executor:
        futures = []

        interfaces = psutil.net_if_addrs()
        for interface in interfaces:
            for addr in interfaces[interface]:
                address = addr.address.split("%")[0]
                if (
                    addr.family == socket.AF_INET6
                    and ipaddress.ip_address(address).is_link_local
                    and address != "fe80::1"
                ):
                    futures.append(
                        executor.submit(mdns_query, interface, address)
                    )

        for future in futures:
            addr = future.result()
            if addr:
                return addr

    logging.error(f'Unable to find IP address for device "{device_mdns_name}"')
    return None
