#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import re
from functools import wraps
from typing import Callable

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord

AP_11ABG_PROFILE_NAME = "whirlwind_11ag_legacy"
SSID_LENGTH_DEFAULT = 15


def create_security_profile(
    test_func: Callable[[WlanSecurityComplianceABGTest], None]
) -> Callable[[WlanSecurityComplianceABGTest], None]:
    """Decorator for generating hostapd security profile object based on the
    test name.
    Args:
        test_func: The test function
    Returns:
        security_profile_generator: The function that generates the security
            profile object
    """

    @wraps(test_func)
    def security_profile_generator(self: WlanSecurityComplianceABGTest) -> None:
        """Function that looks at the name of the function and determines what
        the security profile should be based on what items are in the name

        Example: A function with the name sec_wpa_wpa2_ptk_ccmp_tkip would
            return a security profile that has wpa and wpa2 configure with a
            ptk cipher of ccmp or tkip. Removing one of those options would
            drop it from the config.

        Args:
            *args: args that were sent to the original test function
            **kwargs: kwargs that were sent to the original test function
        Returns:
            The original function that was called
        """
        utf8_password_2g = "2𝔤_𝔊𝔬𝔬𝔤𝔩𝔢"
        utf8_password_2g_french = "du Feÿ Château"
        utf8_password_2g_german = "für Straßenatlas Rat"
        utf8_password_2g_dutch = "niet óúd, is níéuw! Die"
        utf8_password_2g_swedish = "femtioåtta Det är"
        utf8_password_2g_norwegian = "ØÆ Curaçao æ å å å"
        # Danish and Norwegian has the same alphabet
        utf8_password_2g_danish = utf8_password_2g_norwegian
        utf8_password_2g_japanese = "そっくりね。あな"
        utf8_password_2g_spanish = "á,é,í,ó,ú,ü,ñ,¿,¡ ¡No"
        utf8_password_2g_italian = "Pinocchio è italiano? caffè"
        utf8_password_2g_korean = "ㅜㅝㅞㅟㅠㅘㅙㅚㅛ"

        security = re.search(r"sec(.*?)ptk_(.*)", test_func.__name__)
        if security is None:
            raise TypeError(
                f'Test name does not match expected pattern: "{test_func.__name__}"'
            )

        security_mode_raw = security.group(1)
        ptk_type = security.group(2)
        wpa_cipher: str | None = None
        wpa2_cipher: str | None = None

        if "_wpa_wpa2_wpa3_" in security_mode_raw:
            security_mode = SecurityMode.WPA_WPA2_WPA3
        elif "_wpa_wpa2_" in security_mode_raw:
            security_mode = SecurityMode.WPA_WPA2
        elif "_wpa2_wpa3_" in security_mode_raw:
            security_mode = SecurityMode.WPA2_WPA3
        elif "_wep_" in security_mode_raw:
            if self.dut.has_wep_support:
                security_mode = SecurityMode.WEP
            else:
                raise signals.TestSkip("DUT does not support WEP security")
        elif "_wpa_" in security_mode_raw:
            if self.dut.has_wpa_support:
                security_mode = SecurityMode.WPA
            else:
                raise signals.TestSkip("DUT does not support WPA security")
        elif "_wpa2_" in security_mode_raw:
            security_mode = SecurityMode.WPA2
        elif "_wpa3_" in security_mode_raw:
            security_mode = SecurityMode.WPA3
        else:
            raise TypeError(
                f'Security mode "{security_mode_raw}" not supported'
            )

        if "tkip" in ptk_type and "ccmp" in ptk_type:
            wpa_cipher = "TKIP CCMP"
            wpa2_cipher = "TKIP CCMP"
        elif "tkip" in ptk_type:
            wpa_cipher = "TKIP"
            wpa2_cipher = "TKIP"
        elif "ccmp" in ptk_type:
            wpa_cipher = "CCMP"
            wpa2_cipher = "CCMP"
        if "max_length_password" in test_func.__name__:
            password = generate_random_password(
                length=hostapd_constants.MAX_WPA_PASSWORD_LENGTH
            )
        elif "max_length_psk" in test_func.__name__:
            password = str(
                generate_random_password(
                    length=hostapd_constants.MAX_WPA_PSK_LENGTH, hex=True
                )
            ).lower()
        elif "wep_5_chars" in test_func.__name__:
            password = generate_random_password(length=5)
        elif "wep_13_chars" in test_func.__name__:
            password = generate_random_password(length=13)
        elif "wep_10_hex" in test_func.__name__:
            password = str(
                generate_random_password(length=10, hex=True)
            ).lower()
        elif "wep_26_hex" in test_func.__name__:
            password = str(
                generate_random_password(length=26, hex=True)
            ).lower()
        elif "utf8" in test_func.__name__:
            if "french" in test_func.__name__:
                password = utf8_password_2g_french
            elif "german" in test_func.__name__:
                password = utf8_password_2g_german
            elif "dutch" in test_func.__name__:
                password = utf8_password_2g_dutch
            elif "swedish" in test_func.__name__:
                password = utf8_password_2g_swedish
            elif "norwegian" in test_func.__name__:
                password = utf8_password_2g_norwegian
            elif "danish" in test_func.__name__:
                password = utf8_password_2g_danish
            elif "japanese" in test_func.__name__:
                password = utf8_password_2g_japanese
            elif "spanish" in test_func.__name__:
                password = utf8_password_2g_spanish
            elif "italian" in test_func.__name__:
                password = utf8_password_2g_italian
            elif "korean" in test_func.__name__:
                password = utf8_password_2g_korean
            else:
                password = utf8_password_2g
        else:
            password = generate_random_password()

        self.security_profile = Security(
            security_mode=security_mode,
            password=password,
            wpa_cipher=wpa_cipher,
            wpa2_cipher=wpa2_cipher,
        )
        self.client_password = password
        self.target_security = security_mode
        self.ssid = utils.rand_ascii_str(SSID_LENGTH_DEFAULT)

        test_func(self)

    return security_profile_generator


class WlanSecurityComplianceABGTest(base_test.WifiBaseTest):
    """Tests for validating 11a, 11b, and 11g PHYS.

    Test Bed Requirement:
    * One Android device or Fuchsia device
    * One Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()
        self.dut = self.get_dut(AssociationMode.POLICY)

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]

        self.ssid: str
        self.target_security: SecurityMode
        self.security_profile: Security
        self.client_password: str

        self.access_point.stop_all_aps()

    def setup_test(self) -> None:
        super().setup_test()
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
        super().teardown_test()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        self.access_point.stop_all_aps()

    @create_security_profile
    def test_associate_11a_sec_open_wep_5_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_open_wep_13_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_open_wep_10_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_open_wep_26_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_shared_wep_5_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_shared_wep_13_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_shared_wep_10_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_shared_wep_26_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_frag_430_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_high_dtim_low_beacon_int_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_low_dtim_high_beacon_int_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_WMM_with_default_values_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_correct_length_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_zero_length_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_high_dtim_low_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_low_dtim_high_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_WMM_with_default_values_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_correct_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_zero_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_psk_sec_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_psk_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_psk_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_high_dtim_low_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_low_dtim_high_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_WMM_with_default_values_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_correct_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_zero_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_psk_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_frag_430_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_high_dtim_low_beacon_int_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_low_dtim_high_beacon_int_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_WMM_with_default_values_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_correct_length_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_zero_length_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa3_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa3_sae_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa3_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa3_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa3_sae_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa3_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa3_sae_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_frag_430_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_high_dtim_low_beacon_int_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_low_dtim_high_beacon_int_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_WMM_with_default_values_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_correct_length_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_zero_length_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_high_dtim_low_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_low_dtim_high_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_WMM_with_default_values_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_correct_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_zero_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_high_dtim_low_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_low_dtim_high_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_WMM_with_default_values_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_correct_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_zero_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_rts_256_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_high_dtim_low_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_low_dtim_high_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_WMM_with_default_values_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_correct_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_zero_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_rts_256_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_high_dtim_low_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_low_dtim_high_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_WMM_with_default_values_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_correct_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_zero_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11a_pmf_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_open_wep_5_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_open_wep_13_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_open_wep_10_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_open_wep_26_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["open"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_shared_wep_5_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_shared_wep_13_chars_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_shared_wep_10_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_shared_wep_26_hex_ptk_none(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
            additional_ap_parameters=hostapd_constants.WEP_AUTH["shared"],
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_frag_430_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_high_dtim_low_beacon_int_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_low_dtim_high_beacon_int_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_WMM_with_default_values_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_high_dtim_low_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_low_dtim_high_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_WMM_with_default_values_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_psk_sec_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_psk_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_psk_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_false(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Expected failure to associate. This device must support TKIP and "
            "PMF, which is not supported on Fuchsia. If this device is a "
            "mainstream device, we need to reconsider adding support for TKIP "
            "and PMF on Fuchsia.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_frag_430_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_high_dtim_low_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_low_dtim_high_beacon_int_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_WMM_with_default_values_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_correct_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_zero_length_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa_wpa2_psk_ptk_tkip(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_psk_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_wpa2_psk_ptk_tkip(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_frag_430_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_high_dtim_low_beacon_int_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_low_dtim_high_beacon_int_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_WMM_with_default_values_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_wpa2_psk_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa3_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa3_sae_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa3_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa3_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa3_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa3_sae_ptk_tkip_or_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_frag_430_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_high_dtim_low_beacon_int_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_low_dtim_high_beacon_int_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_WMM_with_default_values_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa3_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_high_dtim_low_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_low_dtim_high_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_WMM_with_default_values_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_frag_430_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_high_dtim_low_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_low_dtim_high_beacon_int_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_WMM_with_default_values_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_correct_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_zero_length_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_rts_256_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_high_dtim_low_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_low_dtim_high_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_WMM_with_default_values_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_correct_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_zero_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_max_length_password_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_rts_256_frag_430_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            rts_threshold=256,
            frag_threshold=430,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_high_dtim_low_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.HIGH_DTIM,
            beacon_interval=hostapd_constants.LOW_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_low_dtim_high_beacon_int_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            dtim_period=hostapd_constants.LOW_DTIM,
            beacon_interval=hostapd_constants.HIGH_BEACON_INTERVAL,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_WMM_with_default_values_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            force_wmm=True,
            additional_ap_parameters=hostapd_constants.WMM_11B_DEFAULT_PARAMS,
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_correct_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "correct_length_beacon"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_zero_length_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "zero_length_beacon_without_data"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_11bg_pmf_with_vendor_ie_in_beacon_similar_to_wpa_ie_sec_wpa_wpa2_wpa3_psk_sae_ptk_tkip_or_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            additional_ap_parameters=hostapd_constants.VENDOR_IE[
                "simliar_to_wpa"
            ],
            security=self.security_profile,
            pmf_support=hostapd_constants.PMF_SUPPORT_REQUIRED,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_password_11bg_sec_wpa2_psk_ptk_ccmp(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_french_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_german_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_dutch_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_swedish_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_norwegian_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_danish_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_japanese_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_spanish_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_italian_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )

    @create_security_profile
    def test_associate_utf8_korean_password_11bg_sec_wpa2_psk_ptk_ccmp(
        self,
    ) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name=AP_11ABG_PROFILE_NAME,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile,
            force_wmm=False,
        )

        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                target_security=self.target_security,
                target_pwd=self.client_password,
            ),
            "Failed to associate.",
        )


if __name__ == "__main__":
    test_runner.main()
