#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Battery End-to-End Test."""

import logging

import fuchsia_base_test
from honeydew.affordances.drivers.battery import ChargeStatus
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class BatteryTest(fuchsia_base_test.FuchsiaBaseTest):
    """Test battery and fake battery affordances."""

    async def setup_test(self) -> None:
        await super().setup_test()
        self.battery = self.dut.battery
        self.fake_battery = self.dut.fake_battery

    async def teardown_test(self) -> None:
        try:
            self.fake_battery.set_charging()
        except Exception as err:
            _LOGGER.warning(
                "Failed to reset fake battery state in teardown: %s", err
            )
        await super().teardown_test()

    async def test_get_status(self) -> None:
        """Verify reading battery telemetry status via get_status."""
        _LOGGER.info("Calling get_status on %s", self.dut.device_name)
        status = await self.battery.get_status()
        _LOGGER.info("Battery status received: %s", status)
        asserts.assert_is_not_none(
            status, msg="Battery status should not be None"
        )

    async def test_set_discharging_status(self) -> None:
        """Verify setting battery charge status to discharging via fake battery and verifying with get_status."""
        _LOGGER.info(
            "Setting fake battery charge status to discharging on %s",
            self.dut.device_name,
        )
        self.fake_battery.set_discharging()

        _LOGGER.info(
            "Verifying battery status via get_status on %s",
            self.dut.device_name,
        )
        status = await self.battery.get_status()
        _LOGGER.info("Battery status received: %s", status)
        asserts.assert_equal(
            status.charge_status,
            ChargeStatus.DISCHARGING,
            f"Expected battery charge status to be DISCHARGING, got {status.charge_status}",
        )


if __name__ == "__main__":
    test_runner.main()
