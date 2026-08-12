# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import os
import sys
import unittest
from typing import Any, Dict

from dap_test_framework import (
    DapTestCase,
    get_dap_source_path,
)
from pydap.dap_types import Source, SourceBreakpoint
from pydap.models import (
    InitializeArguments,
    LaunchArguments,
    SetBreakpointsArguments,
)
from zxdb_dap import ZxdbStackTraceArguments


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
        init_fut = self.initialize(InitializeArguments(adapterID="zxdb"))
        self.split_request(init_fut.request_seq, delay=0.1)
        await init_fut


class TestDapDisconnect(DapTestCase):
    async def test_disconnect_on_close(self) -> None:
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

        # Trigger disconnect in background
        disconnect_fut = self.disconnect()

        # Register the callback for the disconnect request sequence
        self.set_sent_callback(
            disconnect_fut.request_seq, close_socket_on_disconnect
        )

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
    # currently we don't use optimize=none because this test only test if we can hit the breakpoint.
    # if we need to check the file line of the breakpoint, then we should set optimize=none.
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


class TestDapBreakpointLine(DapTestCase):
    require_build_type = ["optimize=none"]

    # It's possible to have different file lines to be mapped into a same address when compiling.
    # Breakpoint is implemented based on replacing address of <file line> with a breakpoint trap.
    # when we hit the address, it is impossible to tell whether we are running fileLine1 or fileLine2.
    # To test against the line number, we set optimize=none.
    async def test_breakpoint_line(self) -> None:
        pretty_types_path = get_dap_source_path(
            "src/developer/debug/e2e_tests/inferiors/pretty_types.cc"
        )
        line_number = 38
        bp_resp = await self.set_breakpoints(
            SetBreakpointsArguments(
                source=Source(path=pretty_types_path),
                breakpoints=[SourceBreakpoint(line=line_number)],
            )
        )
        self.assertTrue(bp_resp["success"])
        self.assertEqual(len(bp_resp["body"]["breakpoints"]), 1)
        bp_id = bp_resp["body"]["breakpoints"][0]["id"]

        self.launch(
            LaunchArguments(
                process="fuchsia-pkg://fuchsia.com/zxdb_e2e_inferiors#meta/pretty_types.cm"
            )
        )
        stopped_event = await self.on_event("stopped", timeout=120.0)
        self.assertEqual(stopped_event["body"]["reason"], "breakpoint")
        self.assertIn(bp_id, stopped_event["body"]["hitBreakpointIds"])

        thread_id = stopped_event["body"]["threadId"]
        stack_resp = await self.zxdb_stack_trace(
            ZxdbStackTraceArguments(thread_id=thread_id, remote_unwind=True)
        )
        frames = stack_resp["body"]["stackFrames"]
        self.assertTrue(len(frames) > 0)

        self.assertEqual(frames[0]["line"], line_number)


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


class TestDapStackTrace(DapTestCase):
    # TODO(https://fxbug.dev/529615917): remove target_cpu requirement once core.vim3-vg-release is not flaky.
    require_build_type = ["target_cpu!=arm64", "is_coverage=false"]

    async def test_pretty_stack(self) -> None:
        self.launch(
            LaunchArguments(
                process="fuchsia-pkg://fuchsia.com/crasher#meta/rust_crasher.cm"
            )
        )

        stopped_event = await self.on_event("stopped", timeout=120.0)
        thread_id = stopped_event["body"]["threadId"]

        stack_resp = await self.zxdb_stack_trace(
            ZxdbStackTraceArguments(thread_id=thread_id, remote_unwind=True)
        )

        frames = stack_resp["body"]["stackFrames"]
        self.assertTrue(len(frames) > 0)

        # Find main frame and startup frame
        main_frame = next(
            (f for f in frames if "rust_crasher::main" in f["name"]), None
        )
        startup_frame = next(
            (
                f
                for f in frames
                if f.get("source", {}).get("origin") == "Rust startup"
            ),
            None,
        )

        self.assertIsNotNone(main_frame, "rust_crasher::main frame not found")
        self.assertIsNotNone(
            startup_frame, "'Rust startup' origin frame not found"
        )
        # Explicitly narrow optional union types for Mypy across dynamic assertIsNotNone calls.
        assert main_frame is not None
        assert startup_frame is not None

        # In console zxdb, we fold the frames. In DAP, we mark those frames as "subtle"
        # main frame should NOT be subtle
        self.assertNotEqual(main_frame.get("presentationHint"), "subtle")

        # startup frame SHOULD be subtle and have origin "Rust startup"
        self.assertEqual(startup_frame.get("presentationHint"), "subtle")
        self.assertEqual(
            startup_frame.get("source", {}).get("origin"), "Rust startup"
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
        "--DAP_E2E_TESTS_BUILD_TYPE",
        help="the build_type string containing optimize/target_cpu/lto attributes",
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

    if args.DAP_E2E_TESTS_BUILD_TYPE:
        os.environ["DAP_E2E_TESTS_BUILD_TYPE"] = args.DAP_E2E_TESTS_BUILD_TYPE
        print(
            "BUILD_TYPE = ", os.environ["DAP_E2E_TESTS_BUILD_TYPE"], flush=True
        )

    if args.dump_log:
        os.environ["DAP_DUMP_LOG_ALWAYS"] = "1"

    # Reconstruct sys.argv for unittest.main so that the unittest.main won't complain
    sys.argv = [sys.argv[0]] + unknown
    unittest.main()


if __name__ == "__main__":
    main()
