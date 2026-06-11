#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import re
import time
from dataclasses import dataclass
from ipaddress import IPv4Address, IPv4Network
from pathlib import Path

from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.android_device import AndroidDevice
from antlion.controllers.ap_lib import dhcp_config, hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals
from mobly.config_parser import TestRunConfig
from openwrt_access_point import AddrType as OpenWrtAddrType
from openwrt_access_point import InterfaceName as OpenWrtInterfaceName
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityWpa2,
)


@dataclass
class APParams:
    id: str
    ssid: str
    security: DeprecatedSecurity
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
        if self.openwrt_aps:
            self.openwrt_ap: OpenWrtAP = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point: AccessPoint = self.access_points[0]
        else:
            raise signals.TestAbortClass("Requires at least one access point")

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
        if self.access_point:
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
        if self.access_point:
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
        ssid = AccessPointConfig.random_string(20)
        password = AccessPointConfig.random_string(20)

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_5G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityWpa2(),
                                password=password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)

            router_ip = IPv4Address(
                self.openwrt_ap.get_addr(
                    interface=OpenWrtInterfaceName.lan,
                    addr_type=OpenWrtAddrType.ipv4_private,
                )
            )
            network = IPv4Network(f"{router_ip}/24", strict=False)

            self.openwrt_ap.dhcp.stop_dhcp()

            return APParams(
                id="radio1",
                ssid=ssid,
                security=DeprecatedSecurity(
                    DeprecatedSecurityMode.WPA2, password
                ),
                ip=router_ip,
                network=network,
            )
        elif self.access_point:
            security = DeprecatedSecurity(
                security_mode=DeprecatedSecurityMode.WPA2,
                password=password,
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
        else:
            raise signals.TestAbortClass("Requires at least one access point")

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

    def get_dhcp_logs(self) -> str:
        if self.openwrt_ap:
            val = self.openwrt_ap.dhcp.get_dhcp_logs_since_last_dhcp_start()
            assert isinstance(val, str)
            return val
        elif self.access_point:
            dhcp_logs = self.access_point.get_dhcp_logs()
            if dhcp_logs is None:
                raise signals.TestFailure("No DHCP logs")
            assert isinstance(dhcp_logs, str)
            return dhcp_logs
        else:
            raise signals.TestFailure("No access point found")

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

        if self.openwrt_ap:
            self.openwrt_ap.dhcp.start_dhcp()
            self.connect(ap_params=ap_params)

            try:
                ip = self.get_device_ipv4_addr()
            except ConnectionError:
                self.log.warning(
                    "DHCP logs: %s",
                    self.openwrt_ap.dhcp.get_dhcp_logs_since_last_dhcp_start(),
                )
                raise signals.TestFailure("DUT failed to get an IP address")

        elif self.access_point:
            with self.access_point.tcpdump.start(
                self.access_point.wlan_5g, Path(self.log_path)
            ):
                self.access_point.start_dhcp(dhcp_conf=dhcp_conf)
                self.connect(ap_params=ap_params)

                try:
                    ip = self.get_device_ipv4_addr()
                except ConnectionError:
                    self.log.warning(
                        "DHCP logs: %s", self.access_point.get_dhcp_logs()
                    )
                    raise signals.TestFailure("DUT failed to get an IP address")
        else:
            raise signals.TestAbortClass("Requires at least one access point")

        dhcp_logs = self.get_dhcp_logs()
        discover_count = dhcp_logs.count("DHCPDISCOVER")
        offer_count = len(
            re.findall(rf"DHCPOFFER.*{re.escape(str(ip))}", dhcp_logs)
        )
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

        request_count = len(
            re.findall(rf"DHCPREQUEST.*{re.escape(str(ip))}", dhcp_logs)
        )
        asserts.assert_true(
            request_count >= 1,
            f"Incorrect count of DHCP Requests in logs:\n{dhcp_logs}\n",
        )
        ack_count = len(
            re.findall(rf"DHCPACK.*{re.escape(str(ip))}", dhcp_logs)
        )
        asserts.assert_true(
            ack_count >= 1,
            f"Incorrect count of DHCP Acks in logs:\n{dhcp_logs}\n",
        )

        self.log.info(f"Attempting to ping {ap_params.ip}...")
        ping_result = self.dut.ping(str(ap_params.ip), count=2)
        asserts.assert_true(
            ping_result.success,
            f"DUT failed to ping router at {ap_params.ip}: {ping_result}",
        )
