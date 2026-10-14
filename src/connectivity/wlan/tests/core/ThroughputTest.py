# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import enum
import html
import json
import logging
import os
from dataclasses import dataclass
from datetime import timedelta
from typing import Any, Literal, TypedDict, get_args, overload

import fidl_fuchsia_wlan_internal as fidl_security
import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
import perf_publish.publish as publish
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from honeydew.typing.custom_types import MacAddress
from iperf.iperf_server import IPerfServerOverSsh
from legacy_access_point.access_point import AccessPoint, setup_ap
from legacy_access_point.ap_lib.hostapd import (
    StationStatus as HostapdStationStatus,
)
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from openwrt_access_point import StationStatus as OpenWrtStationStatus
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
from wlan_test_utils import odpm_utils

logger = logging.getLogger(__name__)

IPERF_DURATION: timedelta = timedelta(seconds=10)
FUCHSIAPERF_SUITE_NAME: str = "fuchsia.wlan.throughput_test"
EXPECTED_METRICS_FILENAME: str = "throughput_expected_metrics.txt"
PHY_RATE_TARGET_RATIO: float = 0.8


class PhyGen(enum.IntEnum):
    HT = 4  # 802.11n
    VHT = 5  # 802.11ac
    HE = 6  # 802.11ax
    EHT = 7  # 802.11be


@dataclass(frozen=True)
class WlanPhyCaps:
    gen: PhyGen
    nss: int


# Max PHY rate per 1 SS at top MCS + shortest GI. Store unrounded values and
# round only the final result (e.g. VHT80 2SS = 866.7, not 2 x 433.3 = 866.6).
# HT: MCS7 400ns; VHT: MCS9 400ns (MCS8 @20); HE: MCS11 0.8us; EHT: MCS13 0.8us.
_MAX_PHY_RATE_1SS_MBPS: dict[tuple[PhyGen, int], float] = {
    (PhyGen.HT, 20): 72.2222,
    (PhyGen.HT, 40): 150.0,
    (PhyGen.VHT, 20): 86.6667,
    (PhyGen.VHT, 40): 200.0,
    (PhyGen.VHT, 80): 433.3333,
    (PhyGen.VHT, 160): 866.6667,
    (PhyGen.HE, 20): 143.3824,
    (PhyGen.HE, 40): 286.7647,
    (PhyGen.HE, 80): 600.4902,
    (PhyGen.HE, 160): 1200.9804,
    (PhyGen.EHT, 20): 172.0588,
    (PhyGen.EHT, 40): 344.1176,
    (PhyGen.EHT, 80): 720.5882,
    (PhyGen.EHT, 160): 1441.1765,
}

# Keyed by `ffx target show` board name.
DUT_PHY_CAPS: dict[str, WlanPhyCaps] = {
    "astro": WlanPhyCaps(PhyGen.VHT, nss=1),
    "sherlock": WlanPhyCaps(PhyGen.VHT, nss=1),
    "nelson": WlanPhyCaps(PhyGen.VHT, nss=1),
    "iris": WlanPhyCaps(PhyGen.EHT, nss=2),
}

OPENWRT_ONE_CAPS = WlanPhyCaps(PhyGen.HE, nss=2)
WHIRLWIND_CAPS = WlanPhyCaps(PhyGen.VHT, nss=3)


def theoretical_max_phy_mbps(
    dut: WlanPhyCaps, ap: WlanPhyCaps, band: Band, bw: int
) -> float:
    gen = min(dut.gen, ap.gen)
    if band == Band.BAND_2G and gen == PhyGen.VHT:
        gen = PhyGen.HT  # VHT is 5 GHz only
    if gen == PhyGen.HT:
        bw = min(bw, 40)  # HT maxes out at 40 MHz
    nss = min(dut.nss, ap.nss)
    return round(_MAX_PHY_RATE_1SS_MBPS[(gen, bw)] * nss, 1)


def phy_rate_target_mbps(phy_max_mbps: float) -> float:
    return round(PHY_RATE_TARGET_RATIO * phy_max_mbps, 1)


LinkMetricKey = Literal[
    "ap_rssi",
    "ap_snr",
    "ap_tx_rate_mbps",
    "ap_rx_rate_mbps",
    "dut_rssi",
    "dut_tx_rate_mbps",
    "dut_rx_rate_mbps",
]
LINK_METRIC_KEYS: tuple[LinkMetricKey, ...] = get_args(LinkMetricKey)

StatKey = Literal["avg", "min", "max"]
STAT_KEYS: tuple[StatKey, ...] = get_args(StatKey)


# ---------------------------------------------------------------------------
# JSON Artifact Schema (schema_version: 1)
# ---------------------------------------------------------------------------


class MetricStatsDict(TypedDict):
    avg: float | None
    min: float | int | None
    max: float | int | None


class LinkMetricsDict(TypedDict):
    ap_rssi: MetricStatsDict
    ap_snr: MetricStatsDict
    ap_tx_rate_mbps: MetricStatsDict
    ap_rx_rate_mbps: MetricStatsDict
    dut_rssi: MetricStatsDict
    dut_tx_rate_mbps: MetricStatsDict
    dut_rx_rate_mbps: MetricStatsDict


class TcpIperfDetails(TypedDict):
    server_cpu_utilization_percent: float
    raw_bps: float | int


class UdpIperfDetails(TypedDict):
    server_cpu_utilization_percent: float
    raw_bps: float | int
    corrected_bps: float
    loss_percent: float | int


class MeasurementRecord(TypedDict):
    """Common link quality and throughput metrics for a measurement.

    Note:
        `direction` is always relative to the DUT:
        - "tx": DUT transmits to AP (iperf client upload).
        - "rx": DUT receives from AP (iperf client download via --reverse).
    """

    direction: Literal["tx", "rx"]
    throughput_mbps: float
    phy_mode: str | None
    nss: int | None
    phy_max_mbps: float
    link_metrics: LinkMetricsDict


class TcpMeasurementRecord(MeasurementRecord):
    protocol: Literal["tcp"]
    iperf_details: TcpIperfDetails


class UdpMeasurementRecord(MeasurementRecord):
    protocol: Literal["udp"]
    iperf_details: UdpIperfDetails


class MeasurementsByDirection(TypedDict):
    tcp_tx: TcpMeasurementRecord
    tcp_rx: TcpMeasurementRecord
    udp_tx: UdpMeasurementRecord
    udp_rx: UdpMeasurementRecord


class TestCaseRecord(TypedDict):
    security: str
    channel: int
    channel_bandwidth: int
    band: str
    measurements: MeasurementsByDirection


class ThroughputReportJson(TypedDict):
    schema_version: int
    test_cases: list[TestCaseRecord]


# ---------------------------------------------------------------------------
# Link Quality & Rate Sampling Classes (In-Memory)
# ---------------------------------------------------------------------------


@dataclass
class LinkMetricSample:
    """Represents link quality and rate metrics perceived by AP and DUT at a single point in time."""

    ap_rssi: int | None = None
    ap_snr: int | None = None
    ap_tx_rate_mbps: float | None = None
    ap_rx_rate_mbps: float | None = None
    ap_phy_mode: str | None = None
    ap_nss: int | None = None
    dut_rssi: int | None = None
    dut_tx_rate_mbps: float | None = None
    dut_rx_rate_mbps: float | None = None


@dataclass
class MetricStats:
    avg: float | None = None
    min: float | int | None = None
    max: float | int | None = None

    @classmethod
    def from_values(
        cls, values: list[int | float | None], round_digits: int = 1
    ) -> "MetricStats":
        valid = [v for v in values if v is not None]
        if not valid:
            return cls()
        return cls(
            avg=round(sum(valid) / len(valid), round_digits),
            min=min(valid),
            max=max(valid),
        )

    def to_dict(self) -> MetricStatsDict:
        return {
            "avg": self.avg,
            "min": self.min,
            "max": self.max,
        }


@dataclass
class LinkMetricSummary:
    """Aggregated link quality and rate statistics across all measurement samples."""

    ap_rssi: MetricStats
    ap_snr: MetricStats
    ap_tx_rate_mbps: MetricStats
    ap_rx_rate_mbps: MetricStats
    dut_rssi: MetricStats
    dut_tx_rate_mbps: MetricStats
    dut_rx_rate_mbps: MetricStats
    phy_mode: str | None
    nss: int | None
    samples: list[LinkMetricSample]

    @classmethod
    def from_samples(
        cls, samples: list[LinkMetricSample]
    ) -> "LinkMetricSummary":
        # Find the most recent valid phy_mode and nss from the collected samples,
        # reflecting the steady-state negotiated configuration under load.
        phy_mode: str | None = None
        for s in reversed(samples):
            if s.ap_phy_mode:
                phy_mode = s.ap_phy_mode
                break

        nss: int | None = None
        for s in reversed(samples):
            if s.ap_nss is not None:
                nss = s.ap_nss
                break

        return cls(
            ap_rssi=MetricStats.from_values(
                [s.ap_rssi for s in samples], round_digits=1
            ),
            ap_snr=MetricStats.from_values(
                [s.ap_snr for s in samples], round_digits=1
            ),
            ap_tx_rate_mbps=MetricStats.from_values(
                [s.ap_tx_rate_mbps for s in samples], round_digits=1
            ),
            ap_rx_rate_mbps=MetricStats.from_values(
                [s.ap_rx_rate_mbps for s in samples], round_digits=1
            ),
            dut_rssi=MetricStats.from_values(
                [s.dut_rssi for s in samples], round_digits=1
            ),
            dut_tx_rate_mbps=MetricStats.from_values(
                [s.dut_tx_rate_mbps for s in samples], round_digits=1
            ),
            dut_rx_rate_mbps=MetricStats.from_values(
                [s.dut_rx_rate_mbps for s in samples], round_digits=1
            ),
            phy_mode=phy_mode,
            nss=nss,
            samples=samples,
        )

    def format_summary(self) -> str:
        def fmt_rate(val: float | None) -> str:
            return f"{val:.1f} Mbps" if val is not None else "N/A"

        def fmt_rssi(val: float | None) -> str:
            return f"{val:.1f} dBm" if val is not None else "N/A"

        def fmt_snr(val: float | None) -> str:
            return f"{val:.1f} dB" if val is not None else "N/A"

        mode_str = self.phy_mode or "N/A"
        nss_str = str(self.nss) if self.nss is not None else "N/A"

        return (
            f"AP PHY (Mode: {mode_str}, NSS: {nss_str}, "
            f"RSSI: {fmt_rssi(self.ap_rssi.avg)}, "
            f"SNR: {fmt_snr(self.ap_snr.avg)}, "
            f"TX: {fmt_rate(self.ap_tx_rate_mbps.avg)}, "
            f"RX: {fmt_rate(self.ap_rx_rate_mbps.avg)}), "
            f"DUT PHY (RSSI: {fmt_rssi(self.dut_rssi.avg)}, "
            f"TX: {fmt_rate(self.dut_tx_rate_mbps.avg)}, "
            f"RX: {fmt_rate(self.dut_rx_rate_mbps.avg)})"
        )

    def format_sample_table(self) -> list[str]:
        if not self.samples:
            return []

        def r_val(val: float | None) -> str:
            return f"{val:.1f}" if val is not None else "N/A"

        def v_val(val: int | None) -> str:
            return str(val) if val is not None else "N/A"

        rows = [
            (
                "  RSSI (ap/dut):",
                [
                    f"{v_val(s.ap_rssi)}/{v_val(s.dut_rssi)}"
                    for s in self.samples
                ],
            ),
            (
                "  SNR (ap):",
                [f"{v_val(s.ap_snr)}" for s in self.samples],
            ),
            (
                "  PHY (ap_tx/dut_rx):",
                [
                    f"{r_val(s.ap_tx_rate_mbps)}/{r_val(s.dut_rx_rate_mbps)}"
                    for s in self.samples
                ],
            ),
            (
                "  PHY (ap_rx/dut_tx):",
                [
                    f"{r_val(s.ap_rx_rate_mbps)}/{r_val(s.dut_tx_rate_mbps)}"
                    for s in self.samples
                ],
            ),
        ]
        col_width = max(len(c) for _, cells in rows for c in cells)
        prefix_width = max(len(prefix) for prefix, _ in rows) + 1
        return [
            f"{prefix.ljust(prefix_width)}{'  '.join(c.ljust(col_width) for c in cells)}"
            for prefix, cells in rows
        ]

    def to_link_metrics_dict(self) -> LinkMetricsDict:
        return {
            "ap_rssi": self.ap_rssi.to_dict(),
            "ap_snr": self.ap_snr.to_dict(),
            "ap_tx_rate_mbps": self.ap_tx_rate_mbps.to_dict(),
            "ap_rx_rate_mbps": self.ap_rx_rate_mbps.to_dict(),
            "dut_rssi": self.dut_rssi.to_dict(),
            "dut_tx_rate_mbps": self.dut_tx_rate_mbps.to_dict(),
            "dut_rx_rate_mbps": self.dut_rx_rate_mbps.to_dict(),
        }


# ---------------------------------------------------------------------------
# iperf3 Controller Models
# ---------------------------------------------------------------------------


class IperfUdpResult(TypedDict):
    udp_bps_uncorrected: float | int
    udp_bps_corrected: float
    udp_loss_percent: float | int
    server_cpu_utilization_percent: float


class IperfTcpResult(TypedDict):
    tcp_bps: float | int
    server_cpu_utilization_percent: float


# ---------------------------------------------------------------------------
# Test Execution Parameters
# ---------------------------------------------------------------------------


@dataclass
class TestParams:
    security_mode: Security
    band: Band
    channel: int
    channel_bandwidth: Literal[20, 40, 80, 160]
    record_odpm: bool = False


class ThroughputTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    phy: wlan_core.Phy
    iperf_server: IPerfServerOverSsh
    dut_board: str
    dut_phy_caps: WlanPhyCaps
    ap_phy_caps: WlanPhyCaps

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.json_file_path: str = os.path.join(
            self.log_path, "throughput.json"
        )
        self.html_file_path: str = os.path.join(
            self.log_path, "throughput.html"
        )
        self.fuchsiaperf_file_path: str = os.path.join(
            self.log_path, "throughput.fuchsiaperf.json"
        )
        self.test_cases: list[TestCaseRecord] = []

    async def pre_run(self) -> None:
        tests: list[tuple[TestParams]] = []

        def generate_test_name(test: TestParams) -> str:
            sec_name = ConfigMapper.to_hostapd_security(
                test.security_mode
            ).value
            return f"test_{sec_name}_channel_{test.channel}_{test.channel_bandwidth}mhz"

        channels_and_bandwidths: list[
            tuple[Band, int, Literal[20, 40, 80, 160], bool]
        ] = [
            # 20MHz is the only realistic BW in 2.4GHz
            (Band.BAND_2G, 1, 20, True),
            # Highest US channel, 20MHz to compare to 2.4GHz
            (Band.BAND_5G, 165, 20, False),
            # Widest possible bandwidth in 5GHz supported by Whirlwind APs.
            # Once we move to OpenWRT One, we can raise this to 160MHz.
            (Band.BAND_5G, 36, 80, True),
        ]
        security_modes: list[Security] = [
            SecurityWpa2(),
        ]
        for security_mode in security_modes:
            for (
                band,
                channel,
                bandwidth,
                record_odpm,
            ) in channels_and_bandwidths:
                test = TestParams(
                    security_mode=security_mode,
                    band=band,
                    channel=channel,
                    channel_bandwidth=bandwidth,
                    record_odpm=record_odpm,
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
        self.iperf_server = await self.setup_iperf_server()

        self.dut_board = self.dut.board.lower()
        if self.dut_board not in DUT_PHY_CAPS:
            raise signals.TestFailure(
                f"No PHY caps known for board '{self.dut_board}'"
            )
        self.dut_phy_caps = DUT_PHY_CAPS[self.dut_board]

        if self.openwrt_ap is not None:
            self.ap_phy_caps = OPENWRT_ONE_CAPS
        elif isinstance(self.access_point, AccessPoint):
            self.ap_phy_caps = WHIRLWIND_CAPS
        else:
            raise signals.TestFailure("No supported AP controller available")

        self.test_cases = []
        self._write_json_report()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def teardown_test(self) -> None:
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def teardown_class(self) -> None:
        try:
            self._update_html_report()
            self._publish_fuchsiaperf_report()
        finally:
            await super().teardown_class()

    async def _measure_link_metrics(
        self, iface: wlan_core.ClientIface, dut_mac: MacAddress, band: Band
    ) -> LinkMetricSample:
        metrics = LinkMetricSample()

        # 1. Query AP perspective
        ap_status: OpenWrtStationStatus | HostapdStationStatus
        if self.openwrt_ap is not None:
            ap_status = await asyncio.to_thread(
                self.openwrt_ap.get_sta_status, dut_mac, band
            )
        elif isinstance(self.access_point, AccessPoint):
            ap_iface = (
                self.access_point.wlan_2g
                if band == Band.BAND_2G
                else self.access_point.wlan_5g
            )
            ap_status = await asyncio.to_thread(
                self.access_point.get_sta_status, ap_iface, dut_mac
            )
        else:
            raise RuntimeError(
                "No AP controller available to query station status"
            )
        if ap_status.rssi is None:
            raise signals.TestFailure(
                f"Expected AP RSSI to be present for {dut_mac}: {ap_status}"
            )
        if ap_status.tx_rate_mbps is None:
            raise signals.TestFailure(
                f"Expected AP TX PHY rate to be present for {dut_mac}: {ap_status}"
            )
        asserts.assert_greater(
            ap_status.tx_rate_mbps,
            0,
            f"Expected positive AP TX PHY rate for {dut_mac}: {ap_status}",
        )
        if ap_status.rx_rate_mbps is None:
            raise signals.TestFailure(
                f"Expected AP RX PHY rate to be present for {dut_mac}: {ap_status}"
            )
        asserts.assert_greater(
            ap_status.rx_rate_mbps,
            0,
            f"Expected positive AP RX PHY rate for {dut_mac}: {ap_status}",
        )
        metrics.ap_rssi = ap_status.rssi
        metrics.ap_snr = ap_status.snr
        metrics.ap_tx_rate_mbps = ap_status.tx_rate_mbps
        metrics.ap_rx_rate_mbps = ap_status.rx_rate_mbps
        metrics.ap_phy_mode = ap_status.phy_mode
        metrics.ap_nss = ap_status.nss

        # 2. Query DUT perspective
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
        metrics.dut_rssi = dut_rssi

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
        metrics.dut_tx_rate_mbps = round(tx_rate_500kbps * 0.5, 2)
        # Note: ConnectionSignalReport does not include an RX rate, so
        # metrics.dut_rx_rate_mbps remains None.

        return metrics

    @overload
    async def get_iperf_throughput_bps(
        self,
        iperf_server_address: str,
        reverse: bool,
        udp: Literal[False],
        iface: wlan_core.ClientIface,
        dut_mac: MacAddress,
        band: Band,
        bandwidth: None = None,
    ) -> tuple[IperfTcpResult, LinkMetricSummary]:
        ...

    @overload
    async def get_iperf_throughput_bps(
        self,
        iperf_server_address: str,
        reverse: bool,
        udp: Literal[True],
        iface: wlan_core.ClientIface,
        dut_mac: MacAddress,
        band: Band,
        bandwidth: str | None = None,
    ) -> tuple[IperfUdpResult, LinkMetricSummary]:
        ...

    async def get_iperf_throughput_bps(
        self,
        iperf_server_address: str,
        reverse: bool,
        udp: bool,
        iface: wlan_core.ClientIface,
        dut_mac: MacAddress,
        band: Band,
        bandwidth: str | None = None,
    ) -> tuple[IperfUdpResult | IperfTcpResult, LinkMetricSummary]:
        args = [
            "--client",
            iperf_server_address,
            "--time",
            str(int(IPERF_DURATION.total_seconds())),
            "--format",
            "m",  # 'm' = Mbits/sec
            "--json",
        ]
        if udp:
            args.extend(["--udp", "--bandwidth", bandwidth or "0"])
        if reverse:
            args.append("--reverse")

        # Launch iperf3 in a background thread so link measurements can be taken mid-test
        cmd = f"iperf3 {' '.join(args)}"
        iperf_task = asyncio.create_task(
            asyncio.to_thread(self.dut.ffx.run_ssh_cmd, cmd=cmd)
        )

        start_time = asyncio.get_running_loop().time()
        samples: list[LinkMetricSample] = []
        try:
            loop = asyncio.get_running_loop()
            # Sample link metrics once every second during the test run, starting at
            # t = 1s and ending before the iperf duration elapses.
            for target_second in range(1, int(IPERF_DURATION.total_seconds())):
                # Compensate for measurement drift to maintain 1-second intervals.
                sleep_duration = (start_time + target_second) - loop.time()
                if sleep_duration > 0:
                    done, _ = await asyncio.wait(
                        [iperf_task], timeout=sleep_duration
                    )
                    if done:
                        break
                elif iperf_task.done():
                    break

                samples.append(
                    await self._measure_link_metrics(iface, dut_mac, band)
                )

            # Wait for iperf3 to complete
            output = await iperf_task
        except Exception:
            if not iperf_task.done():
                iperf_task.cancel()
            raise

        # Check for iperf3 errors before verifying samples were collected
        try:
            data = json.loads(output)
            if "error" in data:
                raise signals.TestError(f"iperf3 error: {data['error']}")
        except json.JSONDecodeError as e:
            logger.error("Failed to parse data from command output:")
            logger.error(output)
            raise signals.TestError(f"Invalid JSON from iperf3: {e}") from e

        asserts.assert_true(
            samples, "Expected at least one link metrics sample"
        )
        metrics = LinkMetricSummary.from_samples(samples)

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
            return res_udp, metrics
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
            return res_tcp, metrics

    def _log_measurement_result(
        self,
        test_label: str,
        mbps: float,
        metrics: LinkMetricSummary,
        phy_max_mbps: float,
    ) -> None:
        target_mbps = phy_rate_target_mbps(phy_max_mbps)
        phy_target = (
            f" | PHY Theoretical max: {phy_max_mbps:.1f} Mbps, "
            f"PHY target ({PHY_RATE_TARGET_RATIO:.0%}): {target_mbps:.1f} Mbps"
        )
        logger.info(
            f"{test_label} Result: {mbps} Mbps | {metrics.format_summary()}{phy_target}"
        )
        for line in metrics.format_sample_table():
            logger.info(line)

    def _log_udp_correction(
        self, test_label: str, udp_res: IperfUdpResult, tested_send_speed: str
    ) -> None:
        uncorrected_mbps = self.bps_to_mbps(udp_res["udp_bps_uncorrected"])
        corrected_mbps = self.bps_to_mbps(udp_res["udp_bps_corrected"])
        loss_percent = round(udp_res["udp_loss_percent"], 2)
        logger.info(
            f"{test_label} UDP Correction: tested send speed: {tested_send_speed}, "
            f"loss: {loss_percent}%, "
            f"uncorrected: {uncorrected_mbps} Mbps, "
            f"corrected: {corrected_mbps} Mbps"
        )

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
        dut_mac = await iface.get_mac_address()
        asserts.assert_equal(netstack_iface.mac, dut_mac)
        await self.dut.netstack.wait_for_ipv4_addr(netstack_iface.id_)

        max_phy_mbps = theoretical_max_phy_mbps(
            self.dut_phy_caps,
            self.ap_phy_caps,
            band,
            test.channel_bandwidth,
        )

        if test.record_odpm:
            async with odpm_utils.record_odpm_metrics(
                self.dut.tracing,
                test_case_path=self.test_case_path,
                test_name=self.current_test_info.name,
            ):
                measurements = await self._run_iperf_measurements(
                    iperf_server_address=iperf_server_address,
                    iface=iface,
                    dut_mac=dut_mac,
                    band=band,
                    channel_bandwidth=test.channel_bandwidth,
                    max_phy_mbps=max_phy_mbps,
                )
        else:
            measurements = await self._run_iperf_measurements(
                iperf_server_address=iperf_server_address,
                iface=iface,
                dut_mac=dut_mac,
                band=band,
                channel_bandwidth=test.channel_bandwidth,
                max_phy_mbps=max_phy_mbps,
            )

        test_case_record = self._build_test_case_record(test, measurements)
        self.test_cases.append(test_case_record)
        self._write_json_report()

        target_mbps = phy_rate_target_mbps(max_phy_mbps)
        failures: list[str] = []
        for label, record in [
            ("TCP TX", measurements["tcp_tx"]),
            ("TCP RX", measurements["tcp_rx"]),
            ("UDP TX", measurements["udp_tx"]),
            ("UDP RX", measurements["udp_rx"]),
        ]:
            if self._is_phy_rate_below_target(record):
                # `direction` is relative to the DUT, and the DUT does not
                # report RX PHY rates. Check the AP's rate in the data-carrying
                # direction: AP RX rate when the DUT is transmitting ("tx"),
                # and AP TX rate when the DUT is receiving ("rx").
                metric_key: LinkMetricKey = (
                    "ap_rx_rate_mbps"
                    if record["direction"] == "tx"
                    else "ap_tx_rate_mbps"
                )
                avg_rate = record["link_metrics"][metric_key]["avg"]
                pct = self._compute_phy_avg_pct(record)
                failures.append(
                    f"{label}: avg PHY {avg_rate} Mbps ({pct}%) "
                    f"< {target_mbps} Mbps ({PHY_RATE_TARGET_RATIO:.0%} of {max_phy_mbps} Mbps for {self.dut_board})"
                )
        if failures:
            raise signals.TestFailure(
                "PHY rate below target:\n" + "\n".join(failures)
            )

    async def _run_iperf_measurements(
        self,
        iperf_server_address: str,
        iface: wlan_core.ClientIface,
        dut_mac: MacAddress,
        band: Band,
        channel_bandwidth: Literal[20, 40, 80, 160],
        max_phy_mbps: float,
    ) -> MeasurementsByDirection:
        udp_bandwidth = self.get_target_udp_bandwidth(channel_bandwidth)
        tcp_tx, tcp_tx_metrics = await self.get_iperf_throughput_bps(
            iperf_server_address,
            reverse=False,
            udp=False,
            iface=iface,
            dut_mac=dut_mac,
            band=band,
        )
        tcp_tx_record = self._build_tcp_measurement_record(
            "tx", tcp_tx, tcp_tx_metrics, max_phy_mbps
        )
        self._log_measurement_result(
            "TCP TX",
            tcp_tx_record["throughput_mbps"],
            tcp_tx_metrics,
            max_phy_mbps,
        )

        tcp_rx, tcp_rx_metrics = await self.get_iperf_throughput_bps(
            iperf_server_address,
            reverse=True,
            udp=False,
            iface=iface,
            dut_mac=dut_mac,
            band=band,
        )
        tcp_rx_record = self._build_tcp_measurement_record(
            "rx", tcp_rx, tcp_rx_metrics, max_phy_mbps
        )
        self._log_measurement_result(
            "TCP RX",
            tcp_rx_record["throughput_mbps"],
            tcp_rx_metrics,
            max_phy_mbps,
        )

        udp_tx, udp_tx_metrics = await self.get_iperf_throughput_bps(
            iperf_server_address,
            reverse=False,
            udp=True,
            iface=iface,
            dut_mac=dut_mac,
            band=band,
            bandwidth=udp_bandwidth,
        )
        udp_tx_record = self._build_udp_measurement_record(
            "tx", udp_tx, udp_tx_metrics, max_phy_mbps
        )
        self._log_udp_correction("UDP TX", udp_tx, udp_bandwidth)
        self._log_measurement_result(
            "UDP TX",
            udp_tx_record["throughput_mbps"],
            udp_tx_metrics,
            max_phy_mbps,
        )

        udp_rx, udp_rx_metrics = await self.get_iperf_throughput_bps(
            iperf_server_address,
            reverse=True,
            udp=True,
            iface=iface,
            dut_mac=dut_mac,
            band=band,
            bandwidth=udp_bandwidth,
        )
        udp_rx_record = self._build_udp_measurement_record(
            "rx", udp_rx, udp_rx_metrics, max_phy_mbps
        )
        self._log_udp_correction("UDP RX", udp_rx, udp_bandwidth)
        self._log_measurement_result(
            "UDP RX",
            udp_rx_record["throughput_mbps"],
            udp_rx_metrics,
            max_phy_mbps,
        )

        return {
            "tcp_tx": tcp_tx_record,
            "tcp_rx": tcp_rx_record,
            "udp_tx": udp_tx_record,
            "udp_rx": udp_rx_record,
        }

    @staticmethod
    def _format_band(band: Band) -> str:
        match band:
            case Band.BAND_2G:
                return "2.4GHz"
            case Band.BAND_5G:
                return "5GHz"
            case _:
                return str(band)

    @staticmethod
    def _compute_phy_avg_pct(
        measurement: MeasurementRecord,
    ) -> float | None:
        phy_max_mbps = measurement["phy_max_mbps"]
        if phy_max_mbps <= 0:
            return None
        metric_key: LinkMetricKey = (
            "ap_rx_rate_mbps"
            if measurement["direction"] == "tx"
            else "ap_tx_rate_mbps"
        )
        avg_rate = measurement["link_metrics"][metric_key]["avg"]
        if avg_rate is None:
            return None
        return round((avg_rate / phy_max_mbps) * 100.0, 1)

    @classmethod
    def _build_tcp_measurement_record(
        cls,
        direction: Literal["tx", "rx"],
        iperf_res: IperfTcpResult,
        metrics: LinkMetricSummary,
        phy_max_mbps: float,
    ) -> TcpMeasurementRecord:
        raw_bps = iperf_res["tcp_bps"]
        return {
            "protocol": "tcp",
            "direction": direction,
            "throughput_mbps": cls.bps_to_mbps(raw_bps),
            "phy_mode": metrics.phy_mode,
            "nss": metrics.nss,
            "phy_max_mbps": phy_max_mbps,
            "link_metrics": metrics.to_link_metrics_dict(),
            "iperf_details": {
                "server_cpu_utilization_percent": iperf_res[
                    "server_cpu_utilization_percent"
                ],
                "raw_bps": raw_bps,
            },
        }

    @classmethod
    def _build_udp_measurement_record(
        cls,
        direction: Literal["tx", "rx"],
        iperf_res: IperfUdpResult,
        metrics: LinkMetricSummary,
        phy_max_mbps: float,
    ) -> UdpMeasurementRecord:
        return {
            "protocol": "udp",
            "direction": direction,
            "throughput_mbps": cls.bps_to_mbps(iperf_res["udp_bps_corrected"]),
            "phy_mode": metrics.phy_mode,
            "nss": metrics.nss,
            "phy_max_mbps": phy_max_mbps,
            "link_metrics": metrics.to_link_metrics_dict(),
            "iperf_details": {
                "server_cpu_utilization_percent": iperf_res[
                    "server_cpu_utilization_percent"
                ],
                "raw_bps": iperf_res["udp_bps_uncorrected"],
                "corrected_bps": iperf_res["udp_bps_corrected"],
                "loss_percent": iperf_res["udp_loss_percent"],
            },
        }

    @classmethod
    def _build_test_case_record(
        cls,
        test: TestParams,
        measurements: MeasurementsByDirection,
    ) -> TestCaseRecord:
        sec_name = ConfigMapper.to_hostapd_security(test.security_mode).value
        return {
            "security": sec_name,
            "channel": test.channel,
            "channel_bandwidth": test.channel_bandwidth,
            "band": cls._format_band(test.band),
            "measurements": measurements,
        }

    def _write_json_report(self) -> None:
        payload: ThroughputReportJson = {
            "schema_version": 1,
            "test_cases": self.test_cases,
        }
        with open(self.json_file_path, "w", encoding="utf-8") as json_file:
            json.dump(payload, json_file, indent=2)
            json_file.write("\n")

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

    @staticmethod
    def _is_phy_rate_below_target(measurement: MeasurementRecord) -> bool:
        phy_max = measurement["phy_max_mbps"]
        metric_key: LinkMetricKey = (
            "ap_rx_rate_mbps"
            if measurement["direction"] == "tx"
            else "ap_tx_rate_mbps"
        )
        avg_rate = measurement["link_metrics"][metric_key]["avg"]
        return avg_rate is None or avg_rate < phy_rate_target_mbps(phy_max)

    def _update_html_report(self) -> None:
        """Renders throughput.json test cases into a clean, human-readable HTML table."""
        test_cases = self.test_cases
        if not test_cases and os.path.exists(self.json_file_path):
            with open(self.json_file_path, "r", encoding="utf-8") as f:
                data = json.load(f)
                test_cases = data.get("test_cases", [])

        if not test_cases:
            return

        header_row = [
            "security",
            "band",
            "channel",
            "bandwidth",
            "phy_mode",
            "nss",
            "udp_or_tcp",
            "dut_tx_or_rx",
            "result_mbps",
            "phy_max_mbps",
            "phy_avg_pct",
            *(f"{m}_{s}" for m in LINK_METRIC_KEYS for s in STAT_KEYS),
        ]

        data_rows: list[tuple[bool, list[Any]]] = []
        for test_case in test_cases:
            measurements = test_case["measurements"]
            for proto, measurement in (
                ("tcp", measurements["tcp_tx"]),
                ("tcp", measurements["tcp_rx"]),
                ("udp", measurements["udp_tx"]),
                ("udp", measurements["udp_rx"]),
            ):
                link_metrics = measurement["link_metrics"]
                row: list[Any] = [
                    test_case["security"],
                    test_case["band"],
                    test_case["channel"],
                    test_case["channel_bandwidth"],
                    measurement["phy_mode"],
                    measurement["nss"],
                    proto,
                    measurement["direction"],
                    measurement["throughput_mbps"],
                    measurement["phy_max_mbps"],
                    self._compute_phy_avg_pct(measurement),
                ]
                for m_key in LINK_METRIC_KEYS:
                    stat = link_metrics[m_key]
                    row.extend(stat[s] for s in STAT_KEYS)
                data_rows.append(
                    (self._is_phy_rate_below_target(measurement), row)
                )

        html_lines: list[str] = [
            "<!DOCTYPE html>",
            '<html lang="en">',
            "<head>",
            '  <meta charset="utf-8">',
            "  <title>Throughput Test Results</title>",
            "  <style>",
            "    body {",
            '      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;',
            "      margin: 24px;",
            "      color: #202124;",
            "      background-color: #ffffff;",
            "    }",
            "    h2 {",
            "      margin-bottom: 16px;",
            "    }",
            "    table {",
            "      border-collapse: collapse;",
            "      font-size: 14px;",
            "      box-shadow: 0 1px 3px rgba(0, 0, 0, 0.12);",
            "    }",
            "    th, td {",
            "      border: 1px solid #dadce0;",
            "      padding: 8px 14px;",
            "      white-space: nowrap;",
            "    }",
            "    th {",
            "      background-color: #f1f3f4;",
            "      font-weight: 600;",
            "      text-align: center;",
            "    }",
            "    tr:nth-child(even) {",
            "      background-color: #f8f9fa;",
            "    }",
            "    tr:hover {",
            "      background-color: #f1f3f4;",
            "    }",
            "    tr.fail, tr.fail:nth-child(even) {",
            "      background-color: #fce8e6;",
            "      color: #c5221f;",
            "    }",
            "    tr.fail:hover {",
            "      background-color: #fad2cf;",
            "    }",
            "    .num {",
            "      text-align: right;",
            "      font-variant-numeric: tabular-nums;",
            "    }",
            "    .str {",
            "      text-align: left;",
            "    }",
            "    .na {",
            "      color: #9aa0a6;",
            "      text-align: center;",
            "    }",
            "  </style>",
            "</head>",
            "<body>",
            "  <h2>Throughput Test Results</h2>",
            "  <table>",
            "    <thead>",
            "      <tr>",
        ]

        for h in header_row:
            html_lines.append(f"        <th>{html.escape(h)}</th>")
        html_lines.extend(
            [
                "      </tr>",
                "    </thead>",
                "    <tbody>",
            ]
        )

        for is_fail, row in data_rows:
            html_lines.append(
                '      <tr class="fail">' if is_fail else "      <tr>"
            )
            for cell in row:
                if cell is None or cell == "":
                    html_lines.append('        <td class="na">N/A</td>')
                elif isinstance(cell, (int, float)):
                    html_lines.append(
                        f'        <td class="num">{html.escape(str(cell))}</td>'
                    )
                else:
                    html_lines.append(
                        f'        <td class="str">{html.escape(str(cell))}</td>'
                    )
            html_lines.append("      </tr>")

        html_lines.extend(
            [
                "    </tbody>",
                "  </table>",
                "</body>",
                "</html>\n",
            ]
        )

        with open(self.html_file_path, "w", encoding="utf-8") as f:
            f.write("\n".join(html_lines))

    @staticmethod
    def _build_fuchsiaperf_data(
        test_cases: list[TestCaseRecord],
    ) -> list[dict[str, Any]]:
        entries_by_label: dict[str, dict[str, Any]] = {}
        for test_case in test_cases:
            measurements = test_case["measurements"]
            for proto, measurement in (
                ("tcp", measurements["tcp_tx"]),
                ("tcp", measurements["tcp_rx"]),
                ("udp", measurements["udp_tx"]),
                ("udp", measurements["udp_rx"]),
            ):
                label = (
                    f"ch{test_case['channel']}_{test_case['channel_bandwidth']}mhz/"
                    f"{proto}_{measurement['direction']}"
                )
                if label in entries_by_label:
                    entries_by_label[label]["values"].append(
                        measurement["throughput_mbps"]
                    )
                else:
                    entries_by_label[label] = {
                        "test_suite": FUCHSIAPERF_SUITE_NAME,
                        "label": label,
                        "values": [measurement["throughput_mbps"]],
                        "unit": "unitless_biggerIsBetter",
                    }
        return list(entries_by_label.values())

    def _publish_fuchsiaperf_report(self) -> None:
        """Publishes throughput metrics in fuchsiaperf format for Chromeperf.

        Metrics are scoped per device type by perf_publish's multi-device
        naming on the builder, so the label only encodes the test permutation.
        """
        test_cases = self.test_cases
        if not test_cases and os.path.exists(self.json_file_path):
            with open(self.json_file_path, "r", encoding="utf-8") as f:
                data = json.load(f)
                test_cases = data.get("test_cases", [])

        if not test_cases:
            return

        fuchsiaperf_data = self._build_fuchsiaperf_data(test_cases)
        with open(self.fuchsiaperf_file_path, "w", encoding="utf-8") as f:
            json.dump(fuchsiaperf_data, f, indent=2)
            f.write("\n")

        expected_metrics_path = os.path.join(
            self.log_path, EXPECTED_METRICS_FILENAME
        )
        expected_lines = [
            "[no-summarize-metrics]",
            *sorted(
                {
                    f"{entry['test_suite']}: {entry['label']}"
                    for entry in fuchsiaperf_data
                }
            ),
        ]
        with open(expected_metrics_path, "w", encoding="utf-8") as f:
            f.write("\n".join(expected_lines) + "\n")

        publish.publish_fuchsiaperf(
            [self.fuchsiaperf_file_path],
            EXPECTED_METRICS_FILENAME,
            runtime_deps_dir=self.log_path,
        )


if __name__ == "__main__":
    test_runner.main()
