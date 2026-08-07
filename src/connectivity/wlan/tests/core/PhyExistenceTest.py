# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

from core_testing import base_test
from mobly import test_runner
from mobly.asserts import assert_equal

logger = logging.getLogger(__name__)


class PhyExistenceTest(base_test.CoreBaseTestClass):
    async def test_get_phy_ids(self) -> None:
        list_phys_response = await self.test_kit.device_monitor.list_phys()
        assert (
            list_phys_response.phy_list is not None
        ), "DeviceMonitor.ListPhys() response is missing a phy_list value"
        assert_equal(
            len(list_phys_response.phy_list),
            1,
            "DeviceMonitor.ListPhys() should return exactly one phy_id.",
        )


if __name__ == "__main__":
    test_runner.main()
