# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
from datetime import timedelta

from fuchsia_base_test import fuchsia_base_test
from honeydew.fuchsia_device import fuchsia_device
from honeydew.utils import control_flows, power
from honeydew.utils.deadline import Deadline
from mobly.asserts import assert_equal, assert_less

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SuspendResumeTestSuite(fuchsia_base_test.FuchsiaBaseTest):
    """Suite of tests for suspend and resume."""

    dut: fuchsia_device.FuchsiaDevice

    def setup_class(self) -> None:
        super().setup_class()
        self.dut = self.fuchsia_devices[0]
        assert isinstance(self.dut, fuchsia_device.FuchsiaDevice)

        # TODO(https://fxbug.dev/486154863): It's weird that we're calling
        # this private function, but that's the only way to do it at the
        # moment.
        usb_power_hub, usb_port = self._lookup_usb_power_hub(self.dut)
        self.dut.set_usb_power_hub(usb_power_hub, usb_port)

    def test_suspend_resume(self) -> None:
        power.suspend_resume(
            self.dut, Deadline.from_timeout(timedelta(minutes=1))
        )

    def test_no_suspend_on_usb(self) -> None:
        before_on_usb_idle_stats = power.get_sag_suspend_stats(self.dut)

        # Then, idle a bit while plugged in to make sure we _don't_ suspend.
        control_flows.sleep_for_duration(timedelta(seconds=60))

        while_on_usb_stats = (
            power.get_sag_suspend_stats(self.dut) - before_on_usb_idle_stats
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
