# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import io
import json
import unittest

from zxdb_dap import ZxdbDapClient, ZxdbDetachArguments


class MockWriter(asyncio.StreamWriter):
    def __init__(self) -> None:
        self.buffer = io.BytesIO()
        self.drained = asyncio.Event()

    def write(self, data: bytes) -> None:
        self.buffer.write(data)

    async def drain(self) -> None:
        self.drained.set()


class TestZxdbDapMixin(unittest.IsolatedAsyncioTestCase):
    async def test_zxdb_detach_pid(self) -> None:
        client = ZxdbDapClient()
        writer = MockWriter()
        args = ZxdbDetachArguments(pid=1234)

        send_task = asyncio.create_task(client.zxdb_detach(writer, args))

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

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertEqual(req_val["command"], "zxdb.Detach")
        self.assertEqual(req_val["arguments"]["pid"], 1234)
        self.assertIsNone(req_val["arguments"].get("all"))

    async def test_zxdb_detach_all(self) -> None:
        client = ZxdbDapClient()
        writer = MockWriter()
        args = ZxdbDetachArguments(detach_all=True)

        send_task = asyncio.create_task(client.zxdb_detach(writer, args))

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

        if seq in client._pending_requests:
            client._pending_requests[seq].set_result(response)

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


if __name__ == "__main__":
    unittest.main()
