# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import io
import json
import unittest
from typing import Any

from pydap.client import DapError
from zxdb_dap import (
    ZxdbDapClient,
    ZxdbDetachArguments,
    ZxdbProcessArguments,
    ZxdbStackTraceArguments,
)


class MockWriter(asyncio.StreamWriter):
    def __init__(self) -> None:
        self.buffer = io.BytesIO()
        self.drained = asyncio.Event()

    def write(self, data: bytes) -> None:
        self.buffer.write(data)

    async def drain(self) -> None:
        self.drained.set()


def feed_dap_response(
    reader: asyncio.StreamReader, response: dict[str, Any]
) -> None:
    body = json.dumps(response, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8")
    reader.feed_data(header + body)


class TestZxdbDapMixin(unittest.IsolatedAsyncioTestCase):
    def _start_client(
        self, client: ZxdbDapClient
    ) -> tuple[asyncio.StreamReader, MockWriter]:
        reader = asyncio.StreamReader()
        writer = MockWriter()
        event_queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        client.run(reader, writer, event_queue)
        return reader, writer

    async def test_zxdb_detach_pid(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbDetachArguments(pid=1234)

        send_task = asyncio.create_task(client.zxdb_detach(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Detach",
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertEqual(req_val["command"], "zxdb.Detach")
        self.assertEqual(req_val["arguments"]["pid"], 1234)
        self.assertIsNone(req_val["arguments"].get("all"))

    async def test_zxdb_detach_all(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbDetachArguments(detach_all=True)

        send_task = asyncio.create_task(client.zxdb_detach(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Detach",
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertEqual(req_val["command"], "zxdb.Detach")
        self.assertIsNone(req_val["arguments"].get("pid"))
        self.assertTrue(req_val["arguments"]["all"])

    def test_zxdb_detach_invalid_args(self) -> None:
        with self.assertRaises(ValueError):
            ZxdbDetachArguments(pid=1234, detach_all=True)
        with self.assertRaises(ValueError):
            ZxdbDetachArguments(pid=None, detach_all=None)

    async def test_zxdb_process_all(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)

        send_task = asyncio.create_task(client.zxdb_process())

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Process",
            "body": {
                "processes": [
                    {
                        "id": 1001,
                        "name": "proc1",
                        "threads": [{"id": 2001, "name": "t1"}],
                    },
                    {
                        "id": 1002,
                        "name": "proc2",
                        "threads": [{"id": 2002, "name": "t2"}],
                    },
                ],
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "zxdb.Process")
        self.assertEqual(len(resp.body.processes), 2)
        self.assertEqual(resp.body.processes[0].id, 1001)
        self.assertEqual(resp.body.processes[0].name, "proc1")
        self.assertEqual(len(resp.body.processes[0].threads), 1)
        self.assertEqual(resp.body.processes[0].threads[0].id, 2001)

    async def test_zxdb_process_pid(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbProcessArguments(pid=1001)

        send_task = asyncio.create_task(client.zxdb_process(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Process",
            "body": {
                "processes": [
                    {
                        "id": 1001,
                        "name": "proc1",
                        "threads": [{"id": 2001, "name": "t1"}],
                    }
                ],
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "zxdb.Process")
        self.assertEqual(req_val["arguments"]["pid"], 1001)
        self.assertEqual(len(resp.body.processes), 1)
        self.assertEqual(resp.body.processes[0].id, 1001)

    async def test_zxdb_process_error(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbProcessArguments(pid=9999)

        send_task = asyncio.create_task(client.zxdb_process(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": False,
            "command": "zxdb.Process",
            "message": "Process not found",
        }

        feed_dap_response(reader, response)

        with self.assertRaises(DapError) as ctx:
            await send_task
        self.assertIn("Process not found", str(ctx.exception))

    async def test_zxdb_stack_trace(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbStackTraceArguments(thread_id=5678, remote_unwind=True)

        send_task = asyncio.create_task(client.zxdb_stack_trace(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

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
            "body": {
                "stackFrames": [],
                "totalFrames": 0,
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "stackTrace")
        self.assertEqual(req_val["arguments"]["threadId"], 5678)
        self.assertTrue(req_val["arguments"]["remoteUnwind"])


if __name__ == "__main__":
    unittest.main()
