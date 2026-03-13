# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.custom_types.py."""

import ipaddress
import unittest
from typing import Any

from parameterized import parameterized

from honeydew.typing import custom_types


class CustomTypesTests(unittest.TestCase):
    """Unit tests for honeydew.custom_types.py."""

    @parameterized.expand(
        [
            (
                "valid_ipv4",
                "127.0.0.1:8081",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("127.0.0.1"), port=8081
                ),
            ),
            (
                "valid_ipv6_scope_numeric",
                "[::1%1]:8081",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("::1%1"), port=8081
                ),
            ),
            (
                "valid_ipv6_no_brackets",
                "::1:8081",
                custom_types.IpPort(ip=ipaddress.ip_address("::1"), port=8081),
            ),
        ]
    )
    def test_create_using_ip_and_port(
        self, _: str, addr: str, expected: custom_types.IpPort
    ) -> None:
        """Test cases for IpPort.create_using_ip_and_port()."""
        got: custom_types.IpPort = custom_types.IpPort.create_using_ip_and_port(
            addr
        )
        self.assertEqual(got, expected)

    @parameterized.expand(
        [
            (
                "valid_ipv4",
                "127.0.0.1",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("127.0.0.1"), port=None
                ),
            ),
            (
                "valid_ipv6",
                "::1",
                custom_types.IpPort(ip=ipaddress.ip_address("::1"), port=None),
            ),
            (
                "valid_ipv6_scope_numeric",
                "::1%1",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("::1%1"), port=None
                ),
            ),
            (
                "valid_ipv6_with_brackets",
                "[::1]",
                custom_types.IpPort(ip=ipaddress.ip_address("::1"), port=None),
            ),
        ]
    )
    def test_create_using_ip(
        self, _: str, addr: str, expected: custom_types.IpPort
    ) -> None:
        """Test cases for IpPort.create_using_ip()."""
        got: custom_types.IpPort = custom_types.IpPort.create_using_ip(addr)
        self.assertEqual(got, expected)

    @parameterized.expand(
        [
            ("invalid", "some_str"),
            ("invalid_double_scope", "[::1%2%1]:100"),
            ("invalid_double_percent", "[::1%%1]:100"),
            ("invalid_negative_port", "[::1]:-1"),
            ("invalid_zero_port", "[::1]:0"),
            ("invalid_port_number", "[::1]:asdf"),
        ]
    )
    def test_create_using_ip_and_port_raises(self, _: str, addr: str) -> None:
        """Test cases for IpPort.create_using_ip_and_port() which raise
        exceptions."""
        with self.assertRaises(ValueError):
            custom_types.IpPort.create_using_ip_and_port(addr)

    @parameterized.expand(
        [
            ("invalid", "some_str"),
            ("invalid_double_scope", "[::1%2%1]"),
            ("invalid_double_percent", "[::1%%1]"),
        ]
    )
    def test_create_using_ip_raises(self, _: str, addr: str) -> None:
        """Test cases for IpPort.create_using_ip() which raise exceptions."""
        with self.assertRaises(ValueError):
            custom_types.IpPort.create_using_ip(addr)

    @parameterized.expand(
        [
            (
                "valid_ipv4_and_port",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("127.0.0.1"), port=8081
                ),
                "127.0.0.1:8081",
            ),
            (
                "valid_ipv6_and_port",
                custom_types.IpPort(ip=ipaddress.ip_address("::1"), port=8081),
                "[::1]:8081",
            ),
            (
                "valid_ipv6_with_scope_and_port",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("::1%1"), port=8081
                ),
                "[::1%1]:8081",
            ),
            (
                "valid_ipv4_without_port",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("127.0.0.1"), port=None
                ),
                "127.0.0.1",
            ),
            (
                "valid_ipv6_without_port",
                custom_types.IpPort(
                    ip=ipaddress.ip_address("::1%1"), port=None
                ),
                "[::1%1]",
            ),
        ]
    )
    def test_ipport_str(
        self, _: str, ip_port: custom_types.IpPort, expected: str
    ) -> None:
        """Test cases for IpPort.__str__."""
        got = str(ip_port)
        self.assertEqual(got, expected)

    def test_ipport_ip_str(self) -> None:
        """Test cases for IpPort.ip_str."""
        ip_port = custom_types.IpPort(
            ip=ipaddress.ip_address("127.0.0.1"), port=8081
        )
        self.assertEqual(ip_port.ip_str, "127.0.0.1")
        ip_port = custom_types.IpPort(ip=ipaddress.ip_address("::1"), port=None)
        self.assertEqual(ip_port.ip_str, "::1")

    @parameterized.expand(
        [
            (
                "ip_port_valid",
                "192.168.1.1:8022",
                custom_types.IpPort(ipaddress.ip_address("192.168.1.1"), 8022),
            ),
            (
                "ip_valid",
                "192.168.1.1",
                custom_types.IpPort(ipaddress.ip_address("192.168.1.1"), None),
            ),
            (
                "ipv6_no_port",
                "fe80::1",
                custom_types.IpPort(ipaddress.ip_address("fe80::1"), None),
            ),
            (
                "ipv6_with_port",
                "[fe80::1]:1234",
                custom_types.IpPort(ipaddress.ip_address("fe80::1"), 1234),
            ),
        ]
    )
    def test_target_addr_from_str(
        self, _: str, query: str, expected: custom_types.TargetAddr
    ) -> None:
        """Test cases for TargetAddr.from_str() success."""
        got = custom_types.TargetAddr.from_str(query)
        self.assertEqual(got, expected)

    @parameterized.expand(
        [
            ("invalid_ip", "256.256.256.256"),
            ("random_string", "my-fuchsia-device"),
        ]
    )
    def test_target_addr_from_str_raises(self, _: str, query: str) -> None:
        """Test cases for TargetAddr.from_str() that raise ValueError."""
        with self.assertRaises(ValueError):
            custom_types.TargetAddr.from_str(query)

    @parameterized.expand(
        [
            (
                "ip_default_port_none",
                {"type": "Ip", "ip": "192.168.1.1"},
                custom_types.IpPort(ipaddress.ip_address("192.168.1.1"), None),
            ),
            (
                "ip_explicit_port",
                {"type": "Ip", "ip": "192.168.1.1", "ssh_port": 8022},
                custom_types.IpPort(ipaddress.ip_address("192.168.1.1"), 8022),
            ),
            (
                "ip_port_zero_is_none",
                {"type": "Ip", "ip": "192.168.1.1", "ssh_port": 0},
                custom_types.IpPort(ipaddress.ip_address("192.168.1.1"), None),
            ),
            (
                "ipv6_no_scope",
                {"type": "Ip", "ip": "fe80::1"},
                custom_types.IpPort(ipaddress.ip_address("fe80::1"), None),
            ),
            (
                "ipv6_with_scope",
                {"type": "Ip", "ip": "fe80::1%1"},
                custom_types.IpPort(ipaddress.ip_address("fe80::1%1"), None),
            ),
        ]
    )
    def test_target_addr_from_json(
        self, _: str, obj: dict[str, Any], expected: custom_types.TargetAddr
    ) -> None:
        """Test cases for TargetAddr.from_json() success."""
        got = custom_types.TargetAddr.from_json(obj)
        self.assertEqual(got, expected)

    @parameterized.expand(
        [
            ("invalid_ip", {"type": "Ip", "ip": "256.256.256.256"}),
            ("missing_ip", {"type": "Ip"}),
            ("unsupported_type", {"type": "Unknown"}),
        ]
    )
    def test_target_addr_from_json_raises(
        self, _: str, obj: dict[str, Any]
    ) -> None:
        """Test cases for TargetAddr.from_json() raising ValueError."""
        with self.assertRaises(ValueError):
            custom_types.TargetAddr.from_json(obj)


if __name__ == "__main__":
    unittest.main()
