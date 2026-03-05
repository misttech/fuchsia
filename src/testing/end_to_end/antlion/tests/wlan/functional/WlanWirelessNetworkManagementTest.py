#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import FrozenSet

import fidl_fuchsia_wlan_common as f_wlan_common
from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.radio_measurement import (
    BssidInformation,
    BssidInformationCapabilities,
    NeighborReportElement,
    PhyType,
)
from antlion.controllers.ap_lib.wireless_network_management import (
    BssTransitionCandidateList,
    BssTransitionManagementRequest,
)
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord


@dataclass
class TestParams:
    security_mode: SecurityMode


# Antlion can see (via the wlan_features config directive) whether WNM features
# are enabled, and runs or skips tests depending on presence of WNM features.
class WlanWirelessNetworkManagementTest(base_test.WifiBaseTest):
    """Tests Fuchsia's Wireless Network Management (AKA 802.11v) support.

    Testbed Requirements:
    * One Fuchsia device
    * One Whirlwind access point

    Existing Fuchsia drivers do not yet support WNM features out-of-the-box, so this
    suite skips certain tests depending on whether specific WNM features are enabled.
    """

    def pre_run(self) -> None:
        test_args: list[tuple[TestParams]] = []

        SECURITY_MODES = (
            SecurityMode.OPEN,
            SecurityMode.WEP,
            SecurityMode.WPA,
            SecurityMode.WPA2,
            SecurityMode.WPA3,
        )
        for security_mode in SECURITY_MODES:
            test_args.append(
                (
                    TestParams(
                        security_mode=security_mode,
                    ),
                )
            )

        def generate_roam_on_btm_req_test_name(test: TestParams) -> str:
            return f"test_roam_on_btm_req_from_{test.security_mode}_2g_to_{test.security_mode}_5g"

        self.generate_tests(
            test_logic=self.setup_connect_roam_on_btm_req,
            name_func=generate_roam_on_btm_req_test_name,
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
        wnm_features: FrozenSet[hostapd_constants.WnmFeature] = frozenset(),
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
            wnm_features: Wireless Network Management features to enable
                (default is no WNM features).
        """
        setup_ap(
            access_point=self.access_point,
            profile_name="whirlwind",
            channel=channel,
            ssid=ssid,
            security=security,
            additional_ap_parameters=additional_ap_parameters,
            wnm_features=wnm_features,
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
            result = self.fuchsia_device.honeydew_fd.wlan_core.query_iface(
                wlan_iface
            )
            if result.role == f_wlan_common.WlanMacRole.CLIENT:
                return utils.mac_address_list_to_str(bytes(result.sta_addr))
        raise ValueError(
            "Failed to get client interface mac address. No client interface found."
        )

    def test_bss_transition_is_not_advertised_when_ap_supported_dut_unsupported(
        self,
    ) -> None:
        if self.dut.feature_is_present("BSS_TRANSITION_MANAGEMENT"):
            raise signals.TestSkip(
                "skipping test because BTM feature is present"
            )

        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        self.setup_ap(ssid, wnm_features=wnm_features)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")
        client_mac = self._get_client_mac()
        # Verify that DUT is actually associated (as seen from AP).
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT is not associated on the 2.4GHz band",
        )

        ext_capabilities = self.access_point.get_sta_extended_capabilities(
            self.access_point.wlan_2g, client_mac
        )
        asserts.assert_false(
            ext_capabilities.bss_transition,
            "DUT is incorrectly advertising BSS Transition Management support",
        )

    def test_bss_transition_is_advertised_when_ap_supported_dut_supported(
        self,
    ) -> None:
        if not self.dut.feature_is_present("BSS_TRANSITION_MANAGEMENT"):
            raise signals.TestSkip(
                "skipping test because BTM feature is not present"
            )

        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        self.setup_ap(ssid, wnm_features=wnm_features)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")
        client_mac = self._get_client_mac()
        # Verify that DUT is actually associated (as seen from AP).
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT is not associated on the 2.4GHz band",
        )

        ext_capabilities = self.access_point.get_sta_extended_capabilities(
            self.access_point.wlan_2g, client_mac
        )
        asserts.assert_true(
            ext_capabilities.bss_transition,
            "DUT is not advertising BSS Transition Management support",
        )

    def test_wnm_sleep_mode_is_not_advertised_when_ap_supported_dut_unsupported(
        self,
    ) -> None:
        if self.dut.feature_is_present("WNM_SLEEP_MODE"):
            raise signals.TestSkip(
                "skipping test because WNM feature is present"
            )

        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        wnm_features = frozenset([hostapd_constants.WnmFeature.WNM_SLEEP_MODE])
        self.setup_ap(ssid, wnm_features=wnm_features)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")
        client_mac = self._get_client_mac()
        # Verify that DUT is actually associated (as seen from AP).
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT is not associated on the 2.4GHz band",
        )

        ext_capabilities = self.access_point.get_sta_extended_capabilities(
            self.access_point.wlan_2g, client_mac
        )
        asserts.assert_false(
            ext_capabilities.wnm_sleep_mode,
            "DUT is incorrectly advertising WNM Sleep Mode support",
        )

    # This is called in generate_tests.
    def setup_connect_roam_on_btm_req(self, test: TestParams) -> None:
        """Setup the APs, associate a DUT, amd roam when BTM request is received.

        Args:
            test: Test parameters
        """
        if not self.dut.feature_is_present("BSS_TRANSITION_MANAGEMENT"):
            raise signals.TestSkip(
                "skipping test because BTM feature is not present"
            )

        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        password = None
        if test.security_mode is not SecurityMode.OPEN:
            # Length 13, so it can be used for WEP or WPA
            password = utils.rand_ascii_str(13)

        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )

        # Setup 2.4 GHz AP.
        security = Security(test.security_mode, password)
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        # Setup 2.4 GHz AP.
        self.setup_ap(
            ssid,
            security=security,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            wnm_features=wnm_features,
        )

        asserts.assert_true(
            self.dut.associate(
                ssid, target_pwd=password, target_security=test.security_mode
            ),
            "Failed to associate.",
        )
        # Verify that DUT is actually associated (as seen from AP).
        client_mac = self._get_client_mac()
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT is not associated on the 2.4GHz band",
        )

        # Setup 5 GHz AP with same SSID.
        self.setup_ap(
            ssid,
            security=security,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            wnm_features=wnm_features,
        )

        # Construct a BTM request.
        dest_bssid = self.access_point.get_bssid_from_ssid(
            ssid,
            hostapd_constants.BandType.BAND_5G,
        )
        dest_bssid_info = BssidInformation(
            security=True, capabilities=BssidInformationCapabilities()
        )
        neighbor_5g_ap = NeighborReportElement(
            dest_bssid,
            dest_bssid_info,
            operating_class=116,
            channel_number=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            phy_type=PhyType.VHT,
        )
        btm_req = BssTransitionManagementRequest(
            preferred_candidate_list_included=True,
            disassociation_imminent=True,
            candidate_list=BssTransitionCandidateList([neighbor_5g_ap]),
        )

        # Sleep to avoid concurrent scan during reassociation, necessary due to a firmware bug.
        # TODO(fxbug.dev/42068735) Remove when fixed, or when non-firmware BTM support is merged.
        time.sleep(5)

        # Send BTM request from 2.4 GHz AP to DUT
        self.access_point.send_bss_transition_management_req(
            self.access_point.wlan_2g, client_mac, btm_req
        )

        # Give DUT time to roam.
        ROAM_DEADLINE = datetime.now(timezone.utc) + timedelta(seconds=2)
        while datetime.now(timezone.utc) < ROAM_DEADLINE:
            if self.access_point.sta_authorized(
                self.access_point.wlan_5g, client_mac
            ):
                break
            else:
                time.sleep(0.25)

        # Verify that DUT roamed (as seen from AP).
        asserts.assert_true(
            self.access_point.sta_authenticated(
                self.access_point.wlan_5g, client_mac
            ),
            "DUT is not authenticated on the 5GHz band",
        )
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_5g, client_mac
            ),
            "DUT is not associated on the 5GHz band",
        )
        asserts.assert_true(
            self.access_point.sta_authorized(
                self.access_point.wlan_5g, client_mac
            ),
            "DUT is not 802.1X authorized on the 5GHz band",
        )

    def test_btm_req_ignored_dut_unsupported(self) -> None:
        if self.dut.feature_is_present("BSS_TRANSITION_MANAGEMENT"):
            raise signals.TestSkip(
                "skipping test because BTM feature is present"
            )

        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        # Setup 2.4 GHz AP.
        self.setup_ap(
            ssid,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            wnm_features=wnm_features,
        )

        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        # Verify that DUT is actually associated (as seen from AP).
        client_mac = self._get_client_mac()
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT is not associated on the 2.4GHz band",
        )

        # Setup 5 GHz AP with same SSID.
        self.setup_ap(
            ssid,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            wnm_features=wnm_features,
        )

        # Construct a BTM request.
        dest_bssid = self.access_point.get_bssid_from_ssid(
            ssid,
            hostapd_constants.BandType.BAND_5G,
        )
        dest_bssid_info = BssidInformation(
            security=True, capabilities=BssidInformationCapabilities()
        )
        neighbor_5g_ap = NeighborReportElement(
            dest_bssid,
            dest_bssid_info,
            operating_class=126,
            channel_number=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            phy_type=PhyType.VHT,
        )
        btm_req = BssTransitionManagementRequest(
            disassociation_imminent=True,
            candidate_list=BssTransitionCandidateList([neighbor_5g_ap]),
        )

        # Send BTM request from 2.4 GHz AP to DUT
        self.access_point.send_bss_transition_management_req(
            self.access_point.wlan_2g, client_mac, btm_req
        )

        # Check that DUT has not roamed.
        ROAM_DEADLINE = datetime.now(timezone.utc) + timedelta(seconds=2)
        while datetime.now(timezone.utc) < ROAM_DEADLINE:
            # Fail if DUT has reassociated to 5 GHz AP (as seen from AP).
            if self.access_point.sta_associated(
                self.access_point.wlan_5g, client_mac
            ):
                raise signals.TestFailure(
                    "DUT unexpectedly roamed to target BSS after BTM request"
                )
            else:
                time.sleep(0.25)

        # DUT should have stayed associated to original AP.
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT unexpectedly lost association on the 2.4GHz band after BTM request",
        )

    def test_btm_req_target_ap_rejects_reassoc(self) -> None:
        if not self.dut.feature_is_present("BSS_TRANSITION_MANAGEMENT"):
            raise signals.TestSkip(
                "skipping test because BTM feature is not present"
            )

        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        # Setup 2.4 GHz AP.
        self.setup_ap(
            ssid,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            wnm_features=wnm_features,
        )

        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        # Verify that DUT is actually associated (as seen from AP).
        client_mac = self._get_client_mac()
        asserts.assert_true(
            self.access_point.sta_associated(
                self.access_point.wlan_2g, client_mac
            ),
            "DUT is not associated on the 2.4GHz band",
        )

        # Setup 5 GHz AP with same SSID, but reject all STAs.
        reject_all_sta_param = {"max_num_sta": 0}
        self.setup_ap(
            ssid,
            additional_ap_parameters=reject_all_sta_param,
            channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            wnm_features=wnm_features,
        )

        # Construct a BTM request.
        dest_bssid = self.access_point.get_bssid_from_ssid(
            ssid,
            hostapd_constants.BandType.BAND_5G,
        )
        dest_bssid_info = BssidInformation(
            security=True, capabilities=BssidInformationCapabilities()
        )
        neighbor_5g_ap = NeighborReportElement(
            dest_bssid,
            dest_bssid_info,
            operating_class=116,
            channel_number=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            phy_type=PhyType.VHT,
        )
        btm_req = BssTransitionManagementRequest(
            disassociation_imminent=True,
            candidate_list=BssTransitionCandidateList([neighbor_5g_ap]),
        )

        # Sleep to avoid concurrent scan during reassociation, necessary due to a firmware bug.
        # TODO(fxbug.dev/42068735) Remove when fixed, or when non-firmware BTM support is merged.
        time.sleep(5)

        # Send BTM request from 2.4 GHz AP to DUT
        self.access_point.send_bss_transition_management_req(
            self.access_point.wlan_2g, client_mac, btm_req
        )

        # Check that DUT has not reassociated.
        ROAM_DEADLINE = datetime.now(timezone.utc) + timedelta(seconds=2)
        while datetime.now(timezone.utc) < ROAM_DEADLINE:
            # Check that DUT has not reassociated to 5 GHz AP (as seen from AP).
            if self.access_point.sta_associated(
                self.access_point.wlan_5g, client_mac
            ):
                raise signals.TestFailure(
                    "DUT unexpectedly roamed to 5GHz band"
                )
            else:
                time.sleep(0.25)


if __name__ == "__main__":
    test_runner.main()
