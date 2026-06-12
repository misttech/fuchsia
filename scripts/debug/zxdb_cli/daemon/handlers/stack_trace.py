# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import StackTraceArguments
from shared.protocol import Response
from shared.protocol.stack_trace import StackTraceRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "stackTrace"


async def handle(daemon: Daemon, req: StackTraceRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        await daemon.ensure_stopped(req.thread_id)

        # Now thread is paused, get stack trace
        stack_resp = await daemon.dap_client.stack_trace(
            daemon.zxdb_writer,
            StackTraceArguments(
                threadId=req.thread_id,
            ),
        )

        body = (
            stack_resp.body.model_dump(by_alias=True)
            if stack_resp.body
            else None
        )
        return Response(
            success=True,
            body=body,
        )
    except Exception as e:
        return Response(
            success=False, message=f"Failed to get stack trace: {e}"
        )
