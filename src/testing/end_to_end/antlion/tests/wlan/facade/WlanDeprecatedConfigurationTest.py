#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

import fidl_fuchsia_wlan_common as f_wlan_common
from antlion import utils
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectivityMode,
    OperatingBand,
    SecurityType,
)
from mobly import asserts, test_runner
from mobly.config_parser import TestRunConfig

AP_ROLE = "Ap"
DEFAULT_SSID = "testssid"
TEST_MAC_ADDR = "12:34:56:78:9a:bc"
TEST_MAC_ADDR_SECONDARY = "bc:9a:78:56:34:12"


class WlanDeprecatedConfigurationTest(base_test.WifiBaseTest):
    """Tests for WlanDeprecatedConfigurationFacade"""

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()
        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

    def setup_test(self) -> None:
        super().setup_test()
        self._stop_soft_aps()

    def teardown_test(self) -> None:
        self._stop_soft_aps()
        super().teardown_test()

    def _get_ap_interface_mac_address(self) -> str:
        """Retrieves mac address from wlan interface with role ap

        Returns:
            string, the mac address of the AP interface

        Raises:
            ConnectionError, if SL4F calls fail
            AttributeError, if no interface has role 'Ap'
        """
        for wlan_iface in self.dut.get_wlan_interface_id_list():
            result = self.fuchsia_device.honeydew_fd.wlan_core.query_iface(
                wlan_iface
            )
            if result.role is f_wlan_common.WlanMacRole.AP:
                return utils.mac_address_list_to_str(bytes(result.sta_addr))
        raise AttributeError(
            "Failed to get ap interface mac address. No AP interface found."
        )

    def _start_soft_ap(self) -> None:
        """Starts SoftAP on DUT.

        Raises:
            ConnectionError, if SL4F call fails.
        """
        self.log.info("Starting SoftAP on device %s", self.dut.identifier)
        self.fuchsia_device.honeydew_fd.wlan_policy_ap.start(
            DEFAULT_SSID,
            SecurityType.NONE,
            None,
            ConnectivityMode.LOCAL_ONLY,
            OperatingBand.ANY,
        )

    def _stop_soft_aps(self) -> None:
        """Stops SoftAP on DUT.

        Raises:
            ConnectionError, if SL4F call fails.
        """
        self.log.info("Stopping SoftAP.")
        self.fuchsia_device.honeydew_fd.wlan_policy_ap.stop_all()

    def _suggest_ap_mac_addr(self, mac_addr: str) -> None:
        """Suggests mac address for AP interface.
        Args:
            mac_addr: string, mac address to suggest.

        Raises:
            TestFailure, if SL4F call fails.
        """
        self.log.info(
            "Suggesting AP mac addr (%s) via wlan_deprecated_configuration_lib.",
            mac_addr,
        )
        response = self.fuchsia_device.sl4f.wlan_deprecated_configuration_lib.wlanSuggestAccessPointMacAddress(
            mac_addr
        )
        if response.get("error"):
            asserts.fail(
                f"Failed to suggest AP mac address ({mac_addr}): {response['error']}"
            )

    def _verify_mac_addr(self, expected_addr: str) -> None:
        """Verifies mac address of ap interface is set to expected mac address.

        Args:
            Args:
                expected_addr: string, expected mac address

            Raises:
                TestFailure, if actual mac address is not expected mac address.
        """
        set_mac_addr = self._get_ap_interface_mac_address()
        if set_mac_addr != expected_addr:
            asserts.fail(
                f"Failed to set AP mac address via wlan_deprecated_configuration_lib. "
                f"Expected mac addr: {expected_addr}, Actual mac addr: {set_mac_addr}"
            )
        else:
            self.log.info(f"AP mac address successfully set to {expected_addr}")

    def test_suggest_ap_mac_address(self) -> None:
        """Tests suggest ap mac address SL4F call

        1. Get initial mac address
        2. Suggest new mac address
        3. Verify new mac address is set successfully
        4. Reset to initial mac address
        5. Verify initial mac address is reset successfully


        Raises:
            TestFailure, if wlanSuggestAccessPointMacAddress call fails or
                of mac address is not the suggest value
            ConnectionError, if other SL4F calls fail
        """
        # Retrieve initial ap mac address
        self._start_soft_ap()

        self.log.info("Getting initial mac address.")
        initial_mac_addr = self._get_ap_interface_mac_address()
        self.log.info(f"Initial mac address: {initial_mac_addr}")

        if initial_mac_addr != TEST_MAC_ADDR:
            suggested_mac_addr = TEST_MAC_ADDR
        else:
            suggested_mac_addr = TEST_MAC_ADDR_SECONDARY

        self._stop_soft_aps()

        # Suggest and verify new mac address
        self._suggest_ap_mac_addr(suggested_mac_addr)

        self._start_soft_ap()

        self._verify_mac_addr(suggested_mac_addr)

        self._stop_soft_aps()

        # Reset to initial mac address and verify
        self.log.info(f"Resetting to initial mac address ({initial_mac_addr}).")
        self._suggest_ap_mac_addr(initial_mac_addr)

        self._start_soft_ap()

        self._verify_mac_addr(initial_mac_addr)


if __name__ == "__main__":
    test_runner.main()
