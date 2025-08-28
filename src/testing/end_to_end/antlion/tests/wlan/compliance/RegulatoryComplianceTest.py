#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
from typing import NamedTuple

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.controllers.ap_lib.regulatory_channels import (
    COUNTRY_CHANNELS,
    TEST_CHANNELS,
)
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.types import CountryCode
from mobly import asserts, test_runner
from mobly.config_parser import TestRunConfig

N_CAPABILITIES_DEFAULT = [
    hostapd_constants.N_CAPABILITY_LDPC,
    hostapd_constants.N_CAPABILITY_SGI20,
    hostapd_constants.N_CAPABILITY_SGI40,
    hostapd_constants.N_CAPABILITY_TX_STBC,
    hostapd_constants.N_CAPABILITY_RX_STBC1,
]

MAX_2_4_CHANNEL = 14


class RegulatoryTest(NamedTuple):
    country_code: str
    channel: int
    channel_bandwidth: int
    expect_association: bool


class RegulatoryComplianceTest(base_test.WifiBaseTest):
    """Tests regulatory compliance.

    Testbed Requirement:
    * 1 x Fuchsia device (dut)
    * 1 x access point
    """

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()
        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

        self.access_point = self.access_points[0]
        self.access_point.stop_all_aps()

        self.regulatory_results = [
            "====CountryCode,Channel,Frequency,ChannelBandwith,Connected/Not-Connected===="
        ]

    def pre_run(self) -> None:
        tests: list[RegulatoryTest] = []
        for country in COUNTRY_CHANNELS.values():
            for channel, bandwidths in TEST_CHANNELS.items():
                for bandwidth in bandwidths:
                    tests.append(
                        RegulatoryTest(
                            country_code=country.country_code,
                            channel=channel,
                            channel_bandwidth=bandwidth,
                            expect_association=(
                                channel in country.allowed_channels
                                and bandwidth
                                in country.allowed_channels[channel]
                            ),
                        )
                    )

        def generate_test_name(
            country_code: str,
            channel: int,
            channel_bandwidth: int,
            _expect_association: bool,
        ) -> str:
            return (
                f"test_{country_code}_channel_{channel}_{channel_bandwidth}mhz"
            )

        self.generate_tests(
            self.verify_channel_compliance, generate_test_name, tests
        )

    def teardown_class(self) -> None:
        super().teardown_class()

        regulatory_save_path = f"{self.log_path}/regulatory_results.txt"
        with open(regulatory_save_path, "w", encoding="utf-8") as file:
            file.write("\n".join(self.regulatory_results))

    def setup_test(self) -> None:
        super().setup_test()
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
        self.access_point.stop_all_aps()
        super().teardown_test()

    def setup_ap(
        self,
        channel: int,
        channel_bandwidth: int,
    ) -> str:
        """Start network on AP with basic configuration.

        Args:
            channel: channel to use for network
            channel_bandwidth: channel bandwidth in mhz to use for network,

        Returns:
            SSID of the newly created and running network

        Raises:
            ConnectionError if network is not started successfully.
        """
        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        try:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=channel,
                force_wmm=True,
                ssid=ssid,
                vht_bandwidth=channel_bandwidth,
                setup_bridge=True,
            )
            self.log.info(
                f"Network (ssid: {ssid}) up on channel {channel} "
                f"w/ channel bandwidth {channel_bandwidth} MHz"
            )
            return ssid
        except Exception as err:
            raise ConnectionError(
                f"Failed to setup ap on channel: {channel}, "
                f"channel bandwidth: {channel_bandwidth} MHz. "
            ) from err

    def verify_channel_compliance(
        self,
        country_code: str,
        channel: int,
        channel_bandwidth: int,
        expect_association: bool,
    ) -> None:
        """Verify device complies with provided regulatory requirements for a
        specific channel and channel bandwidth. Run with generated test cases
        in the verify_regulatory_compliance parent test.
        """
        self.fuchsia_device.wlan_controller.set_country_code(
            CountryCode(country_code)
        )

        ssid = self.setup_ap(channel, channel_bandwidth)

        self.log.info(
            f'Attempting to associate to network "{ssid}" on channel '
            f"{channel} @ {channel_bandwidth}mhz"
        )

        associated = self.dut.associate(ssid, SecurityMode.OPEN)

        channel_ghz = "2.4" if channel < 36 else "5"
        association_code = "c" if associated else "nc"
        regulatory_result = f"REGTRACKER: {country_code},{channel},{channel_ghz},{channel_bandwidth},{association_code}"
        self.regulatory_results.append(regulatory_result)
        self.log.info(regulatory_result)

        asserts.assert_true(
            associated == expect_association,
            f"Expected device to{'' if expect_association else ' NOT'} "
            f"associate using country code {country_code} for channel "
            f"{channel} with channel bandwidth {channel_bandwidth} MHz.",
        )


if __name__ == "__main__":
    test_runner.main()
