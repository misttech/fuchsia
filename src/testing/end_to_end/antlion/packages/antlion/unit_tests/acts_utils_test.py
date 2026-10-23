#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import subprocess
import unittest
from unittest import mock

from antlion import utils
from libs.ssh.connection import SshConnection
from libs.ssh.settings import SshSettings

MOCK_ENO1_IP_ADDRESSES = """2: eno1    inet 100.127.110.79/24 brd 100.127.110.255 scope global dynamic noprefixroute eno1\\       valid_lft 86395sec preferred_lft 86395sec
2: eno1    inet6 2401:fa00:480:7a00:8d4f:85ff:cc5c:787e/64 scope global temporary dynamic \\       valid_lft 604796sec preferred_lft 85886sec
2: eno1    inet6 2401:fa00:480:7a00:459:b993:fcbf:1419/64 scope global dynamic mngtmpaddr noprefixroute \\       valid_lft 2591996sec preferred_lft 604796sec
2: eno1    inet6 fe80::c66d:3c75:2cec:1d72/64 scope link noprefixroute \\       valid_lft forever preferred_lft forever"""

MOCK_WLAN1_IP_ADDRESSES = ""

CORRECT_FULL_IP_LIST: dict[str, list[str]] = {
    "ipv4_private": [],
    "ipv4_public": ["100.127.110.79"],
    "ipv6_link_local": ["fe80::c66d:3c75:2cec:1d72"],
    "ipv6_private_local": [],
    "ipv6_public": [
        "2401:fa00:480:7a00:8d4f:85ff:cc5c:787e",
        "2401:fa00:480:7a00:459:b993:fcbf:1419",
    ],
}

CORRECT_EMPTY_IP_LIST: dict[str, list[str]] = {
    "ipv4_private": [],
    "ipv4_public": [],
    "ipv6_link_local": [],
    "ipv6_private_local": [],
    "ipv6_public": [],
}


class IpAddressUtilTest(unittest.TestCase):
    @mock.patch("libs.ssh.connection.SshConnection.run")
    def test_ssh_get_interface_ip_addresses_full(
        self, ssh_mock: mock.Mock
    ) -> None:
        ssh_mock.side_effect = [
            subprocess.CompletedProcess(
                args=[],
                returncode=0,
                stdout=bytes(MOCK_ENO1_IP_ADDRESSES, "utf-8"),
                stderr=b"",
            ),
        ]
        self.assertEqual(
            utils.get_interface_ip_addresses(
                SshConnection(SshSettings("192.168.1.1", "root", "/dev/null")),
                "eno1",
            ),
            CORRECT_FULL_IP_LIST,
        )

    @mock.patch("libs.ssh.connection.SshConnection.run")
    def test_ssh_get_interface_ip_addresses_empty(
        self, ssh_mock: mock.Mock
    ) -> None:
        ssh_mock.side_effect = [
            subprocess.CompletedProcess(
                args=[],
                returncode=0,
                stdout=bytes(MOCK_WLAN1_IP_ADDRESSES, "utf-8"),
                stderr=b"",
            ),
        ]
        self.assertEqual(
            utils.get_interface_ip_addresses(
                SshConnection(SshSettings("192.168.1.1", "root", "/dev/null")),
                "wlan1",
            ),
            CORRECT_EMPTY_IP_LIST,
        )


if __name__ == "__main__":
    unittest.main()
