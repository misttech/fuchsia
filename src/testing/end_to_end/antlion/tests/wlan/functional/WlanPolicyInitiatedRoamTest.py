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
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord


@dataclass
class TestParams:
    dut_security_mode: SecurityMode
    original_security_mode: SecurityMode
    original_band: hostapd_constants.BandType
    target_security_mode: SecurityMode
    target_band: hostapd_constants.BandType
    expect_roam: bool


_DUT_SECURITY_MODES: frozenset[SecurityMode] = frozenset(
    [
        SecurityMode.OPEN,
        SecurityMode.WEP,
        SecurityMode.WPA,
        SecurityMode.WPA2,
        SecurityMode.WPA3,
    ]
)

_AP_SECURITY_MODES: frozenset[SecurityMode] = _DUT_SECURITY_MODES | frozenset(
    [
        SecurityMode.WPA_WPA2,
        SecurityMode.WPA2_WPA3,
    ]
)

_DUT_SECURITY_MODE_TO_COMPATIBLE_AP_MODES: dict[
    SecurityMode, frozenset[SecurityMode]
] = {
    SecurityMode.OPEN: frozenset([SecurityMode.OPEN]),
    SecurityMode.WEP: frozenset([SecurityMode.WEP]),
    SecurityMode.WPA: frozenset([SecurityMode.WPA, SecurityMode.WPA_WPA2]),
    SecurityMode.WPA2: frozenset(
        [
            SecurityMode.WPA2,
            SecurityMode.WPA_WPA2,
            SecurityMode.WPA2_WPA3,
        ]
    ),
    SecurityMode.WPA3: frozenset([SecurityMode.WPA3, SecurityMode.WPA2_WPA3]),
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
            dut_mode,
            compatible_ap_modes,
        ) in _DUT_SECURITY_MODE_TO_COMPATIBLE_AP_MODES.items():
            for ap_mode in compatible_ap_modes:
                # Same compatible security mode on both APs, 2.4 GHz to 5 GHz.
                test_args.append(
                    (
                        TestParams(
                            dut_security_mode=dut_mode,
                            original_security_mode=ap_mode,
                            original_band=hostapd_constants.BandType.BAND_2G,
                            target_security_mode=ap_mode,
                            target_band=hostapd_constants.BandType.BAND_5G,
                            expect_roam=True,
                        ),
                    )
                )

                # Same compatible security mode on both APs, 5 GHz to 2.4 GHz.
                test_args.append(
                    (
                        TestParams(
                            dut_security_mode=dut_mode,
                            original_security_mode=ap_mode,
                            original_band=hostapd_constants.BandType.BAND_5G,
                            target_security_mode=ap_mode,
                            target_band=hostapd_constants.BandType.BAND_2G,
                            expect_roam=True,
                        ),
                    )
                )

                # Test incompatible roams, which should all fail.
                incompatible_modes = _AP_SECURITY_MODES - compatible_ap_modes
                for incompatible_mode in incompatible_modes:
                    test_args.append(
                        (
                            TestParams(
                                dut_security_mode=dut_mode,
                                original_security_mode=ap_mode,
                                original_band=hostapd_constants.BandType.BAND_2G,
                                target_security_mode=incompatible_mode,
                                target_band=hostapd_constants.BandType.BAND_5G,
                                expect_roam=False,
                            ),
                        ),
                    )

        def generate_roam_test_name(test: TestParams) -> str:
            if test.expect_roam:
                expected = "roams"
            else:
                expected = "does_not_roam"
            return f"test_{test.dut_security_mode}_dut_{expected}_from_{test.original_security_mode}_{test.original_band}_to_{test.target_security_mode}_{test.target_band}"

        self.generate_tests(
            test_logic=self.setup_connect_attenuate_roam,
            name_func=generate_roam_test_name,
            arg_sets=test_args,
        )

    def setup_class(self) -> None:
        super().setup_class()

        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

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
        self,
        ssid: str,
        security: Security | None = None,
        additional_ap_parameters: dict[str, int] | None = None,
        channel: int = hostapd_constants.AP_DEFAULT_CHANNEL_2G,
    ) -> None:
        """Sets up an AP using the provided parameters.

        Args:
            ssid: SSID for the AP.
            security: security config for AP, defaults to None (open network
                with no password).
            additional_ap_parameters: A dictionary of parameters that can be set
                directly in the hostapd config file.
            channel: which channel number to set the AP to (default is
                AP_DEFAULT_CHANNEL_2G).
        """
        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=channel,
            ssid=ssid,
            security=security,
            additional_ap_parameters=additional_ap_parameters,
        )

    def _get_client_mac(self) -> str:
        """Get the MAC address of the DUT client interface.

        Returns:
            str, MAC address of the DUT client interface.
        Raises:
            ValueError if there is no DUT client interface.
            WlanError if the DUT interface query fails.
        """
        for wlan_iface in self.dut.get_wlan_interface_id_list():
            result = self.fuchsia_device.honeydew_fd.wlan_core.query_iface2(
                wlan_iface
            )
            if result.role is f_wlan_common.WlanMacRole.CLIENT:
                return utils.mac_address_list_to_str(bytes(result.sta_addr))
        raise ValueError(
            "Failed to get client interface mac address. No client interface found."
        )

    # This is called in generate_tests.
    def setup_connect_attenuate_roam(self, test: TestParams) -> None:
        """Setup the APs, associate a DUT, and slowly reduce AP signal strength until roam.

        Args:
            test: Test parameters
        """
        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        original_password = None
        if test.original_security_mode is not SecurityMode.OPEN:
            # Length 13, so it can be used for WEP or WPA
            original_password = utils.rand_ascii_str(13)

        # Setup original AP.
        original_security = Security(
            test.original_security_mode, original_password
        )
        self.setup_ap(
            ssid,
            security=original_security,
            channel=test.original_band.default_channel(),
        )

        asserts.assert_true(
            self.dut.associate(
                ssid,
                target_pwd=original_password,
                target_security=test.dut_security_mode,
            ),
            "Failed to associate.",
        )
        # Verify that DUT is actually associated (as seen from AP).
        client_mac = self._get_client_mac()

        if test.original_band == hostapd_constants.BandType.BAND_2G:
            original_identifier = self.access_point.wlan_2g
        elif test.original_band == hostapd_constants.BandType.BAND_5G:
            original_identifier = self.access_point.wlan_5g

        asserts.assert_true(
            self.access_point.sta_associated(original_identifier, client_mac),
            f"DUT is not associated on the {test.original_band} band",
        )

        # Setup target AP.
        target_security = Security(test.target_security_mode, original_password)
        self.setup_ap(
            ssid,
            security=target_security,
            channel=test.target_band.default_channel(),
        )

        if test.target_band == hostapd_constants.BandType.BAND_2G:
            target_identifier = self.access_point.wlan_2g
        elif test.target_band == hostapd_constants.BandType.BAND_5G:
            target_identifier = self.access_point.wlan_5g

        FULL_POWER_DBM = 23
        current_dbm = FULL_POWER_DBM
        NUM_ITERATIONS = 10
        PERIOD_S = 10

        for id in (original_identifier, target_identifier):
            # Reset back to full power.
            self.access_point.iwconfig.ap_iwconfig(
                id, f"txpower {FULL_POWER_DBM}"
            )
            self.access_point.iwconfig.ap_iwconfig(id, "txpower auto")

        for i in range(NUM_ITERATIONS):
            # Reduce power, but with a floor of 1 dBm.
            current_dbm = max(current_dbm // 2, 1)
            self.access_point.iwconfig.ap_iwconfig(
                original_identifier, f"txpower {current_dbm}"
            )

            period_deadline = datetime.now() + timedelta(seconds=PERIOD_S)
            while datetime.now() < period_deadline:
                # Check for STA on destination, and if it has roamed, end the test.
                if test.expect_roam:
                    if self.access_point.sta_authorized(
                        target_identifier, client_mac
                    ):
                        break
                    # We want to detect if DUT disconnected from the original BSS without roaming to the
                    # target BSS. Specifically, we want to avoid a false positive if DUT does a full
                    # disconnect from the original BSS followed by a regular connect to the target BSS,
                    # rather than roaming between them. This is not a perfect mechanism to detect this
                    # case, but it suffices for manually run tests. Automated tests will need a better
                    # way to detect this scenario.
                    # TODO(https://fxbug.dev/359966771): Surface intermediate states to Antlion.
                    if not self.access_point.sta_associated(
                        original_identifier, client_mac
                    ):
                        raise signals.TestFailure(
                            "DUT left original BSS without roaming to target BSS"
                        )
                time.sleep(0.25)

        if test.expect_roam:
            # Verify that DUT roamed (as seen from AP).
            asserts.assert_true(
                self.access_point.sta_authenticated(
                    target_identifier, client_mac
                ),
                f"DUT is not authenticated on the {test.target_band} band",
            )
            asserts.assert_true(
                self.access_point.sta_associated(target_identifier, client_mac),
                f"DUT is not associated on the {test.target_band} band",
            )
            asserts.assert_true(
                self.access_point.sta_authorized(target_identifier, client_mac),
                "DUT is not 802.1X authorized on the 5GHz band",
            )
        else:
            # DUT should have stayed on the original BSS.
            asserts.assert_true(
                self.access_point.sta_authenticated(
                    original_identifier, client_mac
                ),
                f"DUT is not authenticated on the {test.original_band} band",
            )


if __name__ == "__main__":
    test_runner.main()
