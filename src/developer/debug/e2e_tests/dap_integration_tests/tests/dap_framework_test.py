# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import sys
import unittest
from io import StringIO
from typing import Any
from unittest.mock import AsyncMock, Mock

from async_utils.command import StderrEvent, StdoutEvent, TerminationEvent
from dap_test_framework import DapTestFramework, RequestFuture
from pydap.client import DapClient


class TestDapFramework(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.framework = DapTestFramework()
        self.framework.client = Mock(spec=DapClient)
        self.framework.client._seq_counter = 1
        self.framework._writer = Mock()
        self.framework._writer.wait_closed = AsyncMock()
        self.framework._process_task = asyncio.create_task(
            self.framework._event_processor_loop()
        )

    async def asyncTearDown(self) -> None:
        await self.framework.teardown()

    async def test_verify_partial_success(self) -> None:
        data = {
            "seq": 1,
            "type": "response",
            "success": True,
            "body": {"threads": []},
        }
        check = {"success": True}
        self.assertTrue(self.framework._verify_partial(data, check))

    async def test_verify_partial_failure(self) -> None:
        data = {"seq": 1, "type": "response", "success": False, "body": {}}
        check = {"success": True}
        with self.assertRaises(AssertionError):
            self.framework._verify_partial(data, check)

    async def test_strip_path_single_key(self) -> None:
        data = {"seq": 1, "type": "response", "success": True}
        self.framework._strip_path(data, "$.seq")
        self.assertNotIn("seq", data)
        self.assertIn("type", data)

    async def test_strip_path_dotted(self) -> None:
        data = {
            "seq": 1,
            "body": {"threadId": 123, "name": "thread1"},
        }
        self.framework._strip_path(data, "$.body.threadId")
        body = data["body"]
        assert isinstance(body, dict)
        self.assertNotIn("threadId", body)
        self.assertIn("name", body)

    async def test_strip_path_dotted_in_list(self) -> None:
        data = {
            "body": {
                "threads": [
                    {"id": 1, "name": "t1"},
                    {"id": 2, "name": "t2"},
                ]
            }
        }
        self.framework._strip_path(data, "$.body.threads.id")
        body = data["body"]
        assert isinstance(body, dict)
        threads = body["threads"]
        assert isinstance(threads, list)
        for t in threads:
            assert isinstance(t, dict)
            self.assertNotIn("id", t)
            self.assertIn("name", t)

    async def test_strip_path_missing_key(self) -> None:
        data = {"body": {"name": "t1"}}
        self.framework._strip_path(data, "$.body.missing_key")
        self.framework._strip_path(data, "$.missing_root.child")
        self.assertEqual(data, {"body": {"name": "t1"}})

    async def test_on_event_matches_unmatched_history(self) -> None:
        self.framework.unmatched_events.append(
            {"type": "event", "event": "stopped", "seq": 5}
        )
        event = await self.framework.on_event("stopped")
        self.assertEqual(event["seq"], 5)
        self.assertEqual(len(self.framework.unmatched_events), 0)

    async def test_request_future_expect_failure_raises_on_await(self) -> None:
        fut = RequestFuture(self.framework, "threads", 1)
        fut.expect({"success": True})

        fut.set_result({"success": False})

        with self.assertRaises(AssertionError):
            await fut

    async def test_expect_event_success(self) -> None:
        self.framework.on_event("stopped").expect(
            {"body": {"reason": "breakpoint"}}
        )

        await self.framework.event_queue.put(
            {
                "type": "event",
                "event": "stopped",
                "body": {"reason": "breakpoint"},
            }
        )

        await self.framework.verify_all_expectations()

    async def test_expect_event_failure(self) -> None:
        self.framework.on_event("stopped").expect(
            {"body": {"reason": "breakpoint"}}
        )

        await self.framework.event_queue.put(
            {
                "type": "event",
                "event": "stopped",
                "body": {"reason": "step"},
            }
        )

        with self.assertRaises(AssertionError):
            await self.framework.verify_all_expectations()

    async def test_on_event_skips_irrelevant_events(self) -> None:
        fut = self.framework.on_event("initialized")

        await self.framework.event_queue.put(
            {"type": "event", "event": "thread", "seq": 1}
        )
        await self.framework.event_queue.put(
            {"type": "event", "event": "initialized", "seq": 2}
        )

        event = await fut
        self.assertEqual(event["event"], "initialized")
        self.assertEqual(event["seq"], 2)

    async def test_on_event_arrives_before_call(self) -> None:
        await self.framework.event_queue.put(
            {"type": "event", "event": "initialized", "seq": 1}
        )

        await asyncio.sleep(0.1)

        fut = self.framework.on_event("initialized")

        event = await fut
        self.assertEqual(event["event"], "initialized")
        self.assertEqual(event["seq"], 1)

    async def test_on_event_arrives_during_wait(self) -> None:
        fut = self.framework.on_event("initialized")

        async def put_event() -> None:
            await asyncio.sleep(0.2)
            await self.framework.event_queue.put(
                {"type": "event", "event": "initialized", "seq": 1}
            )

        async with asyncio.timeout(2.0):
            _, event = await asyncio.gather(put_event(), fut)
            self.assertEqual(event["event"], "initialized")

    async def test_server_log_captured_on_exception(self) -> None:
        class FakeProc:
            def __init__(self) -> None:
                self.events = [
                    StdoutEvent(text=b"stdout line\n"),
                    StderrEvent(text=b"stderr line\n"),
                    TerminationEvent(return_code=0, runtime=1.0),
                ]

            def __aiter__(self) -> "FakeProc":
                return self

            async def __anext__(self) -> Any:
                if not self.events:
                    raise StopAsyncIteration
                await asyncio.sleep(0.01)
                return self.events.pop(0)

            def terminate(self) -> None:
                pass

            def kill(self) -> None:
                pass

        self.framework.proc = FakeProc()  # type: ignore

        # Start the server log task
        self.framework._server_log_task = asyncio.create_task(
            self.framework._read_server_log()
        )

        captured_output = StringIO()
        sys.stdout = captured_output
        try:
            # Simulate a test failure exception
            raise RuntimeError("Simulated test failure")
        except RuntimeError:
            # This is what asyncTearDown does
            await self.framework.teardown()
            self.framework.dump_server_logs()
        finally:
            sys.stdout = sys.__stdout__

        output_str = captured_output.getvalue()
        self.assertIn("[zxdb stdout] stdout line\n", output_str)
        self.assertIn("[zxdb stderr] stderr line\n", output_str)
        self.assertIn("[zxdb terminated] exit code: 0", output_str)


if __name__ == "__main__":
    unittest.main()
