# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from daemon.daemon import CommandHandlerRegistry
from shared.protocol import BaseRequest, Response


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
        from daemon.daemon import Daemon

        daemon = Daemon(port=15678)
        self.assertIn("attach", daemon.registry.handlers)

    async def test_handle_attach_success(self) -> None:
        from unittest.mock import AsyncMock, Mock, patch

        from daemon.daemon import Daemon
        from shared.protocol import AttachRequest

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
        from unittest.mock import AsyncMock, Mock, patch

        from daemon.daemon import Daemon
        from shared.protocol import AttachRequest

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
        from daemon.daemon import Daemon
        from shared.protocol import AttachRequest

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None

        req = AttachRequest(filter="my_process")
        resp = await daemon.handle_attach(req)

        self.assertFalse(resp.success)
        self.assertIn("Not connected", resp.message or "")


if __name__ == "__main__":
    unittest.main()
