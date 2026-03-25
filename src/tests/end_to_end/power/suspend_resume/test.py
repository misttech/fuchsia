# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import fuchsia_base_test
import suspend_resume_test_cases
from mobly import test_runner


class SuspendResumeTest(fuchsia_base_test.AsyncFuchsiaBaseTest):
    TEST_CASES = [suspend_resume_test_cases.SuspendResumeTestCases]

    async def setup_class(self) -> None:
        await super().setup_class()
        self.dut = self.fuchsia_devices[0]

        # TODO(https://fxbug.dev/486154863): It's weird that we're calling
        # this private function, but that's the only way to do it at the
        # moment.
        usb_power_hub, usb_port = self._lookup_usb_power_hub(self.dut)
        self.dut.set_usb_power_hub(usb_power_hub, usb_port)


if __name__ == "__main__":
    test_runner.main()
