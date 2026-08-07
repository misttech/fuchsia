# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydap.models import StackTraceArguments
from shared.protocol import Response
from shared.protocol.stack_trace import StackTraceRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "stackTrace"


def collapse_elided_frames(
    frames: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    result = []
    i = 0
    n = len(frames)
    while i < n:
        frame = frames[i]
        if frame.get("presentationHint") == "subtle":
            start_i = i
            group_origin = (frame.get("source") or {}).get("origin")
            while (
                i + 1 < n
                and frames[i + 1].get("presentationHint") == "subtle"
                and (frames[i + 1].get("source") or {}).get("origin")
                == group_origin
            ):
                i += 1
            end_i = i
            if start_i == end_i:
                result.append(frame)
            else:
                desc = group_origin or "subtle frames"
                collapsed_frame = dict(frames[start_i])
                collapsed_frame[
                    "name"
                ] = f"{start_i}…{end_i} «{desc}» (-r expands)"
                result.append(collapsed_frame)
        else:
            result.append(frame)
        i += 1
    return result


async def handle(daemon: Daemon, req: StackTraceRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        await daemon.ensure_stopped(req.thread_id)

        stack_resp = await daemon.dap_client.stack_trace(
            StackTraceArguments(
                threadId=req.thread_id,
            ),
        )

        body = stack_resp.body.model_dump(by_alias=True)
        if not req.raw:
            body["stackFrames"] = collapse_elided_frames(body["stackFrames"])
        return Response(
            success=True,
            body=body,
        )
    except Exception as e:
        return Response(
            success=False, message=f"Failed to get stack trace: {e}"
        )
