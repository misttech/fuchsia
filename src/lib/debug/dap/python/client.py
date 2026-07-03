# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import logging
from typing import Any

from .dap_types import DapBaseModel
from .models import (
    AttachRequestArguments,
    ContinueArguments,
    DisconnectArguments,
    EvaluateArguments,
    EvaluateResponse,
    InitializeArguments,
    LaunchArguments,
    MessageType,
    PauseArguments,
    Response,
    ScopesArguments,
    ScopesResponse,
    SetBreakpointsArguments,
    SetBreakpointsResponse,
    StackTraceArguments,
    StackTraceResponse,
    ThreadsResponse,
    VariablesArguments,
    VariablesResponse,
)

logger = logging.getLogger(__name__)


class DapError(Exception):
    """Base exception for DAP client errors."""


class DapClient:
    """A client for the Debug Adapter Protocol."""

    def __init__(self) -> None:
        """Initializes the DAP client."""
        self._pending_requests: dict[int, asyncio.Future[Any]] = {}
        self._seq_counter = 1

    async def run(
        self, reader: asyncio.StreamReader, event_queue: asyncio.Queue[Any]
    ) -> None:
        """Runs the client's read loop, processing messages from the reader.

        Args:
            reader: Stream reader to receive messages from the debug adapter.
            event_queue: Queue to put received DAP events into.
        """
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

    async def _send_request(
        self,
        writer: asyncio.StreamWriter,
        command: str,
        arguments: DapBaseModel | None = None,
        timeout: float = 5.0,
    ) -> dict[str, Any]:
        """Sends a request to the debug adapter and waits for the response.

        Args:
            writer: Stream writer to send the request to.
            command: The DAP command name.
            arguments: Optional arguments for the command.
            timeout: Maximum time to wait for response in seconds.

        Returns:
            The response message dictionary from the adapter.

        Raises:
            DapError: If the request times out or framing fails.
        """
        seq = self._seq_counter
        self._seq_counter += 1

        loop = asyncio.get_running_loop()
        fut = loop.create_future()
        self._pending_requests[seq] = fut

        request: dict[str, Any] = {
            "seq": seq,
            "type": MessageType.REQUEST.value,
            "command": command,
        }
        if arguments is not None:
            if not isinstance(arguments, DapBaseModel):
                raise TypeError(
                    f"arguments must be a DapBaseModel, got {type(arguments)}"
                )
            request["arguments"] = arguments.dump_dap()

        await self._write_message(writer, request)
        try:
            resp = await asyncio.wait_for(fut, timeout=timeout)
            if not resp.get("success", True):
                msg = resp.get("message", "Unknown DAP error")
                logger.error(f"DAP request {command} (seq={seq}) failed: {msg}")
                raise DapError(f"DAP request {command} failed: {msg}")
            return resp
        except asyncio.TimeoutError:
            self._pending_requests.pop(seq, None)
            raise DapError(
                f"Request {command} (seq={seq}) timed out after {timeout}s"
            )

    async def initialize(
        self, writer: asyncio.StreamWriter, args: InitializeArguments
    ) -> Response:
        """Sends an initialize request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the initialize request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "initialize", args)
        return Response.model_validate(resp)

    async def disconnect(
        self, writer: asyncio.StreamWriter, args: DisconnectArguments
    ) -> Response:
        """Sends a disconnect request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the disconnect request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "disconnect", args)
        return Response.model_validate(resp)

    async def stack_trace(
        self, writer: asyncio.StreamWriter, args: StackTraceArguments
    ) -> StackTraceResponse:
        """Sends a stackTrace request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the stackTrace request.

        Returns:
            The stackTrace response model.
        """
        resp = await self._send_request(writer, "stackTrace", args)
        return StackTraceResponse.model_validate(resp)

    async def continue_thread(
        self, writer: asyncio.StreamWriter, args: ContinueArguments
    ) -> Response:
        """Sends a continue request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the continue request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "continue", args)
        return Response.model_validate(resp)

    async def pause_thread(
        self, writer: asyncio.StreamWriter, args: PauseArguments
    ) -> Response:
        """Sends a pause request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the pause request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "pause", args)
        return Response.model_validate(resp)

    async def threads(self, writer: asyncio.StreamWriter) -> ThreadsResponse:
        """Sends a threads request.

        Args:
            writer: Stream writer to send the request to.

        Returns:
            The threads response model.
        """
        resp = await self._send_request(writer, "threads")
        return ThreadsResponse.model_validate(resp)

    async def attach(
        self, writer: asyncio.StreamWriter, args: AttachRequestArguments
    ) -> Response:
        """Sends an attach request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the attach request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "attach", args)
        return Response.model_validate(resp)

    async def launch(
        self, writer: asyncio.StreamWriter, args: LaunchArguments
    ) -> Response:
        """Sends a launch request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the launch request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "launch", args)
        return Response.model_validate(resp)

    async def evaluate(
        self, writer: asyncio.StreamWriter, args: EvaluateArguments
    ) -> EvaluateResponse:
        """Sends an evaluate request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the evaluate request.

        Returns:
            The response model.
        """
        resp = await self._send_request(writer, "evaluate", args)
        return EvaluateResponse.model_validate(resp)

    async def scopes(
        self, writer: asyncio.StreamWriter, args: ScopesArguments
    ) -> ScopesResponse:
        """Sends a scopes request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the scopes request.

        Returns:
            The scopes response model.
        """
        resp = await self._send_request(writer, "scopes", args)
        return ScopesResponse.model_validate(resp)

    async def variables(
        self, writer: asyncio.StreamWriter, args: VariablesArguments
    ) -> VariablesResponse:
        """Sends a variables request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the variables request.

        Returns:
            The variables response model.
        """
        resp = await self._send_request(writer, "variables", args)
        return VariablesResponse.model_validate(resp)

    async def set_breakpoints(
        self, writer: asyncio.StreamWriter, args: SetBreakpointsArguments
    ) -> SetBreakpointsResponse:
        """Sends a setBreakpoints request.

        Args:
            writer: Stream writer to send the request to.
            args: Arguments for the setBreakpoints request.

        Returns:
            The setBreakpoints response model.
        """
        resp = await self._send_request(writer, "setBreakpoints", args)
        return SetBreakpointsResponse.model_validate(resp)

    async def _read_message(
        self, reader: asyncio.StreamReader
    ) -> dict[str, Any] | None:
        """Reads a single message from the reader, handling protocol framing.

        Args:
            reader: Stream reader to read from.

        Returns:
            The parsed message dictionary, or None on EOF.

        Raises:
            DapError: If framing headers are invalid or missing.
        """
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
        self, writer: asyncio.StreamWriter, value: dict[str, Any]
    ) -> None:
        """Writes a message to the writer, handling protocol framing.

        Args:
            writer: Stream writer to write to.
            value: The message dictionary to serialize and send.
        """
        content = json.dumps(value, separators=(",", ":")).encode("utf-8")
        header = f"Content-Length: {len(content)}\r\n\r\n".encode("utf-8")
        writer.write(header)
        writer.write(content)
        await writer.drain()
