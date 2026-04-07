# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import pathlib
from datetime import timedelta
from typing import Any, Callable

import fuchsia_base_test
from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from honeydew.utils import control_flows, power
from honeydew.utils.deadline import Deadline
from mobly.asserts import assert_equal, assert_less

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SuspendResumeTestCases(fuchsia_base_test.FuchsiaTestCases):
    """Test cases for suspend and resume."""

    async def test_suspend_resume(self) -> None:
        await power.suspend_resume(
            self.dut, Deadline.from_timeout(timedelta(minutes=1))
        )

    async def test_no_suspend_on_usb(self) -> None:
        before_on_usb_idle_stats = await power.get_sag_suspend_stats(self.dut)

        # Then, idle a bit while plugged in to make sure we _don't_ suspend.
        await control_flows.sleep_for_duration(timedelta(seconds=60))

        while_on_usb_stats = (
            await power.get_sag_suspend_stats(self.dut)
            - before_on_usb_idle_stats
        )

        _LOGGER.info(
            f"Suspend stats during on-charger idle: \n{while_on_usb_stats}"
        )
        assert_equal(
            while_on_usb_stats.success_count,
            0,
            "SAG must not suspend during idle",
        )

        # NOTE(hjfreyer): These checks are meant to detect situations where the device sits in a
        # suspend attempt loop, but doesn't actually suspend. Checking that there were *no* attempts
        # to suspend seems like it could be too harsh and lead to flakes... but the threshold here
        # hasn't been tuned at all.
        assert_less(
            while_on_usb_stats.fail_count,
            10,
            "SAG attempted to suspend too many times while on USB",
        )
