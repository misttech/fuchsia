# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging
import time

import attenuator
import fuchsia_base_test
import openwrt_access_point
from attenuator import Attenuator
from honeydew.affordances.connectivity import netstack
from honeydew.affordances.connectivity.netstack.types import (
    InterfaceProperties,
    PortClass,
)
from iperf import iperf_server
from iperf.iperf_server import IPerfServerOverSsh
from legacy_access_point import access_point
from mobly import signals
from mobly.config_parser import TestRunConfig
from openwrt_access_point import OpenWrtAP

# Time to wait for a WLAN interface to become available.
INTERFACE_TIMEOUT = 30


class FuchsiaWlanBaseTest(fuchsia_base_test.FuchsiaBaseTest):
    """Wlan base test class."""

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.openwrt_ap: OpenWrtAP | None = None
        self.access_point: access_point.AccessPoint | None = None
        self.access_points: list[access_point.AccessPoint] = []
        self.openwrt_aps: list[OpenWrtAP] = []
        self.attenuators: list[Attenuator] = []
        self.iperf_server: IPerfServerOverSsh | None = None
        self.test_start_marker: str | None = None

    async def setup_class(self) -> None:
        await super().setup_class()

        self.access_points = (
            await self.register_controller(
                access_point,
                required=False,
            )
            or []
        )
        self.openwrt_aps = (
            await self.register_controller(
                openwrt_access_point,
                required=False,
            )
            or []
        )
        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]

        self.attenuators = (
            await self.register_controller(
                attenuator,
                required=False,
            )
            or []
        )
        for attn in self.attenuators:
            attn.set_atten(0)

    async def setup_test(self) -> None:
        await super().setup_test()
        import uuid

        self.test_start_marker = (
            f"MOBLY_TEST_START: {self.current_test_info.name}_{uuid.uuid4()}"
        )
        for openwrt_ap in self.openwrt_aps:
            openwrt_ap.log_to_syslog(self.test_start_marker)

    async def setup_iperf_server(self) -> IPerfServerOverSsh:
        """Resolves and starts the single iPerf server in the testbed.

        Returns:
            The resolved and started IPerfServerOverSsh instance.
        """
        iperf_servers: list[IPerfServerOverSsh] = (
            await self.register_controller(
                iperf_server,
                required=False,
            )
            or []
        )
        if self.openwrt_ap is not None:
            self.iperf_server = self.openwrt_ap.iperf_server
        elif iperf_servers:
            self.iperf_server = iperf_servers[0]
        else:
            raise signals.TestError("Requires at least one iperf server")

        self.iperf_server.start()
        return self.iperf_server

    def _download_ap_logs(self, directory: str) -> None:
        """Downloads DHCP and hostapd logs from access points, and iPerf server logs if present."""
        for access_point in self.access_points:
            try:
                access_point.download_ap_logs(directory)
            except Exception as e:
                logging.warning(f"Failed to download DHCP/hostapd logs: {e}")
        for openwrt_ap in self.openwrt_aps:
            try:
                openwrt_ap.download_logs(directory, start_marker=None)
            except Exception as e:
                logging.warning(f"Failed to download OpenWrt logs: {e}")
        if self.iperf_server is not None:
            try:
                self.iperf_server.download_logs(directory)
            except Exception as e:
                logging.warning(f"Failed to download iPerf server logs: {e}")

    async def teardown_test(self) -> None:
        for openwrt_ap in self.openwrt_aps:
            try:
                openwrt_ap.download_logs(
                    self.test_case_path, start_marker=self.test_start_marker
                )
            except Exception as e:
                logging.warning(f"Failed to download OpenWrt logs: {e}")
        await super().teardown_test()

    async def teardown_class(self) -> None:
        self._download_ap_logs(self.log_path)
        await super().teardown_class()

    # TODO(b/537123090): Move to netstack affordance
    async def wait_for_interface(
        self, netstack: netstack.Netstack, port_class: PortClass
    ) -> str:
        """Wait for an interface to become available.

        Args:
            netstack: Netstack affordance
            port_class: Desired type of interface

        Return:
            interface name

        Raises:
            TestAbortClass: Desired interface does not exist
        """
        interfaces: list[InterfaceProperties] = []
        end_time = time.time() + INTERFACE_TIMEOUT
        while time.time() < end_time:
            interfaces = await netstack.list_interfaces()
            for interface in interfaces:
                if interface.port_class is port_class:
                    return interface.name
            await asyncio.sleep(1)  # Prevent denial-of-service
        raise signals.TestAbortClass(
            f"Expected presence of a {port_class.name} interface, got {interfaces}"
        )
