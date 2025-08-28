#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import itertools
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord

# AC Capabilities
"""
Capabilities Not Supported on Whirlwind:
    - Supported Channel Width ([VHT160], [VHT160-80PLUS80]): 160mhz and 80+80
        unsupported
    - SU Beamformer [SU-BEAMFORMER]
    - SU Beamformee [SU-BEAMFORMEE]
    - MU Beamformer [MU-BEAMFORMER]
    - MU Beamformee [MU-BEAMFORMEE]
    - BF Antenna ([BF-ANTENNA-2], [BF-ANTENNA-3], [BF-ANTENNA-4])
    - Rx STBC 2, 3, & 4 ([RX-STBC-12],[RX-STBC-123],[RX-STBC-124])
    - VHT Link Adaptation ([VHT-LINK-ADAPT2],[VHT-LINK-ADAPT3])
    - VHT TXOP Power Save [VHT-TXOP-PS]
    - HTC-VHT [HTC-VHT]
"""
VHT_MAX_MPDU_LEN = [
    hostapd_constants.AC_CAPABILITY_MAX_MPDU_7991,
    hostapd_constants.AC_CAPABILITY_MAX_MPDU_11454,
    "",
]
RXLDPC = [hostapd_constants.AC_CAPABILITY_RXLDPC, ""]
SHORT_GI_80 = [hostapd_constants.AC_CAPABILITY_SHORT_GI_80, ""]
TX_STBC = [hostapd_constants.AC_CAPABILITY_TX_STBC_2BY1, ""]
RX_STBC = [hostapd_constants.AC_CAPABILITY_RX_STBC_1, ""]
MAX_A_MPDU = [
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP0,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP1,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP2,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP3,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP4,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP5,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP6,
    hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP7,
    "",
]
RX_ANTENNA = [hostapd_constants.AC_CAPABILITY_RX_ANTENNA_PATTERN, ""]
TX_ANTENNA = [hostapd_constants.AC_CAPABILITY_TX_ANTENNA_PATTERN, ""]

# Default 11N Capabilities
N_CAPABS_40MHZ = [
    hostapd_constants.N_CAPABILITY_LDPC,
    hostapd_constants.N_CAPABILITY_SGI20,
    hostapd_constants.N_CAPABILITY_RX_STBC1,
    hostapd_constants.N_CAPABILITY_SGI20,
    hostapd_constants.N_CAPABILITY_SGI40,
    hostapd_constants.N_CAPABILITY_MAX_AMSDU_7935,
    hostapd_constants.N_CAPABILITY_HT40_PLUS,
]

N_CAPABS_20MHZ = [
    hostapd_constants.N_CAPABILITY_LDPC,
    hostapd_constants.N_CAPABILITY_SGI20,
    hostapd_constants.N_CAPABILITY_RX_STBC1,
    hostapd_constants.N_CAPABILITY_SGI20,
    hostapd_constants.N_CAPABILITY_MAX_AMSDU_7935,
    hostapd_constants.N_CAPABILITY_HT20,
]

SECURITY_MODES: list[SecurityMode] = [SecurityMode.OPEN, SecurityMode.WPA2]


@dataclass
class TestParams:
    security_mode: SecurityMode
    vht_bandwidth_mhz: int
    # TODO(http://b/290396383): Type AP capabilities as enums
    n_capabilities: list[Any]
    ac_capabilities: list[Any]


# 6912 test cases
class WlanPhyCompliance11ACTest(base_test.WifiBaseTest):
    """Tests for validating 11ac PHYS.

    Test Bed Requirement:
    * One Android device or Fuchsia device
    * One Access Point
    """

    def pre_run(self) -> None:
        test_args: list[tuple[TestParams]] = (
            self._generate_20mhz_test_args()
            + self._generate_40mhz_test_args()
            + self._generate_80mhz_test_args()
        )

        def generate_test_name(test: TestParams) -> str:
            ret = []
            for cap in hostapd_constants.AC_CAPABILITIES_MAPPING.keys():
                if cap in test.ac_capabilities:
                    ret.append(
                        hostapd_constants.AC_CAPABILITIES_MAPPING[cap]
                        .replace("[", "_")
                        .replace("]", "")
                    )
            return f"test_11ac_{test.vht_bandwidth_mhz}mhz_{test.security_mode}{''.join(ret)}"

        self.generate_tests(
            test_logic=self.setup_and_connect,
            name_func=generate_test_name,
            arg_sets=test_args,
        )

    def setup_class(self) -> None:
        super().setup_class()

        if len(self.access_points) == 0:
            raise signals.TestAbortClass(
                "At least one access point is required"
            )
        self.access_point = self.access_points[0]

        self.dut = self.get_dut(AssociationMode.POLICY)
        self.access_point.stop_all_aps()

    def setup_test(self) -> None:
        for ad in self.android_devices:
            ad.droid.wakeLockAcquireBright()
            ad.droid.wakeUpNow()
        self.dut.wifi_toggle_state(True)

    def teardown_test(self) -> None:
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

    def setup_and_connect(self, settings: TestParams) -> None:
        """Setup the AP and then attempt to associate a DUT.

        Args:
            settings: Test parameters
        """
        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        security: Security | None = None
        password: str | None = None

        match settings.security_mode:
            case SecurityMode.OPEN:
                pass
            case SecurityMode.WPA2:
                password = generate_random_password(
                    security_mode=SecurityMode.WPA2
                )
                security = Security(
                    security_mode=SecurityMode.WPA2,
                    password=password,
                    wpa_cipher=hostapd_constants.WPA2_DEFAULT_CIPER,
                    wpa2_cipher=hostapd_constants.WPA2_DEFAULT_CIPER,
                )
            case _:
                raise signals.TestError(
                    f"unsupported security_mode {settings.security_mode}"
                )

        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            mode=hostapd_constants.Mode.MODE_11AC_MIXED,
            channel=36,
            n_capabilities=settings.n_capabilities,
            ac_capabilities=settings.ac_capabilities,
            force_wmm=True,
            ssid=ssid,
            security=security,
            vht_bandwidth=settings.vht_bandwidth_mhz,
        )

        with self.access_point.tcpdump.start(
            self.access_point.wlan_5g, Path(self.log_path)
        ):
            asserts.assert_true(
                self.dut.associate(
                    ssid,
                    target_pwd=password,
                    target_security=settings.security_mode,
                ),
                "Failed to associate.",
            )

    # 1728 tests
    def _generate_20mhz_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []

        # 864 test cases for open security
        # 864 test cases for wpa2 security
        for combination in itertools.product(
            SECURITY_MODES,
            VHT_MAX_MPDU_LEN,
            RXLDPC,
            RX_STBC,
            TX_STBC,
            MAX_A_MPDU,
            RX_ANTENNA,
            TX_ANTENNA,
        ):
            test_args.append(
                (
                    TestParams(
                        security_mode=combination[0],
                        vht_bandwidth_mhz=20,
                        n_capabilities=N_CAPABS_20MHZ,
                        ac_capabilities=list(combination[1:]),
                    ),
                )
            )

        return test_args

    # 1728 tests
    def _generate_40mhz_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []

        # 864 test cases for open security
        # 864 test cases for wpa2 security
        for combination in itertools.product(
            SECURITY_MODES,
            VHT_MAX_MPDU_LEN,
            RXLDPC,
            RX_STBC,
            TX_STBC,
            MAX_A_MPDU,
            RX_ANTENNA,
            TX_ANTENNA,
        ):
            test_args.append(
                (
                    TestParams(
                        security_mode=combination[0],
                        vht_bandwidth_mhz=40,
                        n_capabilities=N_CAPABS_40MHZ,
                        ac_capabilities=list(combination[1:]),
                    ),
                )
            )

        return test_args

    # 3456 tests
    def _generate_80mhz_test_args(self) -> list[tuple[TestParams]]:
        test_args: list[tuple[TestParams]] = []

        # 1728 test cases for open security
        # 1728 test cases for wpa2 security
        for combination in itertools.product(
            SECURITY_MODES,
            VHT_MAX_MPDU_LEN,
            RXLDPC,
            SHORT_GI_80,
            RX_STBC,
            TX_STBC,
            MAX_A_MPDU,
            RX_ANTENNA,
            TX_ANTENNA,
        ):
            test_args.append(
                (
                    TestParams(
                        security_mode=combination[0],
                        vht_bandwidth_mhz=80,
                        n_capabilities=N_CAPABS_40MHZ,
                        ac_capabilities=list(combination[1:]),
                    ),
                )
            )
        return test_args


if __name__ == "__main__":
    test_runner.main()
