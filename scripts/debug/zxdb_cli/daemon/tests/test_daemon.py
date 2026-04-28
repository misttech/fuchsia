# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import CommandHandlerRegistry, Daemon
from shared.protocol import AttachRequest, BaseRequest, Response, ThreadsRequest


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
            mock_attach.return_value = {"success": True}

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

    def test_threads_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("threads", daemon.registry.handlers)

    @patch("daemon.daemon.DapClient")
    async def test_handle_threads(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.name = "main"
        mock_thread2 = Mock()
        mock_thread2.id = 2
        mock_thread2.name = "worker"
        mock_threads_resp.threads = [mock_thread1, mock_thread2]
        mock_dap_client.threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.handle_threads(ThreadsRequest())

        self.assertTrue(resp.success)
        assert resp.body is not None
        threads = resp.body["threads"]
        self.assertEqual(len(threads), 2)
        self.assertEqual(threads[0]["id"], 1)
        self.assertEqual(threads[0]["name"], "main")
        self.assertEqual(threads[1]["id"], 2)
        self.assertEqual(threads[1]["name"], "worker")


if __name__ == "__main__":
    unittest.main()
