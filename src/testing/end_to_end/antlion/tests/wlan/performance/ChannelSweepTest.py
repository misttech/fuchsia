#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import os
import time
from dataclasses import dataclass
from pathlib import Path
from statistics import pstdev
from typing import Literal, cast

from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.regulatory_channels import COUNTRY_CHANNELS
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.controllers.iperf_client import (
    IPerfClientOverAdb,
    IPerfClientOverSsh,
)
from antlion.controllers.iperf_server import IPerfResult, IPerfServerOverSsh
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.types import CountryCode
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DFS_BYPASS_COUNTRY_CODE,
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    HtMode,
    PhyMode,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWep,
    SecurityWpa,
    SecurityWpa2,
    SecurityWpa3,
    SecurityWpaWpa2Mixed,
    VhtMode,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper,
)

DEFAULT_MIN_THROUGHPUT = 0.0
DEFAULT_MAX_STD_DEV = 1.0
DEFAULT_IPERF_TIMEOUT = 30

DEFAULT_TIME_TO_WAIT_FOR_IP_ADDR = 30
GRAPH_CIRCLE_SIZE = 10
MAX_2_4_CHANNEL = 14
TIME_TO_SLEEP_BETWEEN_RETRIES = 1
WEP_HEX_STRING_LENGTH = 10
MIN_WPA_PSK_LENGTH = 8
AP_SSID_LENGTH_2G = 8

MEGABITS_PER_SECOND = "Mbps"


@dataclass
class TestParams:
    country_code: str
    """Country code for the DUT to set before running the test."""

    security_mode: Security
    """Security type of the network to create. None represents an open network."""

    channel: int
    """Channel for the AP to broadcast on"""

    channel_bandwidth: int
    """Channel bandwidth in MHz for the AP to broadcast with"""

    expect_min_rx_throughput_mbps: float = DEFAULT_MIN_THROUGHPUT
    """Expected minimum receive throughput in Mb/s"""

    expect_min_tx_throughput_mbps: float = DEFAULT_MIN_THROUGHPUT
    """Expected minimum transmit throughput in Mb/s"""

    # TODO: Use this value
    expect_max_std_dev: float = DEFAULT_MAX_STD_DEV
    """Expected maximum standard deviation of throughput in Mb/s"""


@dataclass(frozen=True)
class ThroughputKey:
    country_code: str
    security_mode: Security
    channel_bandwidth: int

    @staticmethod
    def from_test(test: TestParams) -> "ThroughputKey":
        return ThroughputKey(
            country_code=test.country_code,
            security_mode=test.security_mode,
            channel_bandwidth=test.channel_bandwidth,
        )


@dataclass
class ThroughputValue:
    channel: int
    tx_throughput_mbps: float | None
    rx_throughput_mbps: float | None


ChannelThroughputMap = dict[ThroughputKey, list[ThroughputValue]]


class ChannelSweepTest(base_test.WifiBaseTest):
    """Tests channel performance.

    Testbed Requirement:
    * 1 x Fuchsia device (dut)
    * 1 x access point
    * 1 x Linux Machine used as IPerfServer

    Note: Performance tests should be done in isolated testbed.
    """

    access_point: AccessPoint

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.iperf_server: IPerfServerOverSsh | None = None
        self.openwrt_ap: OpenWrtAP | None = None
        self.log = logging.getLogger()
        self.channel_throughput: ChannelThroughputMap = {}

        self.time_to_wait_for_ip_addr = configs.user_params.get(
            "channel_sweep_test_params", {}
        ).get("time_to_wait_for_ip_addr", DEFAULT_TIME_TO_WAIT_FOR_IP_ADDR)

    def pre_run(self) -> None:
        tests: list[tuple[TestParams]] = []

        def generate_test_name(test: TestParams) -> str:
            sec_name = AccessPointConfigMapper.to_hostapd_security(
                test.security_mode
            ).value
            return f"test_{test.country_code}_{sec_name}_channel_{test.channel}_{test.channel_bandwidth}mhz"

        def test_params(test_name: str) -> dict[str, float]:
            return self.user_params.get("channel_sweep_test_params", {}).get(
                test_name, {}
            )

        for country_channels in [COUNTRY_CHANNELS["United States of America"]]:
            security_modes: list[Security] = [
                SecurityOpen(),
                SecurityWep(),
                SecurityWpa(),
                SecurityWpa2(),
                SecurityWpaWpa2Mixed(),
                SecurityWpa3(),
            ]
            for security_mode in security_modes:
                for (
                    channel,
                    bandwidths,
                ) in country_channels.allowed_channels.items():
                    for bandwidth in bandwidths:
                        test = TestParams(
                            country_code=country_channels.country_code,
                            security_mode=security_mode,
                            channel=channel,
                            channel_bandwidth=bandwidth,
                        )
                        name = generate_test_name(test)
                        test.expect_min_rx_throughput_mbps = test_params(
                            name
                        ).get("min_rx_throughput", DEFAULT_MIN_THROUGHPUT)
                        test.expect_min_tx_throughput_mbps = test_params(
                            name
                        ).get("min_tx_throughput", DEFAULT_MIN_THROUGHPUT)
                        test.expect_max_std_dev = test_params(name).get(
                            "max_std_dev", DEFAULT_MAX_STD_DEV
                        )
                        tests.append((test,))

        self.generate_tests(
            self.run_channel_performance, generate_test_name, tests
        )

    def get_existing_test_names(self) -> list[str]:
        test_names: list[str] = super().get_existing_test_names()
        # Verify standard deviation last since it depends on the throughput results from
        # all other tests.
        test_names.sort(key=lambda n: n == "test_standard_deviation")
        return test_names

    def setup_class(self) -> None:
        super().setup_class()

        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
            self.iperf_server = self.openwrt_ap.iperf_server
            self.iperf_server.start()
        else:
            if len(self.access_points) == 0:
                raise signals.TestAbortClass(
                    "Requires at least one access point"
                )
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()

            if len(self.iperf_servers) == 0:
                raise signals.TestAbortClass(
                    "Requires at least one iperf server"
                )
            self.iperf_server = self.iperf_servers[0]
            self.iperf_server.start()

        if len(self.iperf_clients) > 0:
            self.iperf_client = self.iperf_clients[0]
        else:
            self.iperf_client = self.dut.create_iperf_client()

    def teardown_class(self) -> None:
        self.write_graph()
        if self.openwrt_ap is not None and self.iperf_server:
            # Stop the manually created iperf server. This is required because we
            # aren't registering the iperf server as a separate Mobly controller
            # when using the OpenWRT AP as iperf, so Mobly won't automatically
            # clean it up.
            self.iperf_server.stop()
        super().teardown_class()

    def setup_test(self) -> None:
        super().setup_test()
        # TODO(https://fxbug.dev/487691497): implement clear_country and uncomment
        # to clear up country changes before tests.
        # for fd in self.fuchsia_devices:
        #     phy_ids_response = fd.wlan_lib.wlanPhyIdList()
        #     if phy_ids_response.get('error'):
        #         raise ConnectionError(
        #             'Failed to retrieve phy ids from FuchsiaDevice (%s). '
        #             'Error: %s' % (fd.ip, phy_ids_response['error']))
        #     for id in phy_ids_response['result']:
        #         clear_country_response = fd.wlan_lib.wlanClearCountry(id)
        #         if clear_country_response.get('error'):
        #             raise EnvironmentError(
        #                 'Failed to reset country code on FuchsiaDevice (%s). '
        #                 'Error: %s' % (fd.ip, clear_country_response['error'])
        #                 )
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        for ad in self.android_devices:
            ad.droid.wakeLockAcquireBright()
            ad.droid.wakeUpNow()
        self.dut.wifi_toggle_state(True)
        self.dut.disconnect()

    def teardown_test(self) -> None:
        for ad in self.android_devices:
            ad.droid.wakeLockRelease()
            ad.droid.goToSleepNow()
        self.dut.turn_location_off_and_scan_toggle_off()
        self.dut.disconnect()
        self.download_logs()
        if self.access_point is not None:
            self.access_point.stop_all_aps()
        super().teardown_test()

    def setup_ap(
        self,
        channel: int,
        channel_bandwidth: int,
        security_mode: Security,
        password: str | None = None,
    ) -> str:
        """Start network on AP with basic configuration.

        Args:
            channel: channel to use for network
            channel_bandwidth: channel bandwidth in mhz to use for network,
            security_mode: security type to use (Security)
            password: password for the network if secured

        Returns:
            SSID of the newly created and running network

        Raises:
            ConnectionError if network is not started successfully.
        """
        ssid = AccessPointConfig.random_string(AP_SSID_LENGTH_2G)
        try:
            if self.openwrt_ap is not None:
                band = (
                    Band.BAND_2G if channel <= MAX_2_4_CHANNEL else Band.BAND_5G
                )
                phy_mode: PhyMode
                if band == Band.BAND_2G:
                    if channel_bandwidth == 40:
                        ext = cast(
                            Literal["+", "-"], "+" if channel <= 7 else "-"
                        )
                        phy_mode = HtMode(bw=40, extension=ext)
                    else:
                        phy_mode = HtMode(bw=20)
                else:
                    phy_mode = VhtMode(
                        bw=cast(Literal[20, 40, 80, 160], channel_bandwidth)
                    )

                config = AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=BssChannel(
                                band=band,
                                number=channel,
                                phy_mode=phy_mode,
                            ),
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    security=security_mode,
                                    password=password,
                                )
                            ],
                            country=DFS_BYPASS_COUNTRY_CODE,
                        )
                    ]
                )
                self.openwrt_ap.configure_wifi(config)
            else:
                # Legacy setup_ap expects antlion Security object
                security_profile = DeprecatedSecurity(
                    security_mode=AccessPointConfigMapper.to_hostapd_security(
                        security_mode
                    ),
                    password=password,
                )
                setup_ap(
                    access_point=self.access_point,
                    profile_name="whirlwind",
                    channel=channel,
                    security=security_profile,
                    force_wmm=True,
                    ssid=ssid,
                    vht_bandwidth=channel_bandwidth,
                    setup_bridge=True,
                )
            self.log.info(
                "Network (ssid: %s) up on channel %s w/ channel bandwidth %s MHz",
                ssid,
                channel,
                channel_bandwidth,
            )
            return ssid
        except Exception as err:
            raise ConnectionError(
                f"Failed to setup ap on channel: {channel}, "
                f"channel bandwidth: {channel_bandwidth} MHz. "
            ) from err

    def get_and_verify_iperf_address(
        self,
        channel: int,
        device: FuchsiaDevice | IPerfServerOverSsh,
        interface: str,
    ) -> str:
        """Get ip address from a devices interface and verify it belongs to
        expected subnet based on APs DHCP config.

        Args:
            channel: channel network is running on, to determine subnet
            device: device to get ip address for
            interface: interface on device to get ip address. If None, uses
                device.test_interface.

        Returns:
            IP address of device on given interface (or test_interface)

        Raises:
            ConnectionError, if device does not have a valid ip address after
                all retries.
        """
        if self.openwrt_ap is not None:
            subnet = self.openwrt_ap.default_subnet
        else:
            if channel <= MAX_2_4_CHANNEL:
                subnet = self.access_point._AP_2G_SUBNET_STR
            else:
                subnet = self.access_point._AP_5G_SUBNET_STR
        end_time = time.time() + self.time_to_wait_for_ip_addr
        while time.time() < end_time:
            device_addresses = device.get_interface_ip_addresses(interface)
            if device_addresses["ipv4_private"]:
                for ip_addr in device_addresses["ipv4_private"]:
                    if utils.ip_in_subnet(ip_addr, subnet):
                        return ip_addr
                    else:
                        self.log.debug(
                            "Device has an ip address (%s), but it is not in subnet %s",
                            ip_addr,
                            subnet,
                        )
            else:
                self.log.debug(
                    "Device does not have a valid ip address. Retrying."
                )
            time.sleep(TIME_TO_SLEEP_BETWEEN_RETRIES)
        raise ConnectionError("Device failed to get an ip address.")

    def get_iperf_throughput(
        self,
        iperf_server_address: str,
        iperf_client_address: str,
        reverse: bool = False,
    ) -> float:
        """Run iperf between client and server and get the throughput.

        Args:
            iperf_server_address: IP address of running iperf server
            iperf_client_address: IP address of iperf client (dut)
            reverse: If True, run traffic in reverse direction, from server to client.

        Returns:
            iperf throughput or 0 if iperf fails
        """
        if reverse:
            self.log.info(
                "Running IPerf traffic from server (%s) to dut (%s).",
                iperf_server_address,
                iperf_client_address,
            )
            iperf_results_file = self.iperf_client.start(
                iperf_server_address,
                "-i 1 -t 10 -R -J",
                "channel_sweep_rx",
                timeout=DEFAULT_IPERF_TIMEOUT,
            )
        else:
            self.log.info(
                "Running IPerf traffic from dut (%s) to server (%s).",
                iperf_client_address,
                iperf_server_address,
            )
            iperf_results_file = self.iperf_client.start(
                iperf_server_address,
                "-i 1 -t 10 -J",
                "channel_sweep_tx",
                timeout=DEFAULT_IPERF_TIMEOUT,
            )
        if iperf_results_file:
            iperf_results = IPerfResult(
                iperf_results_file, reporting_speed_units=MEGABITS_PER_SECOND
            )
            return iperf_results.avg_send_rate or 0.0
        return 0.0

    def log_to_file_and_throughput_data(
        self,
        test: TestParams,
        tx_throughput: float | None,
        rx_throughput: float | None,
    ) -> None:
        """Write performance info to csv file and to throughput data.

        Args:
            channel: int, channel that test was run on
            channel_bandwidth: int, channel bandwidth the test used
            tx_throughput: float, throughput value from dut to iperf server
            rx_throughput: float, throughput value from iperf server to dut
        """
        test_name = self.current_test_info.name
        log_file = Path(os.path.join(self.log_path, "throughput.csv"))
        self.log.info("Writing IPerf results for %s to %s", test_name, log_file)

        if not log_file.is_file():
            with open(log_file, "x", encoding="utf-8") as csv_file:
                csv_file.write(
                    "country code,security,channel,channel bandwidth,tx throughput,rx throughput\n"
                )

        with open(log_file, "a", encoding="utf-8") as csv_file:
            csv_file.write(
                f"{test.country_code},{test.security_mode},{test.channel},{test.channel_bandwidth},{tx_throughput},{rx_throughput}\n"
            )

        key = ThroughputKey.from_test(test)
        if key not in self.channel_throughput:
            self.channel_throughput[key] = []

        self.channel_throughput[key].append(
            ThroughputValue(
                channel=test.channel,
                tx_throughput_mbps=tx_throughput,
                rx_throughput_mbps=rx_throughput,
            )
        )

    def write_graph(self) -> None:
        """Create graph html files from throughput data, plotting channel vs
        tx_throughput and channel vs rx_throughput.
        """
        # If performance measurement is skipped
        if not self.iperf_server:
            return

        try:
            from bokeh.plotting import (
                ColumnDataSource,
                figure,
                output_file,
                save,
            )
        except ImportError:
            self.log.warning(
                "bokeh is not installed: skipping creation of graphs. "
                "Note CSV files are still available. If graphs are "
                'desired, install antlion with the "bokeh" feature.'
            )
            return

        for key, throughputs in self.channel_throughput.items():
            output_file_name = os.path.join(
                self.log_path,
                f"channel_throughput_{key.country_code}_{key.security_mode}_{key.channel_bandwidth}mhz.html",
            )
            output_file(output_file_name)
            channels = []
            tx_throughputs = []
            rx_throughputs = []

            for throughput in sorted(throughputs, key=lambda t: t.channel):
                channels.append(str(throughput.channel))
                tx_throughputs.append(throughput.tx_throughput_mbps)
                rx_throughputs.append(throughput.rx_throughput_mbps)

            channel_vs_throughput_data = ColumnDataSource(
                data=dict(
                    channels=channels,
                    tx_throughput=tx_throughputs,
                    rx_throughput=rx_throughputs,
                )
            )
            TOOLTIPS = [
                ("Channel", "@channels"),
                ("TX_Throughput", "@tx_throughput"),
                ("RX_Throughput", "@rx_throughput"),
            ]
            channel_vs_throughput_graph = figure(
                title="Channels vs. Throughput",
                x_axis_label="Channels",
                x_range=channels,
                y_axis_label="Throughput",
                tooltips=TOOLTIPS,
            )
            channel_vs_throughput_graph.sizing_mode = "stretch_both"
            channel_vs_throughput_graph.title.align = "center"
            channel_vs_throughput_graph.line(
                "channels",
                "tx_throughput",
                source=channel_vs_throughput_data,
                line_width=2,
                line_color="blue",
                legend_label="TX_Throughput",
            )
            channel_vs_throughput_graph.circle(
                "channels",
                "tx_throughput",
                source=channel_vs_throughput_data,
                size=GRAPH_CIRCLE_SIZE,
                color="blue",
            )
            channel_vs_throughput_graph.line(
                "channels",
                "rx_throughput",
                source=channel_vs_throughput_data,
                line_width=2,
                line_color="red",
                legend_label="RX_Throughput",
            )
            channel_vs_throughput_graph.circle(
                "channels",
                "rx_throughput",
                source=channel_vs_throughput_data,
                size=GRAPH_CIRCLE_SIZE,
                color="red",
            )

            channel_vs_throughput_graph.legend.location = "top_left"
            graph_file = save([channel_vs_throughput_graph])
            self.log.info("Saved graph to %s", graph_file)

    def test_standard_deviation(self) -> None:
        """Verify throughputs don't deviate too much across channels.

        Assert the throughput standard deviation across all channels of the same
        country, security, and bandwidth does not exceed the maximum specified in the
        user param config. If no maximum is set, a default of 1.0 standard deviations
        will be used (34.1% from the mean).

        Raises:
            TestFailure, if standard deviation of throughput exceeds max_std_dev
        """
        # If performance measurement is skipped
        if not self.iperf_server:
            return

        max_std_dev = self.user_params.get("channel_sweep_test_params", {}).get(
            "max_std_dev", DEFAULT_MAX_STD_DEV
        )

        self.log.info(
            "Verifying standard deviation across channels does not exceed max standard "
            "deviation of %s Mb/s",
            max_std_dev,
        )

        errors: list[str] = []

        for test, throughputs in self.channel_throughput.items():
            tx_values = []
            rx_values = []
            for throughput in throughputs:
                if throughput.tx_throughput_mbps is not None:
                    tx_values.append(throughput.tx_throughput_mbps)
                if throughput.rx_throughput_mbps is not None:
                    rx_values.append(throughput.rx_throughput_mbps)

            tx_std_dev = pstdev(tx_values)
            rx_std_dev = pstdev(rx_values)

            if tx_std_dev > max_std_dev:
                errors.append(
                    f"[{test.country_code} {test.security_mode} "
                    f"{test.channel_bandwidth}mhz] TX throughput standard deviation "
                    f"{tx_std_dev} Mb/s exceeds expected max of {max_std_dev} Mb/s"
                )
            if rx_std_dev > max_std_dev:
                errors.append(
                    f"[{test.country_code} {test.security_mode} "
                    f"{test.channel_bandwidth}mhz] RX throughput standard deviation "
                    f"{rx_std_dev} Mb/s exceeds expected max of {max_std_dev} Mb/s"
                )

        if errors:
            error_message = "\n - ".join(errors)
            asserts.fail(
                f"Failed to meet standard deviation expectations:\n - {error_message}"
            )

    def run_channel_performance(self, test: TestParams) -> None:
        """Run a single channel performance test

        Log results to csv file and throughput data.

        1. Sets up network with test settings
        2. Associates DUT
        3. Runs traffic between DUT and iperf server (both directions)
        4. Logs channel, tx_throughput (Mb/s), and rx_throughput (Mb/s) to
           log file and throughput data.
        5. Checks throughput values against minimum throughput thresholds.

        Raises:
            TestFailure, if throughput (either direction) is less than
                the directions given minimum throughput threshold.
        """
        self.fuchsia_device.wlan_controller.set_country_code(
            CountryCode(test.country_code)
        )

        if not isinstance(test.security_mode, SecurityOpen):
            if isinstance(test.security_mode, SecurityWep):
                password = AccessPointConfig.random_hex_string(
                    WEP_HEX_STRING_LENGTH
                )
            else:
                password = AccessPointConfig.random_string(MIN_WPA_PSK_LENGTH)
        else:
            password = None

        ssid = self.setup_ap(
            test.channel, test.channel_bandwidth, test.security_mode, password
        )

        if self.openwrt_ap is not None:
            interface = (
                self.openwrt_ap.wlan_2g_interface
                if test.channel <= MAX_2_4_CHANNEL
                else self.openwrt_ap.wlan_5g_interface
            )
            tcpdump_mgr = self.openwrt_ap.tcpdump.start(
                interface, Path(self.log_path)
            )
        else:
            interface = (
                self.access_point.wlan_2g
                if test.channel <= MAX_2_4_CHANNEL
                else self.access_point.wlan_5g
            )
            tcpdump_mgr = self.access_point.tcpdump.start(
                interface, Path(self.log_path)
            )

        with tcpdump_mgr:
            associated = self.dut.associate(
                ssid,
                target_pwd=password,
                target_security=AccessPointConfigMapper.to_hostapd_security(
                    test.security_mode
                ),
            )
            if not associated:
                self.log_to_file_and_throughput_data(test, None, None)
                asserts.fail(f"Device failed to associate to network {ssid}")
            self.log.info(
                'DUT (%s) connected to network "%s"', self.dut.identifier, ssid
            )

            assert self.iperf_server is not None
            if self.openwrt_ap is None:
                self.iperf_server.renew_test_interface_ip_address()
            if not isinstance(self.iperf_server.test_interface, str):
                raise TypeError(
                    "For this test, iperf_server is required to specify the "
                    "test_interface configuration option"
                )

            self.log.info(
                "Getting ip address for iperf server. Will retry for %s seconds.",
                self.time_to_wait_for_ip_addr,
            )
            iperf_server_address = self.get_and_verify_iperf_address(
                test.channel,
                self.iperf_server,
                self.iperf_server.test_interface,
            )
            self.log.info(
                "Getting ip address for DUT. Will retry for %s seconds.",
                self.time_to_wait_for_ip_addr,
            )

            if not isinstance(
                self.iperf_client, (IPerfClientOverSsh, IPerfClientOverAdb)
            ):
                raise TypeError(
                    f'Unknown iperf_client type "{type(self.iperf_client)}"'
                )
            if not isinstance(self.iperf_client.test_interface, str):
                raise TypeError(
                    "For this test, iperf_client is required to specify the "
                    "test_interface configuration option"
                )

            try:
                iperf_client_address = self.get_and_verify_iperf_address(
                    test.channel,
                    self.fuchsia_device,
                    self.iperf_client.test_interface,
                )
                tx_throughput = self.get_iperf_throughput(
                    iperf_server_address, iperf_client_address
                )
                rx_throughput = self.get_iperf_throughput(
                    iperf_server_address, iperf_client_address, reverse=True
                )
                self.log_to_file_and_throughput_data(
                    test, tx_throughput, rx_throughput
                )
                self.log.info(
                    "Throughput (tx, rx): (%s Mb/s, %s Mb/s), "
                    "Minimum threshold (tx, rx): (%s Mb/s, %s Mb/s)",
                    tx_throughput,
                    rx_throughput,
                    test.expect_min_tx_throughput_mbps,
                    test.expect_min_rx_throughput_mbps,
                )
                asserts.assert_greater(
                    tx_throughput,
                    test.expect_min_tx_throughput_mbps,
                    "tx throughput below the minimal threshold",
                )
                asserts.assert_greater(
                    rx_throughput,
                    test.expect_min_rx_throughput_mbps,
                    "rx throughput below the minimal threshold",
                )
            except Exception as e:
                if self.iperf_server and self.iperf_server._ssh_session:
                    ssh = self.iperf_server._ssh_session
                    if self.openwrt_ap is not None:
                        try:
                            self.log.warning(
                                "iperf ps w:\n%s",
                                ssh.run(["ps", "w"]).stdout.decode("utf-8"),
                            )
                        except Exception:
                            pass
                        try:
                            self.log.warning(
                                "iperf sockets:\n%s",
                                ssh.run(["netstat", "-tulpn"]).stdout.decode(
                                    "utf-8"
                                ),
                            )
                        except Exception:
                            pass
                    else:
                        try:
                            self.log.warning(
                                "iperf ps aux:\n%s",
                                ssh.run(["sudo", "ps", "aux"]).stdout.decode(
                                    "utf-8"
                                ),
                            )
                        except Exception:
                            pass
                        try:
                            self.log.warning(
                                "iperf sockets:\n%s",
                                ssh.run(["sudo", "ss", "-tulpn"]).stdout.decode(
                                    "utf-8"
                                ),
                            )
                        except Exception:
                            pass
                raise e


if __name__ == "__main__":
    test_runner.main()
