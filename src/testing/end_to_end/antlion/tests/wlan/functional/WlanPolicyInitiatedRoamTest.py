#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import time
from dataclasses import dataclass
from datetime import datetime, timedelta

import fidl_fuchsia_wlan_common as f_wlan_common
from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord
from openwrt_access_point import Radio
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    Band,
    BssChannel,
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


@dataclass
class TestParams:
    dut_security: Security
    original_security: Security
    original_channel: BssChannel
    target_security: Security
    target_channel: BssChannel
    expect_roam: bool


_DUT_SECURITIES: frozenset[Security] = frozenset(
    [
        SecurityOpen(),
        SecurityWep(),
        SecurityWpa(),
        SecurityWpa2(),
        SecurityWpa3(),
    ]
)

_AP_SECURITIES: frozenset[Security] = _DUT_SECURITIES | frozenset(
    [
        SecurityWpaWpa2Mixed(),
        SecurityWpa2Wpa3Mixed(),
    ]
)

_DUT_SECURITY_TO_COMPATIBLE_AP_SECURITIES: dict[
    Security, frozenset[Security]
] = {
    SecurityOpen(): frozenset([SecurityOpen()]),
    SecurityWep(): frozenset([SecurityWep()]),
    SecurityWpa(): frozenset([SecurityWpa(), SecurityWpaWpa2Mixed()]),
    SecurityWpa2(): frozenset(
        [
            SecurityWpa2(),
            SecurityWpaWpa2Mixed(),
            SecurityWpa2Wpa3Mixed(),
        ]
    ),
    SecurityWpa3(): frozenset([SecurityWpa3(), SecurityWpa2Wpa3Mixed()]),
}


class WlanPolicyInitiatedRoamTest(base_test.WifiBaseTest):
    """Tests Fuchsia's WLAN Policy-initiated roam support.

    Testbed Requirements:
    * One Fuchsia device
    * One Whirlwind access point
    """

    def pre_run(self) -> None:
        test_args: list[tuple[TestParams]] = []

        for (
            dut_security,
            compatible_ap_securities,
        ) in _DUT_SECURITY_TO_COMPATIBLE_AP_SECURITIES.items():
            for ap_security in compatible_ap_securities:
                # Same compatible security mode on both APs, 2.4 GHz to 5 GHz.
                test_args.append(
                    (
                        TestParams(
                            dut_security=dut_security,
                            original_security=ap_security,
                            original_channel=DEFAULT_2G_CHANNEL,
                            target_security=ap_security,
                            target_channel=DEFAULT_5G_CHANNEL,
                            expect_roam=True,
                        ),
                    )
                )

                # Same compatible security mode on both APs, 5 GHz to 2.4 GHz.
                test_args.append(
                    (
                        TestParams(
                            dut_security=dut_security,
                            original_security=ap_security,
                            original_channel=DEFAULT_5G_CHANNEL,
                            target_security=ap_security,
                            target_channel=DEFAULT_2G_CHANNEL,
                            expect_roam=True,
                        ),
                    )
                )

                # Test incompatible roams, which should all fail.
                incompatible_securities = (
                    _AP_SECURITIES - compatible_ap_securities
                )
                for incompatible_security in incompatible_securities:
                    test_args.append(
                        (
                            TestParams(
                                dut_security=dut_security,
                                original_security=ap_security,
                                original_channel=DEFAULT_2G_CHANNEL,
                                target_security=incompatible_security,
                                target_channel=DEFAULT_5G_CHANNEL,
                                expect_roam=False,
                            ),
                        ),
                    )

        def generate_roam_test_name(test: TestParams) -> str:
            expected = "roams" if test.expect_roam else "does_not_roam"
            return f"test_{test.dut_security.uci_encryption}_dut_{expected}_from_{test.original_security.uci_encryption}_{test.original_channel.band}_to_{test.target_security.uci_encryption}_{test.target_channel.band}"

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=generate_roam_test_name,
            arg_sets=test_args,
        )

    def setup_class(self) -> None:
        super().setup_class()

        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

        self.openwrt_ap = None
        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

    def teardown_class(self) -> None:
        self.dut.disconnect()
        if self.access_point:
            self.access_point.stop_all_aps()
        super().teardown_class()

    def teardown_test(self) -> None:
        self.dut.disconnect()
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()
        super().teardown_test()

    def on_fail(self, record: TestResultRecord) -> None:
        self.dut.disconnect()
        if self.access_point:
            self.access_point.stop_all_aps()
        super().on_fail(record)

    def _get_client_mac(self) -> str:
        """Get the MAC address of the DUT client interface.

        Returns:
            str, MAC address of the DUT client interface.
        Raises:
            ValueError if there is no DUT client interface.
            WlanError if the DUT interface query fails.
        """
        for wlan_iface in self.dut.get_wlan_interface_id_list():
            result = self.fuchsia_device.honeydew_fd.wlan_core_deprecated_sync.query_iface(
                wlan_iface
            )
            if result.role == f_wlan_common.WlanMacRole.CLIENT:
                return utils.mac_address_list_to_str(bytes(result.sta_addr))
        raise ValueError(
            "Failed to get client interface mac address. No client interface found."
        )

    def _test_logic(self, test: TestParams) -> None:
        """Setup the APs, associate a DUT, and slowly reduce AP signal strength until roam.

        Args:
            test: Test parameters
        """
        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        original_password = None
        if not isinstance(test.original_security, SecurityOpen):
            # Length 13, so it can be used for WEP or WPA
            original_password = utils.rand_ascii_str(13)

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=test.original_channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=test.original_security,
                                password=original_password,
                            )
                        ],
                    ),
                    RadioConfig.generate(
                        channel=test.target_channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=test.target_security,
                                password=original_password,
                            )
                        ],
                    ),
                ]
            )
            self.openwrt_ap.configure_wifi(config)

            target_radio = (
                Radio.RADIO_5G
                if test.target_channel.band == Band.BAND_5G
                else Radio.RADIO_2G
            )
            # Disable target radio immediately so client connects to original band
            self.openwrt_ap.disable_radio(target_radio)
        elif self.access_point:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=test.original_channel.number,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(
                        test.original_security
                    ),
                    password=original_password,
                ),
            )
        asserts.assert_true(
            self.dut.associate(
                ssid,
                target_pwd=original_password,
                target_security=ConfigMapper.to_hostapd_security(
                    test.dut_security
                ),
            ),
            "Failed to associate.",
        )
        # Verify that DUT is actually associated (as seen from AP).
        client_mac = self._get_client_mac()

        original_identifier = ""
        target_identifier = ""

        if self.openwrt_ap:
            if test.original_channel.band == Band.BAND_2G:
                original_identifier = self.openwrt_ap.wlan_2g_interface
            else:
                original_identifier = self.openwrt_ap.wlan_5g_interface
        elif self.access_point:
            if test.original_channel.band == Band.BAND_2G:
                original_identifier = self.access_point.wlan_2g
            elif test.original_channel.band == Band.BAND_5G:
                original_identifier = self.access_point.wlan_5g

        if self.openwrt_ap:
            status = self.openwrt_ap.get_sta_status(
                client_mac, test.original_channel.band
            )
            iface_status = status.get(original_identifier)
            is_assoc = iface_status and iface_status.assoc
        else:
            assert self.access_point is not None
            is_assoc = self.access_point.sta_associated(
                original_identifier, client_mac
            )

        asserts.assert_true(
            is_assoc,
            f"DUT is not associated on the {test.original_channel.band} band",
        )

        # Setup target AP.
        if self.openwrt_ap:
            target_radio = (
                Radio.RADIO_5G
                if test.target_channel.band == Band.BAND_5G
                else Radio.RADIO_2G
            )
            self.openwrt_ap.enable_radio(target_radio)
        elif self.access_point:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=test.target_channel.number,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(
                        test.target_security
                    ),
                    password=original_password,
                ),
            )

        if self.openwrt_ap:
            if test.target_channel.band == Band.BAND_2G:
                target_identifier = self.openwrt_ap.wlan_2g_interface
            else:
                target_identifier = self.openwrt_ap.wlan_5g_interface
        elif self.access_point:
            if test.target_channel.band == Band.BAND_2G:
                target_identifier = self.access_point.wlan_2g
            elif test.target_channel.band == Band.BAND_5G:
                target_identifier = self.access_point.wlan_5g

        FULL_POWER_DBM = 23
        current_dbm = FULL_POWER_DBM
        NUM_ITERATIONS = 10
        PERIOD_S = 10

        for id in (original_identifier, target_identifier):
            # Reset back to full power.
            if self.openwrt_ap:
                self.openwrt_ap.reset_txpower(id)
            elif self.access_point:
                self.access_point.iwconfig.ap_iwconfig(
                    id, f"txpower {FULL_POWER_DBM}"
                )
                self.access_point.iwconfig.ap_iwconfig(id, "txpower auto")
        for i in range(NUM_ITERATIONS):
            # Reduce power, but with a floor of 1 dBm.
            current_dbm = max(current_dbm // 2, 1)
            if self.openwrt_ap:
                self.openwrt_ap.set_txpower(original_identifier, current_dbm)
            elif self.access_point:
                self.access_point.iwconfig.ap_iwconfig(
                    original_identifier, f"txpower {current_dbm}"
                )

            period_deadline = datetime.now() + timedelta(seconds=PERIOD_S)
            while datetime.now() < period_deadline:
                # Check for STA on destination, and if it has roamed, end the test.
                if test.expect_roam:
                    if self.openwrt_ap:
                        status = self.openwrt_ap.get_sta_status(
                            client_mac, test.target_channel.band
                        )
                        iface_status = status.get(target_identifier)
                        is_authorized = iface_status and iface_status.authorized
                    else:
                        assert self.access_point is not None
                        is_authorized = self.access_point.sta_authorized(
                            target_identifier, client_mac
                        )

                    if is_authorized:
                        break
                    # We want to detect if DUT disconnected from the original BSS without roaming to the
                    # target BSS. Specifically, we want to avoid a false positive if DUT does a full
                    # disconnect from the original BSS followed by a regular connect to the target BSS,
                    # rather than roaming between them. This is not a perfect mechanism to detect this
                    # case, but it suffices for manually run tests. Automated tests will need a better
                    # way to detect this scenario.
                    # TODO(https://fxbug.dev/359966771): Surface intermediate states to Antlion.
                    if self.openwrt_ap:
                        status = self.openwrt_ap.get_sta_status(
                            client_mac, test.original_channel.band
                        )
                        iface_status = status.get(original_identifier)
                        is_assoc = iface_status and iface_status.assoc
                    else:
                        assert self.access_point is not None
                        is_assoc = self.access_point.sta_associated(
                            original_identifier, client_mac
                        )

                    if not is_assoc:
                        raise signals.TestFailure(
                            "DUT left original BSS without roaming to target BSS"
                        )
                time.sleep(0.25)

        if test.expect_roam:
            # Verify that DUT roamed (as seen from AP).
            if self.openwrt_ap:
                status = self.openwrt_ap.get_sta_status(
                    client_mac, test.target_channel.band
                )
                iface_status = status.get(target_identifier)
                is_auth = iface_status and iface_status.auth
                is_assoc = iface_status and iface_status.assoc
                is_authorized = iface_status and iface_status.authorized
            else:
                assert self.access_point is not None
                is_auth = self.access_point.sta_authenticated(
                    target_identifier, client_mac
                )
                is_assoc = self.access_point.sta_associated(
                    target_identifier, client_mac
                )
                is_authorized = self.access_point.sta_authorized(
                    target_identifier, client_mac
                )

            asserts.assert_true(
                is_auth,
                f"DUT is not authenticated on the {test.target_channel.band} band",
            )
            asserts.assert_true(
                is_assoc,
                f"DUT is not associated on the {test.target_channel.band} band",
            )
            asserts.assert_true(
                is_authorized, "DUT is not 802.1X authorized on the 5GHz band"
            )
        else:
            # DUT should have stayed on the original BSS.
            if self.openwrt_ap:
                status = self.openwrt_ap.get_sta_status(
                    client_mac, test.original_channel.band
                )
                iface_status = status.get(original_identifier)
                is_auth = iface_status and iface_status.auth
            else:
                assert self.access_point is not None
                is_auth = self.access_point.sta_authenticated(
                    original_identifier, client_mac
                )

            asserts.assert_true(
                is_auth,
                f"DUT is not authenticated on the {test.original_channel.band.name} band",
            )


if __name__ == "__main__":
    test_runner.main()
