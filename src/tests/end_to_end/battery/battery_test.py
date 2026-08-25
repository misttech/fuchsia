#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Battery End-to-End Test."""

import logging

import fuchsia_base_test
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class BatteryTest(fuchsia_base_test.FuchsiaBaseTest):
    """Test battery affordance get_status."""

    async def setup_test(self) -> None:
        await super().setup_test()
        self.battery = self.dut.battery

    async def test_get_status(self) -> None:
        """Verify reading battery telemetry status via get_status."""
        _LOGGER.info("Calling get_status on %s", self.dut.device_name)
        status = await self.battery.get_status()
        _LOGGER.info("Battery status received: %s", status)
        asserts.assert_is_not_none(
            status, msg="Battery status should not be None"
        )


if __name__ == "__main__":
    test_runner.main()
