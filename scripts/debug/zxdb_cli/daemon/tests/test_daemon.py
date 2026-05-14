# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import CommandHandlerRegistry, Daemon
from shared.protocol import (
    AttachRequest,
    BaseRequest,
    ContinueRequest,
    PauseRequest,
    Response,
    ThreadsRequest,
    WaitForEventRequest,
)


class TestCommandHandlerRegistry(unittest.IsolatedAsyncioTestCase):
    async def test_register_and_handle(self) -> None:
        registry = CommandHandlerRegistry()

        async def mock_handler(_req: BaseRequest) -> Response:
            return Response(success=True, body={"data": "handled"})

        registry.register("test_cmd", mock_handler)

        resp = await registry.handle(
            "test_cmd", BaseRequest(command="test_cmd")
        )

        self.assertTrue(resp.success)
        self.assertEqual(resp.body, {"data": "handled"})

    async def test_unknown_command(self) -> None:
        registry = CommandHandlerRegistry()
        resp = await registry.handle("unknown", BaseRequest(command="unknown"))

        self.assertFalse(resp.success)
        self.assertIsNotNone(resp.message)
        self.assertIn("Unknown command", resp.message or "")

    async def test_attach_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("attach", daemon.registry.handlers)

    async def test_handle_attach_success(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach_resp = Mock()
            mock_attach_resp.dump_dap.return_value = {"success": True}
            mock_attach.return_value = mock_attach_resp

            req = AttachRequest(filter="my_process")
            resp = await daemon.handle_attach(req)

            self.assertTrue(resp.success)
            self.assertEqual(resp.body, {"success": True})
            mock_attach.assert_called_once()

    async def test_handle_attach_failure(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach.side_effect = Exception("Failed to attach")

            req = AttachRequest(filter="my_process")
            resp = await daemon.handle_attach(req)

            self.assertFalse(resp.success)
            self.assertIn("Failed to attach", resp.message or "")

    async def test_handle_attach_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None

        req = AttachRequest(filter="my_process")
        resp = await daemon.handle_attach(req)

        self.assertFalse(resp.success)
        self.assertIn("Not connected", resp.message or "")

    @patch("daemon.daemon.DapClient")
    async def test_handle_continue(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_continue_resp = Mock()
        mock_continue_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.continue_thread = AsyncMock(
            return_value=mock_continue_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.handle_continue(ContinueRequest(thread_id=1))

        self.assertTrue(resp.success)
        mock_dap_client.continue_thread.assert_called_once()

    @patch("daemon.daemon.DapClient")
    async def test_handle_pause_sync(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_dap_client.pause_thread = AsyncMock(return_value={"success": True})

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        async def trigger_stopped() -> None:
            await asyncio.sleep(0.1)
            daemon.event_waiter.notify_thread_stop(
                1,
                {
                    "type": "event",
                    "event": "stopped",
                    "body": {"reason": "pause"},
                },
            )

        loop = asyncio.get_event_loop()
        loop.create_task(trigger_stopped())

        resp = await daemon.handle_pause(PauseRequest(thread_id=1))

        self.assertTrue(resp.success)
        mock_dap_client.pause_thread.assert_called_once()

    def test_threads_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("threads", daemon.registry.handlers)

    @patch("daemon.daemon.DapClient")
    async def test_handle_threads(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.name = "main"
        mock_thread2 = Mock()
        mock_thread2.id = 2
        mock_thread2.name = "worker"
        mock_body.threads = [mock_thread1, mock_thread2]
        mock_body.model_dump.return_value = {
            "threads": [
                {"id": 1, "name": "main"},
                {"id": 2, "name": "worker"},
            ]
        }
        mock_threads_resp.body = mock_body
        mock_dap_client.threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.handle_threads(ThreadsRequest())

        if not resp.success:
            print(f"Test failed with message: {resp.message}")
        self.assertTrue(resp.success)
        assert resp.body is not None
        threads = resp.body["threads"]
        self.assertEqual(len(threads), 2)
        self.assertEqual(threads[0]["id"], 1)
        self.assertEqual(threads[0]["name"], "main")
        self.assertEqual(threads[1]["id"], 2)
        self.assertEqual(threads[1]["name"], "worker")


class TestDaemonEvents(unittest.IsolatedAsyncioTestCase):
    async def get_open_port(self) -> int:
        temp_server = await asyncio.start_server(
            lambda r, w: None, "127.0.0.1", 0
        )
        port = temp_server.sockets[0].getsockname()[1]
        temp_server.close()
        await temp_server.wait_closed()
        return int(port)

    async def test_event_sequencing(self) -> None:
        port = await self.get_open_port()
        daemon = Daemon(port=port)

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
        port = await self.get_open_port()
        daemon = Daemon(port=port)

        daemon.all_events = {
            1: {"seq": 1, "event": "stopped"},
            2: {"seq": 2, "event": "continued"},
        }
        daemon.latest_seq = 2

        resp = await daemon.handle_wait_for_event(
            WaitForEventRequest(last_seen_seq=1)
        )
        self.assertTrue(resp.success)
        assert resp.events is not None
        self.assertEqual(len(resp.events), 1)
        self.assertEqual(resp.events[0]["seq"], 2)

    async def test_wait_for_event_blocking(self) -> None:
        port = await self.get_open_port()
        daemon = Daemon(port=port)

        async def add_event() -> None:
            # Give the waiter time to start waiting
            await asyncio.sleep(0.05)
            daemon.latest_seq = 1
            daemon.all_events[1] = {"seq": 1, "event": "stopped"}
            async with daemon.new_event_condition:
                daemon.new_event_condition.notify_all()

        asyncio.create_task(add_event())

        resp = await daemon.handle_wait_for_event(
            WaitForEventRequest(last_seen_seq=0)
        )
        self.assertTrue(resp.success)
        assert resp.events is not None
        self.assertEqual(len(resp.events), 1)
        self.assertEqual(resp.events[0]["seq"], 1)

    async def test_event_filtering(self) -> None:
        port = await self.get_open_port()
        daemon = Daemon(port=port)

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
        port = await self.get_open_port()
        daemon = Daemon(port=port)

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


if __name__ == "__main__":
    unittest.main()
