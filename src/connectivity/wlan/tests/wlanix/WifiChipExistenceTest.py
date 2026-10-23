# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Test that the device has at least one WifiChip.
"""

import fuchsia_wlan_base_test
import wlanix_testing
from mobly import test_runner
from mobly.asserts import assert_greater


class WifiChipExistenceTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    wlanix: wlanix_testing.Wlanix

    async def setup_class(self) -> None:
        await super().setup_class()
        self.wlanix = wlanix_testing.Wlanix(self.dut)

    async def test_get_chip_ids(self) -> None:
        chip_ids = await self.wlanix.get_chip_ids()
        assert_greater(len(chip_ids), 0)


if __name__ == "__main__":
    test_runner.main()
