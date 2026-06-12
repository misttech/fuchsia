# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from shared.protocol import Response
from shared.protocol.wait_for_event import WaitForEventRequest


class TestDaemonEvents(unittest.IsolatedAsyncioTestCase):
    async def test_event_sequencing(self) -> None:
        # Port is unused in these tests.
        daemon = Daemon(port=None)

        # Put events into queue
        await daemon.event_queue.put(
            {"event": "stopped", "body": {"threadId": 1}}
        )
        await daemon.event_queue.put(
            {"event": "continued", "body": {"threadId": 1}}
        )

        # Run process events in background
        task = asyncio.create_task(daemon._process_events())

        # Wait for events to be processed (avoiding fixed sleep)
        for _ in range(100):
            if len(daemon.all_events) == 2:
                break
            await asyncio.sleep(0.01)

        task.cancel()

        self.assertEqual(len(daemon.all_events), 2)
        self.assertEqual(daemon.all_events[1]["seq"], 1)
        self.assertEqual(daemon.all_events[2]["seq"], 2)

    async def test_wait_for_event_immediate(self) -> None:
        daemon = Daemon(port=None)

        daemon.all_events = {
            1: {"seq": 1, "event": "stopped"},
            2: {"seq": 2, "event": "continued"},
        }
        daemon.latest_seq = 2

        resp = await daemon.registry.handle(
            "wait-for-event", WaitForEventRequest(last_seen_seq=1)
        )
        self.assertTrue(resp.success)
        assert resp.events is not None
        self.assertEqual(len(resp.events), 1)
        self.assertEqual(resp.events[0]["seq"], 2)

    async def test_wait_for_event_blocking(self) -> None:
        daemon = Daemon(port=None)

        async def add_event() -> None:
            # Give the waiter time to start waiting
            await asyncio.sleep(0.05)
            daemon.latest_seq = 1
            daemon.all_events[1] = {"seq": 1, "event": "stopped"}
            async with daemon.new_event_condition:
                daemon.new_event_condition.notify_all()

        asyncio.create_task(add_event())

        resp = await daemon.registry.handle(
            "wait-for-event", WaitForEventRequest(last_seen_seq=0)
        )
        self.assertTrue(resp.success)
        assert resp.events is not None
        self.assertEqual(len(resp.events), 1)
        self.assertEqual(resp.events[0]["seq"], 1)

    async def test_event_filtering(self) -> None:
        daemon = Daemon(port=None)

        # Put allowed and disallowed events into queue
        await daemon.event_queue.put(
            {"event": "stopped", "body": {"threadId": 1}}
        )
        await daemon.event_queue.put(
            {"event": "output", "body": {"output": "stdout"}}
        )
        await daemon.event_queue.put(
            {"event": "exited", "body": {"exitCode": 0}}
        )

        # Run process events in background
        task = asyncio.create_task(daemon._process_events())

        # Wait for events to be processed
        for _ in range(100):
            if len(daemon.all_events) == 2:
                break
            await asyncio.sleep(0.01)

        task.cancel()

        self.assertEqual(len(daemon.all_events), 2)
        self.assertEqual(daemon.all_events[1]["event"], "stopped")
        self.assertEqual(daemon.all_events[2]["event"], "exited")

    async def test_event_acknowledgment(self) -> None:
        daemon = Daemon(port=None)

        daemon.all_events = {
            1: {"seq": 1, "event": "stopped"},
            2: {"seq": 2, "event": "continued"},
            3: {"seq": 3, "event": "exited"},
        }
        daemon.latest_seq = 3

        mock_reader = AsyncMock()
        mock_reader.readline.return_value = (
            b'{"command": "threads", "ack_seq": 2}\n'
        )

        mock_writer = Mock()
        mock_writer.write = Mock()
        mock_writer.drain = AsyncMock()
        mock_writer.wait_closed = AsyncMock()

        with patch.object(
            daemon.registry, "handle", new_callable=AsyncMock
        ) as mock_handle:
            mock_handle.return_value = Response(success=True)
            await daemon.handle_uds_client(mock_reader, mock_writer)

        self.assertEqual(len(daemon.all_events), 1)
        self.assertEqual(daemon.all_events[3]["seq"], 3)

    async def test_process_event_handling(self) -> None:
        daemon = Daemon(port=None)

        # Put process event into queue
        await daemon.event_queue.put(
            {
                "event": "process",
                "body": {"systemProcessId": 1234, "name": "test_process"},
            }
        )

        # Run process events in background
        task = asyncio.create_task(daemon._process_events())

        # Wait for events to be processed
        for _ in range(100):
            if 1234 in daemon.active_processes:
                break
            await asyncio.sleep(0.01)

        task.cancel()

        self.assertIn(1234, daemon.active_processes)
        self.assertEqual(daemon.active_processes[1234], "test_process")


if __name__ == "__main__":
    unittest.main()
