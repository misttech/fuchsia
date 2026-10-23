# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import StackTraceArguments
from shared.protocol.base import Response
from shared.protocol.console import (
    COMMAND_NAME,
    ConsoleRequest,
    ConsoleResponse,
)
from zxdb_dap import ZxdbConsoleCommandArguments

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(
    daemon: Daemon, req: ConsoleRequest
) -> Response[ConsoleResponse]:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    if req.thread_id is None and req.frame_index != 0:
        return Response(
            success=False,
            message="thread_id is required when specifying a non-zero frame_index",
        )

    try:
        frame_id: int | None = None
        if req.thread_id is not None:
            await daemon.ensure_stopped(req.thread_id)

            stack_resp = await daemon.dap_client.stack_trace(
                StackTraceArguments(
                    thread_id=req.thread_id,
                ),
            )

            if not stack_resp.body or not stack_resp.body.stack_frames:
                return Response(success=False, message="No stack frames found")

            if req.frame_index < 0 or req.frame_index >= len(
                stack_resp.body.stack_frames
            ):
                return Response(
                    success=False,
                    message=(
                        f"Frame index {req.frame_index} out of range (stack"
                        f" has {len(stack_resp.body.stack_frames)} frames)"
                    ),
                )

            frame_id = stack_resp.body.stack_frames[req.frame_index].id

        cmd_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=req.input,
                frame_id=frame_id,
            ),
        )

        if not cmd_resp.success or not cmd_resp.body:
            return Response(
                success=False,
                message=cmd_resp.message or "Console command failed",
            )

        return Response(
            success=True,
            body=ConsoleResponse(
                output=cmd_resp.body.output,
            ),
        )

    except Exception as e:
        return Response(
            success=False, message=f"Failed to execute console command: {e}"
        )
