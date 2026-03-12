# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for SystemPowerStateController affordance."""

import logging

import fuchsia_base_test
from mobly import asserts, test_runner

from honeydew import errors

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SystemPowerStateControllerAffordanceTests(
    fuchsia_base_test.AsyncFuchsiaBaseTest
):
    """SystemPowerStateController affordance tests"""

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        self.device = self.fuchsia_devices[0]

    async def test_idle_suspend_timer_based_resume(self) -> None:
        """Test case for SystemPowerStateController.idle_suspend_timer_based_resume()"""
        if self.user_params["is_starnix_supported"]:
            await self.device.system_power_state_controller.idle_suspend_timer_based_resume(
                duration=3
            )
        else:
            with asserts.assert_raises(errors.NotSupportedError):
                await self.device.system_power_state_controller.idle_suspend_timer_based_resume(
                    duration=3,
                )


if __name__ == "__main__":
    test_runner.main()
