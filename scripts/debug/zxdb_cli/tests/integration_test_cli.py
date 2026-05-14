# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import contextlib
import json
import os
import signal
import subprocess
import sys
import unittest
from io import StringIO
from typing import Any

from cli.cli import main
from daemon.daemon import UDS_PATH
from fx_cmd.lib import FxCmd
from shared.protocol import PROTOCOL_VERSION, HelloRequest, serialize

DAEMON_CLEANUP_TIMEOUT = 5.0


async def _cleanup_process_group(proc: subprocess.Popen[Any]) -> None:
    """Kills the process group of the given process."""
    if proc.poll() is None:
        try:
            pgid = os.getpgid(proc.pid)
            os.killpg(pgid, signal.SIGTERM)
            try:
                await asyncio.wait_for(
                    asyncio.to_thread(proc.wait), timeout=DAEMON_CLEANUP_TIMEOUT
                )
            except asyncio.TimeoutError:
                os.killpg(pgid, signal.SIGKILL)
                proc.wait()
        except ProcessLookupError:
            pass


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
                    elif req.get("command") == "attach":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "attach",
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()

                        # Simulate process event
                        event = {
                            "seq": req["seq"] + 1,
                            "type": "event",
                            "event": "process",
                            "body": {
                                "name": "test_process",
                                "systemProcessId": 1234,
                                "isLocalProcess": False,
                                "startMethod": "attach",
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

    async def _setup_daemon_and_server(
        self,
    ) -> tuple[subprocess.Popen[Any], int]:
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
        args = [
            "zxdb-daemon",
            "--port",
            str(port),
            "--connect-to-existing",
        ]

        read_fd, write_fd = os.pipe()
        os.set_inheritable(write_fd, True)
        args.append(f"--ready-fd={write_fd}")

        cmd = fx_cmd.command_line(*args)
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            pass_fds=[write_fd],
        )

        # Close write end in parent
        os.close(write_fd)

        # Wait for signal on the pipe
        loop = asyncio.get_running_loop()
        await asyncio.wait_for(
            loop.run_in_executor(None, os.read, read_fd, 1), timeout=10.0
        )
        os.close(read_fd)

        # Perform handshake to ensure daemon is connected to DAP server
        reader, writer = await asyncio.open_unix_connection(UDS_PATH)
        req = HelloRequest(version=PROTOCOL_VERSION)
        writer.write(serialize(req).encode("utf-8"))
        await writer.drain()
        response = await asyncio.wait_for(reader.readline(), timeout=5.0)
        writer.close()
        await writer.wait_closed()

        resp_dict = json.loads(response.decode("utf-8"))
        self.assertTrue(resp_dict.get("success"))

        return proc, port

    async def test_daemon_lifecycle(self) -> None:
        """Tests that the daemon starts and stops correctly."""
        proc, port = await self._setup_daemon_and_server()

        try:
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
            await _cleanup_process_group(proc)

    async def test_daemon_hello(self) -> None:
        """Tests the versioned hello handshake."""

        async def run_test() -> None:
            proc, port = await self._setup_daemon_and_server()

            try:
                # Test valid version
                reader, writer = await asyncio.open_unix_connection(UDS_PATH)
                req = HelloRequest(version=PROTOCOL_VERSION)
                writer.write(serialize(req).encode("utf-8"))
                await writer.drain()

                try:
                    # TODO(https://fxbug.dev/509581944): this is intentionally kept longer than the
                    # daemon's internal "handle_hello" timeout, which doesn't feel very good. This
                    # should be improved along with the rest of the general timeout error handling.
                    response = await asyncio.wait_for(
                        reader.readline(), timeout=15.0
                    )
                except asyncio.TimeoutError:
                    self.fail("Timed out waiting for response from daemon")
                writer.close()
                await writer.wait_closed()

                resp_dict = json.loads(response.decode("utf-8"))
                self.assertTrue(resp_dict.get("success"))
                self.assertEqual(
                    resp_dict.get("body", {}).get("protocol_version"),
                    PROTOCOL_VERSION,
                )

                # Test invalid version
                reader, writer = await asyncio.open_unix_connection(UDS_PATH)
                req = HelloRequest(version=PROTOCOL_VERSION + 1)
                writer.write(serialize(req).encode("utf-8"))
                await writer.drain()

                try:
                    response = await asyncio.wait_for(
                        reader.readline(), timeout=5.0
                    )
                except asyncio.TimeoutError:
                    self.fail("Timed out waiting for response from daemon")
                writer.close()
                await writer.wait_closed()

                resp_dict = json.loads(response.decode("utf-8"))
                self.assertFalse(resp_dict.get("success"))
                self.assertIn("version mismatch", resp_dict.get("message", ""))

            finally:
                await _cleanup_process_group(proc)
                if self.fake_dap_server:
                    self.fake_dap_server.close()
                    await self.fake_dap_server.wait_closed()
                    self.fake_dap_server = None

        # Run the test twice to ensure that the daemon can be started and stopped
        # repeatedly without leaving stale state or leaking resources.
        try:
            await asyncio.wait_for(run_test(), timeout=30.0)
        except asyncio.TimeoutError:
            self.fail("Test timed out")

        try:
            await asyncio.wait_for(run_test(), timeout=30.0)
        except asyncio.TimeoutError:
            self.fail("Test timed out")

    async def test_pause_continue(self) -> None:
        proc, port = await self._setup_daemon_and_server()

        try:
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
            await _cleanup_process_group(proc)

    async def test_stack_trace(self) -> None:
        proc, port = await self._setup_daemon_and_server()

        try:
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
            self.assertEqual(frames[0]["source"]["name"], "main.cc")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_wait_for_event(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Trigger an event by sending pause command
            exit_code = await main(["pause", "1"])
            self.assertEqual(exit_code, 0)

            # Capture stdout to read the JSON response from wait-for-event
            saved_stdout = sys.stdout
            try:
                out = StringIO()
                sys.stdout = out

                # Give daemon time to process the event
                await asyncio.sleep(0.1)

                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "0"]
                )
                self.assertEqual(exit_code, 0)

                output = out.getvalue().strip()
            finally:
                sys.stdout = saved_stdout

            # Parse output
            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)
            self.assertEqual(events[0]["event"], "stopped")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait()

    async def test_process_event(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Trigger a process event by sending attach command
            exit_code = await main(["attach", "test_process"])
            self.assertEqual(exit_code, 0)

            # Capture stdout to read the JSON response from wait-for-event
            saved_stdout = sys.stdout
            try:
                out = StringIO()
                sys.stdout = out

                # Give daemon time to process the event
                await asyncio.sleep(0.1)

                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "0"]
                )
                self.assertEqual(exit_code, 0)

                output = out.getvalue().strip()
            finally:
                sys.stdout = saved_stdout

            # Parse output
            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)

            # Find process event
            process_event = None
            for event in events:
                if event.get("event") == "process":
                    process_event = event
                    break

            self.assertIsNotNone(
                process_event, "Process event was not received"
            )
            assert process_event is not None
            self.assertEqual(process_event["body"]["systemProcessId"], 1234)
            self.assertEqual(process_event["body"]["name"], "test_process")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)


if __name__ == "__main__":
    unittest.main()
