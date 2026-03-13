# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Custom data types."""

from __future__ import annotations

import abc
import enum
import ipaddress
from dataclasses import dataclass
from typing import Any, TypeVar

AnyString = TypeVar("AnyString", str, bytes)


class LEVEL(enum.StrEnum):
    """Logging level that need to specified to log a message onto device"""

    INFO = "Info"
    WARNING = "Warning"
    ERROR = "Error"


class TargetAddr(abc.ABC):
    """Abstract base class representing a generic Fuchsia Target Address."""

    @property
    @abc.abstractmethod
    def ip_str(self) -> str:
        """Gets a formatted string representation of the IP/identifier without a port."""

    @classmethod
    def from_str(cls, query: str) -> "TargetAddr":
        """Attempts to parse a string query into a TargetAddr.

        Args:
            query: The string representation of a target address (e.g. 'usb:123', '127.0.0.1:8022')

        Returns:
            A TargetAddr subclass instance.

        Raises:
            ValueError: If the query cannot be cleanly parsed as a valid resolved address
                (e.g., if it's just a hostname).
        """
        if cls is TargetAddr:
            for subclass in TargetAddr.__subclasses__():
                try:
                    return subclass.from_str(query)
                except ValueError:
                    continue
            raise ValueError(f"Could not parse '{query}' as TargetAddr")
        raise NotImplementedError("Subclasses must implement from_str")

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "TargetAddr":
        """Parses a FFX target address JSON object into a TargetAddr.

        Args:
            obj: The dictionary parsed from 'ffx --machine json target list'.

        Returns:
            A TargetAddr subclass instance.

        Raises:
            ValueError: If the object type is not supported or missing required fields.
        """
        if cls is TargetAddr:
            for subclass in TargetAddr.__subclasses__():
                try:
                    return subclass.from_json(obj)
                except ValueError:
                    continue

            raise ValueError(f"Unable to create TargetAddr for {obj}")
        raise NotImplementedError("Subclasses must implement from_json")


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

    @classmethod
    def from_str(cls, query: str) -> "IpPort":
        """Attempts to parse a string query into a IpPort.

        Args:
            query: The string representation of a target address (e.g. '127.0.0.1:8022')

        Returns:
            An IpPort.

        Raises:
            ValueError: If the query cannot be cleanly parsed as a valid address
                (e.g., if it's just a hostname), or if it is an ipv6 address with
                a symbolic scope ID.
        """
        # If it's wrapped in brackets, it must be an IP or [IP]:PORT
        if query.startswith("["):
            try:
                ip_port = cls.create_using_ip_and_port(query)
                if ip_port.port is not None:
                    return cls._validate_ipv6_scope(ip_port)
            except ValueError:
                pass
            try:
                return cls._validate_ipv6_scope(cls.create_using_ip(query))
            except ValueError:
                pass
        else:
            # No brackets.
            # An unbracketed string is first tested as a standalone IP address
            # (v4 or v6). If that fails, it's then tested as an address with a
            # port (e.g. "1.2.3.4:8022"). This resolves the ambiguity for some
            # valid IPv6 addresses that contain colons. Users can use
            # brackets for IPv6 (e.g. "[::1]:8022") to force port parsing.
            try:
                return cls._validate_ipv6_scope(cls.create_using_ip(query))
            except ValueError:
                pass
            try:
                return cls._validate_ipv6_scope(
                    cls.create_using_ip_and_port(query)
                )
            except ValueError:
                pass

        raise ValueError(f"Could not parse '{query}' as IpPort")

    @classmethod
    def _validate_ipv6_scope(cls, ip_port: IpPort) -> "IpPort":
        if isinstance(ip_port.ip, ipaddress.IPv6Address):
            scope_id = getattr(ip_port.ip, "scope_id", None)
            if scope_id is not None and not scope_id.isdigit():
                raise ValueError(
                    f"Symbolic scope {scope_id} not supported. Addresses must be resolved"
                )
        return ip_port

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

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "IpPort":
        """Parses a FFX target address JSON object into an IpPort.

        Args:
            obj: The dictionary parsed from 'ffx --machine json target list'.

        Returns:
            An IpPort.

        Raises:
            ValueError: If the object is missing required fields.
        """
        addr_type = obj.get("type")
        if addr_type != "Ip":
            raise ValueError(f"type not Ip in {obj}")
        ssh_ip = obj.get("ip")
        if ssh_ip is None:
            raise ValueError(f"Missing ip address in {obj}")
        ssh_port = obj.get("ssh_port")
        if ssh_port == 0:
            ssh_port = None

        ip_obj = ipaddress.ip_address(ssh_ip)
        return cls._validate_ipv6_scope(
            IpPort(
                ip=ip_obj,
                port=ssh_port,
            )
        )

    @property
    def ip_str(self) -> str:
        """The IP address as a string."""
        return str(self.ip)


@dataclass(frozen=True)
class TargetSshAddress(IpPort):
    """Dataclass that holds target's ssh address information.

    Args:
        ip: Target's SSH IP Address
        port: Target's SSH port
    """


@dataclass(frozen=True)
class Sl4fServerAddress(IpPort):
    """Dataclass that holds sl4f server address information.

    Args:
        ip: IP Address of SL4F server
        port: Port where SL4F server is listening for SL4F requests
    """


@dataclass(frozen=True)
class DeviceInfo:
    """Dataclass that holds Fuchsia device information.

    Args:
        name: Device name returned by `ffx target list`.
        serial_socket: Device serial socket path.
        ip_port: IP Address and port of the device.
    """

    name: str
    ip_port: IpPort | None
    serial_socket: str | None

    def __str__(self) -> str:
        return (
            f"name={self.name}, "
            f"ip_port={self.ip_port}, "
            f"serial_socket={self.serial_socket}, "
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
