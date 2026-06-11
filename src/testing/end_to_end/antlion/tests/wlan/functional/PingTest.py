#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
PingTest exercises sending ICMP and ICMPv6 pings to a wireless access
router and another device behind the AP. Note, this does not reach out to the
internet. The DUT is only responsible for sending a routable packet; any
communication past the first-hop is not the responsibility of the DUT.
"""

import logging
import multiprocessing
from typing import Callable, NamedTuple

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.utils import PingResult, rand_ascii_str
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from openwrt_access_point import AddrType as OpenWrtAddrType
from openwrt_access_point import InterfaceName as OpenWrtInterfaceName
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    Band,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

LOOPBACK_IPV4 = "127.0.0.1"
LOOPBACK_IPV6 = "::1"
PING_RESULT_TIMEOUT_SEC = 60 * 5


class Addrs(NamedTuple):
    gateway_ipv4: str
    gateway_ipv6: str


class Test(NamedTuple):
    name: str
    dest_ip: str | Callable[[Addrs], str]
    packet_count: int = 3
    interval: int = 1000
    timeout: int = 1000
    size: int = 25
    min_success: int | None = None


class PingTest(base_test.WifiBaseTest):
    def pre_run(self) -> None:
        self.generate_tests(
            self.send_ping,
            lambda test_name, *_: f"test_{test_name}",
            [
                Test(
                    "gateway_ipv4_small_packets",
                    lambda addrs: addrs.gateway_ipv4,
                    packet_count=50,
                    min_success=49,
                ),
                Test(
                    "gateway_ipv6_small_packets",
                    lambda addrs: addrs.gateway_ipv6,
                    packet_count=50,
                    min_success=49,
                ),
                Test(
                    "gateway_ipv4_medium_packets",
                    lambda addrs: addrs.gateway_ipv4,
                    packet_count=50,
                    timeout=1500,
                    size=64,
                    min_success=49,
                ),
                Test(
                    "gateway_ipv6_medium_packets",
                    lambda addrs: addrs.gateway_ipv6,
                    packet_count=50,
                    timeout=1500,
                    size=64,
                    min_success=49,
                ),
                Test(
                    "gateway_ipv4_large_packets",
                    lambda addrs: addrs.gateway_ipv4,
                    packet_count=50,
                    timeout=5000,
                    size=500,
                    min_success=49,
                ),
                Test(
                    "gateway_ipv6_large_packets",
                    lambda addrs: addrs.gateway_ipv6,
                    packet_count=50,
                    timeout=5000,
                    size=500,
                    min_success=49,
                ),
            ],
        )

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()
        self.ssid = rand_ascii_str(10)

        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

        band = Band.BAND_2G
        security = SecurityOpen()

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=self.ssid,
                                security=security,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)

            # Retrieve Gateway IPs.
            self.ap_ipv4 = self.openwrt_ap.get_addr(
                OpenWrtInterfaceName.lan,
                OpenWrtAddrType.ipv4_private,
            )
            self.ap_ipv6 = self.openwrt_ap.get_addr(
                OpenWrtInterfaceName.lan,
                OpenWrtAddrType.ipv6_link_local,
            )
        else:
            assert self.access_point is not None
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=ConfigMapper.to_hostapd_band(band).default_channel(),
                ssid=self.ssid,
                setup_bridge=True,
                is_ipv6_enabled=True,
                is_nat_enabled=False,
            )

            ap_bridges = self.access_point.interfaces.get_bridge_interface()
            if not ap_bridges:
                raise signals.TestAbortClass(
                    f"Expected bridge interfaces on the AP, got {ap_bridges}"
                )
            ap_bridge = ap_bridges[0]
            self.ap_ipv4 = utils.get_addr(self.access_point.ssh, ap_bridge)
            self.ap_ipv6 = utils.get_addr(
                self.access_point.ssh, ap_bridge, addr_type="ipv6_link_local"
            )

        self.log.info(
            f"Gateway finished setup ({self.ap_ipv4} | {self.ap_ipv6})"
        )

        self.dut.associate(
            self.ssid, ConfigMapper.to_hostapd_security(security)
        )

        # Wait till the DUT has valid IP addresses after connecting.
        self.fuchsia_device.wait_for_ipv4_addr(
            self.dut.get_default_wlan_test_interface()
        )
        self.fuchsia_device.wait_for_ipv6_addr(
            self.dut.get_default_wlan_test_interface()
        )
        self.log.info("DUT has valid IP addresses on test network")

    def teardown_class(self) -> None:
        if hasattr(self, "dut"):
            self.dut.disconnect()
            self.dut.reset_wifi()
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()
        super().teardown_class()

    def send_ping(
        self,
        _: str,
        get_addr_fn: str | Callable[[Addrs], str],
        count: int = 3,
        interval: int = 500,
        timeout: int = 1000,
        size: int = 25,
        min_success: int | None = None,
    ) -> None:
        dest_ip = (
            get_addr_fn(
                Addrs(
                    gateway_ipv4=self.ap_ipv4,
                    # IPv6 link-local addresses require specification of the
                    # outgoing interface as the scope ID when sending packets.
                    gateway_ipv6=f"{self.ap_ipv6}%{self.dut.get_default_wlan_test_interface()}",
                )
            )
            if callable(get_addr_fn)
            else get_addr_fn
        )

        self.log.info(f"Attempting to ping {dest_ip}...")
        ping_result = self.dut.ping(dest_ip, count, interval, timeout, size)
        min_success = min_success or count
        if not ping_result.success:
            raise signals.TestFailure(
                f"Failed to ping {dest_ip}: {ping_result}"
            )
        asserts.assert_greater_equal(
            ping_result.received,
            min_success,
            f"Expected at least {min_success}/{count} packets received, but got {ping_result.received}/{count}",
        )
        self.log.info(
            f"Ping test to {dest_ip} passed ({ping_result.received}/{count})"
        )

    def test_simultaneous_pings(self) -> None:
        ping_urls = [
            self.ap_ipv4,
            f"{self.ap_ipv6}%{self.dut.get_default_wlan_test_interface()}",
        ]
        ping_processes: list[multiprocessing.Process] = []
        ping_results: list[PingResult] = []

        def ping_from_dut(
            self: PingTest, dest_ip: str, ping_results: list[PingResult]
        ) -> None:
            self.log.info(f"Attempting to ping {dest_ip}...")
            ping_result = self.dut.ping(dest_ip, count=10, size=50)
            if ping_result.success:
                self.log.info(f"Success pinging: {dest_ip}")
            else:
                self.log.info(f"Failure pinging: {dest_ip}")
            ping_results.append(ping_result)

        try:
            # Start multiple ping at the same time
            for index, url in enumerate(ping_urls):
                p = multiprocessing.Process(
                    target=ping_from_dut, args=(self, url, ping_results)
                )
                ping_processes.append(p)
                p.start()

            # Wait for all processes to complete or timeout
            for p in ping_processes:
                p.join(PING_RESULT_TIMEOUT_SEC)

        finally:
            last_alive_index = None
            for index, p in enumerate(ping_processes):
                if p.is_alive():
                    p.terminate()
                    last_alive_index = index

            if last_alive_index is not None:
                raise signals.TestFailure(
                    f"Timed out while pinging {ping_urls[last_alive_index]}"
                )

        for i, ping_result in enumerate(ping_results):
            if not ping_result.success:
                raise signals.TestFailure(
                    f"Failed to ping {ping_urls[i]}: {ping_result}"
                )


if __name__ == "__main__":
    test_runner.main()
