#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import re
import time

from antlion.controllers.ap_lib import dhcp_config
from fuchsia_wlan_base_test.deprecated.dhcp import base_test
from mobly import asserts, test_runner
from mobly_controller.openwrt_access_point.lib.dhcp_config import (
    DhcpConfig,
    Lan,
)


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

        if self.openwrt_ap:
            self.openwrt_ap.dhcp.start_dhcp(
                config=DhcpConfig(lan=Lan(dynamic_dhcp=False))
            )

        elif self.access_point:
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

        dhcp_logs = self.get_dhcp_logs()

        pattern = ""
        if self.openwrt_ap:
            # dnsmasq logs "no address available" when dynamic DHCP is disabled and the client is unknown.
            pattern = r"DHCPDISCOVER.*no address available"
        elif self.access_point:
            # ISC DHCPD logs "no free leases" when it cannot offer a lease due to "deny unknown-clients".
            pattern = r"DHCPDISCOVER.*no free leases"

        asserts.assert_true(
            re.search(pattern, dhcp_logs),
            "Did not find expected message in dhcp logs: " + dhcp_logs + "\n",
        )

    def test_lease_renewal(self) -> None:
        """Validates that a client renews their DHCP lease."""

        ap_params = self.setup_ap()
        LEASE_TIME = 30
        if self.openwrt_ap:
            # The min lease time is 2m for OpenWRT AP
            LEASE_TIME = 120
            self.openwrt_ap.dhcp.start_dhcp(
                config=DhcpConfig(lan=Lan(lease_time=f"{LEASE_TIME}s"))
            )
        elif self.access_point:
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

        dhcp_logs = self.get_dhcp_logs()

        # Fuchsia renews at LEASE_TIME / 2, so there should be at least 2 DHCPREQUESTs in logs.
        # The log lines look like:
        # INFO dhcpd[17385]: DHCPREQUEST for 192.168.9.2 from 01:23:45:67:89:ab via wlan1
        # INFO dhcpd[17385]: DHCPACK on 192.168.9.2 to 01:23:45:67:89:ab via wlan1
        request_matches = len(
            re.findall(rf"DHCPREQUEST.*{re.escape(str(ip))}", dhcp_logs)
        )
        asserts.assert_true(
            request_matches >= 2,
            f"Not enough DHCP renewals in logs: " + dhcp_logs + "\n",
        )


if __name__ == "__main__":
    test_runner.main()
