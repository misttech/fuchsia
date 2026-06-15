# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import sys
import unittest

from dap_test_framework import DapTestCase
from pydap.models import InitializeArguments


class TestDapSmoke(DapTestCase):
    async def test_setup(self) -> None:
        # This test verifies that the setup, such as connecting to the DAP server, succeeds both locally and in the CQ
        pass


# Any tests that send initialize will automatically send disconnect after teardown
class TestDapInit(DapTestCase):
    async def test_initialize(self) -> None:
        await self.initialize(InitializeArguments(adapterID="zxdb"))

    async def test_initialize_partial(self) -> None:
        self.split_request(1, delay=0.1)
        await self.initialize(InitializeArguments(adapterID="zxdb"))


def main() -> None:
    parser = argparse.ArgumentParser()

    parser.add_argument(
        "--DAP_E2E_TESTS_FFX_TEST_DATA",  # The argument is capitalized to match the extra_args in BUILD.gn.
        help="the relative path from host_x64 to the directory of ffx tools",
    )
    args, unknown = parser.parse_known_args()
    if args.DAP_E2E_TESTS_FFX_TEST_DATA:
        os.environ[
            "DAP_E2E_TESTS_FFX_TEST_DATA"
        ] = args.DAP_E2E_TESTS_FFX_TEST_DATA

    # Reconstruct sys.argv for unittest.main so that the unittest.main won't complain
    sys.argv = [sys.argv[0]] + unknown
    unittest.main()


if __name__ == "__main__":
    main()
