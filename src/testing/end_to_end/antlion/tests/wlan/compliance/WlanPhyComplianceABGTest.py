#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord
from mobly_controller.openwrt_access_point import OpenWrtAP
from mobly_controller.openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    HostapdOptions,
    LegacyMode,
    RadioConfig,
    SecurityOpen,
    UciBssOptions,
    UciRadioOptions,
)
from mobly_controller.openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper,
)
from mobly_controller.openwrt_access_point.lib.hostapd_options import (
    AssocRespIe,
    Country3,
    WmmAcm,
    WmmParams,
)
from mobly_controller.openwrt_access_point.lib.uci_options import (
    BasicRate,
    SupportedRates,
    VendorElements,
)


class WlanPhyComplianceABGTest(base_test.WifiBaseTest):
    """Tests for validating 11a, 11b, and 11g PHYS.

    Test Bed Requirement:
    * One Android device or Fuchsia device
    * One Access Point
    """

    access_point: AccessPoint | None = None
    openwrt_ap: OpenWrtAP | None = None

    def setup_class(self) -> None:
        super().setup_class()

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
        else:
            raise signals.TestAbortClass("Requires at least one access point")

        self.dut = self.get_dut(AssociationMode.POLICY)

        open_network = self.get_open_network(False, [])
        open_network_min_len = self.get_open_network(
            False,
            [],
            ssid_length_2g=hostapd_constants.AP_SSID_MIN_LENGTH_2G,
            ssid_length_5g=hostapd_constants.AP_SSID_MIN_LENGTH_5G,
        )
        open_network_max_len = self.get_open_network(
            False,
            [],
            ssid_length_2g=hostapd_constants.AP_SSID_MAX_LENGTH_2G,
            ssid_length_5g=hostapd_constants.AP_SSID_MAX_LENGTH_5G,
        )
        self.open_network_2g = open_network["2g"]
        self.open_network_5g = open_network["5g"]
        self.open_network_max_len_2g = open_network_max_len["2g"]
        self.open_network_max_len_2g["SSID"] = self.open_network_max_len_2g[
            "SSID"
        ][3:]
        self.open_network_max_len_5g = open_network_max_len["5g"]
        self.open_network_max_len_5g["SSID"] = self.open_network_max_len_5g[
            "SSID"
        ][3:]
        self.open_network_min_len_2g = open_network_min_len["2g"]
        self.open_network_min_len_2g["SSID"] = self.open_network_min_len_2g[
            "SSID"
        ][3:]
        self.open_network_min_len_5g = open_network_min_len["5g"]
        self.open_network_min_len_5g["SSID"] = self.open_network_min_len_5g[
            "SSID"
        ][3:]

        self.utf8_ssid_2g = "2𝔤_𝔊𝔬𝔬𝔤𝔩𝔢"
        self.utf8_ssid_5g = "5𝔤_𝔊𝔬𝔬𝔤𝔩𝔢"

        self.utf8_ssid_2g_french = "Château du Feÿ"
        self.utf8_password_2g_french = "du Feÿ Château"

        self.utf8_ssid_2g_german = "Rat für Straßenatlas"
        self.utf8_password_2g_german = "für Straßenatlas Rat"

        self.utf8_ssid_2g_dutch = "Die niet óúd, is níéuw!"
        self.utf8_password_2g_dutch = "niet óúd, is níéuw! Die"

        self.utf8_ssid_2g_swedish = "Det är femtioåtta"
        self.utf8_password_2g_swedish = "femtioåtta Det är"

        self.utf8_ssid_2g_norwegian = "Curaçao ØÆ æ å å å"
        self.utf8_password_2g_norwegian = "ØÆ Curaçao æ å å å"

        # Danish and Norwegian has the same alphabet
        self.utf8_ssid_2g_danish = self.utf8_ssid_2g_norwegian
        self.utf8_password_2g_danish = self.utf8_password_2g_norwegian

        self.utf8_ssid_2g_japanese = "あなた　はお母さん"
        self.utf8_password_2g_japanese = "そっくりね。あな"

        self.utf8_ssid_2g_spanish = "¡No á,é,í,ó,ú,ü,ñ,¿,¡"
        self.utf8_password_2g_spanish = "á,é,í,ó,ú,ü,ñ,¿,¡ ¡No"

        self.utf8_ssid_2g_italian = "caffè Pinocchio è italiano?"
        self.utf8_password_2g_italian = "Pinocchio è italiano? caffè"

        self.utf8_ssid_2g_korean = "ㅘㅙㅚㅛㅜㅝㅞㅟㅠ"
        self.utf8_password_2g_korean = "ㅜㅝㅞㅟㅠㅘㅙㅚㅛ"

        if self.access_point:
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
        if self.access_point:
            self.access_point.stop_all_aps()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        if self.access_point:
            self.access_point.stop_all_aps()

    def _run_test(
        self,
        channel: int,
        ssid: str,
        profile_name: str,
        force_wmm: bool = False,
        additional_ap_parameters: HostapdOptions | None = None,
        frag_threshold: int | None = None,
        rts_threshold: int | None = None,
        dtim_period: int | None = None,
        beacon_interval: int | None = None,
        preamble: bool | None = None,
        hidden: bool = False,
        country_ie: Literal[0, 1] | None = None,
        country: str | None = None,
        supported_rates: list[int] | None = None,
        basic_rate: list[int] | None = None,
        vendor_elements: str | None = None,
    ) -> None:
        """Common function to run PHY compliance tests."""
        band = Band.BAND_2G if channel <= 14 else Band.BAND_5G
        custom_uci_options: UciRadioOptions = {}

        if frag_threshold is not None:
            custom_uci_options["frag"] = frag_threshold

        if beacon_interval is not None:
            custom_uci_options["beacon_int"] = beacon_interval

        if rts_threshold is not None:
            custom_uci_options["rts"] = rts_threshold

        if country_ie is not None:
            custom_uci_options["country_ie"] = country_ie

        if supported_rates is not None:
            custom_uci_options["supported_rates"] = supported_rates
        if basic_rate is not None:
            custom_uci_options["basic_rate"] = basic_rate

        custom_bss_uci_options: UciBssOptions = {}
        if dtim_period is not None:
            custom_bss_uci_options["dtim_period"] = dtim_period
        if vendor_elements is not None:
            custom_bss_uci_options["vendor_elements"] = vendor_elements
        if preamble is not None:
            custom_bss_uci_options["short_preamble"] = 1 if preamble else 0

        final_country = country or "US"

        custom_hostapd_options: HostapdOptions = {}
        if additional_ap_parameters:
            custom_hostapd_options.update(additional_ap_parameters)

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
                    hidden=hidden,
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
                profile_name=profile_name,
                channel=channel,
                ssid=ssid,
                force_wmm=force_wmm,
                additional_ap_parameters=legacy_ap_params,
                frag_threshold=frag_threshold,
                rts_threshold=rts_threshold,
                dtim_period=dtim_period,
                beacon_interval=beacon_interval,
                preamble=preamble,
                hidden=hidden,
            )

        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )

    def test_associate_11b_only_long_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            preamble=False,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_short_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            preamble=True,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_minimal_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            beacon_interval=15,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_maximum_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            beacon_interval=1024,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_frag_threshold_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            frag_threshold=430,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_rts_threshold_256(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            rts_threshold=256,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_rts_256_frag_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            rts_threshold=256,
            frag_threshold=430,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_high_dtim_low_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            dtim_period=3,
            beacon_interval=100,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_low_dtim_high_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            dtim_period=1,
            beacon_interval=300,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_with_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.DEFAULT_11B,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_with_non_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.NON_DEFAULT,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_BK(self) -> None:
        wmm_acm_bits_enabled = WmmParams.DEFAULT_11B | WmmAcm.BK
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            # additional_ap_parameters=wmm_acm_bits_enabled,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_BE(self) -> None:
        wmm_acm_bits_enabled = WmmParams.DEFAULT_11B | WmmAcm.BE
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_VI(self) -> None:
        wmm_acm_bits_enabled = WmmParams.DEFAULT_11B | WmmAcm.VI
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_VO(self) -> None:
        wmm_acm_bits_enabled = WmmParams.DEFAULT_11B | WmmAcm.VO
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_BK_BE_VI(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_11B | WmmAcm.BK | WmmAcm.BE | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_BK_BE_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_11B | WmmAcm.BK | WmmAcm.BE | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_BK_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_11B | WmmAcm.BK | WmmAcm.VI | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_WMM_ACM_on_BE_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_11B | WmmAcm.BE | WmmAcm.VI | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            country_ie=1,
            country="US",
            additional_ap_parameters=Country3.ALL,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_non_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            country_ie=1,
            country="XX",
            additional_ap_parameters=Country3.ALL,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_hidden_ssid(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            hidden=True,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_vendor_ie_in_beacon_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            vendor_elements=VendorElements.CORRECT_LENGTH,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_vendor_ie_in_beacon_zero_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            vendor_elements=VendorElements.ZERO_LENGTH_WITHOUT_DATA,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_vendor_ie_in_assoc_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            additional_ap_parameters=AssocRespIe.CORRECT_LENGTH,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11b_only_with_vendor_ie_in_assoc_zero_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            additional_ap_parameters=AssocRespIe.ZERO_LENGTH_WITHOUT_DATA,
            supported_rates=SupportedRates.CCK,
            basic_rate=BasicRate.CCK,
        )

    def test_associate_11a_only_long_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            preamble=False,
        )

    def test_associate_11a_only_short_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            preamble=True,
        )

    def test_associate_11a_only_minimal_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            beacon_interval=15,
        )

    def test_associate_11a_only_maximum_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            beacon_interval=1024,
        )

    def test_associate_11a_only_frag_threshold_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            frag_threshold=430,
        )

    def test_associate_11a_only_rts_threshold_256(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            rts_threshold=256,
        )

    def test_associate_11a_only_rts_256_frag_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            rts_threshold=256,
            frag_threshold=430,
        )

    def test_associate_11a_only_high_dtim_low_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            dtim_period=3,
            beacon_interval=100,
        )

    def test_associate_11a_only_low_dtim_high_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            dtim_period=1,
            beacon_interval=300,
        )

    def test_associate_11a_only_with_WMM_with_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
        )

    def test_associate_11a_only_with_WMM_with_non_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.NON_DEFAULT,
        )

    def test_associate_11a_only_with_WMM_ACM_on_BK(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.BK
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_BE(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.BE
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_VI(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_BK_BE_VI(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.BE
            | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_BK_BE_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.BE
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_BK_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.VI
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_WMM_ACM_on_BE_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BE
            | WmmAcm.VI
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11a_only_with_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            country_ie=1,
            country="US",
            additional_ap_parameters=Country3.ALL,
        )

    def test_associate_11a_only_with_non_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            country_ie=1,
            country="XX",
            additional_ap_parameters=Country3.ALL,
        )

    def test_associate_11a_only_with_hidden_ssid(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            hidden=True,
        )

    def test_associate_11a_only_with_vendor_ie_in_beacon_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            vendor_elements=VendorElements.CORRECT_LENGTH,
        )

    def test_associate_11a_only_with_vendor_ie_in_beacon_zero_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            vendor_elements=VendorElements.ZERO_LENGTH_WITHOUT_DATA,
        )

    def test_associate_11a_only_with_vendor_ie_in_assoc_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            additional_ap_parameters=AssocRespIe.CORRECT_LENGTH,
        )

    def test_associate_11a_only_with_vendor_ie_in_assoc_zero_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_5g["SSID"],
            additional_ap_parameters=AssocRespIe.ZERO_LENGTH_WITHOUT_DATA,
        )

    def test_associate_11g_only_long_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            preamble=False,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_short_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            preamble=True,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_minimal_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            beacon_interval=15,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_maximum_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            beacon_interval=1024,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_frag_threshold_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            frag_threshold=430,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_rts_threshold_256(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            rts_threshold=256,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_rts_256_frag_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            rts_threshold=256,
            frag_threshold=430,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_high_dtim_low_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            dtim_period=3,
            beacon_interval=100,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_low_dtim_high_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            dtim_period=1,
            beacon_interval=300,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_with_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_with_non_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.NON_DEFAULT,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_BK(self) -> None:
        wmm_params = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.BK
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_params,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_BE(self) -> None:
        wmm_params = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.BE
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_params,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_VI(self) -> None:
        wmm_params = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_params,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_VO(self) -> None:
        wmm_params = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_params,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_BK_BE_VI(self) -> None:
        wmm_params = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.BE
            | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_params,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_BK_BE_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.BE
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_BK_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.VI
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_WMM_ACM_on_BE_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BE
            | WmmAcm.VI
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            country_ie=1,
            country="US",
            additional_ap_parameters=Country3.ALL,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_non_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            country_ie=1,
            country="XX",
            additional_ap_parameters=Country3.ALL,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_hidden_ssid(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            hidden=True,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_vendor_ie_in_beacon_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            vendor_elements=VendorElements.CORRECT_LENGTH,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_vendor_ie_in_beacon_zero_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            vendor_elements=VendorElements.ZERO_LENGTH_WITHOUT_DATA,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_vendor_ie_in_assoc_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            additional_ap_parameters=AssocRespIe.CORRECT_LENGTH,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11g_only_with_vendor_ie_in_assoc_zero_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            additional_ap_parameters=AssocRespIe.ZERO_LENGTH_WITHOUT_DATA,
            supported_rates=SupportedRates.OFDM,
            basic_rate=BasicRate.OFDM_ONLY,
        )

    def test_associate_11bg_only_long_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            preamble=False,
        )

    def test_associate_11bg_short_preamble(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            preamble=True,
        )

    def test_associate_11bg_minimal_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            beacon_interval=15,
        )

    def test_associate_11bg_maximum_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            beacon_interval=1024,
        )

    def test_associate_11bg_frag_threshold_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            frag_threshold=430,
        )

    def test_associate_11bg_rts_threshold_256(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            rts_threshold=256,
        )

    def test_associate_11bg_rts_256_frag_430(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            rts_threshold=256,
            frag_threshold=430,
        )

    def test_associate_11bg_high_dtim_low_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            dtim_period=3,
            beacon_interval=100,
        )

    def test_associate_11bg_low_dtim_high_beacon_interval(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            dtim_period=1,
            beacon_interval=300,
        )

    def test_associate_11bg_with_WMM_with_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
        )

    def test_associate_11bg_with_WMM_with_non_default_values(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=WmmParams.NON_DEFAULT,
        )

    def test_associate_11bg_with_WMM_ACM_on_BK(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.BK
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_BE(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.BE
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_VI(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_BK_BE_VI(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.BE
            | WmmAcm.VI
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_BK_BE_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.BE
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_BK_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BK
            | WmmAcm.VI
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_WMM_ACM_on_BE_VI_VO(self) -> None:
        wmm_acm_bits_enabled = (
            WmmParams.DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS
            | WmmAcm.BE
            | WmmAcm.VI
            | WmmAcm.VO
        )
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            force_wmm=True,
            additional_ap_parameters=wmm_acm_bits_enabled,
        )

    def test_associate_11bg_with_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            country_ie=1,
            country="US",
            additional_ap_parameters=Country3.ALL,
        )

    def test_associate_11bg_with_non_country_code(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            country_ie=1,
            country="XX",
            additional_ap_parameters=Country3.ALL,
        )

    def test_associate_11bg_only_with_hidden_ssid(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            hidden=True,
        )

    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length(
        self,
    ) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            vendor_elements=VendorElements.CORRECT_LENGTH,
        )

    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ag_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_2g["SSID"],
            vendor_elements=VendorElements.ZERO_LENGTH_WITHOUT_DATA,
        )

    def test_minimum_ssid_length_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_min_len_2g["SSID"],
        )

    def test_minimum_ssid_length_5g_11ac_80mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_min_len_5g["SSID"],
        )

    def test_maximum_ssid_length_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.open_network_max_len_2g["SSID"],
        )

    def test_maximum_ssid_length_5g_11ac_80mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.open_network_max_len_5g["SSID"],
        )

    def test_ssid_with_UTF8_characters_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g,
        )

    def test_ssid_with_UTF8_characters_5g_11ac_80mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.utf8_ssid_5g,
        )

    def test_ssid_with_UTF8_characters_french_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_french,
        )

    def test_ssid_with_UTF8_characters_german_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_german,
        )

    def test_ssid_with_UTF8_characters_dutch_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_dutch,
        )

    def test_ssid_with_UTF8_characters_swedish_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_swedish,
        )

    def test_ssid_with_UTF8_characters_norwegian_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_norwegian,
        )

    def test_ssid_with_UTF8_characters_danish_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_danish,
        )

    def test_ssid_with_UTF8_characters_japanese_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_japanese,
        )

    def test_ssid_with_UTF8_characters_spanish_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_spanish,
        )

    def test_ssid_with_UTF8_characters_italian_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_italian,
        )

    def test_ssid_with_UTF8_characters_korean_2g_11n_20mhz(self) -> None:
        self._run_test(
            profile_name="whirlwind_11ab_legacy",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.utf8_ssid_2g_korean,
        )


if __name__ == "__main__":
    test_runner.main()
