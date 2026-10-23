# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Battery of tests of the lifecycle of ifaces managed by wlanix.
"""

import fuchsia_wlan_base_test
import wlanix_testing
from mobly import test_runner
from mobly.asserts import assert_equal, assert_raises


class IfaceLifecycleTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
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

    async def test_create_and_destroy_iface(self) -> None:
        iface_names = await self.chip.get_sta_iface_names()
        assert_equal(
            len(iface_names),
            0,
            "WifiChip should have returned an empty list of iface names",
        )

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

        await iface.remove()

        iface_names = await self.chip.get_sta_iface_names()
        assert_equal(
            len(iface_names),
            0,
            "WifiChip should no longer contain the iface just removed",
        )

        with assert_raises(Exception):
            (await iface.wifi_sta_iface.get_name()).unwrap()


if __name__ == "__main__":
    test_runner.main()
