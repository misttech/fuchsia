# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Example test demonstrating FuchsiaTestCases."""

import logging

import fuchsia_base_test
from mobly import test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class ExampleAsyncTestCases(fuchsia_base_test.FuchsiaTestCases):
    """Example async test cases."""

    async def setup_test(
        self,
    ) -> None:
        await super().setup_test()
        _LOGGER.info("ExampleAsyncTestCases.setup_test() called")

    async def teardown_test(self) -> None:
        _LOGGER.info("ExampleAsyncTestCases.teardown_test() called")
        await super().teardown_test()

    async def test_example_async_case(self) -> None:
        for fuchsia_device in self.fuchsia_devices:
            _LOGGER.info(
                "%s inside test_example_async_case!", fuchsia_device.device_name
            )


class ExampleAsyncTest(fuchsia_base_test.FuchsiaBaseTest):
    """Example async test using TEST_CASES."""

    TEST_CASES = [ExampleAsyncTestCases]


if __name__ == "__main__":
    test_runner.main()
