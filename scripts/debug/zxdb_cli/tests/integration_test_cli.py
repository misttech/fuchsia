# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import contextlib
import json
import subprocess
import unittest
from io import StringIO

from cli.cli import main
from daemon.daemon import UDS_PATH
from fx_cmd.lib import FxCmd


class TestCLIIntegration(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        if UDS_PATH.exists():
            UDS_PATH.unlink()
        self.fake_dap_server: asyncio.AbstractServer | None = None

    async def asyncTearDown(self) -> None:
        if self.fake_dap_server:
            self.fake_dap_server.close()
            await self.fake_dap_server.wait_closed()
        if UDS_PATH.exists():
            UDS_PATH.unlink()

    async def start_fake_dap_server(self, port: int) -> asyncio.AbstractServer:
        async def handle_client(
            reader: asyncio.StreamReader, writer: asyncio.StreamWriter
        ) -> None:
            try:
                while True:
                    header = await reader.readuntil(b"\r\n\r\n")
                    content_length = 0
                    for line in header.decode("utf-8").split("\r\n"):
                        if line.startswith("Content-Length:"):
                            content_length = int(line.split(":")[1].strip())
                    body = await reader.readexactly(content_length)
                    req = json.loads(body.decode("utf-8"))

                    if req.get("command") == "initialize":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "initialize",
                            "body": {},
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "pause":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "pause",
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()

                        # Simulate stopped event
                        event = {
                            "seq": req["seq"] + 1,
                            "type": "event",
                            "event": "stopped",
                            "body": {
                                "reason": "pause",
                                "threadId": req["arguments"]["threadId"],
                            },
                        }
                        event_body = json.dumps(event).encode("utf-8")
                        event_header = (
                            f"Content-Length: {len(event_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(event_header + event_body)
                        await writer.drain()
                    elif req.get("command") == "continue":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "continue",
                            "body": {"allThreadsContinued": True},
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "stackTrace":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "stackTrace",
                            "body": {
                                "stackFrames": [
                                    {
                                        "id": 1,
                                        "name": "main",
                                        "source": {
                                            "name": "main.cc",
                                            "path": "/path/to/main.cc",
                                        },
                                        "line": 10,
                                        "column": 1,
                                    }
                                ],
                                "totalFrames": 1,
                            },
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
            except (asyncio.IncompleteReadError, ConnectionResetError):
                pass
            finally:
                writer.close()
                try:
                    await writer.wait_closed()
                except:
                    pass

        server = await asyncio.start_server(handle_client, "127.0.0.1", port)
        return server

    async def test_daemon_lifecycle(self) -> None:
        # Find an open port
        temp_server = await asyncio.start_server(
            lambda r, w: None, "127.0.0.1", 0
        )
        port = temp_server.sockets[0].getsockname()[1]
        temp_server.close()
        await temp_server.wait_closed()

        # Start fake DAP server
        self.fake_dap_server = await self.start_fake_dap_server(port)

        # Start Daemon manually
        fx_cmd = FxCmd()
        cmd = fx_cmd.command_line(
            "zxdb-daemon",
            "--port",
            str(port),
            "--connect-to-existing",
        )
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        try:
            # Wait for socket to appear
            for _ in range(10):
                if UDS_PATH.exists():
                    break
                await asyncio.sleep(0.5)
            self.assertTrue(UDS_PATH.exists(), "Socket file was not created")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

            # Wait for process to exit
            try:
                await asyncio.wait_for(
                    asyncio.to_thread(proc.wait), timeout=5.0
                )
            except asyncio.TimeoutError:
                self.fail("Daemon process did not exit after stop")

            # Verify socket is deleted
            self.assertFalse(UDS_PATH.exists(), "Socket file was not deleted")

        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait()

    async def test_pause_continue(self) -> None:
        # Find an open port
        temp_server = await asyncio.start_server(
            lambda r, w: None, "127.0.0.1", 0
        )
        port = temp_server.sockets[0].getsockname()[1]
        temp_server.close()
        await temp_server.wait_closed()

        # Start fake DAP server
        self.fake_dap_server = await self.start_fake_dap_server(port)

        # Start Daemon manually
        fx_cmd = FxCmd()
        cmd = fx_cmd.command_line(
            "zxdb-daemon",
            "--port",
            str(port),
            "--connect-to-existing",
        )
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        try:
            # Wait for socket to appear
            for _ in range(10):
                if UDS_PATH.exists():
                    break
                await asyncio.sleep(0.5)
            self.assertTrue(UDS_PATH.exists(), "Socket file was not created")

            # Test Pause
            exit_code = await main(["pause", "1"])
            self.assertEqual(exit_code, 0)

            # Test Continue
            exit_code = await main(["continue", "1"])
            self.assertEqual(exit_code, 0)

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait()

    async def test_stack_trace(self) -> None:
        # Find an open port
        temp_server = await asyncio.start_server(
            lambda r, w: None, "127.0.0.1", 0
        )
        port = temp_server.sockets[0].getsockname()[1]
        temp_server.close()
        await temp_server.wait_closed()

        # Start dummy DAP server
        self.fake_dap_server = await self.start_fake_dap_server(port)

        # Start Daemon manually
        fx_cmd = FxCmd()
        cmd = fx_cmd.command_line(
            "zxdb-daemon",
            "--port",
            str(port),
            "--connect-to-existing",
        )

        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        try:
            # Wait for socket to appear
            for _ in range(10):
                if UDS_PATH.exists():
                    break
                await asyncio.sleep(0.5)
            self.assertTrue(UDS_PATH.exists(), "Socket file was not created")

            f = StringIO()
            with contextlib.redirect_stdout(f):
                exit_code = await main(["stackTrace", "1"])
            self.assertEqual(exit_code, 0)

            output = f.getvalue()
            output_json = json.loads(output)
            self.assertTrue(output_json.get("success"))

            # Verify stack frame
            body = output_json.get("body")
            self.assertIsNotNone(body)
            frames = body.get("stackFrames")
            self.assertEqual(len(frames), 1)
            self.assertEqual(frames[0]["name"], "main")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait()


if __name__ == "__main__":
    unittest.main()
