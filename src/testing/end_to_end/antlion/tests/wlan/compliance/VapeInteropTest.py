#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord


class VapeInteropTest(base_test.WifiBaseTest):
    """Tests interoperability with mock third party AP profiles.

    Test Bed Requirement:
    * One Android or Fuchsia Device
    * One Whirlwind Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()

        self.dut = self.get_dut(AssociationMode.POLICY)

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]

        # Same for both 2g and 5g
        self.ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        self.password = utils.rand_ascii_str(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        self.security_profile_wpa2 = Security(
            security_mode=SecurityMode.WPA2,
            password=self.password,
            wpa2_cipher=hostapd_constants.WPA2_DEFAULT_CIPER,
        )

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

    def test_associate_actiontec_pk5000_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="actiontec_pk5000",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_actiontec_pk5000_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="actiontec_pk5000",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_actiontec_mi424wr_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="actiontec_mi424wr",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_actiontec_mi424wr_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="actiontec_mi424wr",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtac66u_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtac66u_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtac66u_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtac66u_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtac86u_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac86u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtac86u_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac86u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtac86u_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac86u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtac86u_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac86u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtac5300_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac5300",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtac5300_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac5300",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtac5300_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac5300",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtac5300_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtac5300",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtn56u_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn56u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtn56u_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn56u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtn56u_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn56u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtn56u_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn56u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtn66u_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtn66u_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_asus_rtn66u_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_asus_rtn66u_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="asus_rtn66u",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_belkin_f9k1001v5_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="belkin_f9k1001v5",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_belkin_f9k1001v5_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="belkin_f9k1001v5",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_linksys_ea4500_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea4500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_linksys_ea4500_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea4500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_linksys_ea4500_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea4500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_linksys_ea4500_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea4500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_linksys_ea9500_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea9500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_linksys_ea9500_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea9500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_linksys_ea9500_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea9500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_linksys_ea9500_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_ea9500",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_linksys_wrt1900acv2_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_wrt1900acv2",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_linksys_wrt1900acv2_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_wrt1900acv2",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_linksys_wrt1900acv2_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_wrt1900acv2",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_linksys_wrt1900acv2_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="linksys_wrt1900acv2",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_netgear_r7000_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_r7000",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_netgear_r7000_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_r7000",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_netgear_r7000_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_r7000",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_netgear_r7000_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_r7000",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_netgear_wndr3400_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_wndr3400",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_netgear_wndr3400_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_wndr3400",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_netgear_wndr3400_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_wndr3400",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_netgear_wndr3400_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="netgear_wndr3400",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_securifi_almond_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="securifi_almond",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_securifi_almond_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="securifi_almond",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc5_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc5",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc5_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc5",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc5_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc5",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc5_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc5",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc7_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc7",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc7_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc7",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc7_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc7",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_archerc7_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_archerc7",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_c1200_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_c1200",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_c1200_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_c1200",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_c1200_5ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_c1200",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_c1200_5ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_c1200",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )

    def test_associate_tplink_tlwr940n_24ghz_open(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_tlwr940n",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
        )
        asserts.assert_true(
            self.dut.associate(self.ssid, SecurityMode.OPEN),
            "Failed to connect.",
        )

    def test_associate_tplink_tlwr940n_24ghz_wpa2(self) -> None:
        setup_ap(
            access_point=self.access_point,
            profile_name="tplink_tlwr940n",
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            ssid=self.ssid,
            security=self.security_profile_wpa2,
        )
        asserts.assert_true(
            self.dut.associate(
                self.ssid,
                SecurityMode.WPA2,
                target_pwd=self.password,
            ),
            "Failed to connect.",
        )


if __name__ == "__main__":
    test_runner.main()
