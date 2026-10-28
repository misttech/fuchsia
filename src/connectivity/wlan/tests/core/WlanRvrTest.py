# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import itertools
import logging
from dataclasses import dataclass
from datetime import timedelta
from enum import StrEnum, auto, unique

import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from attenuator import (
    Attenuator,
    get_attenuators_for_device,
)
from honeydew.affordances.connectivity.netstack.errors import (
    HoneydewNetstackError,
)
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanError,
)
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from honeydew.transports.ffx.types import MachineFormat
from iperf.iperf_server import IPerfResult, IPerfServerOverSsh
from legacy_access_point.ap_lib.hostapd_ap_preset import create_ap_preset
from legacy_access_point.ap_lib.hostapd_security import (
    Security as HostapdSecurity,
)
from libs.validation import MapValidator
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

REPORTING_SPEED_UNITS = "Mbps"


@unique
class TrafficDirection(StrEnum):
    RX = auto()
    TX = auto()


@unique
class IPVersion(StrEnum):
    V4 = "ipv4"


@dataclass(frozen=True)
class RateByRange:
    relative_attn: int
    throughput: float


@dataclass(frozen=True)
class TestParams:
    band: Band
    security: Security
    password: str | None
    ip_version: IPVersion
    direction: TrafficDirection


def write_csv_rvr_data(
    test_name: str, csv_path: str, results: list[RateByRange]
) -> None:
    """Writes the CSV data for the RvR test

    Args:
        test_name: The name of test that was run.
        csv_path: Where to put the csv file.
        results: A list of RateByRange results to be put in the csv file.
    """
    csv_file_name = f"{csv_path}rvr_throughput_vs_attn_{test_name}.csv"
    with open(csv_file_name, "w+") as csv_fileId:
        csv_fileId.write(
            f"Attenuation(db),Throughput({REPORTING_SPEED_UNITS})\n"
        )
        for res in results:
            csv_fileId.write(f"{res.relative_attn},{res.throughput}\n")


class WlanRvrTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests running WLAN RvR.

    Test Bed Requirement:
    * One Fuchsia device
    * One Access Point
    * One attenuator
    * One Linux iPerf Server
    """

    phy: wlan_core.Phy
    iperf_server: IPerfServerOverSsh
    attenuators: list[Attenuator]
    attenuators_2g: list[Attenuator]
    attenuators_5g: list[Attenuator]

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.rvr_graph_summary: list[object] = []

        params = MapValidator(self.user_params["rvr_settings"])
        self.starting_attn = params.get(int, "starting_attn", 0)
        self.ending_attn = params.get(int, "ending_attn", 95)
        self.step_size_in_db = params.get(int, "step_size_in_db", 1)
        self.dwell_time_in_secs = params.get(int, "dwell_time_in_secs", 10)

        self.reverse_rvr_after_forward = params.get(
            bool, "reverse_rvr_after_forward", False
        )
        self.iperf_flags = params.get(str, "iperf_flags", "-i 1")

    async def pre_run(self) -> None:
        test_params: list[TestParams] = []
        security = SecurityWpa2()

        for band, direction in itertools.product(Band, TrafficDirection):
            password = AccessPointConfig.random_string(20)
            test_params.append(
                TestParams(
                    band,
                    security,
                    password,
                    IPVersion.V4,
                    direction,
                )
            )

        def generate_test_name(t: TestParams) -> str:
            # TODO(http://b/303659781): Keep mode in sync with hostapd.
            mode = "11n" if t.band is Band.BAND_2G else "11ac"
            frequency = "20mhz" if t.band is Band.BAND_2G else "80mhz"
            # Map OpenWrt security to hostapd security string to match legacy test name format
            security = ConfigMapper.to_hostapd_security(t.security)
            return (
                f"test_rvr_{mode}_{t.band.lower()}_{frequency}_{security}_"
                f"{t.direction}_{t.ip_version}"
            )

        self.generate_tests(
            self._test_rvr, generate_test_name, [(p,) for p in test_params]
        )

    async def setup_class(self) -> None:
        await super().setup_class()

        if self.openwrt_ap is None and self.access_point is None:
            raise signals.TestError("Requires at least one access point")

        self.phy = await self.dut.wlan_core.ensure_single_phy()
        await self.phy.set_country(
            KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]
        )
        self.iperf_server = await self.setup_iperf_server()

        if not self.attenuators:
            raise signals.TestError("Requires at least one attenuator")

        ap_controller_type = (
            "OpenWrtAP" if self.openwrt_ap is not None else "AccessPoint"
        )
        self.attenuators_2g = get_attenuators_for_device(
            self.controller_configs[ap_controller_type][0].get(
                "Attenuator", []
            ),
            self.attenuators,
            "attenuator_ports_wifi_2g",
        )
        self.attenuators_5g = get_attenuators_for_device(
            self.controller_configs[ap_controller_type][0].get(
                "Attenuator", []
            ),
            self.attenuators,
            "attenuator_ports_wifi_5g",
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        self.iperf_server.start()
        await self.dut.wlan_core.destroy_all_ifaces()
        if self.access_point is not None:
            self.access_point.stop_all_aps()

    async def teardown_test(self) -> None:
        self.iperf_server.stop()
        await self.dut.wlan_core.destroy_all_ifaces()
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def teardown_class(self) -> None:
        for attenuator in getattr(self, "attenuators", []):
            attenuator.set_atten(0)
        await super().teardown_class()

    async def _wait_for_dut_ip(self, ip_version: IPVersion) -> None:
        netstack_iface = await self.dut.netstack.wait_for_interface(
            PortClass.WLAN_CLIENT
        )
        if ip_version is IPVersion.V4:
            await self.dut.netstack.wait_for_ipv4_addr(netstack_iface.id_)

    async def run_rvr(
        self,
        ssid: str,
        security: Security,
        password: str | None,
        band: Band,
        traffic_dir: TrafficDirection,
        ip_version: IPVersion,
    ) -> list[RateByRange]:
        """Sets up and runs the RvR test.

        Args:
            ssid: The SSID for the client to associate to.
            security: Security of the AP.
            password: Password of the AP.
            band: 2g or 5g.
            traffic_dir: rx or tx, bi is not supported by iperf3.
            ip_version: ipv4 or ipv6.

        Returns:
            The RateByRange results.
        """
        match band:
            case Band.BAND_2G:
                rvr_attenuators = self.attenuators_2g
            case Band.BAND_5G:
                rvr_attenuators = self.attenuators_5g

        for rvr_attenuator in rvr_attenuators:
            rvr_attenuator.set_atten(self.starting_attn)

        iface = await self.phy.create_client_iface()
        fidl_sec = security.to_fidl_wlan_internal()

        # Attempt association to the AP multiple times. This makes the test more
        # resilient to AP flakes that may result in the DUT not being able to
        # find the network in its scan results.
        associate_counter = 0
        associate_max_attempts = 3
        target_channel = (
            band.default_channel
            if self.openwrt_ap is not None
            else ConfigMapper.to_hostapd_band(band).default_channel()
        )
        while associate_counter < associate_max_attempts:
            await iface.disconnect()

            if self.openwrt_ap is not None:
                channel = BssChannel(
                    band,
                    target_channel,
                    HeMode(bw=20 if band == Band.BAND_2G else 80),
                )
                config = AccessPointConfig(
                    radios=[
                        RadioConfig.generate(
                            channel=channel,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    security=security,
                                    password=password,
                                )
                            ],
                        )
                    ]
                )
                self.openwrt_ap.configure_wifi(config)
            elif self.access_point is not None:
                self.access_point.stop_all_aps()
                legacy_security = HostapdSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                )
                self.access_point.start_ap(
                    hostapd_config=create_ap_preset(
                        iface_wlan_2g=self.access_point.wlan_2g,
                        iface_wlan_5g=self.access_point.wlan_5g,
                        profile_name="whirlwind",
                        channel=target_channel,
                        ssid=ssid,
                        security=legacy_security,
                    ),
                    radvd_config=None,
                    setup_bridge=True,
                )

            try:
                await iface.scan_and_connect(
                    ssid=ssid,
                    password=password,
                    security=fidl_sec,
                    channels=[target_channel],
                )
                break
            except HoneydewWlanError as e:
                logger.warning(
                    f"Association attempt {associate_counter + 1} failed: {e}"
                )
                associate_counter += 1
        else:
            asserts.fail(
                f"Unable to associate at starting attenuation: {self.starting_attn}"
            )

        await self._wait_for_dut_ip(ip_version)
        if self.openwrt_ap is None:
            self.iperf_server.renew_test_interface_ip_address()
        iperf_server_ip_address = self.iperf_server.get_addr()

        results = await self.rvr_loop(
            iface=iface,
            traffic_dir=traffic_dir,
            rvr_attenuators=rvr_attenuators,
            iperf_server_ip_address=iperf_server_ip_address,
            ip_version=ip_version,
            ssid=ssid,
            security=security,
            password=password,
            channel=target_channel,
            reverse=False,
        )
        if self.reverse_rvr_after_forward:
            results = results + await self.rvr_loop(
                iface=iface,
                traffic_dir=traffic_dir,
                rvr_attenuators=rvr_attenuators,
                iperf_server_ip_address=iperf_server_ip_address,
                ip_version=ip_version,
                ssid=ssid,
                security=security,
                password=password,
                channel=target_channel,
                reverse=True,
            )

        return results

    async def rvr_loop(
        self,
        iface: wlan_core.ClientIface,
        traffic_dir: TrafficDirection,
        rvr_attenuators: list[Attenuator],
        iperf_server_ip_address: str,
        ip_version: IPVersion,
        ssid: str,
        security: Security,
        password: str | None,
        channel: int,
        reverse: bool,
    ) -> list[RateByRange]:
        """The loop that goes through each attenuation level and runs the iperf
        throughput measurement.
        """
        starting_attn = self.starting_attn
        ending_attn = self.ending_attn
        step_size_in_db = self.step_size_in_db
        if reverse:
            starting_attn = self.ending_attn
            ending_attn = self.starting_attn
            step_size_in_db = step_size_in_db * -1
            await iface.disconnect()

        results: list[RateByRange] = []

        for step in range(starting_attn, ending_attn, step_size_in_db):
            try:
                for attenuator in rvr_attenuators:
                    logger.info(
                        f"Setting relative attenuation of {attenuator.instrument.address} "
                        f"to {step} dB"
                    )
                    attenuator.set_atten(step)
            except ValueError as e:
                logger.error(
                    f"{step} is beyond the max or min of the testbed "
                    f"attenuator's capability. Stopping. {e}"
                )
                break

            logger.info(f"Running iperf at relative attenuation of {step} dB")

            throughput = await self._run_iperf(
                iface=iface,
                traffic_dir=traffic_dir,
                iperf_server_ip_address=iperf_server_ip_address,
                ip_version=ip_version,
                ssid=ssid,
                security=security,
                password=password,
                channel=channel,
                reverse=reverse,
            )
            logger.info(
                f"Iperf traffic complete. {traffic_dir} traffic received at "
                f"{throughput} {REPORTING_SPEED_UNITS} at relative attenuation "
                f"of {step} db"
            )
            results.append(RateByRange(step, throughput))

        return results

    async def _run_iperf(
        self,
        iface: wlan_core.ClientIface,
        traffic_dir: TrafficDirection,
        iperf_server_ip_address: str,
        ip_version: IPVersion,
        ssid: str,
        security: Security,
        password: str | None,
        channel: int,
        reverse: bool,
    ) -> float:
        status = await iface.status()
        if status.connected is None:
            if reverse:
                # In reverse mode, we're going from a high attenuation (weak
                # signal) to a low attenuation (strong signal). It's expected
                # that the DUT is not connected to the AP at the high
                # attenuation level(s), so if we're disconnected here, we
                # should try to associate.
                logger.info("Trying to associate")
                try:
                    await iface.scan_and_connect(
                        ssid=ssid,
                        password=password,
                        security=security.to_fidl_wlan_internal(),
                        channels=[channel],
                    )
                    logger.info("Successfully associated.")
                    try:
                        logger.debug("Getting DUT IP address")
                        await self._wait_for_dut_ip(ip_version)
                    except HoneydewNetstackError:
                        logger.info(
                            f"Association succeeded, but unable to get DUT IP address. Marking a 0 {REPORTING_SPEED_UNITS} "
                            "for throughput. Skipping running traffic and disconnecting."
                        )
                        # Disconnect the DUT, so that we have a fresh attempt
                        # to get an IP at the next iteration of this reverse
                        # test.
                        await iface.disconnect()
                        return 0
                except HoneydewWlanError:
                    logger.info(
                        f"Association failed. Marking a 0 {REPORTING_SPEED_UNITS} "
                        "for throughput. Skipping running traffic."
                    )
                    return 0
            else:
                logger.info(
                    f"Device no longer associated. Marking a 0 {REPORTING_SPEED_UNITS} "
                    "for throughput. Skipping running traffic."
                )
                return 0

        logger.debug("Pinging iperf server from DUT")
        try:
            ping_result = await self.dut.netstack.ping(
                iperf_server_ip_address,
                interval=timedelta(milliseconds=100),
            )
            ping_ok = ping_result.any_pings_received
        except HoneydewNetstackError as e:
            logger.debug(f"Ping failed with error: {e}")
            ping_ok = False

        if not ping_ok:
            logger.info(
                f'Iperf server "{iperf_server_ip_address}" is not pingable. '
                f"Marking a 0 {REPORTING_SPEED_UNITS} for throughput. "
                "Skipping running traffic."
            )
            return 0

        logger.info(f'Iperf server "{iperf_server_ip_address}" is pingable.')

        match traffic_dir:
            case TrafficDirection.TX:
                logger.info(
                    f"Running traffic from DUT to iperf server ({iperf_server_ip_address})"
                )
            case TrafficDirection.RX:
                logger.info(
                    f"Running traffic from iperf server ({iperf_server_ip_address}) to DUT"
                )

        args = [
            "--client",
            iperf_server_ip_address,
            *self.iperf_flags.split(),
            "--time",
            str(self.dwell_time_in_secs),
            "--json",
        ]
        if traffic_dir is TrafficDirection.RX:
            args.append("--reverse")

        cmd = f"iperf3 {' '.join(args)}"
        iperf_timeout = float(self.dwell_time_in_secs * 2)
        try:
            output = await asyncio.to_thread(
                self.dut.ffx.run,
                cmd=["target", "ssh", cmd],
                timeout=iperf_timeout,
                log_status_on_failure=False,
                machine=MachineFormat.RAW,
            )
        except Exception as e:
            self.iperf_server.stop()
            self.iperf_server.start()
            logger.error(
                f"Iperf traffic failed or timed out. Marking 0 {REPORTING_SPEED_UNITS} "
                f"for throughput: {e}"
            )
            return 0

        if not output:
            return 0

        try:
            iperf_results = IPerfResult(
                output,
                reporting_speed_units=REPORTING_SPEED_UNITS,
            )
            if iperf_results.error:
                self.iperf_server.stop()
                self.iperf_server.start()
                logger.error(f"Errors in iperf logs:\n{iperf_results.error}")
            if iperf_results.avg_send_rate:
                return iperf_results.avg_send_rate

            logger.error(
                '"avg_send_rate" not found in iPerf3 results. Marking 0 '
                f"{REPORTING_SPEED_UNITS} for throughput."
                f"\n{iperf_results.get_json()}"
            )
            return 0
        except ValueError as e:
            self.iperf_server.stop()
            self.iperf_server.start()
            logger.error(
                f"No data in iPerf3 output. Marking 0 {REPORTING_SPEED_UNITS} "
                f"for throughput: {e}"
            )
            return 0
        except Exception as e:
            self.iperf_server.stop()
            self.iperf_server.start()
            logger.error(
                f"Unknown exception. Marking 0 {REPORTING_SPEED_UNITS} for "
                f"throughput: {e}"
            )
            return 0

    async def _test_rvr(self, t: TestParams) -> None:
        ssid = AccessPointConfig.random_string(20)
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        results = await self.run_rvr(
            ssid,
            security=t.security,
            password=t.password,
            band=t.band,
            traffic_dir=t.direction,
            ip_version=t.ip_version,
        )
        write_csv_rvr_data(
            self.current_test_info.name,
            self.current_test_info.output_path,
            results,
        )


if __name__ == "__main__":
    test_runner.main()
