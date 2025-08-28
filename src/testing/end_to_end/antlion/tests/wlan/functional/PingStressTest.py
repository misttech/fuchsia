#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
PingStressTest exercises sending ICMP and ICMPv6 pings to a wireless access
router and another device behind the AP. Note, this does not reach out to the
internet. The DUT is only responsible for sending a routable packet; any
communication past the first-hop is not the responsibility of the DUT.
"""

import logging
import multiprocessing
from typing import Callable, NamedTuple

from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from antlion.utils import PingResult, rand_ascii_str
from mobly import asserts, signals, test_runner

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


class PingStressTest(base_test.WifiBaseTest):
    def pre_run(self) -> None:
        self.generate_tests(
            self.send_ping,
            lambda test_name, *_: f"test_{test_name}",
            [
                Test("loopback_ipv4", LOOPBACK_IPV4),
                Test("loopback_ipv6", LOOPBACK_IPV6),
                Test("gateway_ipv4", lambda addrs: addrs.gateway_ipv4),
                Test("gateway_ipv6", lambda addrs: addrs.gateway_ipv6),
                Test(
                    "gateway_ipv4_small_packet",
                    lambda addrs: addrs.gateway_ipv4,
                ),
                Test(
                    "gateway_ipv6_small_packet",
                    lambda addrs: addrs.gateway_ipv6,
                ),
                Test(
                    "gateway_ipv4_small_packet_long",
                    lambda addrs: addrs.gateway_ipv4,
                    packet_count=50,
                ),
                Test(
                    "gateway_ipv6_small_packet_long",
                    lambda addrs: addrs.gateway_ipv6,
                    packet_count=50,
                ),
                Test(
                    "gateway_ipv4_medium_packet",
                    lambda addrs: addrs.gateway_ipv4,
                    size=64,
                ),
                Test(
                    "gateway_ipv6_medium_packet",
                    lambda addrs: addrs.gateway_ipv6,
                    size=64,
                ),
                Test(
                    "gateway_ipv4_medium_packet_long",
                    lambda addrs: addrs.gateway_ipv4,
                    packet_count=50,
                    timeout=1500,
                    size=64,
                ),
                Test(
                    "gateway_ipv6_medium_packet_long",
                    lambda addrs: addrs.gateway_ipv6,
                    packet_count=50,
                    timeout=1500,
                    size=64,
                ),
                Test(
                    "gateway_ipv4_large_packet",
                    lambda addrs: addrs.gateway_ipv4,
                    size=500,
                ),
                Test(
                    "gateway_ipv6_large_packet",
                    lambda addrs: addrs.gateway_ipv6,
                    size=500,
                ),
                Test(
                    "gateway_ipv4_large_packet_long",
                    lambda addrs: addrs.gateway_ipv4,
                    packet_count=50,
                    timeout=5000,
                    size=500,
                ),
                Test(
                    "gateway_ipv6_large_packet_long",
                    lambda addrs: addrs.gateway_ipv6,
                    packet_count=50,
                    timeout=5000,
                    size=500,
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

        if len(self.access_points) < 1:
            raise signals.TestAbortClass(
                "At least one access point is required"
            )
        self.access_point: AccessPoint = self.access_points[0]

        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            setup_bridge=True,
            is_ipv6_enabled=True,
            is_nat_enabled=False,
        )

        ap_bridges = self.access_point.interfaces.get_bridge_interface()
        if ap_bridges and len(ap_bridges) > 0:
            ap_bridge = ap_bridges[0]
        else:
            asserts.abort_class(
                f"Expected one bridge interface on the AP, got {ap_bridges}"
            )
        self.ap_ipv4 = utils.get_addr(self.access_point.ssh, ap_bridge)
        self.ap_ipv6 = utils.get_addr(
            self.access_point.ssh, ap_bridge, addr_type="ipv6_link_local"
        )
        self.log.info(
            f"Gateway finished setup ({self.ap_ipv4} | {self.ap_ipv6})"
        )

        self.dut.associate(self.ssid, SecurityMode.OPEN)

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
        self.access_point.stop_all_aps()
        super().teardown_class()

    def send_ping(
        self,
        _: str,
        get_addr_fn: str | Callable[[Addrs], str],
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
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
        if ping_result.success:
            self.log.info("Ping was successful.")
        else:
            raise signals.TestFailure(f"Ping was unsuccessful: {ping_result}")

    def test_simultaneous_pings(self) -> None:
        ping_urls = [
            self.ap_ipv4,
            f"{self.ap_ipv6}%{self.dut.get_default_wlan_test_interface()}",
        ]
        ping_processes: list[multiprocessing.Process] = []
        ping_results: list[PingResult] = []

        def ping_from_dut(
            self: PingStressTest, dest_ip: str, ping_results: list[PingResult]
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
            is_alive = False

            for index, p in enumerate(ping_processes):
                if p.is_alive():
                    p.terminate()
                    is_alive = True

            if is_alive:
                raise signals.TestFailure(
                    f"Timed out while pinging {ping_urls[index]}"
                )

        for i, ping_result in enumerate(ping_results):
            if not ping_result.success:
                raise signals.TestFailure(
                    f"Failed to ping {ping_urls[i]}: {ping_result}"
                )


if __name__ == "__main__":
    test_runner.main()
