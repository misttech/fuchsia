# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import logging
import os
from dataclasses import dataclass
from datetime import timedelta
from typing import Literal, TypedDict, overload

import fidl_fuchsia_wlan_internal as fidl_security
import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from antlion.controllers import iperf_server
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.iperf_server import IPerfServerOverSsh
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    HeMode,
    RadioConfig,
    Security,
    SecurityWpa2,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

logger = logging.getLogger(__name__)

DEFAULT_IPERF_DURATION: timedelta = timedelta(seconds=10)


class IperfUdpResult(TypedDict):
    udp_bps_uncorrected: int
    udp_bps_corrected: int
    udp_loss_percent: float
    server_cpu_utilization_percent: float


class IperfTcpResult(TypedDict):
    tcp_bps: int
    server_cpu_utilization_percent: float


class IperfResult(TypedDict):
    tcp_tx: IperfTcpResult
    tcp_rx: IperfTcpResult
    udp_tx: IperfUdpResult
    udp_rx: IperfUdpResult


@dataclass
class TestParams:
    security_mode: Security
    band: Band
    channel: int
    channel_bandwidth: Literal[20, 40, 80, 160]


class ThroughputTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    phy: wlan_core.Phy
    iperf_server: IPerfServerOverSsh

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.csv_file_path: str = os.path.join(self.log_path, "throughput.csv")

    async def pre_run(self) -> None:
        tests: list[tuple[TestParams]] = []

        def generate_test_name(test: TestParams) -> str:
            sec_name = ConfigMapper.to_hostapd_security(
                test.security_mode
            ).value
            return f"test_{sec_name}_channel_{test.channel}_{test.channel_bandwidth}mhz"

        channels_and_bandwidths: list[
            tuple[Band, int, Literal[20, 40, 80, 160]]
        ] = [
            # 20MHz is the only realistic BW in 2.4GHz
            (Band.BAND_2G, 1, 20),
            # Highest US channel, 20MHz to compare to 2.4GHz
            (Band.BAND_5G, 165, 20),
            # Widest possible bandwidth in 5GHz supported by Whirlwind APs.
            # Once we move to OpenWRT One, we can raise this to 160MHz.
            (Band.BAND_5G, 36, 80),
        ]
        security_modes: list[Security] = [
            SecurityWpa2(),
        ]
        for security_mode in security_modes:
            for band, channel, bandwidth in channels_and_bandwidths:
                test = TestParams(
                    security_mode=security_mode,
                    band=band,
                    channel=channel,
                    channel_bandwidth=bandwidth,
                )
                tests.append((test,))

        self.generate_tests(
            self.run_channel_performance, generate_test_name, tests
        )

    async def setup_class(self) -> None:
        await super().setup_class()
        self.phy = await self.dut.wlan_core.ensure_single_phy()
        await self.phy.set_country(
            KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]
        )

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

        with open(self.csv_file_path, "w", encoding="utf-8") as csv_file:
            csv_file.write(
                "security,channel,channel_bandwidth,tcp_tx_mbps,tcp_rx_mbps,udp_tx_mbps,udp_rx_mbps\n"
            )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def teardown_test(self) -> None:
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    @overload
    async def get_iperf_throughput_bps(
        self,
        iperf_server_address: str,
        reverse: bool,
        udp: Literal[False],
        bandwidth: None = None,
    ) -> IperfTcpResult:
        ...

    @overload
    async def get_iperf_throughput_bps(
        self,
        iperf_server_address: str,
        reverse: bool,
        udp: Literal[True],
        bandwidth: str | None = None,
    ) -> IperfUdpResult:
        ...

    async def get_iperf_throughput_bps(
        self,
        iperf_server_address: str,
        reverse: bool,
        udp: bool,
        bandwidth: str | None = None,
    ) -> IperfUdpResult | IperfTcpResult:
        args = [
            "--client",
            iperf_server_address,
            "--time",
            str(int(DEFAULT_IPERF_DURATION.total_seconds())),
            "--format",
            "m",  # 'm' = Mbits/sec
            "--json",
        ]
        if udp:
            args.extend(["--udp", "--bandwidth", bandwidth or "0"])
        if reverse:
            args.append("--reverse")

        output = self.dut.ffx.run_ssh_cmd(
            cmd=f"iperf3 {' '.join(args)}",
        )

        try:
            data = json.loads(output)
            if "error" in data:
                raise signals.TestError(f"iperf3 error: {data['error']}")

            end_data = data.get("end", {})

            server_cpu_utilization_percent = end_data.get(
                "cpu_utilization_percent", {}
            ).get("remote_total")
            if server_cpu_utilization_percent is None:
                logger.error(
                    f"Could not extract cpu_utilization_percent from iperf3 JSON: {data}"
                )
                raise signals.TestError(
                    "Could not extract cpu_utilization_percent from iperf3 JSON"
                )

            if udp:
                raw_udp_bps = end_data.get("sum", {}).get("bits_per_second")
                lost_percent = end_data.get("sum", {}).get("lost_percent")
                if raw_udp_bps is None:
                    logger.error(
                        f"Could not extract bits_per_second from iperf3 JSON: {data}"
                    )
                    raise signals.TestError(
                        "Could not extract bits_per_second from iperf3 JSON"
                    )
                bps = raw_udp_bps * (1.0 - (lost_percent / 100.0))

                res_udp: IperfUdpResult = {
                    "udp_bps_uncorrected": raw_udp_bps,
                    "udp_bps_corrected": bps,
                    "udp_loss_percent": lost_percent,
                    "server_cpu_utilization_percent": server_cpu_utilization_percent,
                }
                return res_udp
            else:
                bps = end_data.get("sum_received", {}).get("bits_per_second")
                if bps is None:
                    logger.error(
                        f"Could not extract bits_per_second from iperf3 JSON: {data}"
                    )
                    raise signals.TestError(
                        "Could not extract bits_per_second from iperf3 JSON"
                    )

                res_tcp: IperfTcpResult = {
                    "tcp_bps": bps,
                    "server_cpu_utilization_percent": server_cpu_utilization_percent,
                }
                return res_tcp

        except json.JSONDecodeError as e:
            logger.error("Failed to parse data from command output:")
            logger.error(output)
            raise signals.TestError(f"Invalid JSON from iperf3: {e}") from e

    async def run_channel_performance(self, test: TestParams) -> None:
        iface = await self.phy.create_client_iface()
        ssid = AccessPointConfig.random_string(8)
        password = AccessPointConfig.random_string(8)
        security = test.security_mode
        band = test.band

        if self.openwrt_ap:
            # HE / Wifi 6 / 802.11ax is the latest supported by OpenWRT One
            phy_mode = HeMode(bw=test.channel_bandwidth)
            self.openwrt_ap.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=BssChannel(
                                band=band,
                                number=test.channel,
                                phy_mode=phy_mode,
                            ),
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password,
                                    security=security,
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(self.access_point, AccessPoint):
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=test.channel,
                ssid=ssid,
                vht_bandwidth=test.channel_bandwidth,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                ),
                force_wmm=True,  # Gets maximum speed in 802.11ac
                setup_bridge=True,  # Necessary for the separate iperf server
            )

        await iface.scan_and_connect(
            ssid=ssid,
            password=password,
            security=fidl_security.Protocol.WPA2_PERSONAL,
        )

        if self.openwrt_ap is None:
            # Necessary while the iperf server is separate from the AP
            self.iperf_server.renew_test_interface_ip_address()
        iperf_server_address = self.iperf_server.get_addr()

        # Wait for IP routing to settle before running throughput tests
        netstack_iface = await self.dut.netstack.wait_for_interface(
            PortClass.WLAN_CLIENT
        )
        asserts.assert_equal(netstack_iface.mac, await iface.get_mac_address())
        await self.dut.netstack.wait_for_ipv4_addr(netstack_iface.id_)

        udp_bandwidth = self.get_target_udp_bandwidth(test.channel_bandwidth)
        tcp_tx = await self.get_iperf_throughput_bps(
            iperf_server_address, reverse=False, udp=False
        )
        tcp_rx = await self.get_iperf_throughput_bps(
            iperf_server_address, reverse=True, udp=False
        )
        udp_tx = await self.get_iperf_throughput_bps(
            iperf_server_address,
            reverse=False,
            udp=True,
            bandwidth=udp_bandwidth,
        )
        udp_rx = await self.get_iperf_throughput_bps(
            iperf_server_address,
            reverse=True,
            udp=True,
            bandwidth=udp_bandwidth,
        )

        tcp_tx_mbps = self.bps_to_mbps(tcp_tx["tcp_bps"])
        tcp_rx_mbps = self.bps_to_mbps(tcp_rx["tcp_bps"])
        udp_tx_mbps = self.bps_to_mbps(udp_tx["udp_bps_corrected"])
        udp_rx_mbps = self.bps_to_mbps(udp_rx["udp_bps_corrected"])

        logger.info(
            f"Throughput Result (Channel {test.channel}/{test.channel_bandwidth}MHz) "
            + f"TCP TX: {tcp_tx_mbps} Mbps, TCP RX: {tcp_rx_mbps} Mbps, "
            + f"UDP TX: {udp_tx_mbps} Mbps, UDP RX: {udp_rx_mbps} Mbps"
        )

        sec_name = ConfigMapper.to_hostapd_security(test.security_mode).value
        with open(self.csv_file_path, "a", encoding="utf-8") as csv_file:
            csv_file.write(
                f"{sec_name},{test.channel},{test.channel_bandwidth},"
                + f"{tcp_tx_mbps},{tcp_rx_mbps},{udp_tx_mbps},{udp_rx_mbps}\n"
            )

    @staticmethod
    def get_target_udp_bandwidth(channel_bandwidth: int) -> str:
        """Determines target UDP bitrate scaled to channel width for iperf3.

        Why this is needed:
            In iperf3 UDP tests, setting unlimited bandwidth (`--bandwidth 0`)
            causes the sender (especially the wired iperf server in reverse mode)
            to transmit at line rate (1 Gbps) into the Access Point.
            On narrower channels (e.g. 2.4 GHz 20 MHz where link capacity is only
            ~30-50 Mbps), this massive rate mismatch causes severe bufferbloat and
            >95% packet loss in the AP queues. This drops the TCP control socket
            packets on port 5201, causing iperf3 to abort with "control socket has
            closed unexpectedly".

            Setting the target UDP bandwidth to slightly exceed the theoretical
            maximum physical layer (PHY) capacity ensures the wireless medium is
            fully saturated without overwhelming the AP's packet queues.

        Calculation per channel bandwidth:
            - 20 MHz -> "150M": Max PHY rates are 72.2 Mbps (802.11n 1x1),
              144.4 Mbps (802.11n 2x2), and 143.4 Mbps (802.11ax 1x1).
              150 Mbps ensures saturation while keeping queue drops manageable.
            - 40 MHz -> "300M": Max PHY rates are ~150-300 Mbps (802.11n/ac)
              and 286.8 Mbps (802.11ax 1x1).
            - 80 MHz -> "600M": Max PHY rate for 802.11ac 1x1 is 433.3 Mbps
              (and typical 2x2 real-world throughput is ~400-500 Mbps).
            - 160 MHz -> "1000M": Capped at 1 Gbps to match the wired Ethernet
              link speed between the iperf server and the Access Point.
        """
        if channel_bandwidth <= 20:
            return "150M"
        elif channel_bandwidth <= 40:
            return "300M"
        elif channel_bandwidth <= 80:
            return "600M"
        else:
            return "1000M"

    @staticmethod
    def bps_to_mbps(bps: int | float) -> float:
        throughput_mbps = float(bps) / 1_000_000
        return round(throughput_mbps, 2)


if __name__ == "__main__":
    test_runner.main()
