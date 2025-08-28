#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import time
from dataclasses import dataclass
from ipaddress import IPv4Address, IPv4Network
from pathlib import Path

from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.android_device import AndroidDevice
from antlion.controllers.ap_lib import dhcp_config, hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals
from mobly.config_parser import TestRunConfig


@dataclass
class APParams:
    id: str
    ssid: str
    security: Security
    ip: IPv4Address
    network: IPv4Network


class Dhcpv4InteropFixture(base_test.WifiBaseTest):
    """Test helpers for validating DHCPv4 Interop

    Test Bed Requirement:
    * One Android device or Fuchsia device
    * One Access Point
    """

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()
        self.fuchsia_device: FuchsiaDevice | None = None
        self.access_point: AccessPoint = self.access_points[0]

        device_type = self.user_params.get("dut", "fuchsia_devices")
        if device_type == "fuchsia_devices":
            self.fuchsia_device, self.dut = self.get_dut_type(
                FuchsiaDevice, AssociationMode.POLICY
            )
        elif device_type == "android_devices":
            _, self.dut = self.get_dut_type(
                AndroidDevice, AssociationMode.POLICY
            )
        else:
            raise ValueError(
                f'Invalid "dut" type specified in config: "{device_type}".'
                'Expected "fuchsia_devices" or "android_devices".'
            )

    def setup_class(self) -> None:
        super().setup_class()
        self.access_point.stop_all_aps()

    def setup_test(self) -> None:
        if hasattr(self, "android_devices"):
            for ad in self.android_devices:
                ad.droid.wakeLockAcquireBright()
                ad.droid.wakeUpNow()
        self.dut.wifi_toggle_state(True)

    def teardown_test(self) -> None:
        if hasattr(self, "android_devices"):
            for ad in self.android_devices:
                ad.droid.wakeLockRelease()
                ad.droid.goToSleepNow()
        self.dut.turn_location_off_and_scan_toggle_off()
        self.dut.disconnect()
        self.dut.reset_wifi()
        self.access_point.stop_all_aps()

    def connect(self, ap_params: APParams) -> None:
        asserts.assert_true(
            self.dut.associate(
                ap_params.ssid,
                target_pwd=ap_params.security.password,
                target_security=ap_params.security.security_mode,
            ),
            "Failed to connect.",
        )

    def setup_ap(self) -> APParams:
        """Generates a hostapd config and sets up the AP with that config.

        Does not run a DHCP server.

        Returns:
            APParams for the newly setup AP.
        """
        ssid = utils.rand_ascii_str(20)
        security = Security(
            security_mode=SecurityMode.WPA2,
            password=generate_random_password(length=20),
            wpa_cipher="CCMP",
            wpa2_cipher="CCMP",
        )

        ap_ids = setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            mode=hostapd_constants.Mode.MODE_11N_MIXED,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            n_capabilities=[],
            ac_capabilities=[],
            force_wmm=True,
            ssid=ssid,
            security=security,
        )

        if len(ap_ids) > 1:
            raise Exception("Expected only one SSID on AP")

        configured_subnets = self.access_point.get_configured_subnets()
        if len(configured_subnets) > 1:
            raise Exception("Expected only one subnet on AP")
        router_ip = configured_subnets[0].router
        network = configured_subnets[0].network

        self.access_point.stop_dhcp()

        return APParams(
            id=ap_ids[0],
            ssid=ssid,
            security=security,
            ip=router_ip,
            network=network,
        )

    def get_device_ipv4_addr(
        self, interface: str | None = None, timeout_sec: float = 20.0
    ) -> IPv4Address:
        """Checks if device has an ipv4 private address.

        Only supported on Fuchsia.

        Args:
            interface: name of interface from which to get ipv4 address.
            timeout: seconds to wait until raising ConnectionError

        Raises:
            ConnectionError, if DUT does not have an ipv4 address after all
            timeout.

        Returns:
            The device's IP address
        """
        if self.fuchsia_device is None:
            # TODO(http://b/292289291): Add get_(ipv4|ipv6)_addr to SupportsIP.
            raise TypeError(
                "TODO(http://b/292289291): get_device_ipv4_addr only supports "
                "FuchsiaDevice"
            )

        self.log.debug("Fetching updated WLAN interface list")
        if interface is None:
            interface = self.dut.get_default_wlan_test_interface()
        self.log.info(
            "Checking if DUT has received an ipv4 addr on iface %s. Will retry for %s "
            "seconds." % (interface, timeout_sec)
        )
        timeout_sec = time.time() + timeout_sec
        while time.time() < timeout_sec:
            ip_addrs = self.fuchsia_device.get_interface_ip_addresses(interface)

            if len(ip_addrs["ipv4_private"]) > 0:
                ip = ip_addrs["ipv4_private"][0]
                self.log.info(f"DUT has an ipv4 address: {ip}")
                return IPv4Address(ip)
            else:
                self.log.debug(
                    "DUT does not yet have an ipv4 address...retrying in 1 "
                    "second."
                )
                time.sleep(1)
        else:
            raise ConnectionError("DUT failed to get an ipv4 address.")

    def run_test_case_expect_dhcp_success(
        self,
        dhcp_parameters: dict[str, str],
        dhcp_options: dict[str, int | str],
    ) -> None:
        """Starts the AP and DHCP server, and validates that the client
        connects and obtains an address.

        Args:
            dhcp_parameters: a dictionary of DHCP parameters
            dhcp_options: a dictionary of DHCP options
        """
        ap_params = self.setup_ap()
        subnet_conf = dhcp_config.Subnet(
            subnet=ap_params.network,
            router=ap_params.ip,
            additional_parameters=dhcp_parameters,
            additional_options=dhcp_options,
        )
        dhcp_conf = dhcp_config.DhcpConfig(subnets=[subnet_conf])

        self.log.debug(
            "DHCP Configuration:\n%s\n", dhcp_conf.render_config_file()
        )

        with self.access_point.tcpdump.start(
            self.access_point.wlan_5g, Path(self.log_path)
        ):
            self.access_point.start_dhcp(dhcp_conf=dhcp_conf)
            self.connect(ap_params=ap_params)

            # Typical log lines look like:
            #
            # dhcpd[26695]: DHCPDISCOVER from 01:23:45:67:89:ab via wlan1
            # dhcpd[26695]: DHCPOFFER on 192.168.9.2 to 01:23:45:67:89:ab via wlan1
            # dhcpd[26695]: DHCPREQUEST for 192.168.9.2 (192.168.9.1) from 01:23:45:67:89:ab via wlan1
            # dhcpd[26695]: DHCPACK on 192.168.9.2 to 01:23:45:67:89:ab via wlan1

            # Due to b/384790032, logs can also show duplicate DISCOVER and
            # OFFER packets due to the Fuchsia DHCP client queuing packets while
            # EAPOL is in progress:
            #
            # DHCPDISCOVER from 01:23:45:67:89:ab via wlan1
            # DHCPOFFER on 192.168.9.2 to 01:23:45:67:89:ab via wlan1
            # DHCPDISCOVER from 01:23:45:67:89:ab via wlan1
            # DHCPOFFER on 192.168.9.2 to 01:23:45:67:89:ab via wlan1
            # DHCPREQUEST for 192.168.9.2 (192.168.9.1) from 01:23:45:67:89:ab via wlan1
            # DHCPACK on 192.168.9.2 to 01:23:45:67:89:ab via wlan1

            try:
                ip = self.get_device_ipv4_addr()
            except ConnectionError:
                self.log.warning(
                    "DHCP logs: %s", self.access_point.get_dhcp_logs()
                )
                raise signals.TestFailure("DUT failed to get an IP address")

            # Get updates to DHCP logs
            dhcp_logs = self.access_point.get_dhcp_logs()
            if dhcp_logs is None:
                raise signals.TestFailure("No DHCP logs")

            # TODO(http://b/384790032): Replace with logic below with this
            # comment once DHCP is started after EAPOL finishes. Or remove this
            # comment if queueing is determined expected and acceptable
            # behavior.
            #
            # expected_string = f"DHCPDISCOVER from"
            # asserts.assert_equal(
            #     dhcp_logs.count(expected_string),
            #     1,
            #     f'Incorrect count of DHCP Discovers ("{expected_string}") in logs',
            #     dhcp_logs,
            # )
            #
            # expected_string = f"DHCPOFFER on {ip}"
            # asserts.assert_equal(
            #     dhcp_logs.count(expected_string),
            #     1,
            #     f'Incorrect count of DHCP Offers ("{expected_string}") in logs',
            #     dhcp_logs,
            # )

            discover_count = dhcp_logs.count("DHCPDISCOVER from")
            offer_count = dhcp_logs.count(f"DHCPOFFER on {ip}")
            asserts.assert_greater(
                discover_count,
                0,
                "Expected one or more DHCP Discovers",
                dhcp_logs,
            )
            asserts.assert_equal(
                discover_count,
                offer_count,
                "Expected an equal amount of DHCP Discovers and Offers",
                dhcp_logs,
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

            self.log.info(f"Attempting to ping {ap_params.ip}...")
            ping_result = self.dut.ping(str(ap_params.ip), count=2)
            asserts.assert_true(
                ping_result.success,
                f"DUT failed to ping router at {ap_params.ip}: {ping_result}",
            )
