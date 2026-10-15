# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydap.client import DapError
from pydap.dap_types import Breakpoint, DataBreakpoint
from pydap.models import (
    DataBreakpointInfoArguments,
    SetDataBreakpointsArguments,
    SetDataBreakpointsResponse,
    StackTraceArguments,
)
from shared.protocol.base import Response
from shared.protocol.get_state import WatchpointInfo
from shared.protocol.watch_request import (
    COMMAND_NAME,
    WatchRequest,
)

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


class _WatchStepError(Exception):
    """A non-DAP precondition of a watch operation failed.

    DAP request failures are reported as `DapError`, whose message already
    names the failing request. This covers the remaining checks, e.g. pausing
    the thread or an out-of-range frame index.
    """


async def _resolve_frame_id(
    daemon: Daemon, thread_id: int, frame_index: int
) -> int:
    """Resolves the DAP frameId for evaluating the watchpoint expression."""
    try:
        await daemon.ensure_stopped(thread_id)
    except Exception as e:
        # ensure_stopped raises DapError from the pause request, or a plain
        # Exception if the thread does not report stopped in time.
        raise _WatchStepError(f"pause thread {thread_id} failed: {e}") from e
    stack_resp = await daemon.dap_client.stack_trace(
        StackTraceArguments(thread_id=thread_id)
    )
    if not stack_resp.body or not stack_resp.body.stack_frames:
        raise _WatchStepError(
            f"stackTrace returned no frames for thread {thread_id}"
        )
    num_frames = len(stack_resp.body.stack_frames)
    if frame_index < 0 or frame_index >= num_frames:
        raise _WatchStepError(
            f"frame index {frame_index} out of range for thread {thread_id}"
            f" (stack has {num_frames} frames)"
        )
    return stack_resp.body.stack_frames[frame_index].id


def _to_dap_breakpoint(wp: WatchpointInfo) -> DataBreakpoint:
    return DataBreakpoint(
        data_id=wp.data_id,
        access_type=wp.access_type,
        condition=wp.condition,
        hit_condition=wp.hit_condition,
    )


def _response_breakpoints(
    set_resp: SetDataBreakpointsResponse,
) -> list[Breakpoint]:
    return set_resp.body.breakpoints if set_resp.body else []


async def _send_watchpoints(
    daemon: Daemon, watchpoints: dict[str, WatchpointInfo]
) -> SetDataBreakpointsResponse:
    """Sends the full watchpoint list to zxdb and records the result.

    setDataBreakpoints replaces all data breakpoints, so the whole list is
    always sent. The client raises `DapError` if zxdb rejects the request.
    """
    set_resp = await daemon.dap_client.set_data_breakpoints(
        SetDataBreakpointsArguments(
            breakpoints=[_to_dap_breakpoint(wp) for wp in watchpoints.values()]
        )
    )
    daemon.set_watchpoints(watchpoints, _response_breakpoints(set_resp))
    return set_resp


async def _handle_clear(daemon: Daemon) -> Response[Any]:
    """Clears all active watchpoints."""
    set_resp = await _send_watchpoints(daemon, {})
    return Response(success=True, body=set_resp.body.dump_dap())


async def _handle_delete(daemon: Daemon, data_id: str) -> Response[Any]:
    """Deletes an active watchpoint matching data_id."""
    if data_id not in daemon.active_watchpoints:
        known = ", ".join(daemon.active_watchpoints) or "none"
        return Response(
            success=False,
            message=(
                f"Watchpoint with data_id '{data_id}' not found"
                f" (active data_ids: {known})"
            ),
        )

    new_watchpoints = {
        k: v for k, v in daemon.active_watchpoints.items() if k != data_id
    }
    set_resp = await _send_watchpoints(daemon, new_watchpoints)
    return Response(success=True, body=set_resp.body.dump_dap())


async def _handle_set(
    daemon: Daemon, req: WatchRequest, expression: str
) -> Response[Any]:
    """Sets a new data breakpoint by querying dataBreakpointInfo and updating active watchpoints."""
    missing = []
    if req.thread_id is None:
        missing.append("thread_id")
    if req.access_type is None:
        missing.append("access_type")
    if missing:
        return Response(
            success=False,
            message=f"{' and '.join(missing)} required when setting a watchpoint",
        )
    assert req.thread_id is not None and req.access_type is not None

    frame_id = await _resolve_frame_id(daemon, req.thread_id, req.frame_index)
    info_resp = await daemon.dap_client.data_breakpoint_info(
        DataBreakpointInfoArguments(name=expression, frame_id=frame_id)
    )
    if not info_resp.body or not info_resp.body.data_id:
        desc = (
            info_resp.body.description
            if info_resp.body and info_resp.body.description
            else "no dataId returned"
        )
        return Response(
            success=False,
            message=f"Cannot set watchpoint on '{expression}': {desc}",
        )

    data_id = info_resp.body.data_id
    description = info_resp.body.description or ""

    new_watchpoints = dict(daemon.active_watchpoints)
    new_watchpoints[data_id] = WatchpointInfo(
        data_id=data_id,
        expression=expression,
        thread_id=req.thread_id,
        frame_index=req.frame_index,
        access_type=req.access_type,
        condition=req.condition,
        hit_condition=req.hit_condition,
    )
    set_resp = await _send_watchpoints(daemon, new_watchpoints)
    body = {
        "dataId": data_id,
        "description": description,
        **set_resp.body.dump_dap(),
    }
    return Response(success=True, body=body)


async def handle(daemon: Daemon, req: WatchRequest) -> Response[Any]:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    name = req.name.strip()
    if req.clear:
        operation = "clear watchpoints"
    elif req.delete:
        operation = "delete watchpoint"
    else:
        operation = f"set watchpoint on '{name}'"

    try:
        if req.clear:
            return await _handle_clear(daemon)

        if req.delete:
            if not name:
                return Response(
                    success=False,
                    message="data_id is required when deleting a watchpoint",
                )
            return await _handle_delete(daemon, name)

        if not name:
            return Response(
                success=False,
                message="Expression is required unless clear=True is set",
            )

        return await _handle_set(daemon, req, name)
    except (DapError, _WatchStepError) as e:
        return Response(success=False, message=f"Failed to {operation}: {e}")
    except Exception as e:
        return Response(
            success=False,
            message=f"Failed to {operation}: {type(e).__name__}: {e}",
        )
