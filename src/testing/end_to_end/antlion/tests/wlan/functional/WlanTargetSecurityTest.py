#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_5G,
    AP_SSID_LENGTH_5G,
)
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWep,
    SecurityWpa,
    SecurityWpa2,
    SecurityWpa2Wpa3Mixed,
    SecurityWpa3,
    SecurityWpaWpa2Mixed,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)


# TODO(https://fxbug.dev/68956): Add security protocol check to mixed mode tests
# when info is available.
class WlanTargetSecurityTest(base_test.WifiBaseTest):
    """Tests Fuchsia's target security concept and security upgrading

    Testbed Requirements:
    * One Fuchsia device
    * One Whirlwind Access Point
    """

    def setup_class(self) -> None:
        super().setup_class()

        self.dut = self.get_dut(AssociationMode.POLICY)

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

    def teardown_test(self) -> None:
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()
        super().teardown_test()

    def setup_ap(self, security: Security = SecurityOpen()) -> tuple[str, str]:
        """Sets up an AP using the provided security mode.

        Args:
            security: Security, security mode for AP
        Returns:
            Tuple, (ssid, password). Returns a password even if for open
                security, since non-open target securities require a credential
                to attempt a connection.
        """
        ssid = AccessPointConfig.random_string(AP_SSID_LENGTH_5G)
        # Length 13, so it can be used for WEP or WPA
        password = AccessPointConfig.random_string(13)

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_5G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=security,
                                password=(
                                    password
                                    if security != SecurityOpen()
                                    else None
                                ),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            assert self.access_point is not None
            hostapd_security_mode = ConfigMapper.to_hostapd_security(security)

            security_profile = DeprecatedSecurity(
                security_mode=hostapd_security_mode, password=password
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
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Failed to associate.",
        )

    def test_reject_open_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_reject_open_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_reject_open_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_reject_open_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap()
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Should not have associated.",
        )

    # WEP Security on AP
    def test_reject_wep_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityWep())
        asserts.assert_false(
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_associate_wep_ap_with_wep_target_security(self) -> None:
        # TODO(b/490162087): Remove this skip once OpenWrt supports WEP security
        self.skip_if_wep_not_supported()
        ssid, password = self.setup_ap(SecurityWep())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Failed to associate.",
        )

    def skip_if_wep_not_supported(self) -> None:
        if self.openwrt_ap:
            raise signals.TestSkip("OpenWrt does not support WEP security")

    def test_reject_wep_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWep())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_reject_wep_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWep())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_reject_wep_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWep())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Should not have associated.",
        )

    # WPA Security on AP
    def test_reject_wpa_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityWpa())
        asserts.assert_false(
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_associate_wpa_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_reject_wpa_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_reject_wpa_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Should not have associated.",
        )

    # WPA2 Security on AP
    def test_reject_wpa2_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityWpa2())
        asserts.assert_false(
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa2_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_associate_wpa2_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_associate_wpa2_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_reject_wpa2_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Should not have associated.",
        )

    # WPA/WPA2 Security on AP
    def test_reject_wpa_wpa2_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityWpaWpa2Mixed())
        asserts.assert_false(
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa_wpa2_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpaWpa2Mixed())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_associate_wpa_wpa2_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpaWpa2Mixed())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_associate_wpa_wpa2_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpaWpa2Mixed())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_reject_wpa_wpa2_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpaWpa2Mixed())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Should not have associated.",
        )

    # WPA3 Security on AP
    def test_reject_wpa3_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityWpa3())
        asserts.assert_false(
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa3_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa3())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_associate_wpa3_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa3())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Expected failure to associate. WPA credentials for WPA3 was "
            "temporarily disabled, see https://fxbug.dev/42166758 for context. "
            "If this feature was reenabled, please update this test's "
            "expectation.",
        )

    def test_associate_wpa3_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa3())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_associate_wpa3_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa3())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Failed to associate.",
        )

    # WPA2/WPA3 Security on AP
    def test_reject_wpa2_wpa3_ap_with_open_target_security(self) -> None:
        ssid, _ = self.setup_ap(SecurityWpa2Wpa3Mixed())
        asserts.assert_false(
            self.dut.associate(ssid, DeprecatedSecurityMode.OPEN),
            "Should not have associated.",
        )

    def test_reject_wpa2_wpa3_ap_with_wep_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2Wpa3Mixed())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WEP, target_pwd=password
            ),
            "Should not have associated.",
        )

    def test_associate_wpa2_wpa3_ap_with_wpa_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2Wpa3Mixed())
        asserts.assert_false(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA, target_pwd=password
            ),
            "Expected failure to associate. WPA credentials for WPA3 was "
            "temporarily disabled, see https://fxbug.dev/42166758 for context. "
            "If this feature was reenabled, please update this test's "
            "expectation.",
        )

    def test_associate_wpa2_wpa3_ap_with_wpa2_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2Wpa3Mixed())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA2, target_pwd=password
            ),
            "Failed to associate.",
        )

    def test_associate_wpa2_wpa3_ap_with_wpa3_target_security(self) -> None:
        ssid, password = self.setup_ap(SecurityWpa2Wpa3Mixed())
        asserts.assert_true(
            self.dut.associate(
                ssid, DeprecatedSecurityMode.WPA3, target_pwd=password
            ),
            "Failed to associate.",
        )


if __name__ == "__main__":
    test_runner.main()
