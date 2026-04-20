# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import logging
from typing import Any, Dict, Optional

from pydap.models import (
    ContinueArguments,
    DisconnectArguments,
    InitializeArguments,
    MessageType,
    PauseArguments,
    StackTraceArguments,
    StackTraceResponse,
    ThreadsResponse,
    dataclass_to_dict,
    from_dict,
)

logger = logging.getLogger(__name__)


class DapError(Exception):
    """Base exception for DAP client errors."""


class DapClient:
    """A client for the Debug Adapter Protocol."""

    def __init__(self) -> None:
        self._pending_requests: Dict[int, asyncio.Future[Any]] = {}

        self._seq_counter = 1

    async def run(
        self, reader: asyncio.StreamReader, event_queue: asyncio.Queue[Any]
    ) -> None:
        """Runs the client's read loop, processing messages from the reader."""
        while True:
            try:
                msg = await self._read_message(reader)
                if msg is None:
                    break  # EOF

                msg_type = msg.get("type")
                if msg_type == MessageType.EVENT.value:
                    await event_queue.put(msg)
                elif msg_type == MessageType.RESPONSE.value:
                    req_seq = msg.get("request_seq")
                    if req_seq in self._pending_requests:
                        fut = self._pending_requests.pop(req_seq)
                        if not fut.done():
                            fut.set_result(msg)
            except Exception:
                logger.exception("Error in DAP client run loop")
                break

    async def send_request(
        self,
        writer: asyncio.StreamWriter,
        command: str,
        arguments: Optional[Dict[str, Any]] = None,
        timeout: float = 5.0,
    ) -> Dict[str, Any]:
        """Sends a request to the debug adapter and waits for the response."""
        seq = self._seq_counter
        self._seq_counter += 1

        loop = asyncio.get_running_loop()
        fut = loop.create_future()
        self._pending_requests[seq] = fut

        request = {
            "seq": seq,
            "type": MessageType.REQUEST.value,
            "command": command,
        }
        if arguments:
            request["arguments"] = arguments

        await self._write_message(writer, request)
        try:
            return await asyncio.wait_for(fut, timeout=timeout)
        except asyncio.TimeoutError:
            self._pending_requests.pop(seq, None)
            raise DapError(
                f"Request {command} (seq={seq}) timed out after {timeout}s"
            )

    async def initialize(
        self, writer: asyncio.StreamWriter, args: InitializeArguments
    ) -> Dict[str, Any]:
        """Sends an initialize request."""
        return await self.send_request(
            writer, "initialize", dataclass_to_dict(args)
        )

    async def disconnect(
        self, writer: asyncio.StreamWriter, args: DisconnectArguments
    ) -> Dict[str, Any]:
        """Sends a disconnect request."""
        return await self.send_request(
            writer, "disconnect", dataclass_to_dict(args)
        )

    async def stack_trace(
        self, writer: asyncio.StreamWriter, args: StackTraceArguments
    ) -> StackTraceResponse:
        """Sends a stackTrace request."""
        resp = await self.send_request(
            writer, "stackTrace", dataclass_to_dict(args)
        )
        return from_dict(StackTraceResponse, resp.get("body", {}))

    async def continue_thread(
        self, writer: asyncio.StreamWriter, args: ContinueArguments
    ) -> Dict[str, Any]:
        """Sends a continue request."""
        return await self.send_request(
            writer, "continue", dataclass_to_dict(args)
        )

    async def pause_thread(
        self, writer: asyncio.StreamWriter, args: PauseArguments
    ) -> Dict[str, Any]:
        """Sends a pause request."""
        return await self.send_request(writer, "pause", dataclass_to_dict(args))

    async def threads(self, writer: asyncio.StreamWriter) -> ThreadsResponse:
        """Sends a threads request."""
        resp = await self.send_request(writer, "threads")
        return from_dict(ThreadsResponse, resp.get("body", {}))

    async def _read_message(
        self, reader: asyncio.StreamReader
    ) -> Optional[Dict[str, Any]]:
        """Reads a single message from the reader, handling protocol framing."""
        content_length = None
        while True:
            line = await reader.readline()
            if not line:
                return None  # EOF
            trimmed = line.decode("utf-8").strip()
            if not trimmed:
                break  # End of headers

            if trimmed.startswith("Content-Length:"):
                parts = trimmed.split(":")
                if len(parts) >= 2:
                    try:
                        content_length = int(parts[1].strip())
                    except ValueError:
                        raise DapError(f"Invalid Content-Length: {trimmed}")

        if content_length is None:
            raise DapError("Missing Content-Length header")

        body = await reader.readexactly(content_length)
        return json.loads(body.decode("utf-8"))

    async def _write_message(
        self, writer: asyncio.StreamWriter, value: Dict[str, Any]
    ) -> None:
        """Writes a message to the writer, handling protocol framing."""
        content = json.dumps(value, separators=(",", ":")).encode("utf-8")
        header = f"Content-Length: {len(content)}\r\n\r\n".encode("utf-8")
        writer.write(header)
        writer.write(content)
        await writer.drain()
