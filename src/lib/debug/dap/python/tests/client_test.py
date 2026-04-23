# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import io
import json
import unittest

from pydap.client import DapClient


class MockWriter:
    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def write(self, data: bytes) -> None:
        self.buffer.write(data)

    async def drain(self) -> None:
        pass


class TestDapClient(unittest.IsolatedAsyncioTestCase):
    async def test_read_message(self) -> None:
        data = b'Content-Length: 26\r\n\r\n{"seq":1,"type":"request"}'
        reader = asyncio.StreamReader()
        reader.feed_data(data)
        reader.feed_eof()

        client = DapClient()
        msg = await client._read_message(reader)
        self.assertIsNotNone(msg)
        assert msg is not None
        self.assertEqual(msg["seq"], 1)

        self.assertEqual(msg["type"], "request")

    async def test_write_message(self) -> None:
        client = DapClient()
        value = {"seq": 1, "type": "request"}

        writer = MockWriter()
        await client._write_message(writer, value)  # type: ignore

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        self.assertEqual(headers, b"Content-Length: 26")
        self.assertEqual(json.loads(body.decode("utf-8")), value)

    async def test_send_request(self) -> None:
        client = DapClient()

        writer = MockWriter()

        send_task = asyncio.create_task(
            client.send_request(writer, "initialize")  # type: ignore
        )

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "initialize",
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertEqual(resp["request_seq"], seq)

    async def test_initialize(self) -> None:
        from pydap.models import InitializeArguments

        client = DapClient()

        writer = MockWriter()
        args = InitializeArguments(adapterID="test")
        send_task = asyncio.create_task(
            client.initialize(writer, args)  # type: ignore
        )

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "initialize",
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])

    async def test_disconnect(self) -> None:
        from pydap.models import DisconnectArguments

        client = DapClient()

        writer = MockWriter()
        args = DisconnectArguments(terminateDebuggee=True)
        send_task = asyncio.create_task(
            client.disconnect(writer, args)  # type: ignore
        )

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "disconnect",
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])

    async def test_stack_trace(self) -> None:
        from pydap.models import StackTraceArguments

        client = DapClient()

        writer = MockWriter()
        args = StackTraceArguments(threadId=1)
        send_task = asyncio.create_task(
            client.stack_trace(writer, args)  # type: ignore
        )

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "stackTrace",
            "body": {"stackFrames": []},
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertEqual(resp.stackFrames, [])

    async def test_continue_thread(self) -> None:
        from pydap.models import ContinueArguments

        client = DapClient()

        writer = MockWriter()
        args = ContinueArguments(threadId=1)
        send_task = asyncio.create_task(
            client.continue_thread(writer, args)  # type: ignore
        )

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "continue",
            "body": {"allThreadsContinued": True},
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])

    async def test_pause_thread(self) -> None:
        from pydap.models import PauseArguments

        client = DapClient()

        writer = MockWriter()
        args = PauseArguments(threadId=1)
        send_task = asyncio.create_task(
            client.pause_thread(writer, args)  # type: ignore
        )

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "pause",
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])

    async def test_threads(self) -> None:
        client = DapClient()

        writer = MockWriter()
        send_task = asyncio.create_task(client.threads(writer))  # type: ignore

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "threads",
            "body": {
                "threads": [
                    {"id": 1234, "name": "main"},
                    {"id": 5678, "name": "worker"},
                ]
            },
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertEqual(len(resp.threads), 2)
        self.assertEqual(resp.threads[0].id, 1234)
        self.assertEqual(resp.threads[0].name, "main")

    async def test_attach(self) -> None:
        from pydap.models import AttachRequestArguments

        client = DapClient()

        writer = MockWriter()
        args = AttachRequestArguments(
            _restart=True, extra_fields={"process": "my_process"}
        )
        send_task = asyncio.create_task(client.attach(writer, args))  # type: ignore

        await asyncio.sleep(0.1)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "attach",
        }

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertTrue(req_val["arguments"]["__restart"])
        self.assertEqual(req_val["arguments"]["process"], "my_process")
