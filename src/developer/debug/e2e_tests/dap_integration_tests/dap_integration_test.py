# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import os
import sys
import unittest
from typing import Any, Dict

from dap_test_framework import DapTestCase, get_dap_source_path
from pydap.dap_types import Source, SourceBreakpoint
from pydap.models import (
    InitializeArguments,
    LaunchArguments,
    SetBreakpointsArguments,
)


class TestDapSmoke(DapTestCase):
    async def test_setup(self) -> None:
        # This test verifies that the setup, such as connecting to the DAP server, succeeds both locally and in the CQ
        pass


# Any tests that send initialize will automatically send disconnect after teardown
class TestDapInit(DapTestCase):
    auto_initialize = False

    async def test_initialize(self) -> None:
        await self.initialize(InitializeArguments(adapterID="zxdb"))

    async def test_initialize_partial(self) -> None:
        self.split_request(1, delay=0.1)
        await self.initialize(InitializeArguments(adapterID="zxdb"))


class TestDapDisconnect(DapTestCase):
    async def test_disconnect_on_close(self) -> None:
        seq = self.framework.client._seq_counter

        # Create a future to explicitly synchronize when the callback runs
        callback_run_future = asyncio.get_running_loop().create_future()

        async def close_socket_on_disconnect(
            writer: asyncio.StreamWriter, value: Dict[str, Any]
        ) -> None:
            try:
                writer.close()
                await writer.wait_closed()
            except Exception:
                pass
            finally:
                if not callback_run_future.done():
                    callback_run_future.set_result(True)

        # Register the callback ONLY for this specific sequence number
        self.set_sent_callback(seq, close_socket_on_disconnect)

        # Trigger disconnect in background
        disconnect_fut = self.disconnect()

        # Wait explicitly for the socket closure callback to execute and complete
        await asyncio.wait_for(callback_run_future, timeout=5.0)

        # CRITICAL: Dispose of the disconnect response since we closed the socket
        # immediately and do not expect a response, avoiding unretrieved exception warnings.
        self.dispose_response(disconnect_fut)

        # 3. Wait for the server process to exit voluntarily.
        try:
            await self.framework.wait_for_shutdown(timeout=10.0)
        except asyncio.TimeoutError:
            self.fail(
                "DAP server failed to exit after socket close with pending disconnect (hung/leaked!)"
            )


class TestDapBreakpoint(DapTestCase):
    async def test_breakpoint(self) -> None:
        crasher_path = get_dap_source_path(
            "src/developer/forensics/crasher/cpp/crasher.c"
        )
        bp_resp = await self.set_breakpoints(
            SetBreakpointsArguments(
                source=Source(path=crasher_path),
                breakpoints=[SourceBreakpoint(line=25)],
            )
        )
        self.assertTrue(bp_resp["success"])
        self.assertEqual(len(bp_resp["body"]["breakpoints"]), 1)
        bp_id = bp_resp["body"]["breakpoints"][0]["id"]

        self.launch(
            LaunchArguments(
                process="fuchsia-pkg://fuchsia.com/crasher#meta/cpp_crasher.cm"
            )
        )
        await self.on_event("stopped", 30.0).expect(
            {
                "body": {
                    "reason": "breakpoint",
                    "hitBreakpointIds": [bp_id],
                }
            }
        )

        clear_resp = await self.set_breakpoints(
            SetBreakpointsArguments(
                source=Source(path=crasher_path),
                breakpoints=[],
            )
        )
        self.assertTrue(clear_resp["success"])
        self.assertEqual(len(clear_resp["body"]["breakpoints"]), 0)


class TestLaunch(DapTestCase):
    async def test_strong_attach(self) -> None:
        self.launch(
            LaunchArguments(
                process="fuchsia-pkg://fuchsia.com/crasher#meta/cpp_crasher.cm"
            )
        )
        await self.on_event("stopped", 30.0).expect(
            {
                "body": {
                    "reason": "exception",
                }
            }
        )


def main() -> None:
    parser = argparse.ArgumentParser()

    parser.add_argument(
        "--DAP_E2E_TESTS_FFX_TEST_DATA",  # The argument is capitalized to match the extra_args in BUILD.gn.
        help="the relative path from root_build_dir to the directory of ffx tools",
    )

    parser.add_argument(
        "--DAP_E2E_TESTS_SYMBOL_DIR",
        help="the relative path from root_build_dir to the inferior build-id symbol directory",
    )

    parser.add_argument(
        "--dump-log",
        action="store_true",
        help="print DAP traffic history even if tests succeed",
    )

    args, unknown = parser.parse_known_args()

    if args.DAP_E2E_TESTS_FFX_TEST_DATA:
        os.environ[
            "DAP_E2E_TESTS_FFX_TEST_DATA"
        ] = args.DAP_E2E_TESTS_FFX_TEST_DATA

    if args.DAP_E2E_TESTS_SYMBOL_DIR:
        os.environ["DAP_E2E_TESTS_SYMBOL_DIR"] = args.DAP_E2E_TESTS_SYMBOL_DIR

    if args.dump_log:
        os.environ["DAP_DUMP_LOG_ALWAYS"] = "1"

    # Reconstruct sys.argv for unittest.main so that the unittest.main won't complain
    sys.argv = [sys.argv[0]] + unknown
    unittest.main()


if __name__ == "__main__":
    main()
