# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import EvaluateArguments, StackTraceArguments
from shared.protocol.base import Response
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "evaluate"


async def handle(daemon: Daemon, req: EvaluateRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        if req.thread_id not in daemon.stopped_threads:
            return Response(
                success=False,
                message=(
                    'Thread not stopped, use "fx debug cli pause '
                    f'{req.thread_id}" to stop the thread, or set a breakpoint '
                    'with "fx debug cli break".'
                ),
            )

        # TODO(https://fxbug.dev/524209338): Paginate stack_trace call to avoid unbounded IPC.
        # Retrieve stack trace to get the frameId at the requested frame_index
        stack_resp = await daemon.dap_client.stack_trace(
            daemon.zxdb_writer,
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
                    f"Frame index {req.frame_index} out of range (stack has"
                    f" {len(stack_resp.body.stack_frames)} frames)"
                ),
            )

        frame = stack_resp.body.stack_frames[req.frame_index]

        # Execute evaluation
        eval_resp = await daemon.dap_client.evaluate(
            daemon.zxdb_writer,
            EvaluateArguments(
                expression=req.expression,
                context="repl",
                frame_id=frame.id,
            ),
        )

        if not eval_resp.success or not eval_resp.body:
            return Response(
                success=False,
                message=eval_resp.message or "Evaluation failed",
            )

        # TODO(https://fxbug.dev/529329366): Support variablesReference in the evaluate response.
        return Response(
            success=True,
            body=EvaluateResponse(
                result=eval_resp.body.result,
                type=eval_resp.body.type,
            ),
        )

    except Exception as e:
        return Response(
            success=False, message=f"Failed to evaluate expression: {e}"
        )
