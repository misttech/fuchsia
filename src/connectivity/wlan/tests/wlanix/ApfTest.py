# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for Android Packet Filter (APF) support.
"""

import logging

logger = logging.getLogger(__name__)

import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
import wlanix_testing
from mobly import test_runner
from mobly.asserts import (
    assert_equal,
    assert_false,
    assert_is_not_none,
    assert_true,
)


class ApfTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    wlanix: wlanix_testing.Wlanix
    chip: wlanix_testing.WifiChip
    phy: wlan_core.Phy
    iface: wlanix_testing.StaIface

    async def setup_class(self) -> None:
        await super().setup_class()
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()
        self.phy = await self.dut.wlan_core.ensure_single_phy()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.chip.destroy_all_sta_ifaces()
        self.iface = await self.chip.create_sta_iface()

        client_ifaces = await self.phy.get_client_ifaces()
        assert_equal(
            len(client_ifaces),
            1,
            f"Expected exactly 1 client interface on PHY, got {len(client_ifaces)}",
        )
        self.client_sme = client_ifaces[0].client_sme

    async def teardown_test(self) -> None:
        await self.chip.destroy_all_sta_ifaces()
        await super().teardown_test()

    async def test_get_apf_packet_filter_support(self) -> None:
        """Tests that APF support information can be retrieved."""
        response = (
            await self.iface.wifi_sta_iface.get_apf_packet_filter_support()
        ).unwrap()
        assert_is_not_none(
            response.version, "APF support response is missing version"
        )
        assert_is_not_none(
            response.max_filter_length,
            "APF support response is missing max_filter_length",
        )
        logger.info(
            "APF version: %d, max_filter_length: %d",
            response.version,
            response.max_filter_length,
        )

    async def test_install_apf_packet_filter(self) -> None:
        """Tests that an APF program can be installed."""
        # For testing purposes, we just want to ensure the FIDL call succeeds.
        program = [0x01, 0x02, 0x03, 0x04]
        (
            await self.iface.wifi_sta_iface.install_apf_packet_filter(
                program=program
            )
        ).unwrap()

        # Check that installing the packet filter did not enable it
        response = (
            await self.client_sme.get_apf_packet_filter_enabled()
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")

    async def test_read_apf_packet_filter_data(self) -> None:
        """Tests that APF packet filter data can be read back."""
        program = [0x01, 0x02, 0x03, 0x04]
        (
            await self.iface.wifi_sta_iface.install_apf_packet_filter(
                program=program
            )
        ).unwrap()

        support_response = (
            await self.iface.wifi_sta_iface.get_apf_packet_filter_support()
        ).unwrap()
        max_filter_length = support_response.max_filter_length

        response = (
            await self.iface.wifi_sta_iface.read_apf_packet_filter_data()
        ).unwrap()
        assert (
            response.memory is not None
        ), "Read APF data response is missing memory"
        assert_equal(
            len(response.memory),
            max_filter_length,
            "Read APF data length should match max_filter_length",
        )

    async def test_apf_packet_filter_enabled_by_suspend_mode(self) -> None:
        """Tests that APF packet filter is enabled by suspend mode."""
        # Install a placeholder APF program
        program = [0x01, 0x02, 0x03, 0x04]
        (
            await self.iface.wifi_sta_iface.install_apf_packet_filter(
                program=program
            )
        ).unwrap()

        # Check that the filter is not already enabled
        response = (
            await self.client_sme.get_apf_packet_filter_enabled()
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")

        # Enable suspend mode
        (
            await self.iface.supplicant_sta_iface.set_suspend_mode_enabled(
                enable=True
            )
        ).unwrap()

        # Check if APF is enabled using the SME APIs
        response = (
            await self.client_sme.get_apf_packet_filter_enabled()
        ).unwrap()
        assert_true(response.enabled, "APF should be enabled")

        # Disable suspend mode
        (
            await self.iface.supplicant_sta_iface.set_suspend_mode_enabled(
                enable=False
            )
        ).unwrap()

        # Check that the filter is no longer enabled
        response = (
            await self.client_sme.get_apf_packet_filter_enabled()
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")

        # Ensure other power save modes don't result in APF enablement
        (
            await self.iface.supplicant_sta_iface.set_power_save(enable=True)
        ).unwrap()

        # Check that the filter is still not enabled
        response = (
            await self.client_sme.get_apf_packet_filter_enabled()
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")


if __name__ == "__main__":
    test_runner.main()
