# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for Android Packet Filter (APF) support.
"""

import logging

logger = logging.getLogger(__name__)

from fuchsia_controller_py.wrappers import AsyncAdapter, asyncmethod
from mobly import test_runner
from mobly.asserts import assert_equal, assert_is_not_none
from wlanix_testing import base_test


class ApfTest(AsyncAdapter, base_test.ConnectionBaseTestClass):
    @asyncmethod
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

    @asyncmethod
    async def test_install_apf_packet_filter(self) -> None:
        """Tests that an APF program can be installed."""
        # A simple APF program (e.g., just returning PASS/ACCEPT)
        # For testing purposes, we just want to ensure the FIDL call succeeds.
        program = [0x01, 0x02, 0x03, 0x04]
        (
            await self.wifi_sta_iface_proxy.install_apf_packet_filter(
                program=program
            )
        ).unwrap()

    @asyncmethod
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


if __name__ == "__main__":
    test_runner.main()
