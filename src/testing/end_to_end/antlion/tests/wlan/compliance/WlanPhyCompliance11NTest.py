#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import itertools
import logging
from dataclasses import dataclass
from typing import Any

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_config, hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from mobly.records import TestResultRecord

FREQUENCY_24: str = "2.4GHz"
FREQUENCY_5: str = "5GHz"
CHANNEL_BANDWIDTH_20: str = "HT20"
CHANNEL_BANDWIDTH_40_LOWER: str = "HT40-"
CHANNEL_BANDWIDTH_40_UPPER: str = "HT40+"
SECURITY_OPEN = "open"
SECURITY_WPA2 = "wpa2"
N_MODE = [
    hostapd_constants.Mode.MODE_11N_PURE,
    hostapd_constants.Mode.MODE_11N_MIXED,
]
LDPC = [hostapd_constants.N_CAPABILITY_LDPC, ""]
TX_STBC = [hostapd_constants.N_CAPABILITY_TX_STBC, ""]
RX_STBC = [hostapd_constants.N_CAPABILITY_RX_STBC1, ""]
SGI_20 = [hostapd_constants.N_CAPABILITY_SGI20, ""]
SGI_40 = [hostapd_constants.N_CAPABILITY_SGI40, ""]
DSSS_CCK = [hostapd_constants.N_CAPABILITY_DSSS_CCK_40, ""]
INTOLERANT_40 = [hostapd_constants.N_CAPABILITY_40_INTOLERANT, ""]
MAX_AMPDU_7935 = [hostapd_constants.N_CAPABILITY_MAX_AMSDU_7935, ""]
SMPS = [hostapd_constants.N_CAPABILITY_SMPS_STATIC, ""]


@dataclass
class TestParams:
    frequency: str
    chbw: str
    n_mode: str
    security: SecurityMode
    # TODO(http://b/290396383): Type AP capabilities as enums
    n_capabilities: list[Any]


class WlanPhyCompliance11NTest(base_test.WifiBaseTest):
    """Tests for validating 11n PHYS.

    Test Bed Requirement:
    * One Android device or Fuchsia device
    * One Access Point
    """

    def __init__(self, config: TestRunConfig) -> None:
        super().__init__(config)

    def pre_run(self) -> None:
        test_args: list[tuple[TestParams]] = (
            self._generate_24_HT20_test_args()
            + self._generate_24_HT40_lower_test_args()
            + self._generate_24_HT40_upper_test_args()
            + self._generate_5_HT20_test_args()
            + self._generate_5_HT40_lower_test_args()
            + self._generate_5_HT40_upper_test_args()
            + self._generate_24_HT20_wpa2_test_args()
            + self._generate_24_HT40_lower_wpa2_test_args()
            + self._generate_24_HT40_upper_wpa2_test_args()
            + self._generate_5_HT20_wpa2_test_args()
            + self._generate_5_HT40_lower_wpa2_test_args()
            + self._generate_5_HT40_upper_wpa2_test_args()
        )

        def generate_test_name(test: TestParams) -> str:
            ret = []
            for cap in hostapd_constants.N_CAPABILITIES_MAPPING.keys():
                if cap in test.n_capabilities:
                    ret.append(
                        hostapd_constants.N_CAPABILITIES_MAPPING[cap]
                        .replace("[", "_")
                        .replace("]", "")
                    )
            # '+' is used by Mobile Harness as special character, don't use it in test names
            if test.chbw == "HT40-":
                chbw = "HT40Lower"
            elif test.chbw == "HT40+":
                chbw = "HT40Upper"
            else:
                chbw = test.chbw
            return f"test_11n_{test.frequency}_{chbw}_{test.security}_{test.n_mode}{''.join(ret)}"

        self.generate_tests(
            test_logic=self.setup_and_connect,
            name_func=generate_test_name,
            arg_sets=test_args,
        )

    def setup_class(self) -> None:
        super().setup_class()

        if len(self.access_points) < 1:
            logging.error("At least one access point is required for this test")
            raise signals.TestAbortClass(
                "At least one access point is required"
            )

        self.dut = self.get_dut(AssociationMode.POLICY)

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]
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
        self.download_logs()
        self.access_point.stop_all_aps()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        self.access_point.stop_all_aps()

    def setup_and_connect(self, test: TestParams) -> None:
        """Start hostapd and associate the DUT.

        Args:
               ap_settings: A dictionary of hostapd constant n_capabilities.
        """
        ssid = utils.rand_ascii_str(20)
        security_profile = Security()
        password: str | None = None
        n_capabilities = []
        for n_capability in test.n_capabilities:
            if n_capability in hostapd_constants.N_CAPABILITIES_MAPPING.keys():
                n_capabilities.append(n_capability)

        if test.chbw == "HT20" or test.chbw == "HT40+":
            if test.frequency == "2.4GHz":
                channel = 1
            elif test.frequency == "5GHz":
                channel = 36
            else:
                raise ValueError(f"Invalid frequence: {test.frequency}")

        elif test.chbw == "HT40-":
            if test.frequency == "2.4GHz":
                channel = 11
            elif test.frequency == "5GHz":
                channel = 60
            else:
                raise ValueError(f"Invalid frequency: {test.frequency}")

        else:
            raise ValueError(f"Invalid channel bandwidth: {test.chbw}")

        if test.chbw == "HT40-" or test.chbw == "HT40+":
            if hostapd_config.ht40_plus_allowed(channel):
                extended_channel = hostapd_constants.N_CAPABILITY_HT40_PLUS
            elif hostapd_config.ht40_minus_allowed(channel):
                extended_channel = hostapd_constants.N_CAPABILITY_HT40_MINUS
            else:
                raise ValueError(f"Invalid channel: {channel}")
            n_capabilities.append(extended_channel)

        if test.security is SecurityMode.WPA2:
            security_profile = Security(
                security_mode=SecurityMode.WPA2,
                password=generate_random_password(length=20),
                wpa_cipher="CCMP",
                wpa2_cipher="CCMP",
            )
            password = security_profile.password

        if test.n_mode not in N_MODE:
            raise ValueError(f"Invalid n-mode: {test.n_mode}")

        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            mode=test.n_mode,
            channel=channel,
            n_capabilities=n_capabilities,
            ac_capabilities=[],
            force_wmm=True,
            ssid=ssid,
            security=security_profile,
        )
        asserts.assert_true(
            self.dut.associate(
                ssid,
                target_pwd=password,
                target_security=test.security,
            ),
            "Failed to connect.",
        )

    def _generate_24_HT20_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            N_MODE,
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            INTOLERANT_40,
            MAX_AMPDU_7935,
            SMPS,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_24,
                        chbw=CHANNEL_BANDWIDTH_20,
                        n_mode=combination[0],
                        security=SecurityMode.OPEN,
                        n_capabilities=list(combination[1:]),
                    ),
                )
            )
        return test_args

    def _generate_24_HT40_lower_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_24,
                        chbw=CHANNEL_BANDWIDTH_40_LOWER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.OPEN,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_24_HT40_upper_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_24,
                        chbw=CHANNEL_BANDWIDTH_40_UPPER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.OPEN,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_5_HT20_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            INTOLERANT_40,
            MAX_AMPDU_7935,
            SMPS,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_5,
                        chbw=CHANNEL_BANDWIDTH_20,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.OPEN,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_5_HT40_lower_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_5,
                        chbw=CHANNEL_BANDWIDTH_40_LOWER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.OPEN,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_5_HT40_upper_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            N_MODE,
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_5,
                        chbw=CHANNEL_BANDWIDTH_40_UPPER,
                        n_mode=combination[0],
                        security=SecurityMode.OPEN,
                        n_capabilities=list(combination[1:]),
                    ),
                )
            )
        return test_args

    def _generate_24_HT20_wpa2_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            INTOLERANT_40,
            MAX_AMPDU_7935,
            SMPS,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_24,
                        chbw=CHANNEL_BANDWIDTH_20,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.WPA2,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_24_HT40_lower_wpa2_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_24,
                        chbw=CHANNEL_BANDWIDTH_40_LOWER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.WPA2,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_24_HT40_upper_wpa2_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_24,
                        chbw=CHANNEL_BANDWIDTH_40_UPPER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.WPA2,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_5_HT20_wpa2_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            INTOLERANT_40,
            MAX_AMPDU_7935,
            SMPS,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_5,
                        chbw=CHANNEL_BANDWIDTH_20,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.WPA2,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_5_HT40_lower_wpa2_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_5,
                        chbw=CHANNEL_BANDWIDTH_40_LOWER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.WPA2,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args

    def _generate_5_HT40_upper_wpa2_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []
        for combination in itertools.product(
            LDPC,
            TX_STBC,
            RX_STBC,
            SGI_20,
            SGI_40,
            MAX_AMPDU_7935,
            SMPS,
            DSSS_CCK,
        ):
            test_args.append(
                (
                    TestParams(
                        frequency=FREQUENCY_5,
                        chbw=CHANNEL_BANDWIDTH_40_UPPER,
                        n_mode=hostapd_constants.Mode.MODE_11N_MIXED,
                        security=SecurityMode.WPA2,
                        n_capabilities=list(combination),
                    ),
                )
            )
        return test_args


if __name__ == "__main__":
    test_runner.main()
