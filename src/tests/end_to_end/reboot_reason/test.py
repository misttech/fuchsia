# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import reboot_reason_test_cases
from fuchsia_base_test import fuchsia_base_test
from mobly import test_runner


class RebootReasonTest(fuchsia_base_test.FuchsiaBaseTest):
    TEST_CASES = [reboot_reason_test_cases.RebootReasonTestCases]


if __name__ == "__main__":
    test_runner.main()
