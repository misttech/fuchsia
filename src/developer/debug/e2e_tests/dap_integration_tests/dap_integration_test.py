# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import os
import sys
import unittest
from typing import Any

from dap_test_framework import DapTestCase
from pydap.models import InitializeArguments


class TestDapSmoke(DapTestCase):
    async def test_setup(self) -> None:
        # This test verifies that the setup, such as connecting to the DAP server, succeeds both locally and in the CQ
        pass


class TestDapDisconnect(DapTestCase):
    async def test_disconnect_on_close(self) -> None:
        # 1. Initialize the session to get it into running state
        await self.initialize(InitializeArguments(adapterID="zxdb"))
        await self.on_event("initialized")

        # 2. Pre-calculate the sequence number of the next request
        seq = self.framework.client._seq_counter

        # Create a future to explicitly synchronize when the callback runs
        callback_run_future = asyncio.get_running_loop().create_future()

        # Define the callback to close the socket writer immediately when this request is sent
        async def close_socket_on_disconnect(
            writer: asyncio.StreamWriter, value: dict[str, Any]
        ) -> None:
            try:
                writer.close()
                await writer.wait_closed()
            finally:
                if not callback_run_future.done():
                    callback_run_future.set_result(True)

        # Register the callback ONLY for this specific sequence number
        self.set_sent_callback(seq, close_socket_on_disconnect)

        # Trigger disconnect in background
        disconnect_fut = self.disconnect()

        # Wait explicitly for the socket closure callback to execute and complete
        await callback_run_future

        # CRITICAL: Dispose of the disconnect response since we closed the socket
        # immediately and do not expect a response, avoiding unretrieved exception warnings.
        self.dispose_response(disconnect_fut)

        # 3. Wait for the server process to exit voluntarily.
        assert self.framework.proc is not None
        try:
            await asyncio.wait_for(
                self.framework.proc.run_to_completion(), timeout=10.0
            )
        except asyncio.TimeoutError:
            self.fail(
                "DAP server failed to exit after socket close with pending disconnect (hung/leaked!)"
            )


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
