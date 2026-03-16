# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import asyncio
import unittest
from typing import Any, Dict
from unittest.mock import MagicMock, Mock

import fidl_fuchsia_developer_ffx as ffx
from fidl_codec import encode_fidl_message, method_ordinal
from fuchsia_controller_py import Channel, FcTransportStatus


class MockWaker:
    def __init__(self) -> None:
        self.queues: Dict[int, asyncio.Queue[int]] = {}

    def register(self, h: Any, *, name: str) -> None:
        h_id = h.as_int()
        if h_id not in self.queues:
            self.queues[h_id] = asyncio.Queue()

    def unregister(self, h: Any) -> None:
        pass

    def registration(self, h: Any, *, name: str) -> Any:
        self.register(h, name=name)
        return MagicMock(__enter__=lambda s: s, __exit__=lambda s, *a: None)

    def post_ready(self, h: Any) -> None:
        h_id = h.as_int()
        if h_id not in self.queues:
            self.queues[h_id] = asyncio.Queue()
        self.queues[h_id].put_nowait(h_id)

    async def wait_ready(self, h: Any) -> int:
        return await self.queues[h.as_int()].get()


class FidlClientTests(unittest.IsolatedAsyncioTestCase):
    """Fidl Client tests."""

    async def test_read_and_decode_staged_message(self) -> None:
        channel = Mock()
        channel.__class__ = Channel  # type: ignore[assignment]
        channel.as_int.return_value = 0
        channel.read.side_effect = [
            (bytearray([2, 0, 0, 0]), []),
            FcTransportStatus(FcTransportStatus.FC_ERR_SHOULD_WAIT),
            (bytearray([1, 0, 0, 0]), []),
        ]
        # The proxy here really doesn't matter, we're trying to access internal methods.
        waker = MockWaker()
        proxy = ffx.EchoClient(channel, channel_waker=waker)
        proxy.pending_txids.add(1)
        proxy.pending_txids.add(2)
        waker.post_ready(channel)
        proxy._decode = Mock()  # type: ignore[method-assign]
        proxy._decode.return_value = (bytearray([1, 2, 3]), [])
        loop = asyncio.get_running_loop()
        first_task = loop.create_task(proxy._read_and_decode(1))
        second_task = loop.create_task(proxy._read_and_decode(2))
        await asyncio.gather(first_task, second_task)

    async def test_read_and_decode_blocked(self) -> None:
        channel = Mock()
        channel.__class__ = Channel  # type: ignore[assignment]
        channel.as_int.return_value = 0
        channel.read.side_effect = [
            FcTransportStatus(FcTransportStatus.FC_ERR_SHOULD_WAIT),
            FcTransportStatus(FcTransportStatus.FC_ERR_SHOULD_WAIT),
            FcTransportStatus(FcTransportStatus.FC_ERR_SHOULD_WAIT),
            (bytearray([1, 0, 0, 0]), []),
        ]
        waker = MockWaker()
        proxy = ffx.EchoClient(channel, channel_waker=waker)
        proxy.pending_txids.add(1)
        waker.post_ready(channel)
        waker.post_ready(channel)
        waker.post_ready(channel)
        proxy._decode = Mock()  # type: ignore[method-assign]
        proxy._decode.return_value = (bytearray([1, 2, 3]), [])
        await proxy._read_and_decode(1)

    async def test_read_and_decode_simul_notification(self) -> None:
        channel = Mock()
        channel.__class__ = Channel  # type: ignore[assignment]
        channel.as_int.return_value = 0
        channel.read.side_effect = [
            FcTransportStatus(FcTransportStatus.FC_ERR_SHOULD_WAIT),
            FcTransportStatus(FcTransportStatus.FC_ERR_SHOULD_WAIT),
        ]
        waker = MockWaker()
        proxy = ffx.EchoClient(channel, channel_waker=waker)
        proxy.pending_txids.add(1)
        waker.post_ready(channel)
        proxy.staged_messages[1] = asyncio.Queue(1)
        proxy.staged_messages[1].put_nowait((bytearray([1, 0, 0, 0]), []))
        proxy._decode = Mock()  # type: ignore[method-assign]
        proxy._decode.return_value = (bytearray([1, 2, 3]), [])
        await proxy._read_and_decode(1)

    async def test_unexpected_txid(self) -> None:
        channel = Mock()
        channel.__class__ = Channel  # type: ignore[assignment]
        channel.as_int.return_value = 0
        channel.read.side_effect = [(bytearray([1, 0, 0, 0]), ())]
        waker = MockWaker()
        proxy = ffx.EchoClient(channel, channel_waker=waker)
        waker.post_ready(channel)
        with self.assertRaises(RuntimeError):
            await proxy._read_and_decode(10)

    async def test_staging_stages(self) -> None:
        channel = Mock()
        channel.__class__ = Channel  # type: ignore[assignment]
        channel.as_int.return_value = 0
        proxy = ffx.EchoClient(channel)
        proxy.pending_txids.add(1)
        proxy._stage_message(1, (bytearray([1, 2, 3]), []))
        self.assertEqual(len(proxy.staged_messages), 1)
        got = await proxy._get_staged_message(1)
        self.assertEqual(got, (bytearray([1, 2, 3]), []))

        # This part is a little silly. The decode_fidl_message
        # function can't be mocked, so we're decoding with an actual
        # FIDL message we know is loaded (the Echo protocol from ffx).
        class DecodeObj:
            pass

        obj = DecodeObj()
        obj.__dict__["value"] = "foo"
        proxy._decode(
            1,
            encode_fidl_message(
                object=obj,
                library="fuchsia.developer.ffx",
                type_name="fuchsia.developer.ffx/EchoEchoStringRequest",
                txid=1,
                ordinal=method_ordinal(
                    protocol="fuchsia.developer.ffx/Echo", method="EchoString"
                ),
            ),
        )
        # Verifies state is cleaned up.
        self.assertEqual(len(proxy.staged_messages), 0)
        self.assertEqual(len(proxy.pending_txids), 0)
