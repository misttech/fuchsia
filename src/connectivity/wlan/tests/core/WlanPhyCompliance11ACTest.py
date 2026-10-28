#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import itertools
import logging
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

import fidl_fuchsia_wlan_internal as fidl_security
import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from legacy_access_point.access_point import setup_ap
from legacy_access_point.ap_lib import hostapd_constants
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from legacy_access_point.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib import capabilities
from openwrt_access_point.lib.access_point_config import (
    DFS_BYPASS_COUNTRY_CODE,
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    CapabilitySelection,
    RadioConfig,
    SecurityWpa2,
    VhtMode,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

logger = logging.getLogger(__name__)

# AC capabilities unsupported on OpenWrt One (MT7981B + MT7976C 3T3R 5 GHz,
# VHT capab 0x339a59f6):
# * 80+80 MHz channel width ([VHT160-80PLUS80])
# * 4x4 beamforming ([SOUNDING-DIMENSION-4], [BF-ANTENNA-4])
# * Multi-stream RX STBC ([RX-STBC-12], [RX-STBC-123], [RX-STBC-1234])
# * HTC-VHT and VHT link adaptation ([HTC-VHT], [VHT-LINK-ADAPT2/3])
# * VHT TXOP power save ([VHT-TXOP-PS])
OPENWRT_ONE_UNSUPPORTED_AC_CAPS: frozenset[str] = frozenset(
    {
        capabilities.AC_CAPABILITY_VHT160_80PLUS80,  # driver doesn't support 80+80
        capabilities.AC_CAPABILITY_BF_ANTENNA_4,  # 3x3 AP
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_4,  # 3x3 AP
        capabilities.AC_CAPABILITY_RX_STBC_12,  # driver clamps to STBC-1
        capabilities.AC_CAPABILITY_RX_STBC_123,  # driver clamps to STBC-1
        capabilities.AC_CAPABILITY_RX_STBC_1234,  # driver clamps to STBC-1
        capabilities.AC_CAPABILITY_HTC_VHT,  # driver doesn't support this
        capabilities.AC_CAPABILITY_VHT_LINK_ADAPT2,  # driver doesn't support this
        capabilities.AC_CAPABILITY_VHT_LINK_ADAPT3,  # driver doesn't support this
        capabilities.AC_CAPABILITY_VHT_TXOP_PS,  # driver doesn't support this
    }
)

# Capabilities unsupported on legacy Whirlwind APs (IPQ4019 2x2).
WHIRLWIND_UNSUPPORTED_AC_CAPS: frozenset[str] = frozenset(
    {
        capabilities.AC_CAPABILITY_VHT160,
        capabilities.AC_CAPABILITY_VHT160_80PLUS80,
        capabilities.AC_CAPABILITY_SHORT_GI_160,
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_MU_BEAMFORMER,
        capabilities.AC_CAPABILITY_MU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_BF_ANTENNA_2,
        capabilities.AC_CAPABILITY_BF_ANTENNA_3,
        capabilities.AC_CAPABILITY_BF_ANTENNA_4,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_2,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_3,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_4,
        capabilities.AC_CAPABILITY_RX_STBC_12,
        capabilities.AC_CAPABILITY_RX_STBC_123,
        capabilities.AC_CAPABILITY_RX_STBC_1234,
        capabilities.AC_CAPABILITY_HTC_VHT,
        capabilities.AC_CAPABILITY_VHT_LINK_ADAPT2,
        capabilities.AC_CAPABILITY_VHT_LINK_ADAPT3,
        capabilities.AC_CAPABILITY_VHT_TXOP_PS,
    }
)

ALL_BANDWIDTHS: list[Literal[20, 40, 80, 160]] = [20, 40, 80, 160]
WIDE_BANDWIDTHS: list[Literal[80, 160]] = [80, 160]

BEAMFORMING_STATES: list[list[str]] = [
    # Off (Baseline)
    [],
    # 2x2 SU-BFr
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_2,
    ],
    # 2x2 SU+MU-BFr
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_MU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_2,
    ],
    # 3x3 SU-BFr
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_3,
    ],
    # 3x3 SU+MU-BFr
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_MU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_3,
    ],
    # 4x4 SU-BFr
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_4,
    ],
    # 4x4 SU+MU-BFr
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_MU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_4,
    ],
    # 3x3 Beamformee Only (SU+MU-BFe)
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_MU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_BF_ANTENNA_3,
    ],
    # 4x4 Beamformee Only (SU+MU-BFe)
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_MU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_BF_ANTENNA_4,
    ],
    # 2x2 SU-BFr with 3x3 SU-BFe (Antenna Mismatch)
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_2,
        capabilities.AC_CAPABILITY_SU_BEAMFORMEE,
        capabilities.AC_CAPABILITY_BF_ANTENNA_3,
    ],
]

TX_STBC = ["", capabilities.AC_CAPABILITY_TX_STBC_2BY1]
RX_STBC = [
    "",
    capabilities.AC_CAPABILITY_RX_STBC_1,
    capabilities.AC_CAPABILITY_RX_STBC_12,
    capabilities.AC_CAPABILITY_RX_STBC_123,
    capabilities.AC_CAPABILITY_RX_STBC_1234,
]

BF_STBC_COEXISTENCE_PROFILES: list[list[str]] = [
    # 2x2 SU-BFr + STBC
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_2,
        capabilities.AC_CAPABILITY_TX_STBC_2BY1,
        capabilities.AC_CAPABILITY_RX_STBC_1,
        capabilities.AC_CAPABILITY_RX_ANTENNA_PATTERN,
        capabilities.AC_CAPABILITY_TX_ANTENNA_PATTERN,
    ],
    # 3x3 SU+MU-BFr + STBC
    [
        capabilities.AC_CAPABILITY_SU_BEAMFORMER,
        capabilities.AC_CAPABILITY_MU_BEAMFORMER,
        capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_3,
        capabilities.AC_CAPABILITY_TX_STBC_2BY1,
        capabilities.AC_CAPABILITY_RX_STBC_1,
        capabilities.AC_CAPABILITY_RX_ANTENNA_PATTERN,
        capabilities.AC_CAPABILITY_TX_ANTENNA_PATTERN,
    ],
]

LINK_ADAPTATION_HTC_STATES: list[list[str]] = [
    # HTC=0, LA=0 (Baseline)
    [],
    # HTC=1, LA=0 (HTC only)
    [capabilities.AC_CAPABILITY_HTC_VHT],
    # HTC=1, LA=2 (Unsolicited MFB)
    [
        capabilities.AC_CAPABILITY_HTC_VHT,
        capabilities.AC_CAPABILITY_VHT_LINK_ADAPT2,
    ],
    # HTC=1, LA=3 (MRQ + MFB)
    [
        capabilities.AC_CAPABILITY_HTC_VHT,
        capabilities.AC_CAPABILITY_VHT_LINK_ADAPT3,
    ],
    # HTC=0, LA=3 (Invalid Negative Test)
    [capabilities.AC_CAPABILITY_VHT_LINK_ADAPT3],
]

VHT_MAX_MPDU_LEN = [
    "",
    capabilities.AC_CAPABILITY_MAX_MPDU_7991,
    capabilities.AC_CAPABILITY_MAX_MPDU_11454,
]
MAX_A_MPDU = [
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP0,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP1,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP2,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP3,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP4,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP5,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP6,
    capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP7,
]

RXLDPC = ["", capabilities.AC_CAPABILITY_RXLDPC]

# Omit N_CAPABILITY_RX_STBC1: OpenWrt shares rx_stbc between HT/VHT, so 11ac
# tests control it via ac_capabilities.
N_CAPABS_40MHZ = [
    capabilities.N_CAPABILITY_LDPC,
    capabilities.N_CAPABILITY_SHORT_GI_20,
    capabilities.N_CAPABILITY_SHORT_GI_40,
    capabilities.N_CAPABILITY_MAX_AMSDU_7935,
    capabilities.N_CAPABILITY_HT40_PLUS,
]

N_CAPABS_20MHZ = [
    capabilities.N_CAPABILITY_LDPC,
    capabilities.N_CAPABILITY_SHORT_GI_20,
    capabilities.N_CAPABILITY_MAX_AMSDU_7935,
    capabilities.N_CAPABILITY_HT20,
]


@dataclass
class TestParams:
    vht_bandwidth_mhz: Literal[20, 40, 80, 160]
    # TODO(http://b/290396383): Type AP capabilities as enums
    n_capabilities: list[str]
    ac_capabilities: list[str]


class WlanPhyCompliance11ACTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests for validating 11ac PHYs.

    For test matrix design and rationale, see http://go/wlan-test-optimization.

    Test Bed Requirement:
    * One Fuchsia device
    * One Access Point
    """

    phy: wlan_core.Phy

    async def setup_class(self) -> None:
        await super().setup_class()
        if self.access_point:
            self.access_point.stop_all_aps()
        self.phy = await self.dut.wlan_core.ensure_single_phy()

        # 802.11ac only specifies operation in the 5 GHz band.
        # Set country to US so that 5 GHz channels are supported.
        await self.phy.set_country(
            KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def teardown_test(self) -> None:
        await self.dut.wlan_core.destroy_all_ifaces()
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def pre_run(self) -> None:
        raw_test_args: list[tuple[TestParams]] = (
            self._generate_baseline_and_stress_test_args()
            + self._generate_spatial_stbc_bf_test_args()
            + self._generate_link_adaptation_htc_test_args()
            + self._generate_mpdu_ampdu_limits_test_args()
            + self._generate_phy_mac_interaction_test_args()
        )

        def generate_test_name(params: TestParams) -> str:
            ret = []
            mapped_caps = [
                ConfigMapper.to_hostapd_ac_cap(c)
                for c in params.ac_capabilities
                if c
            ]
            for cap in hostapd_constants.AC_CAPABILITIES_MAPPING.keys():
                if cap in mapped_caps:
                    ret.append(
                        hostapd_constants.AC_CAPABILITIES_MAPPING[cap]
                        .replace("[", "_")
                        .replace("]", "")
                    )

            # Maintain legacy naming for BUILD.gn filters
            return f"test_11ac_{params.vht_bandwidth_mhz}mhz_wpa2{''.join(ret)}"

        # Map test names as dict keys to collapse duplicates, then extract the unique
        # parameter values (avoids set() since TestParams contains an unhashable list).
        test_args = list(
            {generate_test_name(p): (p,) for (p,) in raw_test_args}.values()
        )

        self.generate_tests(
            test_logic=self.setup_and_connect,
            name_func=generate_test_name,
            arg_sets=test_args,
        )

    async def setup_and_connect(self, params: TestParams) -> None:
        """Sets up the AP and associates the DUT."""
        self._skip_if_unsupported(params)

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        password = AccessPointConfig.random_string()
        protocol = fidl_security.Protocol.WPA2_PERSONAL

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=BssChannel(
                            band=Band.BAND_5G,
                            number=36,
                            phy_mode=VhtMode(bw=params.vht_bandwidth_mhz),
                        ),
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityWpa2(),
                                password=password,
                            )
                        ],
                        country=DFS_BYPASS_COUNTRY_CODE,
                        n_capabilities=CapabilitySelection.CUSTOM(
                            params.n_capabilities
                        ),
                        ac_capabilities=CapabilitySelection.CUSTOM(
                            params.ac_capabilities
                        ),
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
            self._verify_openwrt_vht_capabilities(params.ac_capabilities)
            await self._connect_and_validate_channel(
                ssid,
                protocol,
                target_channel=36,
                target_pwd=password,
            )
        elif self.access_point:
            security = DeprecatedSecurity(
                security_mode=DeprecatedSecurityMode.WPA2,
                password=password,
                wpa_cipher=hostapd_constants.WPA2_DEFAULT_CIPER,
                wpa2_cipher=hostapd_constants.WPA2_DEFAULT_CIPER,
            )
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                mode=hostapd_constants.Mode.MODE_11AC_MIXED,
                channel=36,
                n_capabilities=[
                    ConfigMapper.to_hostapd_n_cap(c)
                    for c in params.n_capabilities
                    if c
                ],
                ac_capabilities=[
                    ConfigMapper.to_hostapd_ac_cap(c)
                    for c in params.ac_capabilities
                    if c
                ],
                force_wmm=True,
                ssid=ssid,
                security=security,
                vht_bandwidth=params.vht_bandwidth_mhz,
            )
            with self.access_point.tcpdump.start(
                self.access_point.wlan_5g, Path(self.log_path)
            ):
                await self._connect_and_validate_channel(
                    ssid,
                    protocol,
                    target_channel=36,
                    target_pwd=password,
                )

    def _skip_if_unsupported(self, params: TestParams) -> None:
        """Skips the test case if the active AP does not support params."""
        if self.openwrt_ap:
            ap_name = "OpenWrt One AP"
            unsupported_caps = OPENWRT_ONE_UNSUPPORTED_AC_CAPS
        elif self.access_point:
            asserts.skip_if(
                params.vht_bandwidth_mhz == 160,
                "Skipped because 160 MHz bandwidth is not supported by Whirlwind AP",
            )
            ap_name = "Whirlwind AP"
            unsupported_caps = WHIRLWIND_UNSUPPORTED_AC_CAPS
        else:
            return

        unsupported = [
            c for c in params.ac_capabilities if c in unsupported_caps
        ]
        asserts.skip_if(
            bool(unsupported),
            f"Skipped because capabilities {unsupported} are not supported by {ap_name}",
        )

    def _verify_openwrt_vht_capabilities(
        self, ac_capabilities: list[str]
    ) -> None:
        """Verifies hostapd-phy1.conf matches the requested VHT capabilities."""
        if not self.openwrt_ap:
            return
        res = self.openwrt_ap.ssh.run(
            "grep '^vht_capab=' /var/run/hostapd-phy1.conf || true"
        )
        vht_capab_line = res.stdout.decode("utf-8").strip()
        actual_tokens = set(re.findall(r"\[[^\]]+\]", vht_capab_line))

        expected_tokens: set[str] = set()
        for cap in ac_capabilities:
            if not cap or cap == capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP0:
                # OpenWrt's mac80211.sh / hostapd.uc omits [MAX-A-MPDU-LEN-EXP0]
                # from vht_capab when the exponent is 0 (the IEEE 802.11ac base value).
                continue
            expected_tokens.add(f"[{cap}]")

        # mac80211.sh defaults to 3 antennas when SU-BEAMFORMER/BEAMFORMEE
        # lacks an explicit antenna count override.
        if (
            capabilities.AC_CAPABILITY_SU_BEAMFORMER in ac_capabilities
            and capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_2
            not in ac_capabilities
            and capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_3
            not in ac_capabilities
            and capabilities.AC_CAPABILITY_SOUNDING_DIMENSION_4
            not in ac_capabilities
        ):
            expected_tokens.add("[SOUNDING-DIMENSION-3]")

        if (
            capabilities.AC_CAPABILITY_SU_BEAMFORMEE in ac_capabilities
            and capabilities.AC_CAPABILITY_BF_ANTENNA_2 not in ac_capabilities
            and capabilities.AC_CAPABILITY_BF_ANTENNA_3 not in ac_capabilities
            and capabilities.AC_CAPABILITY_BF_ANTENNA_4 not in ac_capabilities
        ):
            expected_tokens.add("[BF-ANTENNA-3]")

        logger.info(
            "Verifying OpenWrt VHT capabilities: expected=%s, actual=%s (raw=%r)",
            sorted(expected_tokens),
            sorted(actual_tokens),
            vht_capab_line,
        )
        asserts.assert_equal(
            actual_tokens,
            expected_tokens,
            f"Mismatch in /var/run/hostapd-phy1.conf vht_capab! "
            f"Missing: {sorted(expected_tokens - actual_tokens)}, "
            f"Unexpected: {sorted(actual_tokens - expected_tokens)}, "
            f"Raw line: {vht_capab_line}",
        )
        logger.info(
            "Successfully verified OpenWrt VHT capabilities in /var/run/hostapd-phy1.conf"
        )

    async def _connect_and_validate_channel(
        self,
        target_ssid: str,
        target_security: fidl_security.Protocol,
        target_channel: int,
        target_pwd: str | None = None,
    ) -> None:
        iface = await self.phy.create_client_iface()
        await iface.scan_and_connect(
            ssid=target_ssid,
            password=target_pwd,
            security=target_security,
            channels=[target_channel],
        )
        status = await iface.status()
        if status.connected is None:
            raise signals.TestFailure(
                f"Expected connected status, got: {status}"
            )
        got_channel = status.connected.primary.number
        asserts.assert_equal(
            got_channel,
            target_channel,
            f"Connected to wrong channel. Expected channel {target_channel}, "
            f"got {got_channel}.",
        )

    @staticmethod
    def _make_test_params(
        vht_bandwidth_mhz: Literal[20, 40, 80, 160],
        ac_capabilities: list[str],
    ) -> tuple[TestParams]:
        n_caps = N_CAPABS_20MHZ if vht_bandwidth_mhz == 20 else N_CAPABS_40MHZ
        ac_caps: list[str] = []
        if vht_bandwidth_mhz == 160:
            ac_caps.append(capabilities.AC_CAPABILITY_VHT160)
        for cap in ac_capabilities:
            if cap and cap not in ac_caps:
                ac_caps.append(cap)
        return (
            TestParams(
                vht_bandwidth_mhz=vht_bandwidth_mhz,
                n_capabilities=n_caps,
                ac_capabilities=ac_caps,
            ),
        )

    def _generate_spatial_stbc_bf_test_args(self) -> list[tuple[TestParams]]:
        """Generates spatial, STBC, and beamforming test args."""
        test_args: list[tuple[TestParams]] = []

        for bw in ALL_BANDWIDTHS:
            for bf_state in BEAMFORMING_STATES:
                test_args.append(self._make_test_params(bw, bf_state))

            for tx_stbc, rx_stbc in itertools.product(TX_STBC, RX_STBC):
                test_args.append(self._make_test_params(bw, [tx_stbc, rx_stbc]))

            for profile in BF_STBC_COEXISTENCE_PROFILES:
                test_args.append(self._make_test_params(bw, profile))

        return test_args

    def _generate_link_adaptation_htc_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """Generates MAC control and link adaptation dependency test args."""
        test_args: list[tuple[TestParams]] = []

        for bw in ALL_BANDWIDTHS:
            for state in LINK_ADAPTATION_HTC_STATES:
                test_args.append(self._make_test_params(bw, state))

        return test_args

    def _generate_mpdu_ampdu_limits_test_args(self) -> list[tuple[TestParams]]:
        """Generates frame length and A-MPDU aggregation limit test args."""
        test_args: list[tuple[TestParams]] = []

        for mpdu_len, ampdu_exp in itertools.product(
            VHT_MAX_MPDU_LEN, MAX_A_MPDU
        ):
            test_args.append(self._make_test_params(80, [mpdu_len, ampdu_exp]))

        return test_args

    def _generate_phy_mac_interaction_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """Generates de-isolated PHY/MAC feature interaction test args."""
        test_args: list[tuple[TestParams]] = []

        spatial_modes: list[list[str]] = [
            [],
            [
                capabilities.AC_CAPABILITY_SU_BEAMFORMER,
                capabilities.AC_CAPABILITY_TX_STBC_2BY1,
                capabilities.AC_CAPABILITY_RX_STBC_1,
            ],
        ]
        txop_bf_modes: list[list[str]] = [
            [],
            [
                capabilities.AC_CAPABILITY_SU_BEAMFORMER,
                capabilities.AC_CAPABILITY_MU_BEAMFORMER,
            ],
        ]

        for bw in WIDE_BANDWIDTHS:
            sgi_cap = (
                capabilities.AC_CAPABILITY_SHORT_GI_80
                if bw == 80
                else capabilities.AC_CAPABILITY_SHORT_GI_160
            )
            sgi_options = ["", sgi_cap]

            for sgi, ldpc, spatial_mode in itertools.product(
                sgi_options, RXLDPC, spatial_modes
            ):
                test_args.append(
                    self._make_test_params(bw, [sgi, ldpc, *spatial_mode])
                )

            for sgi, bf_mode in itertools.product(sgi_options, txop_bf_modes):
                test_args.append(
                    self._make_test_params(
                        bw,
                        [
                            capabilities.AC_CAPABILITY_VHT_TXOP_PS,
                            sgi,
                            *bf_mode,
                        ],
                    )
                )

        return test_args

    def _generate_baseline_and_stress_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """Generates baseline smoke and composite max-capability stress test args."""
        test_args: list[tuple[TestParams]] = []

        for bw in ALL_BANDWIDTHS:
            test_args.append(self._make_test_params(bw, []))

        # Composite Max VHT Bitmask Stress
        for bw in ALL_BANDWIDTHS:
            sgi_caps: list[str] = []
            if bw in (80, 160):
                sgi_caps.append(capabilities.AC_CAPABILITY_SHORT_GI_80)
            if bw == 160:
                sgi_caps.append(capabilities.AC_CAPABILITY_SHORT_GI_160)

            max_caps = [
                capabilities.AC_CAPABILITY_MAX_MPDU_11454,
                capabilities.AC_CAPABILITY_RXLDPC,
                *sgi_caps,
                capabilities.AC_CAPABILITY_TX_STBC_2BY1,
                capabilities.AC_CAPABILITY_RX_STBC_1,
                capabilities.AC_CAPABILITY_SU_BEAMFORMER,
                capabilities.AC_CAPABILITY_SU_BEAMFORMEE,
                capabilities.AC_CAPABILITY_MU_BEAMFORMER,
                capabilities.AC_CAPABILITY_MU_BEAMFORMEE,
                capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP7,
                capabilities.AC_CAPABILITY_RX_ANTENNA_PATTERN,
                capabilities.AC_CAPABILITY_TX_ANTENNA_PATTERN,
            ]
            test_args.append(self._make_test_params(bw, max_caps))

        return test_args


if __name__ == "__main__":
    test_runner.main()
