# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.


import reboot_reason_suite
from mobly import test_runner


class RebootReasonTest(reboot_reason_suite.RebootReasonTestSuite):
    def setup_class(self) -> None:
        super().setup_class()
        self.dut = self.fuchsia_devices[0]


if __name__ == "__main__":
    test_runner.main()
