#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_5G,
    AP_SSID_LENGTH_5G,
)
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord


# TODO(fxb/68956): Add security protocol check to mixed mode tests when info is
# available.
class WlanTargetSecurityTest(base_test.WifiBaseTest):
    """Tests Fuchsia's target security concept and security upgrading

    Testbed Requirements:
    * One Fuchsia device
    * One Whirlwind Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()

        self.dut = self.get_dut(AssociationMode.POLICY)

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]

    def teardown_class(self) -> None:
        self.dut.disconnect()
        self.access_point.stop_all_aps()
        super().teardown_class()

    def teardown_test(self) -> None:
        self.dut.disconnect()
        self.download_logs()
        self.access_point.stop_all_aps()
        super().teardown_test()

    def on_fail(self, record: TestResultRecord) -> None:
        self.dut.disconnect()
        self.access_point.stop_all_aps()
        super().on_fail(record)

    def setup_ap(
        self, security_mode: SecurityMode = SecurityMode.OPEN
    ) -> tuple[str, str]:
        """Sets up an AP using the provided security mode.

        Args:
            security_mode: string, security mode for AP
        Returns:
            Tuple, (ssid, password). Returns a password even if for open
                security, since non-open target securities require a credential
                to attempt a connection.
        """
        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_5G)
        # Length 13, so it can be used for WEP or WPA
        password = utils.rand_ascii_str(13)
        security_profile = Security(
            security_mode=security_mode, password=password
        )

        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=AP_DEFAULT_CHANNEL_5G,
            ssid=ssid,
            security=security_profile,
        )

        return (ssid, password)

    # Open Security on AP
    def test_associate_open_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap()
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )

    def test_reject_open_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Should not have associated.",
        )

    def test_reject_open_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Should not have associated.",
        )

    def test_reject_open_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Should not have associated.",
        )

    def test_reject_open_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Should not have associated.",
        )

    # WEP Security on AP
    def test_reject_wep_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityMode.WEP)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_associate_wep_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WEP)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Failed to associate.",
        )

    def test_reject_wep_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WEP)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Should not have associated.",
        )

    def test_reject_wep_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WEP)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Should not have associated.",
        )

    def test_reject_wep_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WEP)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Should not have associated.",
        )

    # WPA Security on AP
    def test_reject_wpa_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityMode.WPA)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Should not have associated.",
        )

    def test_associate_wpa_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Failed to associate.",
        )

    def test_reject_wpa_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Should not have associated.",
        )

    def test_reject_wpa_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Should not have associated.",
        )

    # WPA2 Security on AP
    def test_reject_wpa2_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityMode.WPA2)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa2_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Should not have associated.",
        )

    def test_associate_wpa2_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Failed to associate.",
        )

    def test_associate_wpa2_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Failed to associate.",
        )

    def test_reject_wpa2_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Should not have associated.",
        )

    # WPA/WPA2 Security on AP
    def test_reject_wpa_wpa2_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityMode.WPA_WPA2)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa_wpa2_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA_WPA2)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Should not have associated.",
        )

    def test_associate_wpa_wpa2_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA_WPA2)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Failed to associate.",
        )

    def test_associate_wpa_wpa2_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA_WPA2)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Failed to associate.",
        )

    def test_reject_wpa_wpa2_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA_WPA2)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Should not have associated.",
        )

    # WPA3 Security on AP
    def test_reject_wpa3_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityMode.WPA3)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa3_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA3)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Should not have associated.",
        )

    def test_associate_wpa3_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA3)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Expected failure to associate. WPA credentials for WPA3 was "
            "temporarily disabled, see https://fxbug.dev/42166758 for context. "
            "If this feature was reenabled, please update this test's "
            "expectation.",
        )

    def test_associate_wpa3_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA3)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Failed to associate.",
        )

    def test_associate_wpa3_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA3)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Failed to associate.",
        )

    # WPA2/WPA3 Security on AP
    def test_reject_wpa2_wpa3_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityMode.WPA2_WPA3)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa2_wpa3_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2_WPA3)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WEP, target_pwd=password),
            "Should not have associated.",
        )

    def test_associate_wpa2_wpa3_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2_WPA3)
        asserts.assert_false(
            self.dut.associate(ssid, SecurityMode.WPA, target_pwd=password),
            "Expected failure to associate. WPA credentials for WPA3 was "
            "temporarily disabled, see https://fxbug.dev/42166758 for context. "
            "If this feature was reenabled, please update this test's "
            "expectation.",
        )

    def test_associate_wpa2_wpa3_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2_WPA3)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA2, target_pwd=password),
            "Failed to associate.",
        )

    def test_associate_wpa2_wpa3_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityMode.WPA2_WPA3)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.WPA3, target_pwd=password),
            "Failed to associate.",
        )


if __name__ == "__main__":
    test_runner.main()
