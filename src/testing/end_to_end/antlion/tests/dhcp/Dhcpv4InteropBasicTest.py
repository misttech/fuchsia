#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import re
import time

from antlion.controllers.ap_lib import dhcp_config
from antlion.test_utils.dhcp import base_test
from mobly import asserts, signals, test_runner


class Dhcpv4InteropBasicTest(base_test.Dhcpv4InteropFixture):
    """DhcpV4 tests which validate basic DHCP client/server interactions."""

    def test_basic_dhcp_assignment(self) -> None:
        self.run_test_case_expect_dhcp_success(
            dhcp_options={},
            dhcp_parameters={},
        )

    def test_pool_allows_unknown_clients(self) -> None:
        self.run_test_case_expect_dhcp_success(
            dhcp_options={},
            dhcp_parameters={"allow": "unknown-clients"},
        )

    def test_pool_disallows_unknown_clients(self) -> None:
        ap_params = self.setup_ap()
        subnet_conf = dhcp_config.Subnet(
            subnet=ap_params.network,
            router=ap_params.ip,
            additional_parameters={"deny": "unknown-clients"},
        )
        dhcp_conf = dhcp_config.DhcpConfig(subnets=[subnet_conf])
        self.access_point.start_dhcp(dhcp_conf=dhcp_conf)

        self.connect(ap_params=ap_params)
        with asserts.assert_raises(ConnectionError):
            self.get_device_ipv4_addr()

        dhcp_logs = self.access_point.get_dhcp_logs()
        if dhcp_logs is None:
            raise signals.TestError(
                "DHCP logs not found; was the DHCP server started?"
            )

        asserts.assert_true(
            re.search(r"DHCPDISCOVER from .*no free leases", dhcp_logs),
            "Did not find expected message in dhcp logs: " + dhcp_logs + "\n",
        )

    def test_lease_renewal(self) -> None:
        """Validates that a client renews their DHCP lease."""
        LEASE_TIME = 30
        ap_params = self.setup_ap()
        subnet_conf = dhcp_config.Subnet(
            subnet=ap_params.network, router=ap_params.ip
        )
        dhcp_conf = dhcp_config.DhcpConfig(
            subnets=[subnet_conf],
            default_lease_time=LEASE_TIME,
            max_lease_time=LEASE_TIME,
        )
        self.access_point.start_dhcp(dhcp_conf=dhcp_conf)
        self.connect(ap_params=ap_params)
        ip = self.get_device_ipv4_addr()

        SLEEP_TIME = LEASE_TIME + 3
        self.log.info(f"Sleeping {SLEEP_TIME}s to await DHCP renewal")
        time.sleep(SLEEP_TIME)

        dhcp_logs = self.access_point.get_dhcp_logs()
        if dhcp_logs is None:
            raise signals.TestError(
                "DHCP logs not found; was the DHCP server started?"
            )

        # Fuchsia renews at LEASE_TIME / 2, so there should be at least 2 DHCPREQUESTs in logs.
        # The log lines look like:
        # INFO dhcpd[17385]: DHCPREQUEST for 192.168.9.2 from 01:23:45:67:89:ab via wlan1
        # INFO dhcpd[17385]: DHCPACK on 192.168.9.2 to 01:23:45:67:89:ab via wlan1
        expected_string = f"DHCPREQUEST for {ip}"
        asserts.assert_true(
            dhcp_logs.count(expected_string) >= 2,
            f'Not enough DHCP renewals ("{expected_string}") in logs: '
            + dhcp_logs
            + "\n",
        )


if __name__ == "__main__":
    test_runner.main()
