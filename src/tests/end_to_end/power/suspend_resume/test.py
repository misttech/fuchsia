# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.


import suspend_resume_suite
from mobly import test_runner


class SuspendResumeTest(suspend_resume_suite.SuspendResumeTestSuite):
    pass


if __name__ == "__main__":
    test_runner.main()
