#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import enum
from dataclasses import dataclass

import fidl_fuchsia_wlan_internal as fidl_security
import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanError,
)
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from legacy_access_point.access_point import setup_ap
from legacy_access_point.ap_lib import hostapd_constants
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    LegacyMode,
    RadioConfig,
    SecurityOpen,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper,
)
from openwrt_access_point.lib.hostapd_options import (
    AssocRespIe,
    HostapdOptions,
    WmmAcm,
    WmmParams,
)
from openwrt_access_point.lib.uci_bss_options import UciBssOptions
from openwrt_access_point.lib.uci_options import (
    BasicRate,
    Country3,
    SupportedRates,
    VendorElements,
)
from openwrt_access_point.lib.uci_radio_options import UciRadioOptions

AP_SSID_MIN_LENGTH = 1
AP_SSID_MAX_LENGTH = 32
DEFAULT_BEACON_INTERVAL_TU = 100

# Representative UTF-8 SSIDs covering 2-byte, 3-byte, and 4-byte UTF-8 classes.
UTF8_SSID_2BYTE = "Château du Feÿ"  # U+0080..U+07FF (Latin-1 Supplement)
UTF8_SSID_3BYTE = "あなた　はお母さん"  # U+0800..U+FFFF (CJK)
UTF8_SSID_4BYTE = "2𝔤_𝔊𝔬𝔬𝔤𝔩𝔢"  # U+10000..U+10FFFF (Mathematical Alphanumeric)

# Parameters unsupported on OpenWrt One APs (MT7981B + MT7976C).
UNSUPPORTED_ON_OPENWRT_ONE: frozenset[str] = frozenset(
    {
        "frag_threshold",  # mt7915 driver does not implement TX fragmentation threshold
        "rts_threshold",  # mt7915/nl80211 does not apply custom RTS thresholds in AP mode
    }
)


class LegacyPhyMode(enum.StrEnum):
    """Legacy 802.11a/b/g PHY rate-set and band configurations."""

    MODE_11B = "11b"
    MODE_11G = "11g"
    MODE_11BG = "11bg"
    MODE_11A = "11a"

    @property
    def channel(self) -> int:
        if self == LegacyPhyMode.MODE_11A:
            return hostapd_constants.AP_DEFAULT_CHANNEL_5G
        return hostapd_constants.AP_DEFAULT_CHANNEL_2G

    @property
    def profile_name(self) -> str:
        if self in (LegacyPhyMode.MODE_11A, LegacyPhyMode.MODE_11B):
            return "whirlwind_11ab_legacy"
        return "whirlwind_11ag_legacy"

    @property
    def supported_rates(self) -> list[int] | None:
        match self:
            case LegacyPhyMode.MODE_11B:
                return SupportedRates.CCK
            case LegacyPhyMode.MODE_11G:
                return SupportedRates.OFDM
            case LegacyPhyMode.MODE_11BG:
                return SupportedRates.CCK_AND_OFDM
            case LegacyPhyMode.MODE_11A:
                return None

    @property
    def basic_rates(self) -> list[int] | None:
        match self:
            case LegacyPhyMode.MODE_11B:
                return BasicRate.CCK
            case LegacyPhyMode.MODE_11G:
                return BasicRate.OFDM_ONLY
            case LegacyPhyMode.MODE_11BG:
                return BasicRate.CCK_AND_OFDM
            case LegacyPhyMode.MODE_11A:
                return None


@dataclass
class TestParams:
    name: str
    phy_mode: LegacyPhyMode
    ssid: str | None = None
    preamble: bool | None = None
    beacon_interval: int | None = None
    dtim_period: int | None = None
    frag_threshold: int | None = None
    rts_threshold: int | None = None
    ieee80211d: bool | None = None
    country: str | None = None
    country3: str | None = None
    vendor_elements: str | None = None
    force_wmm: bool | None = None
    additional_ap_parameters: HostapdOptions | None = None


class WlanPhyComplianceABGTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests for validating 11a, 11b, and 11g PHYs.

    For test matrix design and rationale, see http://go/wlan-test-optimization.

    Test Bed Requirement:
    * One Fuchsia device
    * One Access Point
    """

    phy: wlan_core.Phy

    async def setup_class(self) -> None:
        await super().setup_class()

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("Requires at least one access point")

        if self.access_point:
            self.access_point.stop_all_aps()

        self.phy = await self.dut.wlan_core.ensure_single_phy()

        # 802.11a operates in the 5 GHz band. Set country to US so that 5 GHz
        # channels are supported.
        await self.phy.set_country(
            KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def teardown_test(self) -> None:
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def pre_run(self) -> None:
        test_args: list[tuple[TestParams]] = (
            self._generate_core_phy_baseline_test_args()
            + self._generate_management_and_ie_sweep_test_args()
            + self._generate_ssid_and_utf8_test_args()
        )

        self.generate_tests(
            test_logic=self.setup_and_connect,
            name_func=lambda params: params.name,
            arg_sets=test_args,
        )

    def _skip_if_unsupported(self, params: TestParams) -> None:
        """Skips the test case if the active AP does not support params."""
        if self.openwrt_ap:
            unsupported = [
                field
                for field in UNSUPPORTED_ON_OPENWRT_ONE
                if getattr(params, field) is not None
            ]
            asserts.skip_if(
                bool(unsupported),
                f"Skipped: {unsupported} unsupported on OpenWrt One (MT7976C).",
            )

    async def setup_and_connect(self, params: TestParams) -> None:
        """Sets up the AP with legacy 802.11a/b/g parameters and associates the DUT."""
        self._skip_if_unsupported(params)

        ssid = params.ssid or AccessPointConfig.random_string(8)
        channel = params.phy_mode.channel
        band = Band.BAND_2G if channel <= 14 else Band.BAND_5G
        supported_rates = params.phy_mode.supported_rates
        basic_rates = params.phy_mode.basic_rates

        custom_uci_options: UciRadioOptions = {}
        if params.frag_threshold is not None:
            custom_uci_options["frag"] = params.frag_threshold
        if params.beacon_interval is not None:
            custom_uci_options["beacon_int"] = params.beacon_interval
        if params.rts_threshold is not None:
            custom_uci_options["rts"] = params.rts_threshold
        if params.ieee80211d is not None:
            custom_uci_options["ieee80211d"] = params.ieee80211d
        if params.country3 is not None:
            custom_uci_options["country3"] = params.country3
        if supported_rates is not None:
            custom_uci_options["supported_rates"] = supported_rates
        if basic_rates is not None:
            custom_uci_options["basic_rates"] = basic_rates

        custom_bss_uci_options: UciBssOptions = {}
        if params.dtim_period is not None:
            custom_bss_uci_options["dtim_period"] = params.dtim_period
        if params.vendor_elements is not None:
            custom_bss_uci_options["vendor_elements"] = [params.vendor_elements]
        if params.preamble is not None:
            custom_bss_uci_options["preamble"] = params.preamble

        final_country = params.country or "US"

        custom_hostapd_options: HostapdOptions = {}
        if params.additional_ap_parameters:
            custom_hostapd_options.update(params.additional_ap_parameters)

        radio_config = RadioConfig(
            channel=BssChannel(
                number=channel, band=band, phy_mode=LegacyMode()
            ),
            custom_uci_options=custom_uci_options,
            custom_hostapd_options=custom_hostapd_options,
            country=final_country,
            bss_settings=[
                BssSettings(
                    ssid=ssid,
                    security=SecurityOpen(),
                    custom_uci_options=custom_bss_uci_options,
                )
            ],
        )

        if self.openwrt_ap:
            config = AccessPointConfig(radios=[radio_config])
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            legacy_ap_params = AccessPointConfigMapper.to_legacy_params(
                radio_config
            )
            setup_ap(
                access_point=self.access_point,
                profile_name=params.phy_mode.profile_name,
                channel=channel,
                ssid=ssid,
                force_wmm=params.force_wmm,
                additional_ap_parameters=legacy_ap_params,
                frag_threshold=params.frag_threshold,
                rts_threshold=params.rts_threshold,
                dtim_period=params.dtim_period,
                beacon_interval=params.beacon_interval,
                preamble=params.preamble,
            )

        await self._connect_and_validate_channel(
            ssid,
            fidl_security.Protocol.OPEN,
            target_channel=channel,
            beacon_interval=params.beacon_interval,
        )

    async def _connect_and_validate_channel(
        self,
        target_ssid: str,
        target_security: fidl_security.Protocol,
        target_channel: int,
        target_pwd: str | None = None,
        beacon_interval: int | None = None,
    ) -> None:
        iface = await self.phy.create_client_iface()
        # Even at the default 100 TU (102.4ms) beacon interval, a single ~110ms
        # passive scan dwell on a busy RF channel can miss a deferred beacon.
        max_attempts = max(
            3,
            ((beacon_interval or 0) // DEFAULT_BEACON_INTERVAL_TU) * 2,
        )
        for attempt in range(max_attempts):
            try:
                await iface.scan_and_connect(
                    ssid=target_ssid,
                    password=target_pwd,
                    security=target_security,
                    channels=[target_channel],
                )
                break
            except HoneydewWlanError:
                if attempt == max_attempts - 1:
                    raise
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

    def _generate_core_phy_baseline_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """1a. Legacy Waveform & Rate-Set Negotiation (4 tests)."""
        return [
            (
                TestParams(
                    name="test_associate_11b_only_long_preamble",
                    phy_mode=LegacyPhyMode.MODE_11B,
                    preamble=False,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11g_only_long_preamble",
                    phy_mode=LegacyPhyMode.MODE_11G,
                    preamble=False,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_only_long_preamble",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    preamble=False,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11a_only_long_preamble",
                    phy_mode=LegacyPhyMode.MODE_11A,
                    preamble=False,
                ),
            ),
        ]

    def _generate_management_and_ie_sweep_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """2a-2g. Orthogonal Management & IE Sweeps (16 tests)."""
        return [
            # 2a. Short Preamble (1 test on 11b DSSS)
            (
                TestParams(
                    name="test_associate_11b_only_short_preamble",
                    phy_mode=LegacyPhyMode.MODE_11B,
                    preamble=True,
                ),
            ),
            # 2b. Beacon Interval: 15 & 1024 TU (2 tests)
            (
                TestParams(
                    name="test_associate_11bg_minimal_beacon_interval",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    beacon_interval=15,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_maximum_beacon_interval",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    beacon_interval=1024,
                ),
            ),
            # 2c. DTIM Period: 3/100 & 1/300 (2 tests)
            (
                TestParams(
                    name="test_associate_11bg_high_dtim_low_beacon_interval",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    dtim_period=3,
                    beacon_interval=100,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_low_dtim_high_beacon_interval",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    dtim_period=1,
                    beacon_interval=300,
                ),
            ),
            # 2d. 802.11d Country IE: US on 5 GHz (11a) & XX on 2.4 GHz (11bg) (2 tests)
            (
                TestParams(
                    name="test_associate_11a_only_with_country_code",
                    phy_mode=LegacyPhyMode.MODE_11A,
                    ieee80211d=True,
                    country="US",
                    country3=Country3.ALL,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_with_non_country_code",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    ieee80211d=True,
                    country="XX",
                    country3=Country3.ALL,
                ),
            ),
            # 2e. Vendor Specific IE: [Beacon, AssocResp] x [Normal, Zero-Length] (4 tests)
            (
                TestParams(
                    name="test_associate_11bg_with_vendor_ie_in_beacon_correct_length",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    vendor_elements=VendorElements.CORRECT_LENGTH,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_with_vendor_ie_in_beacon_zero_length",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    vendor_elements=VendorElements.ZERO_LENGTH_WITHOUT_DATA,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_with_vendor_ie_in_assoc_correct_length",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    additional_ap_parameters=AssocRespIe.CORRECT_LENGTH,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_with_vendor_ie_in_assoc_zero_length",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    additional_ap_parameters=AssocRespIe.ZERO_LENGTH_WITHOUT_DATA,
                ),
            ),
            # 2f. WMM EDCA & ACM: Non-Default EDCA (ACM All-Off) & ACM All-On (2 tests)
            (
                TestParams(
                    name="test_associate_11bg_with_WMM_with_non_default_values",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    force_wmm=True,
                    additional_ap_parameters=WmmParams.NON_DEFAULT,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_with_WMM_ACM_all_on",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    force_wmm=True,
                    additional_ap_parameters=(
                        WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
                        | WmmAcm.ALL
                    ),
                ),
            ),
            # 2g. RTS & Fragmentation Threshold (3 tests; skipped on OpenWrt One)
            (
                TestParams(
                    name="test_associate_11bg_frag_threshold_430",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    frag_threshold=430,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_rts_threshold_256",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    rts_threshold=256,
                ),
            ),
            (
                TestParams(
                    name="test_associate_11bg_rts_256_frag_430",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    rts_threshold=256,
                    frag_threshold=430,
                ),
            ),
        ]

    def _generate_ssid_and_utf8_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """3a-3b. SSID Octet Length Boundary & UTF-8 Byte-Width Classes (5 tests)."""
        return [
            # 3a. Octet Length Boundary: 1-char & 32-char ASCII (2 tests)
            (
                TestParams(
                    name="test_minimum_ssid_length_2g_11bg",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    ssid=AccessPointConfig.random_string(AP_SSID_MIN_LENGTH),
                ),
            ),
            (
                TestParams(
                    name="test_maximum_ssid_length_2g_11bg",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    ssid=AccessPointConfig.random_string(AP_SSID_MAX_LENGTH),
                ),
            ),
            # 3b. UTF-8 Byte-Width Classes: 2-byte, 3-byte, 4-byte (3 tests)
            (
                TestParams(
                    name="test_ssid_with_UTF8_characters_2byte_2g_11bg",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    ssid=UTF8_SSID_2BYTE,
                ),
            ),
            (
                TestParams(
                    name="test_ssid_with_UTF8_characters_3byte_2g_11bg",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    ssid=UTF8_SSID_3BYTE,
                ),
            ),
            (
                TestParams(
                    name="test_ssid_with_UTF8_characters_4byte_2g_11bg",
                    phy_mode=LegacyPhyMode.MODE_11BG,
                    ssid=UTF8_SSID_4BYTE,
                ),
            ),
        ]


if __name__ == "__main__":
    test_runner.main()
