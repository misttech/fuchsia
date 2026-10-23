# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests of various (sometimes hardcoded) properties of ifaces.
"""

import struct

import fuchsia_wlan_base_test
import wlanix_testing
from mobly import test_runner
from mobly.asserts import assert_equal


class IfaceCorrectnessAndConsistencyTest(
    fuchsia_wlan_base_test.FuchsiaWlanBaseTest
):
    wlanix: wlanix_testing.Wlanix
    chip: wlanix_testing.WifiChip

    async def setup_class(self) -> None:
        await super().setup_class()
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.chip.destroy_all_sta_ifaces()

    async def teardown_test(self) -> None:
        await self.chip.destroy_all_sta_ifaces()
        await super().teardown_test()

    # TODO(https://fxbug.dev/368005870): Need to reconsider the consequences of using
    # the same iface name, even when an iface is recreated.
    async def test_iface_name_hardcoded_as_wlan(self) -> None:
        iface = await self.chip.create_sta_iface()
        assert_equal(
            await iface.get_name(),
            "wlan",
            'WifiStaIface should always return the hardcoded iface name "wlan"',
        )

    async def test_iface_name_consistency(self) -> None:
        iface = await self.chip.create_sta_iface()
        iface_names = await self.chip.get_sta_iface_names()
        assert_equal(
            len(iface_names),
            1,
            "WifiChip should have returned the iface just created",
        )
        assert_equal(
            iface_names[0],
            await iface.get_name(),
            "WifiStaIface returns a different name than WifiChip",
        )

    async def test_iface_nl80211_fields(self) -> None:
        """Verifies that NL80211_CMD_GET_INTERFACE returns correct fields."""
        iface = await self.chip.create_sta_iface()
        attrs = await iface.query_nl80211_attrs()

        # Verify fields
        ifIndex = struct.unpack(
            "<I", attrs[wlanix_testing.NL80211_ATTR_IFINDEX]
        )[0]
        assert ifIndex >= 0, f"Invalid ifIndex: {ifIndex}"

        wiphyIndex = struct.unpack(
            "<I", attrs[wlanix_testing.NL80211_ATTR_WIPHY]
        )[0]
        assert wiphyIndex >= 0, f"Invalid wiphyIndex: {wiphyIndex}"

        ifName = (
            attrs[wlanix_testing.NL80211_ATTR_IFNAME]
            .decode("utf-8")
            .rstrip("\x00")
        )
        assert_equal(ifName, "wlan", 'ifName should be "wlan"')

        macAddress = attrs[wlanix_testing.NL80211_ATTR_MAC]
        assert_equal(
            len(macAddress), 6, f"MAC address length invalid: {len(macAddress)}"
        )


if __name__ == "__main__":
    test_runner.main()
