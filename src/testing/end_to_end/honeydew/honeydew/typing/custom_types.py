# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Custom data types."""

import abc
import builtins
import enum
import ipaddress
import logging
import random
from dataclasses import dataclass
from typing import TypeVar

_LOGGER: logging.Logger = logging.getLogger(__name__)

AnyString = TypeVar("AnyString", str, bytes)


class LEVEL(enum.StrEnum):
    """Logging level that need to specified to log a message onto device"""

    INFO = "Info"
    WARNING = "Warning"
    ERROR = "Error"


class TargetAddr(abc.ABC):
    """Abstract base class representing a generic Fuchsia Target Address."""


@dataclass(frozen=True)
class UsbAddress(TargetAddr):
    """Dataclass that holds a USB Target Address

    Args:
        target_id: USB Target ID
    """

    target_id: int

    def __post_init__(self) -> None:
        """Validates target_id arg.

        Raises:
            ValueError
        """
        if self.target_id < 0:
            raise ValueError(f"target_id: {self.target_id} was negative")

    def __str__(self) -> str:
        return f"usb:cid:{self.target_id}"

    @staticmethod
    def create_using_usb(usb: str | int) -> UsbAddress:
        """Factory method to create UsbAddress object using USB target ID or address string.

        Args:
            usb: USB address or target ID of the fuchsia device. This is of
                 one of the following formats:
                    usb:cid:{target_id}
                    {target_id}

        Returns:
            A valid UsbAddress

        Raises:
            ValueError
        """
        if isinstance(usb, bool):
            raise ValueError(f"Value: {usb} was not a valid UsbAddress")
        if isinstance(usb, str):
            usb_str = usb.strip()
            if usb_str.startswith("usb:cid:"):
                usb_str = usb_str.removeprefix("usb:cid:")
            return UsbAddress(int(usb_str))
        if isinstance(usb, int):
            return UsbAddress(usb)
        raise ValueError(f"Value: {usb} was not a valid UsbAddress")


@dataclass(frozen=True)
class IpPort(TargetAddr):
    """Dataclass that holds IP Address and Port

    Args:
        ip: Ip Address
        port: Port Number
    """

    ip: ipaddress.IPv4Address | ipaddress.IPv6Address
    port: int | None

    def __post_init__(self) -> None:
        """Validates ip and port args.

        Raises:
            ValueError
        """
        if self.port is not None and self.port < 1:
            raise ValueError(
                f"port number: {self.port} was not a positive integer"
            )

    def __str__(self) -> str:
        host: str = f"{self.ip}"
        if isinstance(self.ip, ipaddress.IPv6Address):
            host = f"[{host}]"
        if self.port:
            return f"{host}:{self.port}"
        else:
            return f"{host}"

    @staticmethod
    def create_using_ip_and_port(ip_port: str) -> IpPort:
        """Factory method to create IpPort object using str that has both ip
        and port values.

        Args:
            ip_port: IP address and port of the fuchsia device. This is of
                     one the following formats:
                        {ipv4_address}:{port}
                        [{ipv6_address}]:{port}
                        {ipv6_address}:{port}

        Returns:
            A valid IpPort

        Raises:
          ValueError
        """
        try:
            # If we have something of form
            #     192.168.1.1:8888 ==> ["192.168.1.1", "8888"]
            # If we have something of form
            #     [::1]:8888 ==> ["[::1]", "8888"]
            arr: list[str] = ip_port.rsplit(":", 1)
            if len(arr) != 1 and len(arr) != 2:
                raise ValueError(
                    f"Value: {ip_port} was not a valid IpPort (needs "
                    f"IP Address and optional Port)"
                )
            addr_part: str = arr[0]
            # Remove [] that might be surrounding an IPv6 address
            if addr_part.startswith("[") and addr_part.endswith("]"):
                addr_part = addr_part[1:-1]

            port = None
            if len(arr) == 2:
                port_part: str = arr[1]
                port = int(port_part)
                if port < 1:
                    raise ValueError(
                        f"For IpPort: {ip_port}, port number: {port} was "
                        f"not a positive integer)"
                    )

            return IpPort(ipaddress.ip_address(addr_part), port)
        except ValueError as e:
            raise e

    @staticmethod
    def create_using_ip(ip: str) -> IpPort:
        """Factory method to create IpPort object using str that has ip address.

        Args:
            ip: IP address and port of the fuchsia device. This is of
                     one the following formats:
                        {ipv4_address}
                        [{ipv6_address}]
                        {ipv6_address}

        Returns:
            A valid IpPort

        Raises:
          ValueError
        """
        try:
            # Remove [] that might be surrounding an IPv6 address
            if ip.startswith("[") and ip.endswith("]"):
                ip = ip[1:-1]
            return IpPort(ipaddress.ip_address(ip), None)
        except ValueError as e:
            raise e

    @property
    def ip_str(self) -> str:
        """The IP address as a string."""
        return str(self.ip)


# TODO: Remove TargetSshAddress once //vendor/google code is updated to use
# get_target_address() and IpPort.
@dataclass(frozen=True)
class TargetSshAddress(IpPort):
    """Dataclass that holds target's ssh address information.

    Args:
        ip: Target's SSH IP Address
        port: Target's SSH port
    """


@dataclass(frozen=True)
class DeviceInfo:
    """Dataclass that holds Fuchsia device information.

    Args:
        name: Device name returned by `ffx target list`.
        serial_number: Device serial number.
        address: Target address of the device (IP or USB).
        serial_socket: Device serial socket path.
        fastboot_node_id: Fastboot node ID.
        ip_port: Deprecated. Use `address` instead.
    """

    name: str
    serial_number: str | None = None
    address: TargetAddr | None = None
    serial_socket: str | None = None
    fastboot_node_id: str | None = None
    # TODO: Remove ip_port once //vendor/google code is updated to use address.
    ip_port: IpPort | None = None

    def __post_init__(self) -> None:
        if self.address is None and self.ip_port is not None:
            object.__setattr__(self, "address", self.ip_port)
        elif self.ip_port is None and isinstance(self.address, IpPort):
            object.__setattr__(self, "ip_port", self.address)

        for attr in ("serial_number", "serial_socket", "fastboot_node_id"):
            val = getattr(self, attr)
            if isinstance(val, str):
                cleaned = val.strip()
                if "\n" in cleaned:
                    raise ValueError(
                        f"Multi-line value {val!r} is not allowed for DeviceInfo.{attr}"
                    )
                if not cleaned or cleaned.lower() in ("unknown", "<unknown>"):
                    _LOGGER.debug(
                        "Invalid value %r provided for DeviceInfo.%s; setting to None.",
                        val,
                        attr,
                    )
                    object.__setattr__(self, attr, None)
                else:
                    object.__setattr__(self, attr, cleaned)

    def __str__(self) -> str:
        return (
            f"name={self.name}, "
            f"address={self.address}, "
            f"serial_socket={self.serial_socket}, "
            f"fastboot_node_id={self.fastboot_node_id}"
        )


@dataclass(frozen=True)
class FidlEndpoint:
    """Dataclass that holds FIDL end point information.

    Args:
        moniker: moniker pointing to the FIDL end point
        protocol: protocol name of the FIDL end point
    """

    moniker: str
    protocol: str


class MacAddress:
    """MAC address following the EUI-48 identifier format.

    Used by IEEE 802 networks as unique identifiers assigned to network
    interface controllers.
    """

    _mac: bytes

    def __init__(self, mac: str | bytes | bytearray) -> None:
        if isinstance(mac, str):
            mac = bytes([int(a, 16) for a in mac.split(":")])

        if len(mac) != 6:
            raise ValueError(f"Expected 6 bytes, got {len(mac)}")

        self._mac = bytes(mac)

    @classmethod
    def random(cls) -> MacAddress:
        """Create a random MAC address."""
        return cls(random.randbytes(6))

    def __bytes__(self) -> builtins.bytes:
        return self._mac

    def with_unicast_bit(self) -> MacAddress:
        """Return a copy of the MAC address with the unicast bit set."""
        b = bytearray(self._mac)
        b[0] &= 0xFE
        return MacAddress(b)

    def with_multicast_bit(self) -> MacAddress:
        """Return a copy of the MAC address with the multicast bit set."""
        b = bytearray(self._mac)
        b[0] |= 0x01
        return MacAddress(b)

    def with_locally_administered_bit(self) -> MacAddress:
        """Return a copy of the MAC address with the locally administered bit set."""
        b = bytearray(self._mac)
        b[0] |= 0x02
        return MacAddress(b)

    def with_universally_administered_bit(self) -> MacAddress:
        """Return a copy of the MAC address with the universally administered bit set."""
        b = bytearray(self._mac)
        b[0] &= 0xFD
        return MacAddress(b)

    def with_last_octet_incremented(self) -> MacAddress:
        """Return a copy of the MAC address with the last octet incremented by 1."""
        b = bytearray(self._mac)
        b[-1] = (b[-1] + 1) % 256
        return MacAddress(b)

    def with_last_octet(self, o: int) -> MacAddress:
        """Return a copy of the MAC address with the last octet set to `o`."""
        if not 0 <= o < 255:
            raise ValueError(f"Octet must fit in a single byte: {o}")
        b = bytearray(self._mac)
        b[-1] = o
        return MacAddress(b)

    def __str__(self) -> str:
        """Return MAC address in the form "xx:xx:xx:xx:xx:xx"."""
        return ":".join([f"{octet:0>2x}" for octet in self._mac])

    def __repr__(self) -> str:
        return f"MacAddress('{self}')"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, MacAddress):
            return False
        return self._mac == other._mac

    def __hash__(self) -> int:
        return hash(self._mac)
