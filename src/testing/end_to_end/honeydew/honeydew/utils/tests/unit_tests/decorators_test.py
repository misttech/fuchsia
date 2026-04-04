# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.utils.decorators.py."""

import asyncio
import unittest
from unittest import mock

from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.utils import decorators


class DecoratorsTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for honeydew.utils.decorators."""

    def test_notify_intentional_disconnect_sync(self) -> None:
        """Test notify_intentional_disconnect on a sync method."""
        mock_ffx = mock.Mock()

        class TestClass:
            ffx: ffx_transport.FFX

            def __init__(self) -> None:
                self.ffx = mock_ffx

            @decorators.notify_intentional_disconnect
            def test_method(self, arg1: str) -> str:
                return arg1

        obj = TestClass()
        result = obj.test_method("hello")

        self.assertEqual(result, "hello")
        mock_ffx.notify_intentional_disconnect.assert_called_once()

    async def test_notify_intentional_disconnect_async(self) -> None:
        """Test notify_intentional_disconnect on an async method."""
        mock_ffx = mock.Mock()

        class TestClass:
            ffx: ffx_transport.FFX

            def __init__(self) -> None:
                self.ffx = mock_ffx

            @decorators.notify_intentional_disconnect
            async def test_method(self, arg1: str) -> str:
                await asyncio.sleep(0)
                return arg1

        obj = TestClass()
        result = await obj.test_method("hello")

        self.assertEqual(result, "hello")
        mock_ffx.notify_intentional_disconnect.assert_called_once()


if __name__ == "__main__":
    unittest.main()
