# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for Android Packet Filter (APF) support.
"""

import logging

logger = logging.getLogger(__name__)

import fidl_fuchsia_wlan_device_service as fidl_device_svc
import fidl_fuchsia_wlan_sme as fidl_sme
from honeydew.typing.custom_types import FidlEndpoint
from mobly import test_runner
from mobly.asserts import (
    assert_equal,
    assert_false,
    assert_is_not_none,
    assert_true,
)
from wlanix_testing import base_test


class ApfTest(base_test.ConnectionBaseTestClass):
    async def setup_test(self) -> None:
        await super().setup_test()

        device_monitor_proxy = fidl_device_svc.DeviceMonitorClient(
            self.fuchsia_device.fuchsia_controller.connect_device_proxy(
                FidlEndpoint(
                    "core/wlandevicemonitor",
                    "fuchsia.wlan.device.service.DeviceMonitor",
                )
            )
        )
        proxy, server = self.fuchsia_device.fuchsia_controller.channel_create()

        # Find the interface ID for the client SME
        list_ifaces_response = await device_monitor_proxy.list_ifaces()
        # Assuming the first interface found is the one we're interested in
        iface_id = list_ifaces_response.iface_list[0]
        assert_is_not_none(iface_id, "Could not find a client interface.")

        # Get a reference to the client SME.
        (
            await device_monitor_proxy.get_client_sme(
                iface_id=iface_id,
                sme_server=server.take(),
            )
        )
        self.client_sme = fidl_sme.ClientSmeClient(proxy)

    async def test_get_apf_packet_filter_support(self) -> None:
        """Tests that APF support information can be retrieved."""
        response = (
            await self.wifi_sta_iface_proxy.get_apf_packet_filter_support()
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
        # Precondition: check APF is not already enabled
        response = (
            await self.client_sme.get_apf_packet_filter_enabled()
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")

        # For testing purposes, we just want to ensure the FIDL call succeeds.
        program = [0x01, 0x02, 0x03, 0x04]
        (
            await self.wifi_sta_iface_proxy.install_apf_packet_filter(
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
        support_response = (
            await self.wifi_sta_iface_proxy.get_apf_packet_filter_support()
        ).unwrap()
        max_filter_length = support_response.max_filter_length

        response = (
            await self.wifi_sta_iface_proxy.read_apf_packet_filter_data()
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
            (
                await self.wifi_sta_iface_proxy.install_apf_packet_filter(
                    program=program
                )
            )
        ).unwrap()

        # Check that the filter is not already enabled
        response = (
            (await self.client_sme.get_apf_packet_filter_enabled())
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")

        # Enable suspend mode
        (
            (
                await self.supplicant_sta_iface_proxy.set_suspend_mode_enabled(
                    enable=True
                )
            )
        ).unwrap()

        # Check if APF is enabled using the SME APIs
        response = (
            (await self.client_sme.get_apf_packet_filter_enabled())
        ).unwrap()
        assert_true(response.enabled, "APF should be enabled")

        # Disable suspend mode
        (
            (
                await self.supplicant_sta_iface_proxy.set_suspend_mode_enabled(
                    enable=False
                )
            )
        ).unwrap()

        # Check that the filter is no longer enabled
        response = (
            (await self.client_sme.get_apf_packet_filter_enabled())
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")

        # Ensure other power save modes don't result in APF enablement
        (
            (await self.supplicant_sta_iface_proxy.set_power_save(enable=True))
        ).unwrap()

        # Check that the filter is still not enabled
        response = (
            (await self.client_sme.get_apf_packet_filter_enabled())
        ).unwrap()
        assert_false(response.enabled, "APF should not be enabled")


if __name__ == "__main__":
    test_runner.main()
