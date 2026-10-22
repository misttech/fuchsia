# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio

import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from iperf.iperf_server import IPerfServerOverSsh
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib.access_point_config import AccessPointConfig
from wlan_test_utils import odpm_utils, throughput_utils


class ThroughputTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    phy: wlan_core.Phy
    iperf_server: IPerfServerOverSsh
    dut_board: str
    dut_phy_caps: throughput_utils.WlanPhyCaps
    ap_phy_caps: throughput_utils.WlanPhyCaps
    reporter: throughput_utils.ThroughputReporter

    async def pre_run(self) -> None:
        self.generate_tests(
            self.run_channel_performance,
            throughput_utils.generate_test_name,
            throughput_utils.build_default_test_params(),
        )

    async def setup_class(self) -> None:
        await super().setup_class()
        self.phy = await self.dut.wlan_core.ensure_single_phy()
        await self.phy.set_country(
            KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]
        )
        self.iperf_server = await self.setup_iperf_server()

        self.dut_board = self.dut.board.lower()
        self.dut_phy_caps = throughput_utils.get_dut_phy_caps(self.dut_board)
        self.ap_phy_caps = throughput_utils.resolve_ap_phy_caps(
            self.openwrt_ap, self.access_point
        )

        self.reporter = throughput_utils.ThroughputReporter(
            self.log_path,
            fuchsiaperf_suite_name=throughput_utils.FUCHSIAPERF_SUITE_NAME,
        )
        self.reporter.write_initial_json()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def teardown_test(self) -> None:
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def teardown_class(self) -> None:
        try:
            self.reporter.finalize_reports()
        finally:
            await super().teardown_class()

    async def _sample_dut_metrics(
        self, iface: wlan_core.ClientIface
    ) -> throughput_utils.DutLinkMetrics:
        signal_report = await iface.get_signal_report()
        conn_report = getattr(signal_report, "connection_signal_report", None)
        if conn_report is None:
            raise signals.TestFailure(
                f"DUT signal report missing connection_signal_report: {signal_report}"
            )
        dut_rssi = getattr(conn_report, "rssi_dbm", None)
        if dut_rssi is None:
            raise signals.TestFailure(
                f"DUT connection signal report missing rssi_dbm: {conn_report}"
            )

        tx_rate_500kbps = getattr(conn_report, "tx_rate_500kbps", None)
        if tx_rate_500kbps is None:
            raise signals.TestFailure(
                f"DUT connection signal report missing tx_rate_500kbps: {conn_report}"
            )
        asserts.assert_greater(
            tx_rate_500kbps,
            0,
            f"DUT connection signal report non-positive tx_rate_500kbps: {tx_rate_500kbps}",
        )
        # Note: ConnectionSignalReport does not include an RX rate, so
        # rx_rate_mbps remains None.
        return throughput_utils.DutLinkMetrics(
            rssi=dut_rssi,
            tx_rate_mbps=round(tx_rate_500kbps * 0.5, 2),
        )

    async def _run_iperf_client(self, args: list[str]) -> str:
        cmd = f"iperf3 {' '.join(args)}"
        return await asyncio.to_thread(self.dut.ffx.run_ssh_cmd, cmd=cmd)

    async def run_channel_performance(
        self, test: throughput_utils.TestParams
    ) -> None:
        iface = await self.phy.create_client_iface()
        ssid = AccessPointConfig.random_string(8)
        password = throughput_utils.generate_password(test.security_mode)
        throughput_utils.configure_ap(
            self.openwrt_ap,
            test,
            ssid,
            password,
            access_point=self.access_point,
        )

        await iface.scan_and_connect(
            ssid=ssid,
            password=password,
            security=test.security_mode.to_fidl_wlan_internal(),
        )

        if self.openwrt_ap is None:
            # Necessary while the iperf server is separate from the AP
            self.iperf_server.renew_test_interface_ip_address()
        iperf_server_address = self.iperf_server.get_addr()

        # Wait for IP routing to settle before running throughput tests
        netstack_iface = await self.dut.netstack.wait_for_interface(
            PortClass.WLAN_CLIENT
        )
        dut_mac = await iface.get_mac_address()
        asserts.assert_equal(netstack_iface.mac, dut_mac)
        await self.dut.netstack.wait_for_ipv4_addr(netstack_iface.id_)

        max_phy_mbps = throughput_utils.theoretical_max_phy_mbps(
            self.dut_phy_caps,
            self.ap_phy_caps,
            test.band,
            test.channel_bandwidth,
        )

        async def _measure() -> throughput_utils.MeasurementsByDirection:
            return await throughput_utils.run_iperf_measurements(
                iperf_server_address=iperf_server_address,
                openwrt_ap=self.openwrt_ap,
                access_point=self.access_point,
                dut_mac=dut_mac,
                band=test.band,
                channel_bandwidth=test.channel_bandwidth,
                max_phy_mbps=max_phy_mbps,
                run_iperf_client=self._run_iperf_client,
                sample_dut_metrics=lambda: self._sample_dut_metrics(iface),
            )

        if test.record_odpm:
            async with odpm_utils.record_odpm_metrics(
                self.dut.tracing,
                test_case_path=self.test_case_path,
                test_name=self.current_test_info.name,
            ):
                measurements = await _measure()
        else:
            measurements = await _measure()

        self.reporter.record_test_case(test, measurements)
        throughput_utils.assert_phy_rate_targets(
            measurements, max_phy_mbps, self.dut_board
        )


if __name__ == "__main__":
    test_runner.main()
