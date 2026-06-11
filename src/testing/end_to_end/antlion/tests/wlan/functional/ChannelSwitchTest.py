#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests STA handling of channel switch announcements.
"""

import logging
import random
import time
from typing import Sequence

import fidl_fuchsia_wlan_common as f_wlan_common
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.utils import rand_ascii_str
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanError,
)
from honeydew.affordances.connectivity.wlan.utils.types import (
    ClientStatusConnected,
    ConnectivityMode,
    OperatingBand,
    SecurityType,
)
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    HtMode,
    PhyMode,
    RadioConfig,
    SecurityOpen,
    VhtMode,
)
from openwrt_access_point.lib.uci_radio_options import UciRadioOptions

# Number of channel switch announcement beacons to send.
CSA_BEACON_COUNT = 10

# Beacon interval in unit of kus.
BEACON_INTERVAL_KUS = 100

# 1 kus = 1.024ms.
SEC_PER_KUS = 0.001024

US_DFS_CHANNELS = [
    52,
    56,
    60,
    64,
    100,
    104,
    108,
    112,
    116,
    120,
    124,
    128,
    132,
    136,
    140,
    144,
]


class ChannelSwitchTest(base_test.WifiBaseTest):
    # Time to wait between issuing channel switches
    WAIT_BETWEEN_CHANNEL_SWITCHES_S = 15

    # For operating class 115 tests.
    GLOBAL_OPERATING_CLASS_115_CHANNELS = [36, 40, 44, 48]
    # A channel outside the operating class.
    NON_GLOBAL_OPERATING_CLASS_115_CHANNEL = 52

    # For operating class 124 tests.
    GLOBAL_OPERATING_CLASS_124_CHANNELS = [149, 153, 157, 161]
    # A channel outside the operating class.
    NON_GLOBAL_OPERATING_CLASS_124_CHANNEL = 52

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()
        self.ssid = rand_ascii_str(10)

        if self.openwrt_aps:
            self.openwrt_ap = self.openwrt_aps[0]
        elif self.access_points:
            self.access_point = self.access_points[0]
            self.access_point.stop_all_aps()
        else:
            raise signals.TestAbortClass("Requires at least one access point")

        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

    def setup_class(self) -> None:
        super().setup_class()

    def teardown_test(self) -> None:
        self.dut.disconnect()
        self.dut.reset_wifi()
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()
        try:
            self.fuchsia_device.honeydew_fd.wlan_policy_ap_deprecated_sync.stop_all()
        except HoneydewWlanError as e:
            # This is expected for devices without soft-AP support.
            self.log.info("Failed to stop soft APs: %s", e)
        super().teardown_test()

    def channel_switch(
        self,
        band: Band,
        starting_channel: int,
        channel_switches: Sequence[int],
        test_with_soft_ap: bool = False,
    ) -> None:
        """Setup and run a channel switch test with the given parameters.

        Creates an AP, associates to it, and then issues channel switches
        through the provided channels. After each channel switch, the test
        checks that the DUT is connected for a period of time before considering
        the channel switch successful. If directed to start a SoftAP, the test
        will also check that the SoftAP is on the expected channel after each
        channel switch.

        Args:
            band: band that AP will use
            starting_channel: channel number that AP will use at startup
            channel_switches: ordered list of channels that the test will
                attempt to switch to
            test_with_soft_ap: whether to start a SoftAP before beginning the
                channel switches (default is False); note that if a SoftAP is
                started, the test will also check that the SoftAP handles
                channel switches correctly
        """
        current_channel = starting_channel

        phy_mode: PhyMode
        match band:
            case Band.BAND_2G:
                wlan_band = Band.BAND_2G
                phy_mode = HtMode(bw=20)
                if self.openwrt_ap:
                    ap_iface = self.openwrt_ap.wlan_2g_interface
                elif self.access_point:
                    ap_iface = self.access_point.wlan_2g
                else:
                    raise signals.TestAbortClass("No access point initialized")
            case Band.BAND_5G:
                wlan_band = Band.BAND_5G
                phy_mode = VhtMode(bw=20)
                if self.openwrt_ap:
                    ap_iface = self.openwrt_ap.wlan_5g_interface
                elif self.access_point:
                    ap_iface = self.access_point.wlan_5g
                else:
                    raise signals.TestAbortClass("No access point initialized")

        asserts.assert_true(
            self._channels_valid_for_band([current_channel], band),
            (
                f"starting channel {current_channel} not a valid channel for band {band}"
            ),
        )
        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=BssChannel(
                            band=wlan_band,
                            number=current_channel,
                            phy_mode=phy_mode,
                        ),
                        custom_uci_options=UciRadioOptions(
                            beacon_int=BEACON_INTERVAL_KUS
                        ),
                        bss_settings=[
                            BssSettings(
                                ssid=self.ssid,
                                security=SecurityOpen(),
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=current_channel,
                ssid=self.ssid,
                beacon_interval=BEACON_INTERVAL_KUS,
                # Antlion channel_switch currently only supports 20 MHz.
                vht_bandwidth=20,
            )

        if test_with_soft_ap:
            self._start_soft_ap()
        self.log.info("sending associate command for ssid %s", self.ssid)
        self.dut.associate(self.ssid, SecurityMode.OPEN)
        asserts.assert_true(self.dut.is_connected(), "Failed to connect.")

        asserts.assert_true(
            channel_switches, "Cannot run test, no channels to switch to"
        )
        asserts.assert_true(
            self._channels_valid_for_band(channel_switches, band),
            (
                f"channel_switches {channel_switches} includes invalid channels "
                f"for band {band}"
            ),
        )

        for channel_num in channel_switches:
            if channel_num == current_channel:
                continue
            # TODO(b/504795188): Support switching to DFS channels.
            if channel_num in US_DFS_CHANNELS:
                self.log.info(f"Skipping DFS channel {channel_num}")
                continue
            self.log.info(f"channel switch: {current_channel} -> {channel_num}")
            if self.openwrt_ap:
                self.openwrt_ap.channel_switch(
                    ap_iface, channel_num, CSA_BEACON_COUNT
                )
                channel_num_after_switch = self.openwrt_ap.get_current_channel(
                    ap_iface
                )
            else:
                assert self.access_point is not None
                self.access_point.channel_switch(
                    ap_iface, channel_num, CSA_BEACON_COUNT
                )
                channel_num_after_switch = (
                    self.access_point.get_current_channel(ap_iface)
                )

            asserts.assert_equal(
                channel_num_after_switch,
                channel_num,
                "AP failed to channel switch",
            )
            previous_channel = current_channel
            current_channel = channel_num

            # Check periodically to see if DUT stays connected. Sometimes
            # CSA-induced disconnects occur seconds after last channel switch.

            change_channel_after = (
                time.time() + self.WAIT_BETWEEN_CHANNEL_SWITCHES_S
            )
            must_change_channel_within = (
                BEACON_INTERVAL_KUS * SEC_PER_KUS * CSA_BEACON_COUNT
            )
            must_change_channel_by = time.time() + must_change_channel_within

            while time.time() < change_channel_after:
                status = (
                    self.fuchsia_device.honeydew_fd.wlan_core_deprecated_sync.status()
                )
                if not isinstance(status, ClientStatusConnected):
                    raise signals.TestFailure(
                        f"want ClientStatusConnected, got {type(status)} after "
                        f"switching from channel {previous_channel} to "
                        f"channel {current_channel}"
                    )

                got_channel = status.channel.primary

                if got_channel == previous_channel:
                    asserts.assert_less(
                        time.time(),
                        must_change_channel_by,
                        "expected channel to switch from channel "
                        f"{previous_channel} to {current_channel} "
                        f"within {must_change_channel_within:.2}s",
                    )
                    time.sleep(0.1)
                    continue

                asserts.assert_equal(
                    got_channel,
                    current_channel,
                    f"want channel={current_channel}, got {got_channel}",
                )
                if test_with_soft_ap:
                    soft_ap_channel = self._soft_ap_channel()
                    asserts.assert_equal(
                        soft_ap_channel,
                        channel_num,
                        f"SoftAP interface on wrong channel ({soft_ap_channel})",
                    )
                time.sleep(1)

    def test_channel_switch_2g(self) -> None:
        """Channel switch through all (US only) channels in the 2 GHz band."""
        self.channel_switch(
            band=Band.BAND_2G,
            starting_channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            channel_switches=hostapd_constants.US_CHANNELS_2G,
        )

    def test_channel_switch_2g_with_soft_ap(self) -> None:
        """Channel switch through (US only) 2 Ghz channels with SoftAP up."""
        self.channel_switch(
            band=Band.BAND_2G,
            starting_channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            channel_switches=hostapd_constants.US_CHANNELS_2G,
            test_with_soft_ap=True,
        )

    def test_channel_switch_2g_shuffled_with_soft_ap(self) -> None:
        """Switch through shuffled (US only) 2 Ghz channels with SoftAP up."""
        channels = hostapd_constants.US_CHANNELS_2G
        random.shuffle(channels)
        self.log.info(f"Shuffled channel switch sequence: {channels}")
        self.channel_switch(
            band=Band.BAND_2G,
            starting_channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
            channel_switches=channels,
            test_with_soft_ap=True,
        )

    def test_channel_switch_5g(self) -> None:
        """Channel switch through all (US only) channels in the 5 GHz band."""
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            channel_switches=hostapd_constants.US_CHANNELS_5G,
        )

    def test_channel_switch_5g_with_soft_ap(self) -> None:
        """Channel switch through (US only) 5 GHz channels with SoftAP up."""
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            channel_switches=hostapd_constants.US_CHANNELS_5G,
            test_with_soft_ap=True,
        )

    def test_channel_switch_5g_shuffled_with_soft_ap(self) -> None:
        """Switch through shuffled (US only) 5 Ghz channels with SoftAP up."""
        channels = hostapd_constants.US_CHANNELS_5G
        random.shuffle(channels)
        self.log.info(f"Shuffled channel switch sequence: {channels}")
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            channel_switches=channels,
            test_with_soft_ap=True,
        )

    def test_channel_switch_regression_global_operating_class_115(self) -> None:
        """Channel switch into, through, and out of global op. class 115 channels.

        Global operating class 115 is described in IEEE 802.11-2016 Table E-4.
        Regression test for fxbug.dev/42165602.
        """
        channels = self.GLOBAL_OPERATING_CLASS_115_CHANNELS + [
            self.NON_GLOBAL_OPERATING_CLASS_115_CHANNEL
        ]
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=self.NON_GLOBAL_OPERATING_CLASS_115_CHANNEL,
            channel_switches=channels,
        )

    def test_channel_switch_regression_global_operating_class_115_with_soft_ap(
        self,
    ) -> None:
        """Test global operating class 124 channel switches, with SoftAP.

        Regression test for fxbug.dev/42165602.
        """
        channels = self.GLOBAL_OPERATING_CLASS_115_CHANNELS + [
            self.NON_GLOBAL_OPERATING_CLASS_115_CHANNEL
        ]
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=self.NON_GLOBAL_OPERATING_CLASS_115_CHANNEL,
            channel_switches=channels,
            test_with_soft_ap=True,
        )

    def test_channel_switch_regression_global_operating_class_124(self) -> None:
        """Switch into, through, and out of global op. class 124 channels.

        Global operating class 124 is described in IEEE 802.11-2016 Table E-4.
        Regression test for fxbug.dev/42142868.
        """
        channels = self.GLOBAL_OPERATING_CLASS_124_CHANNELS + [
            self.NON_GLOBAL_OPERATING_CLASS_124_CHANNEL
        ]
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=self.NON_GLOBAL_OPERATING_CLASS_124_CHANNEL,
            channel_switches=channels,
        )

    def test_channel_switch_regression_global_operating_class_124_with_soft_ap(
        self,
    ) -> None:
        """Test global operating class 124 channel switches, with SoftAP.

        Regression test for fxbug.dev/42142868.
        """
        channels = self.GLOBAL_OPERATING_CLASS_124_CHANNELS + [
            self.NON_GLOBAL_OPERATING_CLASS_124_CHANNEL
        ]
        self.channel_switch(
            band=Band.BAND_5G,
            starting_channel=self.NON_GLOBAL_OPERATING_CLASS_124_CHANNEL,
            channel_switches=channels,
            test_with_soft_ap=True,
        )

    def _channels_valid_for_band(
        self, channels: Sequence[int], band: Band
    ) -> bool:
        """Determine if the channels are valid for the band (US only).

        Args:
            channels: channel numbers
            band: a valid band
        """
        channels_set = frozenset(channels)
        match band:
            case Band.BAND_2G:
                band_channels = frozenset(hostapd_constants.US_CHANNELS_2G)
            case Band.BAND_5G:
                band_channels = frozenset(hostapd_constants.US_CHANNELS_5G)
        return channels_set <= band_channels

    def _start_soft_ap(self) -> None:
        """Start a SoftAP on the DUT.

        Raises:
            EnvironmentError: if the SoftAP does not start
        """
        ssid = rand_ascii_str(10)
        self.log.info(f'Starting SoftAP on DUT with ssid "{ssid}"')

        self.fuchsia_device.honeydew_fd.wlan_policy_ap_deprecated_sync.start(
            ssid,
            SecurityType.NONE,
            None,
            ConnectivityMode.LOCAL_ONLY,
            OperatingBand.ANY,
        )
        self.log.info(f"SoftAp network ({ssid}) is up.")

    def _soft_ap_channel(self) -> int:
        """Determine the channel of the DUT SoftAP interface.

        If the interface is not connected, the method will assert a test
        failure.

        Returns: channel number

        Raises:
            EnvironmentError: if SoftAP interface channel cannot be determined.
            signals.TestFailure: when the SoftAP interface is not connected.
        """
        iface_ids = self.dut.get_wlan_interface_id_list()
        for iface_id in iface_ids:
            try:
                result = self.fuchsia_device.honeydew_fd.wlan_core_deprecated_sync.query_iface(
                    iface_id
                )
            except HoneydewWlanError as e:
                self.log.warning(f"Query iface {iface_id} failed: {e}")
                continue
            if result.role == f_wlan_common.WlanMacRole.AP:
                status = (
                    self.fuchsia_device.honeydew_fd.wlan_core_deprecated_sync.status()
                )
                if not isinstance(status, ClientStatusConnected):
                    raise signals.TestFailure(
                        f"want ClientStatusConnected, got {type(status)}"
                    )
                return status.channel.primary
        raise EnvironmentError("Could not determine SoftAP channel")


if __name__ == "__main__":
    test_runner.main()
