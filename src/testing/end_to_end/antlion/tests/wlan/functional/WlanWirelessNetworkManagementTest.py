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
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
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
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord
from openwrt_access_point import Radio, StationStatus
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    Band,
    BssSettings,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWep,
    SecurityWpa,
    SecurityWpa2,
    SecurityWpa3,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)
from openwrt_access_point.lib.extended_capabilities import ExtendedCapabilities
from openwrt_access_point.lib.uci_bss_options import UciBssOptions


@dataclass
class TestParams:
    security: Security


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

        SECURITY_MODES: tuple[Security, ...] = (
            SecurityOpen(),
            SecurityWep(),
            SecurityWpa(),
            SecurityWpa2(),
            SecurityWpa3(),
        )
        for security in SECURITY_MODES:
            test_args.append(
                (
                    TestParams(
                        security=security,
                    ),
                )
            )

        def generate_roam_on_btm_req_test_name(test: TestParams) -> str:
            return f"test_roam_on_btm_req_from_{test.security.uci_encryption}_2g_to_{test.security.uci_encryption}_5g"

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

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

    def get_single_sta_status(self, mac: str, band: Band) -> StationStatus:
        """Gets station status and asserts there is only one interface."""
        assert self.openwrt_ap is not None, "openwrt_ap is not initialized"
        sta_dict = self.openwrt_ap.get_sta_status(mac, band)
        assert (
            len(sta_dict) == 1
        ), f"Expected station on exactly one interface, but found: {list(sta_dict.keys())}"
        return list(sta_dict.values())[0]

    def get_single_sta_ext_capabilities(
        self, mac: str, band: Band
    ) -> ExtendedCapabilities:
        """Gets extended capabilities and asserts there is only one interface."""
        assert self.openwrt_ap is not None, "openwrt_ap is not initialized"
        ext_caps_dict = self.openwrt_ap.get_sta_extended_capabilities(mac, band)
        assert (
            len(ext_caps_dict) == 1
        ), f"Expected station on exactly one interface, but found: {list(ext_caps_dict.keys())}"
        return list(ext_caps_dict.values())[0]

    def teardown_class(self) -> None:
        if self.access_point:
            self.access_point.stop_all_aps()
        super().teardown_class()

    def teardown_test(self) -> None:
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()
        super().teardown_test()

    def on_fail(self, record: TestResultRecord) -> None:
        if self.access_point:
            self.access_point.stop_all_aps()
        super().on_fail(record)

    def setup_ap(
        self,
        ssid: str,
        security: DeprecatedSecurity | None = None,
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
        assert self.access_point is not None
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
            result = self.fuchsia_device.honeydew_fd.wlan_core_deprecated_sync.query_iface(
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

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        if self.openwrt_ap:
            channel = DEFAULT_2G_CHANNEL
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            self.setup_ap(ssid, wnm_features=wnm_features)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")
        client_mac = self._get_client_mac()
        # Verify that DUT is actually associated (as seen from AP).

        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc,
                "DUT is not associated on the 2.4GHz band",
            )
            ext_capabilities = self.get_single_sta_ext_capabilities(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_false(
                ext_capabilities.bss_transition,
                "DUT is incorrectly advertising BSS Transition Management support",
            )
        elif self.access_point:
            asserts.assert_true(
                self.access_point.sta_associated(
                    self.access_point.wlan_2g, client_mac
                ),
                "DUT is not associated on the 2.4GHz band",
            )
            legacy_ext_capabilities = (
                self.access_point.get_sta_extended_capabilities(
                    self.access_point.wlan_2g, client_mac
                )
            )
            asserts.assert_false(
                legacy_ext_capabilities.bss_transition,
                "DUT is incorrectly advertising BSS Transition Management support",
            )

    def test_bss_transition_is_advertised_when_ap_supported_dut_supported(
        self,
    ) -> None:
        if not self.dut.feature_is_present("BSS_TRANSITION_MANAGEMENT"):
            raise signals.TestSkip(
                "skipping test because BTM feature is not present"
            )

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )

        if self.openwrt_ap:
            channel = DEFAULT_2G_CHANNEL
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
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
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc, "DUT is not associated on the 2.4GHz band"
            )
            ext_capabilities = self.get_single_sta_ext_capabilities(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                ext_capabilities.bss_transition,
                "DUT is not advertising BSS Transition Management support",
            )
        elif self.access_point:
            asserts.assert_true(
                self.access_point.sta_associated(
                    self.access_point.wlan_2g, client_mac
                ),
                "DUT is not associated on the 2.4GHz band",
            )
            legacy_ext_capabilities = (
                self.access_point.get_sta_extended_capabilities(
                    self.access_point.wlan_2g, client_mac
                )
            )
            asserts.assert_true(
                legacy_ext_capabilities.bss_transition,
                "DUT is not advertising BSS Transition Management support",
            )

    def test_wnm_sleep_mode_is_not_advertised_when_ap_supported_dut_unsupported(
        self,
    ) -> None:
        if self.dut.feature_is_present("WNM_SLEEP_MODE"):
            raise signals.TestSkip(
                "skipping test because WNM feature is present"
            )

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        wnm_features = frozenset([hostapd_constants.WnmFeature.WNM_SLEEP_MODE])
        if self.openwrt_ap:
            channel = DEFAULT_2G_CHANNEL
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    wnm_sleep_mode=True
                                ),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            self.setup_ap(ssid, wnm_features=wnm_features)
        asserts.assert_true(
            self.dut.associate(ssid, SecurityMode.OPEN),
            "Failed to associate.",
        )
        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")
        client_mac = self._get_client_mac()
        # Verify that DUT is actually associated (as seen from AP).
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc, "DUT is not associated on the 2.4GHz band"
            )
            ext_capabilities = self.get_single_sta_ext_capabilities(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_false(
                ext_capabilities.wnm_sleep_mode,
                "DUT is incorrectly advertising WNM Sleep Mode support",
            )
        elif self.access_point:
            asserts.assert_true(
                self.access_point.sta_associated(
                    self.access_point.wlan_2g, client_mac
                ),
                "DUT is not associated on the 2.4GHz band",
            )
            legacy_ext_capabilities = (
                self.access_point.get_sta_extended_capabilities(
                    self.access_point.wlan_2g, client_mac
                )
            )
            asserts.assert_false(
                legacy_ext_capabilities.wnm_sleep_mode,
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

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        password = None
        if not isinstance(test.security, SecurityOpen):
            # Length 13, so it can be used for WEP or WPA
            password = AccessPointConfig.random_string(13)

        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )

        # Setup 2.4 GHz AP.
        legacy_security = ConfigMapper.to_hostapd_security(test.security)
        security = DeprecatedSecurity(legacy_security, password)
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        # Setup 2.4 GHz AP.
        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=test.security,
                                password=password,
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    ),
                    RadioConfig(
                        channel=DEFAULT_5G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=test.security,
                                password=password,
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    ),
                ]
            )
            self.openwrt_ap.configure_wifi(config)
            # Disable 5 GHz radio immediately so client connects to 2G
            self.openwrt_ap.disable_radio(Radio.RADIO_5G)
        elif self.access_point:
            self.setup_ap(
                ssid,
                security=security,
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                wnm_features=wnm_features,
            )

        asserts.assert_true(
            self.dut.associate(
                ssid,
                target_pwd=password,
                target_security=ConfigMapper.to_hostapd_security(test.security),
            ),
            "Failed to associate.",
        )
        # Verify that DUT is actually associated (as seen from AP).
        client_mac = self._get_client_mac()
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc, "DUT is not associated on the 2.4GHz band"
            )
        elif self.access_point:
            asserts.assert_true(
                self.access_point.sta_associated(
                    self.access_point.wlan_2g, client_mac
                ),
                "DUT is not associated on the 2.4GHz band",
            )

        # Setup 5 GHz AP with same SSID.
        if self.openwrt_ap:
            self.openwrt_ap.enable_radio(Radio.RADIO_5G)
        elif self.access_point:
            self.setup_ap(
                ssid,
                security=security,
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                wnm_features=wnm_features,
            )

        # Construct a BTM request.
        if self.openwrt_ap:
            dest_bssid = self.openwrt_ap.get_bssid_from_ssid(ssid, Band.BAND_5G)
        else:
            assert self.access_point is not None
            dest_bssid = self.access_point.get_bssid_from_ssid(
                ssid, hostapd_constants.BandType.BAND_5G
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
        if self.openwrt_ap:
            self.openwrt_ap.send_bss_transition_management_req(
                client_mac, band=Band.BAND_2G, btm_req=btm_req
            )
        else:
            assert self.access_point is not None
            self.access_point.send_bss_transition_management_req(
                self.access_point.wlan_2g, client_mac, btm_req
            )

        # Give DUT time to roam.
        ROAM_DEADLINE = datetime.now(timezone.utc) + timedelta(seconds=2)
        while datetime.now(timezone.utc) < ROAM_DEADLINE:
            if self.openwrt_ap:
                sta_status = next(
                    iter(
                        self.openwrt_ap.get_sta_status(
                            client_mac, band=Band.BAND_5G
                        ).values()
                    ),
                    StationStatus(auth=False, assoc=False, authorized=False),
                )
                if sta_status.authorized:
                    break
                else:
                    time.sleep(0.25)
            elif self.access_point:
                if self.access_point.sta_authorized(
                    self.access_point.wlan_5g, client_mac
                ):
                    break
                else:
                    time.sleep(0.25)

        # Verify that DUT roamed (as seen from AP).
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_5G
            )
            asserts.assert_true(
                sta_status.auth, "DUT is not authenticated on the 5GHz band"
            )
            asserts.assert_true(
                sta_status.assoc, "DUT is not associated on the 5GHz band"
            )
            asserts.assert_true(
                sta_status.authorized,
                "DUT is not 802.1X authorized on the 5GHz band",
            )
        elif self.access_point:
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

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        # Setup 2.4 GHz AP.
        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    ),
                    RadioConfig(
                        channel=DEFAULT_5G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    ),
                ]
            )
            self.openwrt_ap.configure_wifi(config)
            # Disable 5 GHz radio immediately so client connects to 2G
            self.openwrt_ap.disable_radio(Radio.RADIO_5G)
        elif self.access_point:
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
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc, "DUT is not associated on the 2.4GHz band"
            )
        elif self.access_point:
            asserts.assert_true(
                self.access_point.sta_associated(
                    self.access_point.wlan_2g, client_mac
                ),
                "DUT is not associated on the 2.4GHz band",
            )

        # Setup 5 GHz AP with same SSID.
        if self.openwrt_ap:
            self.openwrt_ap.enable_radio(Radio.RADIO_5G)
        elif self.access_point:
            self.setup_ap(
                ssid,
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                wnm_features=wnm_features,
            )

        # Construct a BTM request.
        if self.openwrt_ap:
            dest_bssid = self.openwrt_ap.get_bssid_from_ssid(ssid, Band.BAND_5G)
        else:
            assert self.access_point is not None
            dest_bssid = self.access_point.get_bssid_from_ssid(
                ssid, hostapd_constants.BandType.BAND_5G
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
        if self.openwrt_ap:
            self.openwrt_ap.send_bss_transition_management_req(
                client_mac, band=Band.BAND_2G, btm_req=btm_req
            )
        else:
            assert self.access_point is not None
            self.access_point.send_bss_transition_management_req(
                self.access_point.wlan_2g, client_mac, btm_req
            )

        # Check that DUT has not roamed.
        ROAM_DEADLINE = datetime.now(timezone.utc) + timedelta(seconds=2)
        while datetime.now(timezone.utc) < ROAM_DEADLINE:
            # Fail if DUT has reassociated to 5 GHz AP (as seen from AP).
            if self.openwrt_ap:
                sta_status = next(
                    iter(
                        self.openwrt_ap.get_sta_status(
                            client_mac, band=Band.BAND_5G
                        ).values()
                    ),
                    StationStatus(auth=False, assoc=False, authorized=False),
                )
                if sta_status.authorized:
                    raise signals.TestFailure(
                        "DUT unexpectedly roamed to target BSS after BTM request"
                    )
                else:
                    time.sleep(0.25)
            elif self.access_point:
                if self.access_point.sta_associated(
                    self.access_point.wlan_5g, client_mac
                ):
                    raise signals.TestFailure(
                        "DUT unexpectedly roamed to target BSS after BTM request"
                    )
                else:
                    time.sleep(0.25)

        # DUT should have stayed associated to original AP.
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc,
                "DUT unexpectedly lost association on the 2.4GHz band after BTM request",
            )
        elif self.access_point:
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

        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        wnm_features = frozenset(
            [hostapd_constants.WnmFeature.BSS_TRANSITION_MANAGEMENT]
        )
        # Setup 2.4 GHz AP.
        if self.openwrt_ap:
            channel = DEFAULT_2G_CHANNEL
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
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
        if self.openwrt_ap:
            sta_status = self.get_single_sta_status(
                client_mac, band=Band.BAND_2G
            )
            asserts.assert_true(
                sta_status.assoc, "DUT is not associated on the 2.4GHz band"
            )
        elif self.access_point:
            asserts.assert_true(
                self.access_point.sta_associated(
                    self.access_point.wlan_2g, client_mac
                ),
                "DUT is not associated on the 2.4GHz band",
            )

        # Setup 5 GHz AP with same SSID, but reject all STAs.
        reject_all_sta_param = {"max_num_sta": 0}
        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=DEFAULT_2G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                    ),
                    RadioConfig(
                        channel=DEFAULT_5G_CHANNEL,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=SecurityOpen(),
                                custom_uci_options=UciBssOptions(
                                    bss_transition=True
                                ),
                            )
                        ],
                        custom_hostapd_options={"max_num_sta": 0},  # type: ignore
                    ),
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            self.setup_ap(
                ssid,
                additional_ap_parameters=reject_all_sta_param,
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                wnm_features=wnm_features,
            )

        # Construct a BTM request.
        if self.openwrt_ap:
            dest_bssid = self.openwrt_ap.get_bssid_from_ssid(ssid, Band.BAND_5G)
        else:
            assert self.access_point is not None
            dest_bssid = self.access_point.get_bssid_from_ssid(
                ssid, hostapd_constants.BandType.BAND_5G
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
        if self.openwrt_ap:
            self.openwrt_ap.send_bss_transition_management_req(
                client_mac, band=Band.BAND_2G, btm_req=btm_req
            )
        else:
            assert self.access_point is not None
            self.access_point.send_bss_transition_management_req(
                self.access_point.wlan_2g, client_mac, btm_req
            )

        # Check that DUT has not reassociated.
        ROAM_DEADLINE = datetime.now(timezone.utc) + timedelta(seconds=2)
        while datetime.now(timezone.utc) < ROAM_DEADLINE:
            # Check that DUT has not reassociated to 5 GHz AP (as seen from AP).
            if self.openwrt_ap:
                sta_status = self.get_single_sta_status(
                    client_mac, band=Band.BAND_5G
                )
                if sta_status.assoc:
                    raise signals.TestFailure(
                        "DUT unexpectedly roamed to 5GHz band"
                    )
                else:
                    time.sleep(0.25)
            elif self.access_point:
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
