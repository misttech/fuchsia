# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.


import test_case_revive as test_case_revive_pkg

FuchsiaDeviceOperation = test_case_revive_pkg.FuchsiaDeviceOperation

TestMethodExecutionFrequency = test_case_revive_pkg.TestMethodExecutionFrequency

opt_out = test_case_revive_pkg.opt_out

tag_test = test_case_revive_pkg.tag_test


class TestCaseRevive(test_case_revive_pkg.AsyncTestCaseRevive):
    pass
