#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import ipaddress
import re
from ipaddress import IPv4Address

from antlion.controllers.ap_lib import dhcp_config
from antlion.controllers.utils_lib.commands import ip
from antlion.test_utils.dhcp import base_test
from mobly import asserts, signals, test_runner


class Dhcpv4DuplicateAddressTest(base_test.Dhcpv4InteropFixture):
    def setup_test(self) -> None:
        super().setup_test()
        self.extra_addresses: list[IPv4Address] = []
        self.ap_params = self.setup_ap()
        self.ap_ip_cmd = ip.LinuxIpCommand(self.access_point.ssh)

    def teardown_test(self) -> None:
        super().teardown_test()
        for ip in self.extra_addresses:
            self.ap_ip_cmd.remove_ipv4_address(self.ap_params.id, ip)

    def test_duplicate_address_assignment(self) -> None:
        """It's possible for a DHCP server to assign an address that already exists on the network.
        DHCP clients are expected to perform a "gratuitous ARP" of the to-be-assigned address, and
        refuse to assign that address. Clients should also recover by asking for a different
        address.
        """
        # Modify subnet to hold fewer addresses.
        # A '/29' has 8 addresses (6 usable excluding router / broadcast)
        subnet = next(self.ap_params.network.subnets(new_prefix=29))
        subnet_conf = dhcp_config.Subnet(
            subnet=subnet,
            router=self.ap_params.ip,
            # When the DHCP server is considering dynamically allocating an IP address to a client,
            # it first sends an ICMP Echo request (a ping) to the address being assigned. It waits
            # for a second, and if no ICMP Echo response has been heard, it assigns the address.
            # If a response is heard, the lease is abandoned, and the server does not respond to
            # the client.
            # The ping-check configuration parameter can be used to control checking - if its value
            # is false, no ping check is done.
            additional_parameters={"ping-check": "false"},
        )
        dhcp_conf = dhcp_config.DhcpConfig(subnets=[subnet_conf])
        self.access_point.start_dhcp(dhcp_conf=dhcp_conf)

        # Add each of the usable IPs as an alias for the router's interface, such that the router
        # will respond to any pings on it.
        for ip in subnet.hosts():
            self.ap_ip_cmd.add_ipv4_address(
                self.ap_params.id,
                ipaddress.IPv4Interface(f"{ip}/{ip.max_prefixlen}"),
            )
            # Ensure we remove the address in self.teardown_test() even if the test fails
            self.extra_addresses.append(ip)

        self.connect(ap_params=self.ap_params)
        with asserts.assert_raises(ConnectionError):
            self.get_device_ipv4_addr()

        # Per spec, the flow should be:
        # Discover -> Offer -> Request -> Ack -> client optionally performs DAD
        dhcp_logs = self.access_point.get_dhcp_logs()
        if dhcp_logs is None:
            raise signals.TestError(
                "DHCP logs not found; was the DHCP server started?"
            )

        for expected_message in [
            r"DHCPDISCOVER from \S+",
            r"DHCPOFFER on [0-9.]+ to \S+",
            r"DHCPREQUEST for [0-9.]+",
            r"DHCPACK on [0-9.]+",
            r"DHCPDECLINE of [0-9.]+ from \S+ via .*: abandoned",
            r"Abandoning IP address [0-9.]+: declined",
        ]:
            asserts.assert_true(
                re.search(expected_message, dhcp_logs),
                f"Did not find expected message ({expected_message}) in dhcp logs: {dhcp_logs}"
                + "\n",
            )

        # Remove each of the IP aliases.
        # Note: this also removes the router's address (e.g. 192.168.1.1), so pinging the
        # router after this will not work.
        while self.extra_addresses:
            self.ap_ip_cmd.remove_ipv4_address(
                self.ap_params.id, self.extra_addresses.pop()
            )

        # Now, we should get an address successfully
        ip = self.get_device_ipv4_addr()
        dhcp_logs = self.access_point.get_dhcp_logs()
        if dhcp_logs is None:
            raise signals.TestError(
                "DHCP logs not found; was the DHCP server started?"
            )

        expected_string = f"DHCPREQUEST for {ip}"
        asserts.assert_true(
            dhcp_logs.count(expected_string) >= 1,
            f'Incorrect count of DHCP Requests ("{expected_string}") in logs: '
            + dhcp_logs
            + "\n",
        )

        expected_string = f"DHCPACK on {ip}"
        asserts.assert_true(
            dhcp_logs.count(expected_string) >= 1,
            f'Incorrect count of DHCP Acks ("{expected_string}") in logs: '
            + dhcp_logs
            + "\n",
        )


if __name__ == "__main__":
    test_runner.main()
