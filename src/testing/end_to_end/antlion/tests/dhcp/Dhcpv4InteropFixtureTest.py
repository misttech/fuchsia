#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion.controllers.ap_lib import dhcp_config
from antlion.test_utils.dhcp import base_test
from mobly import asserts, test_runner


class Dhcpv4InteropFixtureTest(base_test.Dhcpv4InteropFixture):
    """Tests which validate the behavior of the Dhcpv4InteropFixture.

    In theory, these are more similar to unit tests than ACTS tests, but
    since they interact with hardware (specifically, the AP), we have to
    write and run them like the rest of the ACTS tests."""

    def test_invalid_options_not_accepted(self) -> None:
        """Ensures the DHCP server doesn't accept invalid options"""
        ap_params = self.setup_ap()
        subnet_conf = dhcp_config.Subnet(
            subnet=ap_params.network,
            router=ap_params.ip,
            additional_options={"foo": "bar"},
        )
        dhcp_conf = dhcp_config.DhcpConfig(subnets=[subnet_conf])
        with asserts.assert_raises_regex(Exception, r"failed to start"):
            self.access_point.start_dhcp(dhcp_conf=dhcp_conf)

    def test_invalid_parameters_not_accepted(self) -> None:
        """Ensures the DHCP server doesn't accept invalid parameters"""
        ap_params = self.setup_ap()
        subnet_conf = dhcp_config.Subnet(
            subnet=ap_params.network,
            router=ap_params.ip,
            additional_parameters={"foo": "bar"},
        )
        dhcp_conf = dhcp_config.DhcpConfig(subnets=[subnet_conf])
        with asserts.assert_raises_regex(Exception, r"failed to start"):
            self.access_point.start_dhcp(dhcp_conf=dhcp_conf)

    def test_no_dhcp_server_started(self) -> None:
        """Validates that the test fixture does not start a DHCP server."""
        ap_params = self.setup_ap()
        self.connect(ap_params=ap_params)
        with asserts.assert_raises(ConnectionError):
            self.get_device_ipv4_addr()


if __name__ == "__main__":
    test_runner.main()
